#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, c_void};
use std::io::Write as _;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use hd_core::{DisplayViewportV2, NativeDisplayTargetV2, WorkerIdentityV2};
use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, GENERIC_READ, GENERIC_WRITE, GetLastError, HWND, INVALID_HANDLE_VALUE,
    LocalFree, RECT, STILL_ACTIVE, SetLastError,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, GetWindowRgn, NULLREGION, SetWindowRgn,
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
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    DETACHED_PROCESS, GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess, GetProcessTimes,
    OpenProcess, OpenProcessToken, OpenThread, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_TERMINATE, ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, EnumChildWindows, EnumWindows, GWL_STYLE, GetClassNameW,
    GetClientRect, GetParent, GetPropW, GetWindowLongPtrW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindow, SMTO_ABORTIFHUNG, SMTO_BLOCK, SW_HIDE, SW_SHOW,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    SendMessageTimeoutW, SetParent, SetPropW, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_USER,
    WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_POPUP, WS_VISIBLE,
};

use crate::{NativeCapabilityProbe, PlatformError};

const DEVICE_PATH_PREFIX: [u16; 4] = [92, 92, 63, 92];
const UNC_PATH_PREFIX: [u16; 8] = [92, 92, 63, 92, 85, 78, 67, 92];
const UNC_PATH_START: [u16; 2] = [92, 92];
// crosvm's Windows gpu_display WndProc uses this private message to resize gfxstream's `subWin`,
// rebuild its Vulkan display surface, and update the host-to-guest pointer projection. A regular
// SetWindowPos only resizes the outer CROSVM HWND and leaves all three of those states stale.
const WM_USER_HOST_VIEWPORT_CHANGE_INTERNAL: u32 = WM_USER + 1;
const FORCE_VIEWPORT_COMMIT_WPARAM: usize = 1 << 31;
const WM_USER_SELECT_DISPLAY_INTERNAL: u32 = WM_USER + 3;
const NATIVE_DISPLAY_TOPLEVEL_ENV: &str = "HD_DEV_NATIVE_DISPLAY_TOPLEVEL";
const DISPLAY_PARKING_TITLE: &str = "HD crosvm display parking";
const NATIVE_DISPLAY_TOPLEVEL_X: i32 = 280;
const NATIVE_DISPLAY_TOPLEVEL_Y: i32 = 64;

const fn wide_ascii_z<const N: usize>(value: &[u8; N]) -> [u16; N] {
    let mut output = [0_u16; N];
    let mut index = 0;
    while index < N {
        assert!(value[index].is_ascii());
        output[index] = value[index] as u16;
        index += 1;
    }
    assert!(N > 0 && output[N - 1] == 0);
    output
}

const LATEST_VIEWPORT_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_LATEST_VIEWPORT_V1\0");
const LATEST_ROTATION_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_LATEST_ROTATION_V1\0");
const FORCED_VIEWPORT_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_FORCED_VIEWPORT_V1\0");
const FORCED_ROTATION_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_FORCED_ROTATION_V1\0");
const FORCE_VIEWPORT_PENDING_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_FORCE_VIEWPORT_PENDING_V1\0");
const GFXSTREAM_APPLIED_VIEWPORT_PROPERTY_W: &[u16] =
    &wide_ascii_z(b"HD_GFXSTREAM_APPLIED_VIEWPORT_V1\0");
const NATIVE_DISPLAY_ROTATION_PROPERTY_W: &[u16] =
    &wide_ascii_z(b"HD_NATIVE_DISPLAY_ROTATION_V1\0");

fn native_display_toplevel_value_enabled(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| {
        !value.is_empty() && value != "0" && !value.to_string_lossy().eq_ignore_ascii_case("false")
    })
}

fn native_input_visual_region_is_empty(input: HWND) -> bool {
    // SAFETY: the temporary region is owned by this function, is passed only to a synchronous
    // inspection call, and is deleted before returning.
    unsafe {
        let probe = CreateRectRgn(0, 0, 0, 0);
        if probe.is_null() {
            return false;
        }
        let region_type = GetWindowRgn(input, probe);
        let _ = DeleteObject(probe);
        region_type == NULLREGION
    }
}

fn make_native_input_visual_region_empty(input: HWND) -> Result<(), PlatformError> {
    if native_input_visual_region_is_empty(input) {
        return Ok(());
    }
    // The CROSVM HWND carries Win32 input and the authoritative host-to-guest transform; the
    // sibling gfxstream subWin is the only visual surface. Give the input HWND an empty visual
    // region so its opaque black class brush can never cover the retained Vulkan surface during a
    // transient cross-process sibling/focus transition. Its client rectangle, message queue,
    // SetCapture behavior and EventDevice route remain intact.
    unsafe {
        let region = CreateRectRgn(0, 0, 0, 0);
        if region.is_null() {
            return Err(last_identity_error(
                "CreateRectRgn(crosvm input-only region)",
            ));
        }
        if SetWindowRgn(input, region, 0) == 0 {
            let _ = DeleteObject(region);
            return Err(last_identity_error(
                "SetWindowRgn(crosvm input-only region)",
            ));
        }
        // On success Windows owns `region` until the region is replaced or the HWND is destroyed.
    }
    Ok(())
}

pub(crate) fn native_display_toplevel_debug_enabled() -> bool {
    native_display_toplevel_value_enabled(std::env::var_os(NATIVE_DISPLAY_TOPLEVEL_ENV).as_deref())
}

pub fn mark_file_sparse(file: &std::fs::File, path: &Path) -> Result<(), PlatformError> {
    let mut returned = 0_u32;
    // SAFETY: the handle remains owned by `file` for this synchronous call. FSCTL_SET_SPARSE
    // accepts no input or output buffer, and the return value is checked.
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        return Err(PlatformError::Io {
            operation: "mark expanded overlay as filesystem sparse",
            path: path.to_owned(),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

struct CrosvmWindowSearch {
    pids: HashSet<u32>,
    scanout_id: u32,
    hwnd: HWND,
    client_area: u64,
    exact_scanout: bool,
}

unsafe fn window_matches_crosvm(hwnd: HWND, search: &CrosvmWindowSearch) -> bool {
    let mut pid = 0_u32;
    // SAFETY: hwnd is supplied by Win32 enumeration and pid points to writable storage.
    unsafe { GetWindowThreadProcessId(hwnd, &raw mut pid) };
    if !search.pids.contains(&pid) {
        return false;
    }
    let mut class_name = [0_u16; 128];
    let Ok(class_name_capacity) = i32::try_from(class_name.len()) else {
        return false;
    };
    // SAFETY: the buffer is writable and its element count is exact.
    let length = unsafe { GetClassNameW(hwnd, class_name.as_mut_ptr(), class_name_capacity) };
    let Ok(length) = usize::try_from(length) else {
        return false;
    };
    if length == 0 || !String::from_utf16_lossy(&class_name[..length]).starts_with("CROSVM") {
        return false;
    }
    true
}

unsafe fn window_title(hwnd: HWND) -> String {
    let mut title = [0_u16; 128];
    let Ok(capacity) = i32::try_from(title.len()) else {
        return String::new();
    };
    // SAFETY: hwnd is supplied by Win32 enumeration and title is a writable UTF-16 buffer.
    let length = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), capacity) };
    let Ok(length) = usize::try_from(length) else {
        return String::new();
    };
    String::from_utf16_lossy(&title[..length])
}

unsafe extern "system" fn find_crosvm_child_callback(hwnd: HWND, lparam: isize) -> i32 {
    // SAFETY: EnumWindows synchronously invokes this callback with the pointer supplied below.
    let search = unsafe { &mut *(lparam as *mut CrosvmWindowSearch) };
    if unsafe { window_matches_crosvm(hwnd, search) } {
        let exact_scanout =
            unsafe { window_title(hwnd).ends_with(&format!("-scanout-{}", search.scanout_id)) };
        let mut rect = RECT::default();
        // SAFETY: hwnd came from Win32 enumeration and rect is writable for this synchronous call.
        let area = if unsafe { GetClientRect(hwnd, &raw mut rect) } != 0 {
            u64::try_from(rect.right.saturating_sub(rect.left))
                .unwrap_or_default()
                .saturating_mul(
                    u64::try_from(rect.bottom.saturating_sub(rect.top)).unwrap_or_default(),
                )
        } else {
            0
        };
        // crosvm pre-creates one 1x1 CROSVM window for every possible scanout. The active guest
        // scanout is the only large window. If surface creation has not resized it yet, keep the
        // last equal-sized candidate: EnumWindows visits the newest scanouts first, while scanout
        // zero (the primary display) was created first and is therefore visited last.
        // A legacy single-display runtime has no scanout suffix; retain the area fallback only for
        // scanout zero. Secondary displays must be selected by exact crosvm-owned identity.
        let legacy_primary = search.scanout_id == 0 && !search.exact_scanout;
        if exact_scanout || (legacy_primary && area >= search.client_area) {
            search.hwnd = hwnd;
            search.client_area = area;
            search.exact_scanout = exact_scanout;
        }
    }
    1
}

unsafe extern "system" fn find_crosvm_root_callback(hwnd: HWND, lparam: isize) -> i32 {
    // SAFETY: EnumWindows synchronously invokes this callback with the pointer supplied below.
    {
        let search = unsafe { &mut *(lparam as *mut CrosvmWindowSearch) };
        if unsafe { window_matches_crosvm(hwnd, search) } {
            // Reuse the same ranking logic for top-level and already attached child windows.
            unsafe { find_crosvm_child_callback(hwnd, lparam) };
        }
    }
    // Once attached, the CROSVM window is no longer returned by EnumWindows. Search every root's
    // descendant tree so resize and detach continue to address the same HWND after SetParent.
    unsafe { EnumChildWindows(hwnd, Some(find_crosvm_child_callback), lparam) };
    1
}

fn crosvm_window(pid: u32, scanout_id: u32) -> Result<HWND, PlatformError> {
    let pids = process_tree(pid)?;
    let mut search = CrosvmWindowSearch {
        pids,
        scanout_id,
        hwnd: std::ptr::null_mut(),
        client_area: 0,
        exact_scanout: false,
    };
    // SAFETY: the pointer remains valid for the synchronous enumeration.
    unsafe {
        EnumWindows(Some(find_crosvm_root_callback), (&raw mut search) as isize);
    }
    if search.hwnd.is_null() {
        Err(PlatformError::Process(format!(
            "crosvm GUI window for scanout {scanout_id} was not found for process {pid}"
        )))
    } else {
        Ok(search.hwnd)
    }
}

struct GfxstreamWindowSearch {
    pids: HashSet<u32>,
    hwnd: HWND,
}

unsafe extern "system" fn find_gfxstream_window_callback(hwnd: HWND, lparam: isize) -> i32 {
    // SAFETY: EnumChildWindows synchronously invokes this callback with the pointer supplied by
    // gfxstream_window below.
    let search = unsafe { &mut *(lparam as *mut GfxstreamWindowSearch) };
    let mut pid = 0_u32;
    // SAFETY: hwnd is supplied by Win32 and pid points to writable storage.
    unsafe { GetWindowThreadProcessId(hwnd, &raw mut pid) };
    if !search.pids.contains(&pid) {
        return 1;
    }
    let mut class_name = [0_u16; 64];
    let length = unsafe {
        GetClassNameW(
            hwnd,
            class_name.as_mut_ptr(),
            i32::try_from(class_name.len()).unwrap_or(64),
        )
    };
    let Ok(length) = usize::try_from(length) else {
        return 1;
    };
    let class_name = String::from_utf16_lossy(&class_name[..length]);
    if class_name == "subWin" || class_name == "vulkan-subWin" {
        search.hwnd = hwnd;
        return 0;
    }
    1
}

unsafe extern "system" fn find_gfxstream_root_callback(hwnd: HWND, lparam: isize) -> i32 {
    // Check both a top-level window and all of its descendants. gfxstream is normally a child,
    // but detach must not depend on the external viewport still being valid.
    if unsafe { find_gfxstream_window_callback(hwnd, lparam) } == 0 {
        return 0;
    }
    unsafe { EnumChildWindows(hwnd, Some(find_gfxstream_window_callback), lparam) };
    1
}

fn gfxstream_window_anywhere(child_pid: u32) -> Result<HWND, PlatformError> {
    let mut search = GfxstreamWindowSearch {
        pids: process_tree(child_pid)?,
        hwnd: std::ptr::null_mut(),
    };
    // SAFETY: the pointer remains live for synchronous desktop window enumeration.
    unsafe {
        EnumWindows(
            Some(find_gfxstream_root_callback),
            (&raw mut search) as isize,
        );
    }
    if search.hwnd.is_null() {
        Err(PlatformError::Process(
            "gfxstream native render surface is not ready".to_owned(),
        ))
    } else {
        Ok(search.hwnd)
    }
}

fn process_tree(root_pid: u32) -> Result<HashSet<u32>, PlatformError> {
    // SAFETY: ToolHelp returns an owned snapshot handle and writes only into the sized entry.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(last_identity_error(
            "CreateToolhelp32Snapshot(process tree)",
        ));
    }
    let mut parents = HashMap::new();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = u32::try_from(std::mem::size_of::<PROCESSENTRY32W>())
        .map_err(|_| PlatformError::Process("PROCESSENTRY32W size overflow".to_owned()))?;
    // SAFETY: snapshot is valid and entry has the required dwSize.
    let mut available = unsafe { Process32FirstW(snapshot, &raw mut entry) } != 0;
    while available {
        parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        // SAFETY: same valid snapshot and entry are reused for the next record.
        available = unsafe { Process32NextW(snapshot, &raw mut entry) } != 0;
    }
    // SAFETY: snapshot is owned by this function and no longer used.
    unsafe { CloseHandle(snapshot) };

    let mut descendants = HashSet::from([root_pid]);
    loop {
        let previous_len = descendants.len();
        for (candidate, parent) in &parents {
            if descendants.contains(parent) {
                descendants.insert(*candidate);
            }
        }
        if descendants.len() == previous_len {
            break;
        }
    }
    Ok(descendants)
}

fn display_parking_window() -> Result<HWND, PlatformError> {
    static PARKING_WINDOW: OnceLock<usize> = OnceLock::new();
    if let Some(hwnd) = PARKING_WINDOW.get().copied() {
        let hwnd = hwnd as HWND;
        // SAFETY: the value was produced by CreateWindowExW in this process.
        if unsafe { IsWindow(hwnd) } != 0 {
            return Ok(hwnd);
        }
        return Err(PlatformError::Process(
            "Windows display parking window became invalid".to_owned(),
        ));
    }
    // SAFETY: STATIC is a system class. The window is intentionally hidden and process-owned; it
    // only keeps the cross-process crosvm child alive between short-lived Player viewports.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide(DISPLAY_PARKING_TITLE).as_ptr(),
            WS_POPUP | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(last_identity_error("CreateWindowExW(display parking)"));
    }
    if PARKING_WINDOW.set(hwnd as usize).is_err() {
        // Another worker thread won the one-time creation race. Use that live window and release
        // this unused hidden candidate.
        unsafe { DestroyWindow(hwnd) };
    }
    PARKING_WINDOW
        .get()
        .copied()
        .map(|hwnd| hwnd as HWND)
        .ok_or_else(|| PlatformError::Process("Windows display parking state is empty".to_owned()))
}

fn windows_target(
    target: &NativeDisplayTargetV2,
) -> Result<(HWND, &WorkerIdentityV2), PlatformError> {
    match target {
        NativeDisplayTargetV2::WindowsHwnd { hwnd, owner, .. } => {
            let raw_hwnd = usize::try_from(*hwnd).map_err(|_| {
                PlatformError::Identity(
                    "Player native display target exceeds pointer width".to_owned(),
                )
            })?;
            let hwnd = raw_hwnd as HWND;
            if hwnd.is_null() || !process_identity_is_alive(owner) {
                return Err(PlatformError::Identity(
                    "Player window owner is not alive".to_owned(),
                ));
            }
            // SAFETY: IsWindow and GetWindowThreadProcessId only inspect the numeric handle.
            if unsafe { IsWindow(hwnd) } == 0 {
                return Err(PlatformError::Identity(
                    "Player native display target is not a window".to_owned(),
                ));
            }
            let mut owner_pid = 0_u32;
            // SAFETY: owner_pid points to writable storage.
            unsafe { GetWindowThreadProcessId(hwnd, &raw mut owner_pid) };
            if owner_pid != owner.pid {
                return Err(PlatformError::Identity(
                    "Player native display target owner mismatch".to_owned(),
                ));
            }
            let mut owner_session = 0_u32;
            let mut worker_session = 0_u32;
            // The native HWND is valid only in the interactive desktop session of this Worker.
            // This rejects a PID from a different RDP/console session even when its identity is
            // otherwise alive under the same user account.
            if unsafe { ProcessIdToSessionId(owner.pid, &raw mut owner_session) } == 0
                || unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut worker_session) }
                    == 0
                || owner_session != worker_session
            {
                return Err(PlatformError::Identity(
                    "Player native display target is outside the Worker desktop session".to_owned(),
                ));
            }
            Ok((hwnd, owner))
        }
        NativeDisplayTargetV2::MacCaContext { .. } => Err(PlatformError::Identity(
            "Windows display requires an HWND target".to_owned(),
        )),
    }
}

fn win32_style(value: u32) -> isize {
    isize::try_from(value.cast_signed())
        .unwrap_or_else(|_| unreachable!("Win32 LONG style must fit LONG_PTR"))
}

fn window_has_visible_style(window: HWND) -> bool {
    // IsWindowVisible folds ancestor visibility into its result. The Player intentionally hides
    // NativeDisplayHost while minimized or covered by another HD page, so using it for a child
    // heartbeat would falsely report both retained crosvm windows as hidden every five seconds.
    unsafe { GetWindowLongPtrW(window, GWL_STYLE) & win32_style(WS_VISIBLE) != 0 }
}

#[derive(Clone, Copy)]
struct NativeWindowSnapshot {
    window: HWND,
    parent: HWND,
    style: isize,
    visible: bool,
    input_visual_region_empty: Option<bool>,
}

fn snapshot_native_window(window: HWND) -> NativeWindowSnapshot {
    NativeWindowSnapshot {
        window,
        parent: unsafe { GetParent(window) },
        style: unsafe { GetWindowLongPtrW(window, GWL_STYLE) },
        visible: window_has_visible_style(window),
        input_visual_region_empty: None,
    }
}

fn snapshot_native_input_window(window: HWND) -> NativeWindowSnapshot {
    NativeWindowSnapshot {
        input_visual_region_empty: Some(native_input_visual_region_is_empty(window)),
        ..snapshot_native_window(window)
    }
}

fn restore_native_display_siblings(
    snapshots: [NativeWindowSnapshot; 2],
) -> Result<(), PlatformError> {
    let mut failures = Vec::new();
    for snapshot in snapshots {
        unsafe {
            ShowWindow(snapshot.window, SW_HIDE);
            SetLastError(0);
            if SetParent(snapshot.window, snapshot.parent).is_null() && GetLastError() != 0 {
                failures.push(last_identity_error("rollback native display parent").to_string());
            }
            SetWindowLongPtrW(snapshot.window, GWL_STYLE, snapshot.style);
            if SetWindowPos(
                snapshot.window,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED
                    | SWP_NOMOVE
                    | SWP_NOSIZE
                    | SWP_NOACTIVATE
                    | SWP_NOOWNERZORDER
                    | SWP_NOZORDER,
            ) == 0
            {
                failures.push(last_identity_error("rollback native display style").to_string());
            }
            if GetParent(snapshot.window) != snapshot.parent {
                failures.push("rollback native display parent verification failed".to_owned());
            }
            match snapshot.input_visual_region_empty {
                Some(true) => {
                    if let Err(error) = make_native_input_visual_region_empty(snapshot.window) {
                        failures.push(format!(
                            "rollback native input visual region failed: {error}"
                        ));
                    }
                }
                Some(false) => {
                    if SetWindowRgn(snapshot.window, std::ptr::null_mut(), 1) == 0 {
                        failures
                            .push("rollback native input full visual region failed".to_owned());
                    }
                }
                None => {}
            }
            ShowWindow(
                snapshot.window,
                if snapshot.visible { SW_SHOW } else { SW_HIDE },
            );
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(PlatformError::Process(failures.join("; ")))
    }
}

fn viewport_dimensions(viewport: &DisplayViewportV2) -> Result<(i32, i32), PlatformError> {
    let width = i32::try_from(viewport.width_px).map_err(|_| {
        PlatformError::Process("native display width exceeds Win32 bounds".to_owned())
    })?;
    let height = i32::try_from(viewport.height_px).map_err(|_| {
        PlatformError::Process("native display height exceeds Win32 bounds".to_owned())
    })?;
    Ok((width, height))
}

fn embedded_viewport_dimensions(
    parent: HWND,
    viewport: &DisplayViewportV2,
) -> Result<(i32, i32), PlatformError> {
    let requested = viewport_dimensions(viewport)?;
    let mut parent_rect = RECT::default();
    // The Player owns the target HWND and commits its WebView/titlebar layout before the
    // asynchronous display-session request is observed by the Worker. Treat that live client
    // extent as authoritative whenever it is usable. This prevents an older heartbeat from
    // interpreting Windows' automatic child resize as drift and replaying the previous
    // orientation over a newer UI transaction.
    if unsafe { GetClientRect(parent, &raw mut parent_rect) } != 0 {
        let width = parent_rect.right.saturating_sub(parent_rect.left);
        let height = parent_rect.bottom.saturating_sub(parent_rect.top);
        if width > 1 && height > 1 {
            return Ok((width, height));
        }
    }
    Ok(requested)
}

fn notify_crosvm_viewport_impl(
    child: HWND,
    width: i32,
    height: i32,
    rotation_quarters: u8,
    force_commit: bool,
) -> Result<bool, PlatformError> {
    let width = u16::try_from(width).map_err(|_| {
        PlatformError::Process("native display width exceeds crosvm message bounds".to_owned())
    })?;
    let height = u16::try_from(height).map_err(|_| {
        PlatformError::Process("native display height exceeds crosvm message bounds".to_owned())
    })?;
    let packed = (u32::from(height) << 16) | u32::from(width);
    let lparam = isize::try_from(packed)
        .map_err(|_| PlatformError::Process("native display viewport packing failed".to_owned()))?;
    let property = LATEST_VIEWPORT_PROPERTY_W;
    // SAFETY: property is NUL-terminated and stores only the nonzero packed dimensions. crosvm
    // resolves queued notifications through this latest value so stale resize messages cannot
    // revert a newer Vulkan extent.
    if unsafe { SetPropW(child, property.as_ptr(), lparam as *mut c_void) } == 0 {
        return Err(last_identity_error("SetPropW(crosvm latest viewport)"));
    }
    let rotation = usize::from((rotation_quarters & 3) + 1);
    let rotation_property = LATEST_ROTATION_PROPERTY_W;
    // SAFETY: the nonzero value encodes canonical Android rotation as one through four. Keeping
    // it beside the latest viewport lets crosvm resolve a queued resize to one coherent geometry
    // and input transaction after an orientation change.
    if unsafe { SetPropW(child, rotation_property.as_ptr(), rotation as *mut c_void) } == 0 {
        return Err(last_identity_error("SetPropW(crosvm latest rotation)"));
    }
    if force_commit {
        let force_property = FORCE_VIEWPORT_PENDING_PROPERTY_W;
        if unsafe { SetPropW(child, force_property.as_ptr(), lparam as *mut c_void) } == 0 {
            return Err(last_identity_error(
                "SetPropW(crosvm force viewport pending)",
            ));
        }
    }
    let mut message_result = 0_usize;
    // SAFETY: child is the validated crosvm HWND. Crosvm is the sole geometry owner for both its
    // input HWND and gfxstream's Vulkan HWND; wait for that single transaction to finish before
    // allowing DWM to compose the new HD layout. A bounded timeout keeps the UI recoverable if
    // the renderer is stalled.
    if unsafe {
        SendMessageTimeoutW(
            child,
            WM_USER_HOST_VIEWPORT_CHANGE_INTERNAL,
            rotation
                | if force_commit {
                    FORCE_VIEWPORT_COMMIT_WPARAM
                } else {
                    0
                },
            lparam,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            1_000,
            &raw mut message_result,
        )
    } == 0
    {
        return Err(last_identity_error("SendMessageTimeoutW(crosvm viewport)"));
    }
    // A zero result means the HWND is alive but its Surface is temporarily parked in crosvm's
    // vacant dispatcher map. The latest-value property remains authoritative and is replayed when
    // the Surface attaches. A result of one is the applied acknowledgement; reject other values so
    // protocol drift does not silently reintroduce redundant swapchain rebuilds.
    if message_result > 1 {
        return Err(PlatformError::Process(format!(
            "crosvm returned an invalid viewport acknowledgement: {message_result}"
        )));
    }
    if message_result == 1 {
        // These legacy-named properties are the cross-thread projection acknowledgement, not a
        // record that the force bit was used. Publishing every successful atomic projection keeps
        // the asynchronous Worker from forcing the same viewport after an ordinary UI rotation.
        let forced_viewport_property = FORCED_VIEWPORT_PROPERTY_W;
        if unsafe {
            SetPropW(
                child,
                forced_viewport_property.as_ptr(),
                lparam as *mut c_void,
            )
        } == 0
        {
            return Err(last_identity_error("publish forced crosvm viewport"));
        }
        let forced_rotation_property = FORCED_ROTATION_PROPERTY_W;
        if unsafe {
            SetPropW(
                child,
                forced_rotation_property.as_ptr(),
                rotation as *mut c_void,
            )
        } == 0
        {
            return Err(last_identity_error("publish forced crosvm rotation"));
        }
    }
    Ok(message_result == 1)
}

pub(crate) fn notify_crosvm_viewport(
    child: HWND,
    width: i32,
    height: i32,
    rotation_quarters: u8,
) -> Result<(), PlatformError> {
    notify_crosvm_viewport_impl(child, width, height, rotation_quarters, false).map(|_| ())
}

pub(crate) fn force_notify_crosvm_viewport(
    child: HWND,
    width: i32,
    height: i32,
    rotation_quarters: u8,
) -> Result<bool, PlatformError> {
    notify_crosvm_viewport_impl(child, width, height, rotation_quarters, true)
}

fn select_crosvm_display(input: HWND) -> Result<(), PlatformError> {
    let mut message_result = 0_usize;
    // SAFETY: input is the exact validated CROSVM_<scanout> HWND. Crosvm derives the scanout id
    // from that window and returns only after gfxstream has selected and, when available,
    // synchronously reposted the retained frame. Abort rather than hang the UI on a stalled GPU.
    let sent = unsafe {
        SendMessageTimeoutW(
            input,
            WM_USER_SELECT_DISPLAY_INTERNAL,
            0,
            0,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            1_000,
            &raw mut message_result,
        )
    };
    if sent == 0 {
        return Err(last_identity_error(
            "SendMessageTimeoutW(crosvm display selection)",
        ));
    }
    if message_result != 1 && message_result != 2 {
        return Err(PlatformError::Process(
            "crosvm rejected the selected native display scanout".to_owned(),
        ));
    }
    Ok(())
}

fn show_native_display_toplevel(
    child: HWND,
    width: i32,
    height: i32,
    move_to_debug_origin: bool,
) -> Result<(), PlatformError> {
    // SAFETY: child is the validated crosvm HWND. This development-only path changes the same
    // parent/style fields as the production attach/detach path, but leaves the render HWND visible
    // as a top-level popup so HD embedding can be isolated from gfxstream presentation.
    unsafe {
        ShowWindow(child, SW_HIDE);
        SetLastError(0);
        if SetParent(child, std::ptr::null_mut()).is_null() && GetLastError() != 0 {
            return Err(last_identity_error("SetParent(debug top-level display)"));
        }
        if !GetParent(child).is_null() {
            return Err(PlatformError::Process(
                "crosvm debug display parent did not clear".to_owned(),
            ));
        }
        let style = GetWindowLongPtrW(child, GWL_STYLE);
        SetWindowLongPtrW(
            child,
            GWL_STYLE,
            (style & !win32_style(WS_CHILD)) | win32_style(WS_POPUP) | win32_style(WS_CLIPCHILDREN),
        );
        let (x, y, position_flags) = if move_to_debug_origin {
            (NATIVE_DISPLAY_TOPLEVEL_X, NATIVE_DISPLAY_TOPLEVEL_Y, 0)
        } else {
            (0, 0, SWP_NOMOVE | SWP_NOZORDER)
        };
        if SetWindowPos(
            child,
            std::ptr::null_mut(),
            x,
            y,
            width,
            height,
            SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOOWNERZORDER | position_flags,
        ) == 0
        {
            return Err(last_identity_error("SetWindowPos(debug top-level display)"));
        }
        ShowWindow(child, SW_SHOW);
    }
    select_crosvm_display(child)?;
    notify_crosvm_viewport(child, width, height, 0)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn prepare_native_display(
    child_pid: u32,
    target: &NativeDisplayTargetV2,
    viewport: &DisplayViewportV2,
) -> Result<(), PlatformError> {
    if !viewport.is_valid() {
        return Err(PlatformError::Process(
            "native display viewport is outside supported bounds".to_owned(),
        ));
    }
    let (parent, _) = windows_target(target)?;
    let input = crosvm_window(child_pid, target.scanout_id())?;
    let render = gfxstream_window_anywhere(child_pid)?;
    let (width, height) = embedded_viewport_dimensions(parent, viewport)?;
    let rotation_property = NATIVE_DISPLAY_ROTATION_PROPERTY_W;
    let rotation_quarters = (unsafe { GetPropW(parent, rotation_property.as_ptr()) } as usize)
        .checked_sub(1)
        .and_then(|value| u8::try_from(value).ok())
        .filter(|value| *value < 4)
        .unwrap_or_default();
    if !viewport.visible {
        // A minimized native root has no valid client extent. Hide both native surfaces without
        // moving them or notifying gfxstream; rebuilding Intel's swapchain while the root is
        // iconic can return an invalid Win32 surface extent.
        unsafe {
            ShowWindow(input, SW_HIDE);
            if let Ok(render) = gfxstream_window_anywhere(child_pid) {
                ShowWindow(render, SW_HIDE);
            }
        }
        return Ok(());
    }
    let input_parent_changed = unsafe { GetParent(input) != parent };
    let render_parent_changed = unsafe { GetParent(render) != parent };
    let input_was_visible = window_has_visible_style(input);
    let render_was_visible = window_has_visible_style(render);
    let snapshots = [
        snapshot_native_window(render),
        snapshot_native_input_window(input),
    ];
    let mut input_rect = RECT::default();
    let mut render_rect = RECT::default();
    let input_size_changed = unsafe { GetClientRect(input, &raw mut input_rect) } == 0
        || input_rect.right.saturating_sub(input_rect.left) != width
        || input_rect.bottom.saturating_sub(input_rect.top) != height;
    let render_size_changed = unsafe { GetClientRect(render, &raw mut render_rect) } == 0
        || render_rect.right.saturating_sub(render_rect.left) != width
        || render_rect.bottom.saturating_sub(render_rect.top) != height;
    // HD owns only hierarchy and visibility. Crosvm's window thread owns the geometry transaction
    // because it must update input transforms, gfxstream's DisplaySurface and the Win32 Vulkan
    // swapchain together. Directly sizing either child here bypasses that state and caused the
    // observed 1287x724 render / 900x506 input split.
    let prepare_result = (|| -> Result<(), PlatformError> {
        unsafe {
            let input_style = GetWindowLongPtrW(input, GWL_STYLE);
            let expected_input_style = (input_style & !win32_style(WS_POPUP))
                | win32_style(WS_CHILD)
                | win32_style(WS_CLIPCHILDREN)
                | win32_style(WS_CLIPSIBLINGS);
            let input_style_changed = input_style != expected_input_style;
            if input_style_changed {
                SetWindowLongPtrW(input, GWL_STYLE, expected_input_style);
            }
            let render_style = GetWindowLongPtrW(render, GWL_STYLE);
            let expected_render_style = (render_style & !win32_style(WS_POPUP))
                | win32_style(WS_CHILD)
                | win32_style(WS_CLIPCHILDREN)
                | win32_style(WS_CLIPSIBLINGS);
            let render_style_changed = render_style != expected_render_style;
            if render_style_changed {
                SetWindowLongPtrW(render, GWL_STYLE, expected_render_style);
            }

            if input_parent_changed {
                SetLastError(0);
                if SetParent(input, parent).is_null() && GetLastError() != 0 {
                    return Err(last_identity_error("SetParent(crosvm input surface)"));
                }
            }
            if render_parent_changed {
                SetLastError(0);
                if SetParent(render, parent).is_null() && GetLastError() != 0 {
                    return Err(last_identity_error("SetParent(gfxstream render surface)"));
                }
            }

            make_native_input_visual_region_empty(input)?;

            let hierarchy_changed = input_parent_changed || render_parent_changed;
            let geometry_changed = hierarchy_changed
                || input_size_changed
                || render_size_changed
                || input_style_changed
                || render_style_changed;
            let root = GetParent(parent);
            let interactive_resize_property =
                crate::windows_titlebar::ROOT_INTERACTIVE_RESIZE_PROPERTY_W;
            let interactive_resize =
                !root.is_null() && !GetPropW(root, interactive_resize_property.as_ptr()).is_null();
            let width_word = u16::try_from(width).map_err(|_| {
                PlatformError::Process(
                    "native display width exceeds crosvm message bounds".to_owned(),
                )
            })?;
            let height_word = u16::try_from(height).map_err(|_| {
                PlatformError::Process(
                    "native display height exceeds crosvm message bounds".to_owned(),
                )
            })?;
            let expected_forced_viewport = (u32::from(height_word) << 16) | u32::from(width_word);
            let expected_forced_rotation = usize::from((rotation_quarters & 3) + 1);
            let forced_viewport_property = FORCED_VIEWPORT_PROPERTY_W;
            let forced_rotation_property = FORCED_ROTATION_PROPERTY_W;
            let forced_viewport = GetPropW(input, forced_viewport_property.as_ptr()) as usize
                == expected_forced_viewport as usize;
            let forced_rotation = GetPropW(input, forced_rotation_property.as_ptr()) as usize
                == expected_forced_rotation;
            let forced_projection_verified = forced_viewport && forced_rotation;
            let gfxstream_applied_viewport_property = GFXSTREAM_APPLIED_VIEWPORT_PROPERTY_W;
            let gfxstream_projection_verified =
                GetPropW(render, gfxstream_applied_viewport_property.as_ptr()) as usize
                    == expected_forced_viewport as usize;
            if input_style_changed
                && SetWindowPos(
                    input,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED
                        | SWP_NOMOVE
                        | SWP_NOSIZE
                        | SWP_NOACTIVATE
                        | SWP_NOOWNERZORDER
                        | SWP_NOZORDER,
                ) == 0
            {
                return Err(last_identity_error("apply crosvm input child style"));
            }
            if render_style_changed
                && SetWindowPos(
                    render,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED
                        | SWP_NOMOVE
                        | SWP_NOSIZE
                        | SWP_NOACTIVATE
                        | SWP_NOOWNERZORDER
                        | SWP_NOZORDER,
                ) == 0
            {
                return Err(last_identity_error("apply gfxstream render child style"));
            }
            if hierarchy_changed {
                select_crosvm_display(input)?;
            }
            // The Player commits ordinary local layout changes synchronously from
            // NativeDisplayHost::set_bounds. The Worker owns attach/repair and only crosses the
            // crosvm message boundary when hierarchy or observed geometry actually drifted. This
            // keeps a later asynchronous display-session response from replaying an older viewport.
            if (!forced_projection_verified || !gfxstream_projection_verified || geometry_changed)
                && !interactive_resize
            {
                let _ = force_notify_crosvm_viewport(input, width, height, rotation_quarters)?;
            }
            if !input_was_visible {
                ShowWindow(input, SW_SHOW);
            }
            if !render_was_visible {
                ShowWindow(render, SW_SHOW);
            }
        }
        Ok(())
    })();
    if let Err(error) = prepare_result {
        // Attaching two process-owned siblings and committing crosvm/gfxstream projection cannot
        // be one Win32 syscall. Restore the exact pre-attach hierarchy when any later step fails;
        // otherwise a retryable attach error can expose a parent with input but no Vulkan render.
        return match restore_native_display_siblings(snapshots) {
            Ok(()) => Err(error),
            Err(rollback) => Err(PlatformError::Process(format!(
                "native display attach failed ({error}); sibling rollback also failed ({rollback})"
            ))),
        };
    }
    Ok(())
}

pub(crate) fn attach_native_display(
    child_pid: u32,
    target: &NativeDisplayTargetV2,
    viewport: &DisplayViewportV2,
) -> Result<(), PlatformError> {
    if !viewport.is_valid() {
        return Err(PlatformError::Process(
            "native display viewport is outside supported bounds".to_owned(),
        ));
    }
    let (parent, _) = windows_target(target)?;
    let (width, height) = embedded_viewport_dimensions(parent, viewport)?;
    if native_display_toplevel_debug_enabled() {
        let child = crosvm_window(child_pid, target.scanout_id())?;
        return show_native_display_toplevel(child, width, height, true);
    }
    let _ = parent;
    prepare_native_display(child_pid, target, viewport)
}

pub(crate) fn resize_native_display(
    child_pid: u32,
    target: &NativeDisplayTargetV2,
    viewport: &DisplayViewportV2,
) -> Result<(), PlatformError> {
    let (parent, _) = windows_target(target)?;
    let (width, height) = embedded_viewport_dimensions(parent, viewport)?;
    if native_display_toplevel_debug_enabled() {
        let child = crosvm_window(child_pid, target.scanout_id())?;
        return show_native_display_toplevel(child, width, height, false);
    }
    let _ = parent;
    prepare_native_display(child_pid, target, viewport)
}

pub(crate) fn ensure_native_display(
    child_pid: u32,
    target: &NativeDisplayTargetV2,
    viewport: &DisplayViewportV2,
) -> Result<(), PlatformError> {
    if !viewport.is_valid() {
        return Err(PlatformError::Process(
            "native display viewport is outside supported bounds".to_owned(),
        ));
    }
    let (parent, _) = windows_target(target)?;
    let input = crosvm_window(child_pid, target.scanout_id())?;
    let render = gfxstream_window_anywhere(child_pid)?;
    let (width, height) = embedded_viewport_dimensions(parent, viewport)?;
    let expected_visible = i32::from(viewport.visible);
    let expected_forced_viewport = u16::try_from(width)
        .ok()
        .zip(u16::try_from(height).ok())
        .map(|(width, height)| (u32::from(height) << 16) | u32::from(width));
    let rotation_property = NATIVE_DISPLAY_ROTATION_PROPERTY_W;
    let rotation_quarters = (unsafe { GetPropW(parent, rotation_property.as_ptr()) } as usize)
        .checked_sub(1)
        .and_then(|value| u8::try_from(value).ok())
        .filter(|value| *value < 4)
        .unwrap_or_default();
    let forced_viewport_property = FORCED_VIEWPORT_PROPERTY_W;
    let forced_rotation_property = FORCED_ROTATION_PROPERTY_W;
    let gfxstream_applied_viewport_property = GFXSTREAM_APPLIED_VIEWPORT_PROPERTY_W;
    let forced_projection_verified = expected_forced_viewport.is_some_and(|expected| {
        (unsafe { GetPropW(input, forced_viewport_property.as_ptr()) }) as usize
            == expected as usize
            && (unsafe { GetPropW(input, forced_rotation_property.as_ptr()) }) as usize
                == usize::from((rotation_quarters & 3) + 1)
    });
    let mut input_rect = RECT::default();
    let mut render_rect = RECT::default();
    // SAFETY: all HWNDs were validated above. These calls only inspect current Win32 hierarchy,
    // visibility and client extents; they do not mutate the render or input surfaces.
    let in_sync = unsafe {
        GetParent(input) == parent
            && GetParent(render) == parent
            && GetClientRect(input, &raw mut input_rect) != 0
            && GetClientRect(render, &raw mut render_rect) != 0
            && input_rect.right.saturating_sub(input_rect.left) == width
            && input_rect.bottom.saturating_sub(input_rect.top) == height
            && render_rect.right.saturating_sub(render_rect.left) == width
            && render_rect.bottom.saturating_sub(render_rect.top) == height
            && expected_forced_viewport.is_some_and(|expected| {
                GetPropW(render, gfxstream_applied_viewport_property.as_ptr()) as usize
                    == expected as usize
            })
            && i32::from(window_has_visible_style(input)) == expected_visible
            && i32::from(window_has_visible_style(render)) == expected_visible
            && native_input_visual_region_is_empty(input)
            && forced_projection_verified
    };
    if in_sync {
        return Ok(());
    }
    // gfxstream may apply its initial 1024x768 child extent after HD's first attach. Repair only
    // observed hierarchy/geometry drift; healthy heartbeats remain read-only and never rebuild the
    // Vulkan swapchain.
    prepare_native_display(child_pid, target, viewport)
}

pub(crate) fn set_native_display_visibility(
    child_pid: u32,
    target: &NativeDisplayTargetV2,
    visible: bool,
) -> Result<(), PlatformError> {
    let _ = windows_target(target)?;
    let render = gfxstream_window_anywhere(child_pid)?;
    let input = crosvm_window(child_pid, target.scanout_id())?;
    unsafe {
        if visible {
            // Preserve the established geometry and z-order. A visibility-only restore must not
            // notify gfxstream or recreate Intel's swapchain after a minimized root window.
            ShowWindow(input, SW_SHOW);
            ShowWindow(render, SW_SHOW);
            select_crosvm_display(input)?;
        } else {
            ShowWindow(render, SW_HIDE);
            ShowWindow(input, SW_HIDE);
        }
    }
    Ok(())
}

pub(crate) fn detach_native_display(
    child_pid: u32,
    target: &NativeDisplayTargetV2,
) -> Result<(), PlatformError> {
    let _ = windows_target(target)?;
    let render = gfxstream_window_anywhere(child_pid)?;
    let child = crosvm_window(child_pid, target.scanout_id())?;
    let parking = display_parking_window()?;
    let snapshots = [
        snapshot_native_window(render),
        snapshot_native_window(child),
    ];
    // SAFETY: hide and move both sibling HWNDs from the short-lived Player viewport into the
    // Worker's hidden native parking host. Leaving gfxstream's subWin behind would destroy the
    // actual render surface with the old HD window even if the crosvm input window survived.
    let detach_result = (|| -> Result<(), PlatformError> {
        unsafe {
            ShowWindow(render, SW_HIDE);
            ShowWindow(child, SW_HIDE);
            let render_style = GetWindowLongPtrW(render, GWL_STYLE);
            SetWindowLongPtrW(
                render,
                GWL_STYLE,
                (render_style & !win32_style(WS_POPUP))
                    | win32_style(WS_CHILD)
                    | win32_style(WS_CLIPCHILDREN),
            );
            SetLastError(0);
            if SetParent(render, parking).is_null() && GetLastError() != 0 {
                return Err(last_identity_error("SetParent(park gfxstream render)"));
            }
            let style = GetWindowLongPtrW(child, GWL_STYLE);
            SetWindowLongPtrW(
                child,
                GWL_STYLE,
                (style & !win32_style(WS_POPUP))
                    | win32_style(WS_CHILD)
                    | win32_style(WS_CLIPCHILDREN),
            );
            SetLastError(0);
            if SetParent(child, parking).is_null() && GetLastError() != 0 {
                return Err(last_identity_error("SetParent(park display)"));
            }
            if GetParent(render) != parking || GetParent(child) != parking {
                return Err(PlatformError::Process(
                    "native display siblings did not enter the Worker parking host".to_owned(),
                ));
            }
            // Preserve both extents while parked. Shrinking gfxstream's HWND to 1x1 behind its
            // back poisoned the retained Win32 surface; the next attach could recreate a 1x1
            // swapchain even though HD had already expanded the child visually.
        }
        Ok(())
    })();
    if let Err(error) = detach_result {
        // SetParent is not atomic across sibling HWNDs. Restore the exact hierarchy, styles and
        // visibility when either move fails so one recoverable detach error cannot strand the
        // Vulkan renderer in parking while the input surface remains under Player (or vice versa).
        return match restore_native_display_siblings(snapshots) {
            Ok(()) => Err(error),
            Err(rollback) => Err(PlatformError::Process(format!(
                "native display detach failed ({error}); sibling rollback also failed ({rollback})"
            ))),
        };
    }
    Ok(())
}

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
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_SUSPENDED);
}

pub(crate) fn configure_latency_sensitive_managed(command: &mut Command) {
    command.creation_flags(
        CREATE_NEW_PROCESS_GROUP
            | CREATE_NO_WINDOW
            | CREATE_SUSPENDED
            | ABOVE_NORMAL_PRIORITY_CLASS,
    );
}

pub(crate) fn configure_transient(command: &mut Command) {
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

pub(crate) fn resume_managed_process(pid: u32) -> Result<(), PlatformError> {
    // SAFETY: the snapshot handle is owned by this function and closed on every path below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(last_identity_error("CreateToolhelp32Snapshot(threads)"));
    }
    let result = (|| {
        let mut entry = THREADENTRY32 {
            dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>())
                .map_err(|error| PlatformError::Identity(error.to_string()))?,
            ..Default::default()
        };
        // SAFETY: snapshot is a valid thread snapshot and entry is correctly sized and writable.
        let mut has_entry = unsafe { Thread32First(snapshot, &raw mut entry) } != 0;
        let mut resumed = 0_u32;
        while has_entry {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: OpenThread is called for a thread ID obtained from the live snapshot.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(last_identity_error("OpenThread(resume managed process)"));
                }
                // SAFETY: the handle was opened with THREAD_SUSPEND_RESUME. A newly created
                // CREATE_SUSPENDED process must have exactly one suspension owned by HD.
                let previous = unsafe { ResumeThread(thread) };
                let resume_error = if previous == u32::MAX {
                    Some(last_identity_error("ResumeThread(managed process)"))
                } else if previous != 1 {
                    Some(PlatformError::Process(format!(
                        "managed process thread had unexpected suspend count {previous}"
                    )))
                } else {
                    None
                };
                // SAFETY: the temporary thread handle is closed exactly once.
                unsafe {
                    CloseHandle(thread);
                }
                if let Some(error) = resume_error {
                    return Err(error);
                }
                resumed = resumed.saturating_add(1);
            }
            // SAFETY: snapshot and entry remain valid for enumeration.
            has_entry = unsafe { Thread32Next(snapshot, &raw mut entry) } != 0;
        }
        if resumed != 1 {
            return Err(PlatformError::Process(format!(
                "managed process {pid} exposed {resumed} resumable threads"
            )));
        }
        Ok(())
    })();
    // SAFETY: snapshot is an owned handle and is closed exactly once.
    unsafe {
        CloseHandle(snapshot);
    }
    result
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::native_display_toplevel_value_enabled;

    #[test]
    fn native_display_toplevel_debug_flag_requires_truthy_value() {
        assert!(!native_display_toplevel_value_enabled(None));
        assert!(!native_display_toplevel_value_enabled(Some(OsStr::new(""))));
        assert!(!native_display_toplevel_value_enabled(Some(OsStr::new(
            "0"
        ))));
        assert!(!native_display_toplevel_value_enabled(Some(OsStr::new(
            "FALSE"
        ))));
        assert!(native_display_toplevel_value_enabled(Some(OsStr::new("1"))));
        assert!(native_display_toplevel_value_enabled(Some(OsStr::new(
            "true"
        ))));
    }
}
