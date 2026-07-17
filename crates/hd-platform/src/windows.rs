#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io::Write as _;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::FromRawHandle as _;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use hd_core::WorkerIdentityV2;
use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, GENERIC_READ, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE,
    LocalFree, STILL_ACTIVE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_ALWAYS, OPEN_EXISTING,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS, GetCurrentProcess,
    GetExitCodeProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE, TerminateProcess,
};

use crate::{NativeCapabilityProbe, PlatformError};

const DEVICE_PATH_PREFIX: [u16; 4] = [92, 92, 63, 92];
const UNC_PATH_PREFIX: [u16; 8] = [92, 92, 63, 92, 85, 78, 67, 92];
const UNC_PATH_START: [u16; 2] = [92, 92];

pub(crate) fn platform_baseline() -> NativeCapabilityProbe {
    let version = windows_version().unwrap_or((0, 0, 0));
    let supported = (version.0 > 10 || (version.0 == 10 && version.2 >= 22_000))
        && cfg!(target_arch = "x86_64");
    NativeCapabilityProbe {
        supported,
        detail: format!("Windows {}.{}.{}", version.0, version.1, version.2),
        properties: std::collections::BTreeMap::from([
            ("major".to_owned(), version.0.to_string()),
            ("minor".to_owned(), version.1.to_string()),
            ("build".to_owned(), version.2.to_string()),
            ("required".to_owned(), "Windows 11 x64".to_owned()),
        ]),
    }
}

pub(crate) fn hypervisor_available() -> NativeCapabilityProbe {
    use windows_sys::Win32::System::Hypervisor::{
        WHvCapabilityCodeHypervisorPresent, WHvGetCapability,
    };
    let mut present = 0_u32;
    let mut written = 0_u32;
    // SAFETY: all pointers refer to live writable integers and the buffer size matches.
    let result = unsafe {
        WHvGetCapability(
            WHvCapabilityCodeHypervisorPresent,
            (&raw mut present).cast(),
            u32::try_from(std::mem::size_of_val(&present)).unwrap_or(4),
            &raw mut written,
        )
    };
    let supported = result >= 0 && present != 0;
    NativeCapabilityProbe {
        supported,
        detail: if supported {
            "Windows Hypervisor Platform is present".to_owned()
        } else {
            format!("WHPX capability failed: HRESULT {result:#x}")
        },
        properties: std::collections::BTreeMap::from([
            ("present".to_owned(), (present != 0).to_string()),
            ("written_bytes".to_owned(), written.to_string()),
        ]),
    }
}

pub(crate) fn memory_capacity() -> Result<(u64, u64, &'static str), PlatformError> {
    let mut status = MEMORYSTATUSEX {
        dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).map_err(|error| {
            PlatformError::Process(format!("MEMORYSTATUSEX size conversion failed: {error}"))
        })?,
        ..Default::default()
    };
    // SAFETY: `status` is writable, correctly sized, and its required length field is initialized.
    if unsafe { GlobalMemoryStatusEx(&raw mut status) } == 0 {
        return Err(last_identity_error("GlobalMemoryStatusEx"));
    }
    Ok((
        status.ullTotalPhys,
        status.ullAvailPhys,
        "GlobalMemoryStatusEx.ullAvailPhys",
    ))
}

pub(crate) fn current_user_sid() -> Result<String, PlatformError> {
    let mut token = std::ptr::null_mut();
    // SAFETY: the pseudo process handle is valid and token points to writable storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(last_identity_error("OpenProcessToken"));
    }
    let result = (|| {
        let mut bytes = 0_u32;
        // SAFETY: documented sizing call with a null output buffer.
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &raw mut bytes);
        }
        if bytes == 0 {
            return Err(last_identity_error("GetTokenInformation(size)"));
        }
        let words = (bytes as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        // SAFETY: buffer was sized by the API and remains alive while TOKEN_USER is read.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                bytes,
                &raw mut bytes,
            )
        } == 0
        {
            return Err(last_identity_error("GetTokenInformation"));
        }
        // SAFETY: the successful call wrote TOKEN_USER at the start of the aligned buffer.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut sid_text = std::ptr::null_mut();
        // SAFETY: the SID pointer is valid for the buffer lifetime and sid_text is writable.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut sid_text) } == 0 {
            return Err(last_identity_error("ConvertSidToStringSidW"));
        }
        let mut length = 0;
        // SAFETY: the API returned a null-terminated LocalAlloc UTF-16 string.
        while unsafe { *sid_text.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: the string contains `length` initialized UTF-16 code units.
        let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_text, length) });
        // SAFETY: ConvertSidToStringSidW requires LocalFree for this allocation.
        unsafe {
            LocalFree(sid_text.cast());
        }
        Ok(sid)
    })();
    // SAFETY: token was opened above and is closed exactly once.
    unsafe {
        CloseHandle(token);
    }
    result
}

pub(crate) fn process_start_marker(pid: u32) -> Result<String, PlatformError> {
    // SAFETY: OpenProcess is called with a numeric PID and no inherited handle.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(last_identity_error("OpenProcess"));
    }
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all FILETIME outputs point to initialized writable storage.
    let ok = unsafe {
        GetProcessTimes(
            process,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    // SAFETY: process is a real handle opened above and is closed exactly once.
    unsafe {
        CloseHandle(process);
    }
    if ok == 0 {
        return Err(last_identity_error("GetProcessTimes"));
    }
    let marker = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    Ok(format!("{marker:016x}"))
}

pub(crate) fn process_identity_is_alive(identity: &WorkerIdentityV2) -> bool {
    // SAFETY: OpenProcess is called with a numeric PID and no inherited handle.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, identity.pid) };
    if process.is_null() {
        return false;
    }
    let mut exit_code = 0_u32;
    // SAFETY: process is live for the call and exit_code is writable.
    let active = unsafe { GetExitCodeProcess(process, &raw mut exit_code) } != 0
        && exit_code == STILL_ACTIVE.cast_unsigned();
    // SAFETY: process is closed exactly once.
    unsafe {
        CloseHandle(process);
    }
    active
        && process_start_marker(identity.pid)
            .is_ok_and(|marker| marker == identity.process_start_marker)
}

pub(crate) fn terminate_process_identity(identity: &WorkerIdentityV2) -> Result<(), PlatformError> {
    // SAFETY: OpenProcess is called with a numeric PID and no inherited handle.
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            0,
            identity.pid,
        )
    };
    if process.is_null() {
        return Err(last_identity_error("OpenProcess(terminate)"));
    }
    let result = (|| {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: process is a live handle and all outputs are writable.
        if unsafe {
            GetProcessTimes(
                process,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        } == 0
        {
            return Err(last_identity_error("GetProcessTimes(terminate)"));
        }
        let marker = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        if format!("{marker:016x}") != identity.process_start_marker {
            return Err(PlatformError::Identity(
                "refusing to terminate a reused process id".to_owned(),
            ));
        }
        // SAFETY: the verified process handle was opened with PROCESS_TERMINATE.
        if unsafe { TerminateProcess(process, 1) } == 0 {
            return Err(last_identity_error("TerminateProcess"));
        }
        Ok(())
    })();
    // SAFETY: process is closed exactly once after all synchronous operations.
    unsafe {
        CloseHandle(process);
    }
    result
}

pub(crate) fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), PlatformError> {
    let sid = current_user_sid()?;
    let sddl = format!("D:P(A;;GA;;;{sid})");
    let sddl_wide = wide(&sddl);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: input is null-terminated, output pointer is writable, and the returned descriptor
    // is released with LocalFree after SetFileSecurityW.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_identity_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        ));
    }
    let path_wide = wide_path(path)?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|error| PlatformError::Identity(error.to_string()))?,
        lpSecurityDescriptor: descriptor.cast(),
        bInheritHandle: 0,
    };
    // SAFETY: the path, attributes and descriptor remain valid through synchronous creation.
    // CREATE_NEW prevents following or overwriting an attacker-controlled destination.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_WRITE,
            0,
            &raw mut attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    let create_error = if handle == INVALID_HANDLE_VALUE {
        // SAFETY: GetLastError must be captured before LocalFree can change the thread-local code.
        Some(unsafe { GetLastError() })
    } else {
        None
    };
    // SAFETY: descriptor came from LocalAlloc through the conversion API.
    unsafe {
        LocalFree(descriptor.cast());
    }
    if let Some(code) = create_error {
        return Err(PlatformError::Io {
            operation: "create owner-only file",
            path: path.to_owned(),
            source: std::io::Error::from_raw_os_error(code.cast_signed()),
        });
    }
    // SAFETY: CreateFileW returned an owned file handle which is transferred exactly once.
    let mut file = unsafe { std::fs::File::from_raw_handle(handle.cast()) };
    file.write_all(bytes).map_err(|source| PlatformError::Io {
        operation: "write owner-only file",
        path: path.to_owned(),
        source,
    })?;
    file.sync_all().map_err(|source| PlatformError::Io {
        operation: "sync owner-only file",
        path: path.to_owned(),
        source,
    })
}

pub(crate) fn ensure_owner_only_directory(path: &Path) -> Result<(), PlatformError> {
    let absolute = std::path::absolute(path).map_err(|source| PlatformError::Io {
        operation: "resolve secure directory path",
        path: path.to_owned(),
        source,
    })?;
    reject_reparse_ancestors(&absolute)?;
    let mut missing = Vec::new();
    let mut cursor = absolute.as_path();
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(PlatformError::Process(format!(
                        "secure directory path is not a real directory: {}",
                        cursor.display()
                    )));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(cursor.to_owned());
                cursor = cursor.parent().ok_or_else(|| {
                    PlatformError::Process("secure directory path has no ancestor".to_owned())
                })?;
            }
            Err(source) => {
                return Err(PlatformError::Io {
                    operation: "inspect secure directory",
                    path: cursor.to_owned(),
                    source,
                });
            }
        }
    }
    for directory in missing.iter().rev() {
        create_owner_only_directory(directory)?;
        let metadata =
            std::fs::symlink_metadata(directory).map_err(|source| PlatformError::Io {
                operation: "verify secure directory",
                path: directory.clone(),
                source,
            })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(PlatformError::Process(format!(
                "secure directory creation was replaced: {}",
                directory.display()
            )));
        }
    }
    restrict_path_to_owner(&absolute, true)
}

fn reject_reparse_ancestors(path: &Path) -> Result<(), PlatformError> {
    use std::os::windows::fs::FileTypeExt as _;

    let mut cursor = Some(path);
    while let Some(candidate) = cursor {
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    || metadata.file_type().is_symlink_dir()
                    || metadata.file_type().is_symlink_file() =>
            {
                return Err(PlatformError::Process(format!(
                    "secure path traverses a reparse link: {}",
                    candidate.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(PlatformError::Io {
                    operation: "inspect secure path ancestor",
                    path: candidate.to_owned(),
                    source,
                });
            }
        }
        cursor = candidate.parent();
    }
    Ok(())
}

pub(crate) fn open_owner_only_rw(path: &Path) -> Result<std::fs::File, PlatformError> {
    use std::os::windows::fs::FileTypeExt as _;

    let sid = current_user_sid()?;
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})");
    let (descriptor, mut attributes) = security_attributes(&sddl)?;
    let path_wide = wide_path(path)?;
    // SAFETY: all pointers remain live during synchronous creation; OPEN_REPARSE_POINT prevents
    // transparently opening a substituted junction or symbolic-link target.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &raw mut attributes,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    let create_error = if handle == INVALID_HANDLE_VALUE {
        // SAFETY: capture before freeing the descriptor.
        Some(unsafe { GetLastError() })
    } else {
        None
    };
    // SAFETY: descriptor was allocated by the SDDL conversion routine.
    unsafe {
        LocalFree(descriptor.cast());
    }
    if let Some(code) = create_error {
        return Err(PlatformError::Io {
            operation: "open owner-only file",
            path: path.to_owned(),
            source: std::io::Error::from_raw_os_error(code.cast_signed()),
        });
    }
    // SAFETY: the owned handle is transferred exactly once to File.
    let file = unsafe { std::fs::File::from_raw_handle(handle.cast()) };
    let metadata = file.metadata().map_err(|source| PlatformError::Io {
        operation: "inspect owner-only file handle",
        path: path.to_owned(),
        source,
    })?;
    let file_type = metadata.file_type();
    if !file_type.is_file()
        || file_type.is_symlink()
        || file_type.is_symlink_dir()
        || file_type.is_symlink_file()
    {
        return Err(PlatformError::Process(format!(
            "owner-only path is not a regular file: {}",
            path.display()
        )));
    }
    restrict_path_to_owner(path, false)?;
    Ok(file)
}

pub(crate) fn open_regular_read_nofollow(path: &Path) -> Result<std::fs::File, PlatformError> {
    use std::os::windows::fs::FileTypeExt as _;

    let path_wide = wide_path(path)?;
    // SAFETY: the path is live and null terminated. OPEN_REPARSE_POINT ensures the returned
    // handle describes the named object rather than a substituted target.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_io_error(
            "open regular file without following links",
            path,
        ));
    }
    // SAFETY: the owned handle is transferred exactly once to File.
    let file = unsafe { std::fs::File::from_raw_handle(handle.cast()) };
    let metadata = file.metadata().map_err(|source| PlatformError::Io {
        operation: "inspect regular file handle",
        path: path.to_owned(),
        source,
    })?;
    let file_type = metadata.file_type();
    if !file_type.is_file()
        || file_type.is_symlink()
        || file_type.is_symlink_dir()
        || file_type.is_symlink_file()
    {
        return Err(PlatformError::Process(format!(
            "path is not a regular non-reparse file: {}",
            path.display()
        )));
    }
    Ok(file)
}

fn create_owner_only_directory(path: &Path) -> Result<(), PlatformError> {
    let sid = current_user_sid()?;
    let sddl = format!("D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;{sid})");
    let (descriptor, mut attributes) = security_attributes(&sddl)?;
    let path_wide = wide_path(path)?;
    // SAFETY: path and security attributes remain valid for the synchronous call.
    let created = unsafe { CreateDirectoryW(path_wide.as_ptr(), &raw mut attributes) };
    let create_error = if created == 0 {
        // SAFETY: capture before freeing the descriptor.
        Some(unsafe { GetLastError() })
    } else {
        None
    };
    // SAFETY: descriptor was allocated by the SDDL conversion routine.
    unsafe {
        LocalFree(descriptor.cast());
    }
    if let Some(code) = create_error {
        if std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        {
            return restrict_path_to_owner(path, true);
        }
        return Err(PlatformError::Io {
            operation: "create secure directory",
            path: path.to_owned(),
            source: std::io::Error::from_raw_os_error(code.cast_signed()),
        });
    }
    Ok(())
}

fn restrict_path_to_owner(path: &Path, directory: bool) -> Result<(), PlatformError> {
    let sid = current_user_sid()?;
    let sddl = if directory {
        format!("D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;{sid})")
    } else {
        format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})")
    };
    let sddl_wide = wide(&sddl);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the SDDL is null terminated and the output pointer is valid.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_identity_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW(path)",
        ));
    }
    let path_wide = wide_path(path)?;
    // SAFETY: descriptor and path are live during the synchronous update.
    let updated =
        unsafe { SetFileSecurityW(path_wide.as_ptr(), DACL_SECURITY_INFORMATION, descriptor) };
    let update_error = if updated == 0 {
        // SAFETY: capture before freeing the descriptor.
        Some(unsafe { GetLastError() })
    } else {
        None
    };
    // SAFETY: descriptor was allocated by the SDDL conversion routine.
    unsafe {
        LocalFree(descriptor.cast());
    }
    if let Some(code) = update_error {
        return Err(PlatformError::Io {
            operation: "restrict path ACL",
            path: path.to_owned(),
            source: std::io::Error::from_raw_os_error(code.cast_signed()),
        });
    }
    Ok(())
}

fn security_attributes(
    sddl: &str,
) -> Result<(PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES), PlatformError> {
    let sddl_wide = wide(sddl);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: input is null terminated and output is writable.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_identity_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW(attributes)",
        ));
    }
    let length = u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).map_err(|error| {
        // SAFETY: descriptor was allocated by the SDDL conversion routine.
        unsafe {
            LocalFree(descriptor.cast());
        }
        PlatformError::Identity(error.to_string())
    })?;
    Ok((
        descriptor,
        SECURITY_ATTRIBUTES {
            nLength: length,
            lpSecurityDescriptor: descriptor.cast(),
            bInheritHandle: 0,
        },
    ))
}

pub(crate) fn create_owner_only_named_pipe(
    options: &tokio::net::windows::named_pipe::ServerOptions,
    endpoint: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, PlatformError> {
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    let sid = current_user_sid()?;
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})");
    let sddl_wide = wide(&sddl);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the SDDL is null-terminated and descriptor is a valid writable output pointer.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_identity_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW(named pipe)",
        ));
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|error| PlatformError::Identity(error.to_string()))?,
        lpSecurityDescriptor: descriptor.cast(),
        bInheritHandle: 0,
    };
    // SAFETY: attributes and its descriptor remain valid during CreateNamedPipeW; tokio does not
    // retain the pointer after this synchronous creation call.
    let result = unsafe {
        options.create_with_security_attributes_raw(endpoint, (&raw mut attributes).cast())
    };
    // SAFETY: descriptor was allocated by LocalAlloc through the conversion API.
    unsafe {
        LocalFree(descriptor.cast());
    }
    result.map_err(|source| PlatformError::Io {
        operation: "create owner-only named pipe",
        path: PathBuf::from(endpoint),
        source,
    })
}

pub(crate) fn replace_file(source: &Path, destination: &Path) -> Result<(), PlatformError> {
    let source_wide = wide_path(source)?;
    let destination_wide = wide_path(destination)?;
    // SAFETY: both paths are live, null-terminated and reside in the same directory.  The
    // replace+write-through flags provide one atomic commit boundary for readers.
    if unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(last_io_error("commit replacement file", destination));
    }
    Ok(())
}

pub(crate) fn configure_detached(command: &mut Command) {
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

pub(crate) fn configure_managed(command: &mut Command) {
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[derive(Debug)]
pub(crate) struct WindowsJob {
    job: windows_sys::Win32::Foundation::HANDLE,
    process_id: u32,
}

// SAFETY: the job handle is only closed on Drop; Win32 job handles may be transferred between
// threads and all operations performed by this type are synchronized by the kernel.
unsafe impl Send for WindowsJob {}
// SAFETY: shared references expose only the immutable PID; the owned handle closes once on Drop.
unsafe impl Sync for WindowsJob {}

impl WindowsJob {
    pub(crate) const fn process_id(&self) -> u32 {
        self.process_id
    }
}

impl Drop for WindowsJob {
    fn drop(&mut self) {
        // SAFETY: job is a real handle created by CreateJobObjectW and closed exactly once.
        unsafe {
            CloseHandle(self.job);
        }
    }
}

pub(crate) fn contain_process(pid: u32) -> Result<WindowsJob, PlatformError> {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    // SAFETY: null security/name pointers request an unnamed job with default security.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(last_identity_error("CreateJobObjectW"));
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: limits points to the correct structure for the selected information class.
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            u32::try_from(std::mem::size_of_val(&limits)).unwrap_or(u32::MAX),
        )
    } == 0
    {
        // SAFETY: job is owned and must be closed on the failure path.
        unsafe {
            CloseHandle(job);
        }
        return Err(last_identity_error("SetInformationJobObject"));
    }
    // SAFETY: OpenProcess is called for a numeric PID with the access required by job assignment.
    let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
    if process.is_null() {
        // SAFETY: job is owned and must be closed on the failure path.
        unsafe {
            CloseHandle(job);
        }
        return Err(last_identity_error("OpenProcess(job assignment)"));
    }
    // SAFETY: both handles are valid for the synchronous assignment call.
    let assigned = unsafe { AssignProcessToJobObject(job, process) };
    // SAFETY: process is a temporary handle and is closed exactly once.
    unsafe {
        CloseHandle(process);
    }
    if assigned == 0 {
        // SAFETY: job is owned and must be closed on the failure path.
        unsafe {
            CloseHandle(job);
        }
        return Err(last_identity_error("AssignProcessToJobObject"));
    }
    Ok(WindowsJob {
        job,
        process_id: pid,
    })
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(path: &Path) -> Result<Vec<u16>, PlatformError> {
    let absolute = std::path::absolute(path).map_err(|source| PlatformError::Io {
        operation: "resolve absolute Windows path",
        path: path.to_owned(),
        source,
    })?;
    let raw = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut extended = Vec::with_capacity(raw.len().saturating_add(9));
    if raw.starts_with(&DEVICE_PATH_PREFIX) {
        extended.extend_from_slice(&raw);
    } else if raw.starts_with(&UNC_PATH_START) {
        extended.extend_from_slice(&UNC_PATH_PREFIX);
        extended.extend_from_slice(&raw[2..]);
    } else {
        extended.extend_from_slice(&DEVICE_PATH_PREFIX);
        extended.extend_from_slice(&raw);
    }
    extended.push(0);
    Ok(extended)
}

fn last_io_error(operation: &'static str, path: &Path) -> PlatformError {
    // SAFETY: GetLastError has no preconditions and is called immediately after the failed API.
    let code = unsafe { GetLastError() };
    PlatformError::Io {
        operation,
        path: path.to_owned(),
        source: std::io::Error::from_raw_os_error(code.cast_signed()),
    }
}

fn last_identity_error(operation: &str) -> PlatformError {
    // SAFETY: GetLastError has no preconditions.
    let code = unsafe { GetLastError() };
    PlatformError::Identity(format!("{operation} failed with Windows error {code}"))
}

fn windows_version() -> Option<(u32, u32, u32)> {
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
    }
    let mut info = OsVersionInfo {
        size: u32::try_from(std::mem::size_of::<OsVersionInfo>()).ok()?,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // SAFETY: info is correctly sized and writable for the synchronous OS call.
    (unsafe { RtlGetVersion(&raw mut info) } >= 0).then_some((info.major, info.minor, info.build))
}
