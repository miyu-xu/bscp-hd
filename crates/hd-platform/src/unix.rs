#![allow(unsafe_code)]

use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use hd_core::WorkerIdentityV2;

use crate::{NativeCapabilityProbe, PlatformError};

pub(crate) fn platform_baseline() -> NativeCapabilityProbe {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let values = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.trim_matches('"').to_owned()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let supported = values.get("ID").is_some_and(|value| value == "ubuntu")
            && values
                .get("VERSION_ID")
                .is_some_and(|value| value == "24.04")
            && cfg!(target_arch = "x86_64");
        NativeCapabilityProbe {
            supported,
            detail: format!(
                "{} {}",
                values.get("ID").map_or("unknown", String::as_str),
                values.get("VERSION_ID").map_or("unknown", String::as_str)
            ),
            properties: std::collections::BTreeMap::from([
                (
                    "session".to_owned(),
                    std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".to_owned()),
                ),
                ("required".to_owned(), "Ubuntu 24.04 x64".to_owned()),
            ]),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let version = macos_product_version().unwrap_or_else(|| "unknown".to_owned());
        let major = version
            .split('.')
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        NativeCapabilityProbe {
            supported: major >= 15 && cfg!(target_arch = "aarch64"),
            detail: format!("macOS {version}"),
            properties: std::collections::BTreeMap::from([
                ("version".to_owned(), version),
                ("required".to_owned(), "macOS 15+ Apple Silicon".to_owned()),
            ]),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        NativeCapabilityProbe {
            supported: false,
            detail: "unsupported Unix platform".to_owned(),
            properties: std::collections::BTreeMap::new(),
        }
    }
}

pub(crate) fn hypervisor_available() -> NativeCapabilityProbe {
    #[cfg(target_os = "linux")]
    {
        let result = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm");
        let supported = result.is_ok();
        NativeCapabilityProbe {
            supported,
            detail: result.map_or_else(
                |error| format!("open /dev/kvm: {error}"),
                |_| "KVM is accessible".to_owned(),
            ),
            properties: std::collections::BTreeMap::from([(
                "device".to_owned(),
                "/dev/kvm".to_owned(),
            )]),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let supported = macos_sysctl_u32("kern.hv_support").is_some_and(|value| value != 0);
        NativeCapabilityProbe {
            supported,
            detail: if supported {
                "Hypervisor.framework is supported".to_owned()
            } else {
                "kern.hv_support is unavailable or zero".to_owned()
            },
            properties: std::collections::BTreeMap::new(),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        NativeCapabilityProbe {
            supported: false,
            detail: "unsupported Unix hypervisor".to_owned(),
            properties: std::collections::BTreeMap::new(),
        }
    }
}

pub(crate) fn memory_capacity() -> Result<(u64, u64, &'static str), PlatformError> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: sysconf accepts these scalar selectors and retains no pointer.
        let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
        // SAFETY: sysconf accepts these scalar selectors and retains no pointer.
        let available_pages = unsafe { libc::sysconf(libc::_SC_AVPHYS_PAGES) };
        // SAFETY: sysconf accepts these scalar selectors and retains no pointer.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if pages <= 0 || available_pages <= 0 || page_size <= 0 {
            return Err(PlatformError::Process(format!(
                "sysconf memory query failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let page_size = u64::try_from(page_size)
            .map_err(|error| PlatformError::Process(format!("page size overflow: {error}")))?;
        let total = u64::try_from(pages)
            .ok()
            .and_then(|value| value.checked_mul(page_size))
            .ok_or_else(|| PlatformError::Process("total memory size overflow".to_owned()))?;
        let available = u64::try_from(available_pages)
            .ok()
            .and_then(|value| value.checked_mul(page_size))
            .ok_or_else(|| PlatformError::Process("available memory size overflow".to_owned()))?;
        Ok((total, available, "sysconf(_SC_AVPHYS_PAGES)"))
    }
    #[cfg(target_os = "macos")]
    {
        let total = macos_sysctl_u64("hw.memsize")
            .ok_or_else(|| PlatformError::Process("sysctl hw.memsize is unavailable".to_owned()))?;
        // Darwin does not expose an atomic, stable "available" value. A three-quarter budget
        // leaves deterministic space for the host, graphics stack and compressed-memory swings.
        let available = total.saturating_mul(3) / 4;
        Ok((total, available, "sysctl(hw.memsize)*75% budget"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(PlatformError::Unsupported("host memory capacity"))
    }
}

pub(crate) fn current_user_scope() -> String {
    // SAFETY: geteuid has no preconditions and retains no pointer.
    unsafe { libc::geteuid() }.to_string()
}

pub(crate) fn ensure_open_file_limit(minimum: u64) -> Result<(), PlatformError> {
    #[cfg(target_pointer_width = "64")]
    let requested: libc::rlim_t = minimum;
    #[cfg(not(target_pointer_width = "64"))]
    let requested: libc::rlim_t = minimum
        .try_into()
        .map_err(|error| PlatformError::Process(format!("open-file limit overflow: {error}")))?;
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: limit points to writable storage for one rlimit value.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(PlatformError::Process(format!(
            "query open-file limit: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: getrlimit succeeded and initialized the value.
    let mut limit = unsafe { limit.assume_init() };
    if limit.rlim_cur >= requested {
        return Ok(());
    }
    if limit.rlim_max < requested {
        return Err(PlatformError::Process(format!(
            "open-file hard limit {} is below required {minimum}",
            limit.rlim_max
        )));
    }
    limit.rlim_cur = requested;
    // SAFETY: limit contains values returned by getrlimit with only rlim_cur raised to rlim_max.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) } != 0 {
        return Err(PlatformError::Process(format!(
            "raise open-file limit to {minimum}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

pub(crate) fn process_start_marker(pid: u32) -> Result<String, PlatformError> {
    #[cfg(target_os = "linux")]
    {
        let path = std::path::PathBuf::from(format!("/proc/{pid}/stat"));
        let text = std::fs::read_to_string(&path).map_err(|source| PlatformError::Io {
            operation: "read Linux process start marker",
            path,
            source,
        })?;
        let closing = text
            .rfind(')')
            .ok_or_else(|| PlatformError::Identity("invalid /proc stat record".to_owned()))?;
        let fields = text[closing + 1..].split_whitespace().collect::<Vec<_>>();
        let start_time = fields
            .get(19)
            .ok_or_else(|| PlatformError::Identity("missing /proc start time".to_owned()))?;
        Ok((*start_time).to_owned())
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let expected = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
            .map_err(|error| PlatformError::Identity(error.to_string()))?;
        // SAFETY: the buffer is correctly sized for PROC_PIDTBSDINFO and remains live.
        let written = unsafe {
            libc::proc_pidinfo(
                pid.cast_signed(),
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                expected,
            )
        };
        if written != expected {
            return Err(PlatformError::Identity(format!(
                "proc_pidinfo({pid}) returned {written}, expected {expected}"
            )));
        }
        // SAFETY: proc_pidinfo reported that it initialized the full structure.
        let info = unsafe { info.assume_init() };
        Ok(format!(
            "{:016x}-{:06x}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(PlatformError::Unsupported("process identity"))
    }
}

pub(crate) fn process_identity_is_alive(identity: &WorkerIdentityV2) -> bool {
    // SAFETY: kill(pid, 0) sends no signal and is the documented existence/permission probe.
    let alive = unsafe { libc::kill(identity.pid.cast_signed(), 0) } == 0;
    alive
        && process_start_marker(identity.pid)
            .is_ok_and(|marker| marker == identity.process_start_marker)
}

pub(crate) fn terminate_process_identity(identity: &WorkerIdentityV2) -> Result<(), PlatformError> {
    if !process_identity_is_alive(identity) {
        return Err(PlatformError::Identity(
            "refusing to terminate a stale or reused process identity".to_owned(),
        ));
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: pidfd_open receives only the verified numeric pid and returns a new fd.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid.cast_signed(), 0) };
        if pidfd < 0 {
            return Err(PlatformError::Identity(format!(
                "pidfd_open failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let pidfd = i32::try_from(pidfd)
            .map_err(|error| PlatformError::Identity(format!("invalid pidfd: {error}")))?;
        // Recheck after acquiring the stable kernel handle, before signaling it.
        if process_start_marker(identity.pid)? != identity.process_start_marker {
            // SAFETY: pidfd is owned by this function.
            unsafe { libc::close(pidfd) };
            return Err(PlatformError::Identity(
                "refusing to terminate a reused process id".to_owned(),
            ));
        }
        // SAFETY: pidfd is a live pidfd and null siginfo requests a normal SIGTERM.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd,
                libc::SIGTERM,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        // SAFETY: pidfd is owned by this function and closed exactly once.
        unsafe { libc::close(pidfd) };
        if result != 0 {
            return Err(PlatformError::Identity(format!(
                "pidfd_send_signal failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        // macOS has no pidfd. The start-time recheck immediately precedes the signal and prevents
        // normal PID-reuse mistakes; kill receives no pointer and targets only this numeric PID.
        if process_start_marker(identity.pid)? != identity.process_start_marker {
            return Err(PlatformError::Identity(
                "refusing to terminate a reused process id".to_owned(),
            ));
        }
        // SAFETY: the PID and signal are valid scalar values.
        if unsafe { libc::kill(identity.pid.cast_signed(), libc::SIGTERM) } != 0 {
            return Err(PlatformError::Identity(format!(
                "kill(SIGTERM) failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(PlatformError::Unsupported("terminate process identity"))
    }
}

pub(crate) fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), PlatformError> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(path).map_err(|source| PlatformError::Io {
        operation: "create owner-only file",
        path: path.to_owned(),
        source,
    })?;
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
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    let absolute = std::path::absolute(path).map_err(|source| PlatformError::Io {
        operation: "resolve secure directory path",
        path: path.to_owned(),
        source,
    })?;
    reject_symlink_ancestors(&absolute)?;
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
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(PlatformError::Io {
                    operation: "create secure directory",
                    path: directory.clone(),
                    source,
                });
            }
        }
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
    std::fs::set_permissions(&absolute, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        PlatformError::Io {
            operation: "restrict secure directory permissions",
            path: absolute,
            source,
        }
    })
}

fn reject_symlink_ancestors(path: &Path) -> Result<(), PlatformError> {
    let mut cursor = Some(path);
    while let Some(candidate) = cursor {
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(PlatformError::Process(format!(
                    "secure path traverses a symbolic link: {}",
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
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path).map_err(|source| PlatformError::Io {
        operation: "open owner-only file",
        path: path.to_owned(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| PlatformError::Io {
        operation: "inspect owner-only file handle",
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(PlatformError::Process(format!(
            "owner-only path is not a regular file: {}",
            path.display()
        )));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|source| PlatformError::Io {
            operation: "restrict owner-only file permissions",
            path: path.to_owned(),
            source,
        })?;
    Ok(file)
}

pub(crate) fn open_regular_read_nofollow(path: &Path) -> Result<std::fs::File, PlatformError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path).map_err(|source| PlatformError::Io {
        operation: "open regular file without following links",
        path: path.to_owned(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| PlatformError::Io {
        operation: "inspect regular file handle",
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(PlatformError::Process(format!(
            "path is not a regular non-symlink file: {}",
            path.display()
        )));
    }
    Ok(file)
}

pub(crate) fn create_owner_only_fifo(path: &Path) -> Result<(), PlatformError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::FileTypeExt as _;

    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_fifo() {
            std::fs::remove_file(path).map_err(|source| PlatformError::Io {
                operation: "remove stale input FIFO",
                path: path.to_owned(),
                source,
            })?;
        } else {
            return Err(PlatformError::Process(format!(
                "refusing to replace non-FIFO path {}",
                path.display()
            )));
        }
    }
    let path_c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| PlatformError::Process("FIFO path contains a NUL byte".to_owned()))?;
    // SAFETY: path_c is a live, null-terminated pathname and mode contains permission bits only.
    if unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) } != 0 {
        return Err(PlatformError::Io {
            operation: "create owner-only input FIFO",
            path: path.to_owned(),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

pub(crate) fn replace_file(source: &Path, destination: &Path) -> Result<(), PlatformError> {
    std::fs::rename(source, destination).map_err(|source_error| PlatformError::Io {
        operation: "commit replacement file",
        path: destination.to_owned(),
        source: source_error,
    })?;
    let parent = destination.parent().ok_or_else(|| {
        PlatformError::Process("replacement destination has no parent directory".to_owned())
    })?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| PlatformError::Io {
            operation: "sync replacement directory",
            path: parent.to_owned(),
            source,
        })
}

pub(crate) fn configure_detached(command: &mut Command) {
    command.process_group(0);
}

#[derive(Debug)]
pub(crate) struct UnixContainment {
    process_id: u32,
}

impl UnixContainment {
    pub(crate) const fn process_id(&self) -> u32 {
        self.process_id
    }
}

impl Drop for UnixContainment {
    fn drop(&mut self) {
        let Ok(process_group) = i32::try_from(self.process_id) else {
            return;
        };
        // SAFETY: managed children are created as process-group leaders. The leader can exit
        // before one of its descendants (for example, a launcher whose runtime child is still
        // alive), and getpgid(leader_pid) then fails even though the original process group still
        // exists. killpg targets that containment group directly and harmlessly returns ESRCH
        // after the full tree has already exited.
        unsafe {
            let _ = libc::killpg(process_group, libc::SIGKILL);
        }
    }
}

pub(crate) const fn contain_process(pid: u32) -> UnixContainment {
    UnixContainment { process_id: pid }
}

pub(crate) fn configure_managed(command: &mut Command) {
    command.process_group(0);
    #[cfg(target_os = "macos")]
    let open_max = {
        // Query this before fork. The pre-exec watchdog path must use only async-signal-safe
        // syscalls and must close every inherited descriptor except its private kqueue.
        let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        if limit > 0 { limit } else { 1024 }
    };
    // SAFETY: the closure only invokes process-control syscalls before exec and captures no
    // allocation-backed state.
    unsafe {
        command.pre_exec(move || {
            #[cfg(target_os = "linux")]
            {
                let parent = libc::getppid();
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::raise(libc::SIGKILL);
                }
            }
            #[cfg(target_os = "macos")]
            {
                configure_macos_managed_watchdog(open_max)?;
            }
            Ok(())
        });
    }
}

#[cfg(target_os = "macos")]
fn configure_macos_managed_watchdog(open_max: i64) -> std::io::Result<()> {
    // SAFETY: this runs after fork and before exec, using only async-signal-safe process and fd
    // syscalls. The watchdog child never returns into Rust runtime code.
    unsafe {
        // std applies Command::process_group after user pre_exec callbacks on Darwin. Establish
        // the group before forking so the runtime and watchdog inherit the same containment.
        if libc::setpgid(0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let worker_pid = libc::getppid();
        let worker_ident = libc::uintptr_t::try_from(worker_pid)
            .map_err(|_| std::io::Error::from_raw_os_error(libc::ESRCH))?;
        let mut handshake_pipe = [-1_i32; 2];
        if libc::pipe(handshake_pipe.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let monitor_pid = libc::fork();
        if monitor_pid < 0 {
            let error = std::io::Error::last_os_error();
            libc::close(handshake_pipe[0]);
            libc::close(handshake_pipe[1]);
            return Err(error);
        }
        if monitor_pid == 0 {
            macos_parent_watchdog_main(worker_pid, worker_ident, handshake_pipe, open_max);
        }
        libc::close(handshake_pipe[1]);
        let mut handshake = [0_u8];
        let mut read_result = libc::read(
            handshake_pipe[0],
            handshake.as_mut_ptr().cast(),
            handshake.len(),
        );
        while read_result < 0 && *libc::__error() == libc::EINTR {
            read_result = libc::read(
                handshake_pipe[0],
                handshake.as_mut_ptr().cast(),
                handshake.len(),
            );
        }
        libc::close(handshake_pipe[0]);
        if read_result != 1 || handshake[0] != 1 {
            libc::kill(monitor_pid, libc::SIGKILL);
            return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
        }
        if libc::getppid() != worker_pid {
            libc::kill(monitor_pid, libc::SIGKILL);
            libc::raise(libc::SIGKILL);
            return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn macos_parent_watchdog_main(
    worker_pid: libc::pid_t,
    worker_ident: libc::uintptr_t,
    handshake_pipe: [i32; 2],
    open_max: i64,
) -> ! {
    // SAFETY: this is the single-threaded post-fork watchdog path. It uses only scalar syscalls,
    // stack values and _exit, and never returns to allocation-backed runtime state.
    unsafe {
        libc::close(handshake_pipe[0]);
        // Darwin kqueues do not survive fork, so the watchdog creates and registers its own.
        let queue = libc::kqueue();
        if queue < 0 {
            watchdog_handshake_exit(handshake_pipe[1]);
        }
        let change = libc::kevent {
            ident: worker_ident,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        if libc::kevent(
            queue,
            &raw const change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        ) < 0
        {
            watchdog_handshake_exit(handshake_pipe[1]);
        }
        let ready = [1_u8];
        if libc::write(handshake_pipe[1], ready.as_ptr().cast(), 1) != 1 {
            libc::_exit(127);
        }
        libc::close(handshake_pipe[1]);
        if libc::kill(worker_pid, 0) != 0 && *libc::__error() == libc::ESRCH {
            libc::killpg(libc::getpgrp(), libc::SIGKILL);
            libc::_exit(0);
        }
        // Close standard streams too: a managed capability probe commonly pipes stdout/stderr,
        // and retaining either writer would prevent its reader from ever observing EOF.
        let mut fd = 0;
        while i64::from(fd) < open_max {
            if fd != queue {
                libc::close(fd);
            }
            fd += 1;
        }
        let mut event: libc::kevent = std::mem::zeroed();
        loop {
            let result = libc::kevent(
                queue,
                std::ptr::null(),
                0,
                &raw mut event,
                1,
                std::ptr::null(),
            );
            if result > 0 {
                libc::killpg(libc::getpgrp(), libc::SIGKILL);
                libc::_exit(0);
            }
            if result < 0 && *libc::__error() == libc::EINTR {
                continue;
            }
            libc::_exit(127);
        }
    }
}

#[cfg(target_os = "macos")]
fn watchdog_handshake_exit(handshake_fd: i32) -> ! {
    // SAFETY: the descriptor belongs to the watchdog, the byte is stack-backed for the write,
    // and _exit prevents returning into post-fork runtime state.
    unsafe {
        let failed = [0_u8];
        libc::write(handshake_fd, failed.as_ptr().cast(), 1);
        libc::_exit(127);
    }
}

pub(crate) fn configure_transient(command: &mut Command) {
    command.process_group(0);
}

#[cfg(target_os = "macos")]
fn macos_product_version() -> Option<String> {
    macos_sysctl_string("kern.osproductversion")
}

#[cfg(target_os = "macos")]
fn macos_sysctl_u32(name: &str) -> Option<u32> {
    use std::ffi::CString;
    let name = CString::new(name).ok()?;
    let mut value = 0_u32;
    let mut size = std::mem::size_of::<u32>();
    // SAFETY: name is null-terminated and output buffers match their declared size.
    (unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    } == 0)
        .then_some(value)
}

#[cfg(target_os = "macos")]
fn macos_sysctl_u64(name: &str) -> Option<u64> {
    use std::ffi::CString;
    let name = CString::new(name).ok()?;
    let mut value = 0_u64;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: name is null-terminated and output buffers match their declared size.
    let succeeded = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    (succeeded && size == std::mem::size_of::<u64>()).then_some(value)
}

#[cfg(target_os = "macos")]
fn macos_sysctl_string(name: &str) -> Option<String> {
    use std::ffi::CString;
    let name = CString::new(name).ok()?;
    let mut size = 0_usize;
    // SAFETY: documented sizing call with no output buffer.
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size == 0
    {
        return None;
    }
    let mut bytes = vec![0_u8; size];
    // SAFETY: bytes is allocated to the size returned by the sizing call.
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes).ok()
}
