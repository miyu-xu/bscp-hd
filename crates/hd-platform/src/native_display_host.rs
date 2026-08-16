#![cfg_attr(any(windows, target_os = "macos"), allow(unsafe_code))]

use std::path::PathBuf;

use hd_core::{NativeDisplayTargetV2, WorkerIdentityV2};
use raw_window_handle::RawWindowHandle;

use crate::PlatformError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeDisplayBounds {
    pub x_px: i32,
    pub y_px: i32,
    pub width_px: u32,
    pub height_px: u32,
    /// Clockwise Android display rotation in quarter turns from its natural orientation.
    pub rotation_quarters: u8,
    pub visible: bool,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MacTitlebarControlContract {
    pub symbol: String,
    pub tooltip: String,
    pub message: String,
    pub placement: String,
}

#[cfg(target_os = "macos")]
pub fn map_macos_pointer(
    normalized_x: f64,
    normalized_y: f64,
    rotation_quarters: u8,
    guest_width: u32,
    guest_height: u32,
) -> (i32, i32) {
    let x = normalized_x.clamp(0.0, 1.0);
    let y = normalized_y.clamp(0.0, 1.0);
    let (raw_x, raw_y) = match rotation_quarters & 3 {
        1 => (1.0 - y, x),
        2 => (1.0 - x, 1.0 - y),
        3 => (y, 1.0 - x),
        _ => (x, y),
    };
    let width = guest_width.max(1);
    let height = guest_height.max(1);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (
        (raw_x * f64::from(width))
            .floor()
            .clamp(0.0, f64::from(width - 1)) as i32,
        (raw_y * f64::from(height))
            .floor()
            .clamp(0.0, f64::from(height - 1)) as i32,
    )
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
pub extern "C" fn hd_macos_map_pointer_contract(
    normalized_x: f64,
    normalized_y: f64,
    rotation_quarters: u8,
    guest_width: u32,
    guest_height: u32,
    output_x: *mut i32,
    output_y: *mut i32,
) {
    let (x, y) = map_macos_pointer(
        normalized_x,
        normalized_y,
        rotation_quarters,
        guest_width,
        guest_height,
    );
    // SAFETY: AppKit passes pointers to stack-owned i32 outputs.
    unsafe {
        if let Some(output) = output_x.as_mut() {
            *output = x;
        }
        if let Some(output) = output_y.as_mut() {
            *output = y;
        }
    }
}

/// Volatile, platform-owned display container used by gfxstream/crosvm.
///
/// Implementations never persist the native handle. The UI owns this object for exactly as long
/// as its top-level window and sends only the handle plus process identity through a short-lived
/// display session.
pub trait NativeDisplayHost: std::fmt::Debug {
    fn handle(&self) -> u64;
    fn target(&self, owner: WorkerIdentityV2, scanout_id: u32) -> NativeDisplayTargetV2;
    fn set_bounds(&mut self, bounds: NativeDisplayBounds) -> Result<(), PlatformError>;
    fn hide(&mut self) -> Result<(), PlatformError>;
    fn focus_guest(&self) -> Result<(), PlatformError>;
    fn root_is_minimized(&self) -> bool;
}

pub fn create_native_display_host(
    parent: RawWindowHandle,
) -> Result<Box<dyn NativeDisplayHost>, PlatformError> {
    #[cfg(windows)]
    {
        WindowsNativeDisplayHost::new(parent)
            .map(|host| Box::new(host) as Box<dyn NativeDisplayHost>)
    }
    #[cfg(target_os = "macos")]
    {
        MacNativeDisplayHost::new(parent).map(|host| Box::new(host) as Box<dyn NativeDisplayHost>)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = parent;
        Err(PlatformError::Unsupported(
            "NativeDisplayHost is not implemented for this platform",
        ))
    }
}

#[cfg(windows)]
pub fn windows_root_has_foreground_focus(parent: RawWindowHandle) -> Result<bool, PlatformError> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let RawWindowHandle::Win32(handle) = parent else {
        return Err(PlatformError::Unsupported(
            "Windows foreground focus requires a Win32 root window",
        ));
    };
    let root = handle.hwnd.get() as windows_sys::Win32::Foundation::HWND;
    // A WebView2 or native Android child can own keyboard focus while the product root remains the
    // foreground top-level window. This platform query deliberately distinguishes that internal
    // focus transfer from a real application deactivation.
    Ok(unsafe { GetForegroundWindow() } == root)
}

pub fn activate_existing_desktop_application() -> Result<bool, PlatformError> {
    #[cfg(windows)]
    {
        crate::windows_titlebar::activate_existing_application()
    }
    #[cfg(target_os = "macos")]
    {
        macos::activate_existing_application()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Ok(false)
    }
}

/// Configures process-wide desktop behavior that must be in place before the native event loop.
///
/// On macOS, HD owns window restoration through its versioned Host and instance state. Disabling
/// `AppKit`'s independent persistent UI restoration prevents a stale crash-history prompt from
/// blocking winit before `Resumed`. Other platforms currently require no early configuration.
pub fn configure_desktop_application_startup() -> Result<(), PlatformError> {
    #[cfg(target_os = "macos")]
    {
        macos::configure_application_startup()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(())
    }
}

pub fn choose_apk_file() -> Result<Option<PathBuf>, PlatformError> {
    #[cfg(windows)]
    {
        windows_choose_file(
            "选择要安装的 APK",
            "Android 应用 (*.apk)\0*.apk\0所有文件 (*.*)\0*.*\0\0",
        )
    }
    #[cfg(target_os = "macos")]
    {
        macos::choose_apk_file()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(PlatformError::Unsupported(
            "native APK file selection is not implemented for this platform",
        ))
    }
}

pub fn choose_location_route_file() -> Result<Option<PathBuf>, PlatformError> {
    #[cfg(windows)]
    {
        windows_choose_file(
            "选择 GPX 或 KML 路线",
            "定位路线 (*.gpx;*.kml)\0*.gpx;*.kml\0所有文件 (*.*)\0*.*\0\0",
        )
    }
    #[cfg(target_os = "macos")]
    {
        macos::choose_location_route_file()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(PlatformError::Unsupported(
            "native GPX/KML file selection is not implemented for this platform",
        ))
    }
}

#[cfg(windows)]
fn windows_choose_file(title: &str, filter: &str) -> Result<Option<PathBuf>, PlatformError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;

    use windows_sys::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
        OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

    let mut path = vec![0_u16; 32_768].into_boxed_slice();
    let mut title = title.encode_utf16().collect::<Vec<_>>();
    title.push(0);
    let filter = filter.encode_utf16().collect::<Vec<_>>();
    let mut dialog = OPENFILENAMEW {
        lStructSize: u32::try_from(std::mem::size_of::<OPENFILENAMEW>()).map_err(|_| {
            PlatformError::Process("Windows file dialog structure size overflow".to_owned())
        })?,
        // SAFETY: this UI-thread query has no preconditions and returns either the active HD HWND
        // or null; the common dialog API accepts both.
        hwndOwner: unsafe { GetActiveWindow() },
        lpstrFilter: filter.as_ptr(),
        lpstrFile: path.as_mut_ptr(),
        nMaxFile: u32::try_from(path.len()).map_err(|_| {
            PlatformError::Process("Windows file dialog buffer size overflow".to_owned())
        })?,
        lpstrTitle: title.as_ptr(),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..OPENFILENAMEW::default()
    };
    // SAFETY: `dialog` points to initialized storage, and all referenced UTF-16 buffers remain
    // alive and writable for the duration of this synchronous call.
    if unsafe { GetOpenFileNameW(&raw mut dialog) } != 0 {
        let length = path.iter().position(|value| *value == 0).ok_or_else(|| {
            PlatformError::Process("Windows file dialog returned an unterminated path".to_owned())
        })?;
        return Ok(Some(PathBuf::from(OsString::from_wide(&path[..length]))));
    }
    // SAFETY: this reads the current thread's common-dialog error immediately after the failed
    // call. Zero is the documented user-cancel result.
    let error = unsafe { CommDlgExtendedError() };
    if error == 0 {
        Ok(None)
    } else {
        Err(PlatformError::Process(format!(
            "Windows file selection failed with common-dialog error {error}"
        )))
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsNativeDisplayHost {
    parent_hwnd: windows_sys::Win32::Foundation::HWND,
    hwnd: windows_sys::Win32::Foundation::HWND,
    last_bounds: Option<NativeDisplayBounds>,
    last_forced_bounds: Option<NativeDisplayBounds>,
}

#[cfg(windows)]
const fn wide_ascii_z<const N: usize>(value: &[u8; N]) -> [u16; N] {
    let mut output = [0_u16; N];
    let mut index = 0;
    while index < N {
        assert!(
            value[index].is_ascii(),
            "Win32 property names must be ASCII"
        );
        output[index] = value[index] as u16;
        index += 1;
    }
    assert!(
        N > 0 && output[N - 1] == 0,
        "Win32 property names must be NUL terminated"
    );
    output
}

#[cfg(windows)]
const WINDOWS_CROSVM_INPUT_CHILD_PROPERTY: &[u16] = &wide_ascii_z(b"HD_CROSVM_INPUT_CHILD_V1\0");
#[cfg(windows)]
const WINDOWS_CROSVM_INPUT_CHILD_COOKIE: &[u16] =
    &wide_ascii_z(b"HD_CROSVM_INPUT_CHILD_COOKIE_V1\0");
#[cfg(windows)]
const WINDOWS_APPLIED_VIEWPORT_PROPERTY: &[u16] = &wide_ascii_z(b"HD_APPLIED_VIEWPORT_V1\0");
#[cfg(windows)]
const WINDOWS_APPLIED_ROTATION_PROPERTY: &[u16] = &wide_ascii_z(b"HD_APPLIED_ROTATION_V1\0");
#[cfg(windows)]
const WINDOWS_FORCED_VIEWPORT_PROPERTY: &[u16] = &wide_ascii_z(b"HD_FORCED_VIEWPORT_V1\0");
#[cfg(windows)]
const WINDOWS_FORCED_ROTATION_PROPERTY: &[u16] = &wide_ascii_z(b"HD_FORCED_ROTATION_V1\0");
#[cfg(windows)]
const WINDOWS_NATIVE_DISPLAY_ROTATION_PROPERTY: &[u16] =
    &wide_ascii_z(b"HD_NATIVE_DISPLAY_ROTATION_V1\0");
#[cfg(windows)]
const WINDOWS_CROSVM_CLASS_PREFIX: [u16; 6] = [
    b'C' as u16,
    b'R' as u16,
    b'O' as u16,
    b'S' as u16,
    b'V' as u16,
    b'M' as u16,
];

#[cfg(windows)]
unsafe fn windows_child_has_visible_style(child: windows_sys::Win32::Foundation::HWND) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GWL_STYLE, GetWindowLongPtrW, WS_VISIBLE};

    let visible_style = isize::try_from(WS_VISIBLE.cast_signed())
        .unwrap_or_else(|_| unreachable!("Win32 LONG style must fit LONG_PTR"));
    unsafe { GetWindowLongPtrW(child, GWL_STYLE) & visible_style != 0 }
}

#[cfg(windows)]
unsafe fn windows_crosvm_input_child(
    parent: windows_sys::Win32::Foundation::HWND,
) -> windows_sys::Win32::Foundation::HWND {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GW_CHILD, GW_HWNDNEXT, GetClassNameW, GetParent, GetPropW, GetWindow, IsWindow, SetPropW,
    };

    // gfxstream's render HWND and crosvm's input HWND are direct siblings under this viewport.
    // Pointer messages run at common 125 Hz mouse rates, so retain the verified HWND instead of
    // enumerating children, decoding a class name and allocating a String for every event. The
    // parent relationship prevents a destroyed/reused HWND or a scanout reattach from surviving
    // validation. Check the child's own visible style because an intentionally hidden HD parent
    // makes IsWindowVisible(child) false even though the retained input surface remains enabled.
    let property = WINDOWS_CROSVM_INPUT_CHILD_PROPERTY;
    let cookie = WINDOWS_CROSVM_INPUT_CHILD_COOKIE;
    let cached = unsafe { GetPropW(parent, property.as_ptr()) };
    if !cached.is_null()
        && unsafe { IsWindow(cached) } != 0
        && unsafe { GetParent(cached) } == parent
        && unsafe { GetPropW(cached, cookie.as_ptr()) } == parent
        && unsafe { windows_child_has_visible_style(cached) }
        && unsafe { IsWindowEnabled(cached) } != 0
    {
        return cached;
    }
    let mut child = unsafe { GetWindow(parent, GW_CHILD) };
    while !child.is_null() {
        let mut class_name = [0_u16; 64];
        let length = unsafe {
            GetClassNameW(
                child,
                class_name.as_mut_ptr(),
                i32::try_from(class_name.len()).unwrap_or(64),
            )
        };
        if let Ok(length) = usize::try_from(length)
            && length >= WINDOWS_CROSVM_CLASS_PREFIX.len()
            && class_name[..WINDOWS_CROSVM_CLASS_PREFIX.len()] == WINDOWS_CROSVM_CLASS_PREFIX
            && unsafe { windows_child_has_visible_style(child) }
            && unsafe { IsWindowEnabled(child) } != 0
        {
            // A failed scalar property publication only forfeits the optimization; the verified
            // child remains valid for this event and will be rediscovered on the next one.
            unsafe { SetPropW(child, cookie.as_ptr(), parent.cast()) };
            unsafe { SetPropW(parent, property.as_ptr(), child.cast()) };
            return child;
        }
        child = unsafe { GetWindow(child, GW_HWNDNEXT) };
    }
    std::ptr::null_mut()
}

#[cfg(windows)]
fn commit_windows_crosvm_viewport_if_needed(
    parent: windows_sys::Win32::Foundation::HWND,
    width: i32,
    height: i32,
    rotation_quarters: u8,
) -> Result<(), PlatformError> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetPropW;

    let input = unsafe { windows_crosvm_input_child(parent) };
    if input.is_null() {
        return Ok(());
    }
    let width_u16 = u16::try_from(width).map_err(|_| {
        PlatformError::Process("native display width exceeds crosvm message bounds".to_owned())
    })?;
    let height_u16 = u16::try_from(height).map_err(|_| {
        PlatformError::Process("native display height exceeds crosvm message bounds".to_owned())
    })?;
    let expected = (u32::from(height_u16) << 16) | u32::from(width_u16);
    let property = WINDOWS_APPLIED_VIEWPORT_PROPERTY;
    let current =
        u32::try_from(unsafe { GetPropW(input, property.as_ptr()) } as usize).unwrap_or_default();
    let rotation_property = WINDOWS_APPLIED_ROTATION_PROPERTY;
    let current_rotation =
        u8::try_from(unsafe { GetPropW(input, rotation_property.as_ptr()) } as usize)
            .ok()
            .and_then(|value| value.checked_sub(1));
    if current != expected || current_rotation != Some(rotation_quarters & 3) {
        crate::windows::notify_crosvm_viewport(input, width, height, rotation_quarters)?;
    }
    Ok(())
}

#[cfg(windows)]
fn force_commit_windows_crosvm_viewport(
    parent: windows_sys::Win32::Foundation::HWND,
    width: i32,
    height: i32,
    rotation_quarters: u8,
) -> Result<bool, PlatformError> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetPropW;

    let input = unsafe { windows_crosvm_input_child(parent) };
    if input.is_null() {
        return Ok(false);
    }
    let width_word = u16::try_from(width).map_err(|_| {
        PlatformError::Process("native display width exceeds crosvm message bounds".to_owned())
    })?;
    let height_word = u16::try_from(height).map_err(|_| {
        PlatformError::Process("native display height exceeds crosvm message bounds".to_owned())
    })?;
    let expected_viewport = (u32::from(height_word) << 16) | u32::from(width_word);
    let expected_rotation = usize::from((rotation_quarters & 3) + 1);
    let forced_viewport = WINDOWS_FORCED_VIEWPORT_PROPERTY;
    let forced_rotation = WINDOWS_FORCED_ROTATION_PROPERTY;
    if unsafe { GetPropW(input, forced_viewport.as_ptr()) } as usize == expected_viewport as usize
        && unsafe { GetPropW(input, forced_rotation.as_ptr()) } as usize == expected_rotation
    {
        // A root WM_SIZE/rotation transaction already received crosvm's applied acknowledgement.
        // Treat that shared marker as this host's local acknowledgement instead of bypassing
        // crosvm's dedupe with a second force from a later equal-bounds winit event.
        return Ok(true);
    }
    crate::windows::force_notify_crosvm_viewport(input, width, height, rotation_quarters)
}

#[cfg(windows)]
const WINDOWS_POINTER_FOCUS_FAILURES_PROPERTY: &[u16] =
    &wide_ascii_z(b"HD_POINTER_FOCUS_FAILURES_V1\0");
#[cfg(windows)]
const WINDOWS_POINTER_FOCUS_MAX_MICROS_PROPERTY: &[u16] =
    &wide_ascii_z(b"HD_POINTER_FOCUS_MAX_MICROS_V1\0");
#[cfg(windows)]
const WINDOWS_POINTER_FORWARD_FAILURES_PROPERTY: &[u16] =
    &wide_ascii_z(b"HD_POINTER_FORWARD_FAILURES_V1\0");
#[cfg(windows)]
const WINDOWS_POINTER_FORWARD_MAX_MICROS_PROPERTY: &[u16] =
    &wide_ascii_z(b"HD_POINTER_FORWARD_MAX_MICROS_V1\0");

#[cfg(windows)]
unsafe fn increment_windows_native_counter(
    hwnd: windows_sys::Win32::Foundation::HWND,
    property: &[u16],
) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetPropW, SetPropW};

    let value = (unsafe { GetPropW(hwnd, property.as_ptr()) } as usize).saturating_add(1);
    unsafe { SetPropW(hwnd, property.as_ptr(), value as *mut std::ffi::c_void) };
}

#[cfg(windows)]
fn focus_windows_native_guest(
    hwnd: windows_sys::Win32::Foundation::HWND,
) -> Result<(), PlatformError> {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GUITHREADINFO, GetGUIThreadInfo, GetWindowThreadProcessId,
    };

    let input = unsafe { windows_crosvm_input_child(hwnd) };
    let target = if input.is_null() { hwnd } else { input };
    let mut gui = GUITHREADINFO {
        cbSize: u32::try_from(std::mem::size_of::<GUITHREADINFO>()).unwrap_or_default(),
        ..Default::default()
    };
    if unsafe { GetGUIThreadInfo(0, &raw mut gui) } != 0 && gui.hwndFocus == target {
        return Ok(());
    }
    let target_thread = unsafe { GetWindowThreadProcessId(target, std::ptr::null_mut()) };
    if target_thread == 0 {
        return Err(PlatformError::Process(
            "resolve crosvm input thread for focus failed".to_owned(),
        ));
    }
    // The crosvm child normally keeps focus between Android pointer presses. Query its owning
    // thread directly before joining input queues: repeating AttachThreadInput + SetFocus for
    // every click causes needless focus notifications across WebView2/DWM even though the target
    // did not change, which is visible as titlebar/Android composition jitter under rapid input.
    let mut target_gui = GUITHREADINFO {
        cbSize: u32::try_from(std::mem::size_of::<GUITHREADINFO>()).unwrap_or_default(),
        ..Default::default()
    };
    if unsafe { GetGUIThreadInfo(target_thread, &raw mut target_gui) } != 0
        && target_gui.hwndFocus == target
    {
        return Ok(());
    }
    let current_thread = unsafe { GetCurrentThreadId() };
    let attached = current_thread != target_thread;
    if attached && unsafe { AttachThreadInput(current_thread, target_thread, 1) } == 0 {
        return Err(PlatformError::Process(
            "attach HD and crosvm input queues for focus failed".to_owned(),
        ));
    }
    // SAFETY: target is the live native host or its live crosvm input child. Different input
    // queues are joined only for this bounded focus transaction and detached immediately below.
    unsafe { SetFocus(target) };
    let focused = unsafe { GetGUIThreadInfo(target_thread, &raw mut target_gui) } != 0
        && target_gui.hwndFocus == target;
    let detached = !attached || unsafe { AttachThreadInput(current_thread, target_thread, 0) } != 0;
    if !detached {
        return Err(PlatformError::Process(
            "detach HD and crosvm input queues after focus failed".to_owned(),
        ));
    }
    if !focused {
        return Err(PlatformError::Process(
            "restore crosvm input focus after WebView action failed".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
unsafe extern "system" fn windows_native_display_window_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, GetPropW, PostMessageW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW,
        SetPropW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
        WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    };

    if matches!(
        message,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_XBUTTONDOWN
            | WM_XBUTTONUP
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
    ) {
        let input = unsafe { windows_crosvm_input_child(hwnd) };
        if !input.is_null() {
            if matches!(
                message,
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
            ) {
                let started = std::time::Instant::now();
                let focus_result = focus_windows_native_guest(hwnd);
                let elapsed_micros = usize::try_from(started.elapsed().as_micros())
                    .unwrap_or(usize::MAX)
                    .max(1);
                let latency_property = WINDOWS_POINTER_FOCUS_MAX_MICROS_PROPERTY;
                let previous_max = unsafe { GetPropW(hwnd, latency_property.as_ptr()) } as usize;
                if elapsed_micros > previous_max {
                    // SAFETY: hwnd is live for this window procedure. Both properties store only
                    // nonzero scalar diagnostics and are reclaimed automatically with the HWND.
                    unsafe {
                        SetPropW(
                            hwnd,
                            latency_property.as_ptr(),
                            elapsed_micros as *mut std::ffi::c_void,
                        )
                    };
                }
                if focus_result.is_err() {
                    unsafe {
                        increment_windows_native_counter(
                            hwnd,
                            WINDOWS_POINTER_FOCUS_FAILURES_PROPERTY,
                        );
                    }
                }
            }
            // The viewport and crosvm input HWND occupy the same client rectangle, so client and
            // wheel coordinates retain their Win32 meaning. Never synchronously wait for the
            // crosvm GUI thread on every 125 Hz WM_MOUSEMOVE: a busy render turn can consume the
            // four-millisecond timeout, stall HD's UI/DWM composition and then silently discard
            // the newest coordinate. Crosvm already coalesces motion to the display cadence while
            // retaining its fractional remainder, so queue motion asynchronously. Keep button and
            // wheel messages on the bounded synchronous path to preserve their lossless ordering.
            let started = std::time::Instant::now();
            let forwarded = if message == WM_MOUSEMOVE {
                // SAFETY: input is the cached, parent/cookie-validated crosvm HWND. PostMessageW
                // copies these scalar parameters into that thread's queue before returning.
                unsafe { PostMessageW(input, message, wparam, lparam) != 0 }
            } else {
                let mut message_result = 0_usize;
                unsafe {
                    SendMessageTimeoutW(
                        input,
                        message,
                        wparam,
                        lparam,
                        SMTO_ABORTIFHUNG | SMTO_BLOCK,
                        20,
                        &raw mut message_result,
                    ) != 0
                }
            };
            let elapsed_micros = usize::try_from(started.elapsed().as_micros())
                .unwrap_or(usize::MAX)
                .max(1);
            let latency_property = WINDOWS_POINTER_FORWARD_MAX_MICROS_PROPERTY;
            let previous_max = unsafe { GetPropW(hwnd, latency_property.as_ptr()) } as usize;
            if elapsed_micros > previous_max {
                unsafe {
                    SetPropW(
                        hwnd,
                        latency_property.as_ptr(),
                        elapsed_micros as *mut std::ffi::c_void,
                    )
                };
            }
            if !forwarded {
                unsafe {
                    increment_windows_native_counter(
                        hwnd,
                        WINDOWS_POINTER_FORWARD_FAILURES_PROPERTY,
                    );
                }
            }
        } else if message != WM_MOUSEMOVE {
            unsafe {
                increment_windows_native_counter(hwnd, WINDOWS_POINTER_FORWARD_FAILURES_PROPERTY);
            }
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[cfg(windows)]
impl WindowsNativeDisplayHost {
    fn publish_rotation(&self, rotation_quarters: u8) -> Result<(), PlatformError> {
        use windows_sys::Win32::UI::WindowsAndMessaging::SetPropW;

        let property = WINDOWS_NATIVE_DISPLAY_ROTATION_PROPERTY;
        let encoded = usize::from((rotation_quarters & 3) + 1);
        if unsafe {
            SetPropW(
                self.hwnd,
                property.as_ptr(),
                encoded as *mut std::ffi::c_void,
            )
        } == 0
        {
            return Err(PlatformError::Process(
                "publish NativeDisplayHost rotation failed".to_owned(),
            ));
        }
        Ok(())
    }

    fn restore_retained_bounds(
        &mut self,
        bounds: NativeDisplayBounds,
        width: i32,
        height: i32,
    ) -> Result<bool, PlatformError> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{IsWindowVisible, SW_SHOW, ShowWindow};

        let visibility_only_restore = self.last_bounds.is_some_and(|last| {
            !last.visible
                && last.x_px == bounds.x_px
                && last.y_px == bounds.y_px
                && last.width_px == bounds.width_px
                && last.height_px == bounds.height_px
                && last.rotation_quarters == bounds.rotation_quarters
        });
        if !visibility_only_restore {
            return Ok(false);
        }
        // A page overlay or minimize hid only this HD-owned parent. Its crosvm input and gfxstream
        // render children retained the already-presented frame and projection. Repair a genuinely
        // missing acknowledgement if necessary. Reveal without SetWindowPos or a forced viewport
        // or swapchain transaction.
        commit_windows_crosvm_viewport_if_needed(
            self.hwnd,
            width,
            height,
            bounds.rotation_quarters,
        )?;
        if unsafe { IsWindowVisible(self.hwnd) } == 0 {
            unsafe { ShowWindow(self.hwnd, SW_SHOW) };
        }
        self.last_bounds = Some(bounds);
        Ok(true)
    }

    fn commit_projection_only_rotation(
        &mut self,
        bounds: NativeDisplayBounds,
        width: i32,
        height: i32,
    ) -> Result<bool, PlatformError> {
        let projection_only_rotation = self.last_bounds.is_some_and(|last| {
            last.visible
                && last.x_px == bounds.x_px
                && last.y_px == bounds.y_px
                && last.width_px == bounds.width_px
                && last.height_px == bounds.height_px
                && last.rotation_quarters != bounds.rotation_quarters
        });
        if !projection_only_rotation {
            return Ok(false);
        }
        // Reverse portrait/landscape changes the Android transform without changing one host
        // pixel. Do not submit an equal SetWindowPos for the clipping parent: it can trigger a
        // redundant DWM composition between the old frame and the atomic crosvm projection.
        commit_windows_crosvm_viewport_if_needed(
            self.hwnd,
            width,
            height,
            bounds.rotation_quarters,
        )?;
        self.last_forced_bounds = None;
        self.last_bounds = Some(bounds);
        Ok(true)
    }

    fn position_if_needed(
        &self,
        bounds: NativeDisplayBounds,
        width: i32,
        height: i32,
        first_layout: bool,
    ) -> Result<(), PlatformError> {
        use windows_sys::Win32::Foundation::{POINT, RECT};
        use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowRect, HWND_BOTTOM, IsWindowVisible, SW_SHOW, SWP_NOACTIVATE,
            SWP_NOOWNERZORDER, SWP_NOZORDER, SetWindowPos, ShowWindow,
        };

        let mut current = RECT::default();
        let mut origin = POINT::default();
        let geometry_matches = unsafe { GetWindowRect(self.hwnd, &raw mut current) } != 0
            && {
                origin.x = current.left;
                origin.y = current.top;
                (unsafe { ScreenToClient(self.parent_hwnd, &raw mut origin) }) != 0
            }
            && origin.x == bounds.x_px
            && origin.y == bounds.y_px
            && current.right.saturating_sub(current.left) == width
            && current.bottom.saturating_sub(current.top) == height;
        if geometry_matches {
            if unsafe { IsWindowVisible(self.hwnd) } == 0 {
                unsafe { ShowWindow(self.hwnd, SW_SHOW) };
            }
            return Ok(());
        }

        // SAFETY: hwnd is a live child owned by this object. Coordinates are bounded physical
        // pixels supplied by the UI, and no activation or cross-thread ownership transfer occurs.
        let positioned = unsafe {
            let positioned = SetWindowPos(
                self.hwnd,
                if first_layout {
                    HWND_BOTTOM
                } else {
                    std::ptr::null_mut()
                },
                bounds.x_px,
                bounds.y_px,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | if first_layout { 0 } else { SWP_NOZORDER },
            );
            if positioned != 0 && IsWindowVisible(self.hwnd) == 0 {
                ShowWindow(self.hwnd, SW_SHOW);
            }
            positioned
        };
        if positioned == 0 {
            // A transient root/WebView geometry race must not turn an otherwise retained Android
            // frame into an explicit black interval. On first layout this HWND is still hidden by
            // construction; on later layouts leave the last successfully presented viewport
            // visible while the error propagates and the next coalesced layout can repair it.
            return Err(PlatformError::Process(
                "position NativeDisplayHost child HWND failed".to_owned(),
            ));
        }
        Ok(())
    }

    fn new(parent: RawWindowHandle) -> Result<Self, PlatformError> {
        use windows_sys::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::Graphics::Gdi::{BLACK_BRUSH, GetStockObject};
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, GCLP_HBRBACKGROUND, GWL_STYLE, GetWindowLongPtrW, RegisterClassExW,
            SetClassLongPtrW, SetWindowLongPtrW, WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN,
            WS_CLIPSIBLINGS,
        };

        let RawWindowHandle::Win32(handle) = parent else {
            return Err(PlatformError::Unsupported(
                "Windows NativeDisplayHost requires a Win32 parent window",
            ));
        };
        let parent_hwnd = handle.hwnd.get() as windows_sys::Win32::Foundation::HWND;
        let class = wide("HD_NATIVE_DISPLAY_HOST_V2");
        // SAFETY: parent_hwnd comes from a live winit Window. The registered class stores no
        // instance pointers; the viewport receives input through gfxstream's disabled presentation
        // child, forwards only scalar mouse messages to the exact crosvm input sibling and
        // otherwise delegates to DefWindowProc.
        let hwnd = unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let black_brush = GetStockObject(BLACK_BRUSH);
            let window_class = WNDCLASSEXW {
                cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap_or_default(),
                // This HWND is only a stable clipping/parent surface. gfxstream's Vulkan child
                // owns every display pixel. CS_HREDRAW/CS_VREDRAW or a class brush would erase
                // the retained Vulkan image before the replacement frame is presented, which is
                // visible as a black flash during WebView clicks and live resize.
                style: 0,
                lpfnWndProc: Some(windows_native_display_window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: module,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
                hIconSm: std::ptr::null_mut(),
            };
            if RegisterClassExW(&raw const window_class) == 0
                && GetLastError() != ERROR_CLASS_ALREADY_EXISTS
            {
                return Err(PlatformError::Process(
                    "register NativeDisplayHost Win32 class failed".to_owned(),
                ));
            }
            let style = GetWindowLongPtrW(parent_hwnd, GWL_STYLE);
            SetWindowLongPtrW(
                parent_hwnd,
                GWL_STYLE,
                style | isize::try_from(WS_CLIPCHILDREN.cast_signed()).unwrap_or_default(),
            );
            // The WebView surfaces deliberately leave the Player rectangle completely
            // uncovered. Paint that root client area black so aspect-fit letterboxing never
            // exposes the default white winit background around the Android child HWND.
            if !black_brush.is_null() {
                SetClassLongPtrW(parent_hwnd, GCLP_HBRBACKGROUND, black_brush as isize);
            }
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                0,
                0,
                1,
                1,
                parent_hwnd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(PlatformError::Process(
                "create NativeDisplayHost child HWND failed".to_owned(),
            ));
        }
        // Publish the stable clipping host once. The root WM_SIZE controller validates both the
        // parent relationship and this host's root cookie before using the cached HWND, so handle
        // reuse fails closed and only a genuinely missing/stale cache falls back to enumeration.
        unsafe { crate::windows_titlebar::cache_windows_native_display_host(parent_hwnd, hwnd) };
        Ok(Self {
            parent_hwnd,
            hwnd,
            last_bounds: None,
            last_forced_bounds: None,
        })
    }
}

#[cfg(windows)]
impl NativeDisplayHost for WindowsNativeDisplayHost {
    fn handle(&self) -> u64 {
        self.hwnd as usize as u64
    }

    fn target(&self, owner: WorkerIdentityV2, scanout_id: u32) -> NativeDisplayTargetV2 {
        NativeDisplayTargetV2::WindowsHwnd {
            hwnd: self.handle(),
            scanout_id,
            owner,
        }
    }

    fn set_bounds(&mut self, bounds: NativeDisplayBounds) -> Result<(), PlatformError> {
        if !bounds.visible {
            return self.hide();
        }
        let width = i32::try_from(bounds.width_px.max(1)).unwrap_or(i32::MAX);
        let height = i32::try_from(bounds.height_px.max(1)).unwrap_or(i32::MAX);
        // Publish before crosvm attaches so the first viewport observes Player orientation.
        self.publish_rotation(bounds.rotation_quarters)?;
        let interactive_resize =
            crate::windows_titlebar::windows_root_is_interactive_resize(self.parent_hwnd);
        if interactive_resize {
            // The root subclass owns the stable clipping viewport throughout the modal drag.
            // Winit can deliver older Resized events after WM_SIZE; applying them here would move
            // the parent backwards and enqueue another crosvm projection for a historical extent.
            return Ok(());
        }
        // SetWindowPos from another UI thread can leave winit Resized events queued until after
        // WM_EXITSIZEMOVE has already committed the final root geometry. The root subclass moves
        // this stable clipping HWND synchronously, so an older same-orientation layout must not
        // move it backwards and rebuild gfxstream's swapchain once per stale event. Orientation
        // changes are exempt because they intentionally replace the current aspect projection.
        let same_orientation = self
            .last_bounds
            .is_some_and(|last| last.rotation_quarters == bounds.rotation_quarters);
        if same_orientation
            && let Some((expected_x, expected_y, expected_width, expected_height)) =
                crate::windows_titlebar::current_windows_root_content_bounds(self.parent_hwnd)
            && (bounds.x_px != expected_x
                || bounds.y_px != expected_y
                || width != expected_width
                || height != expected_height)
        {
            return Ok(());
        }
        if self.last_bounds == Some(bounds) {
            // Bounds can be committed before crosvm finishes attaching its Surface. Deduplicate
            // only when crosvm has acknowledged the applied viewport; otherwise repair the
            // deferred transaction without moving any WebView or native window.
            if self.last_forced_bounds != Some(bounds)
                && force_commit_windows_crosvm_viewport(
                    self.hwnd,
                    width,
                    height,
                    bounds.rotation_quarters,
                )?
            {
                self.last_forced_bounds = Some(bounds);
                return Ok(());
            }
            return commit_windows_crosvm_viewport_if_needed(
                self.hwnd,
                width,
                height,
                bounds.rotation_quarters,
            );
        }
        if self.commit_projection_only_rotation(bounds, width, height)? {
            return Ok(());
        }
        if self.restore_retained_bounds(bounds, width, height)? {
            return Ok(());
        }
        self.position_if_needed(bounds, width, height, self.last_bounds.is_none())?;
        // Commit the parent extent and crosvm's input/render projection as one UI-thread
        // transaction.  The Player owns this HWND, so it observes the final WebView/titlebar
        // layout before the versioned display-session request reaches the Worker.  Waiting for
        // crosvm here prevents DWM from composing a child HWND at the new size while gfxstream's
        // swapchain still contains the previous orientation (the visible one-step stretch/black
        // flash on titlebar actions).  If Android has not attached yet, the Worker performs the
        // same initial transaction when it reparents the crosvm surfaces below this viewport.
        // WM_SIZE already publishes a coalescing latest-value notification while the user drags
        // a window edge. Do not turn the later winit Resized event back into a synchronous
        // cross-process wait; WM_EXITSIZEMOVE performs the final acknowledged commit.
        // WM_SIZE/root rotation may already have committed this exact projection. Use the
        // acknowledged applied properties to repair only a missing transaction; a second force
        // would deliberately bypass crosvm's size/rotation dedupe and rebuild the swapchain.
        commit_windows_crosvm_viewport_if_needed(
            self.hwnd,
            width,
            height,
            bounds.rotation_quarters,
        )?;
        self.last_forced_bounds = None;
        self.last_bounds = Some(bounds);
        Ok(())
    }

    fn hide(&mut self) -> Result<(), PlatformError> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{IsWindowVisible, SW_HIDE, ShowWindow};
        // SAFETY: hwnd remains owned by this object until Drop.
        if unsafe { IsWindowVisible(self.hwnd) } != 0 {
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        }
        if let Some(bounds) = self.last_bounds.as_mut() {
            bounds.visible = false;
        }
        Ok(())
    }

    fn focus_guest(&self) -> Result<(), PlatformError> {
        // The visible gfxstream HWND is normally first in sibling Z-order. Focus the explicit
        // CROSVM input child with the same verified transaction used by native pointer presses.
        focus_windows_native_guest(self.hwnd)
    }

    fn root_is_minimized(&self) -> bool {
        // SAFETY: parent_hwnd is owned by the still-live winit Window.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsIconic(self.parent_hwnd) != 0 }
    }
}

#[cfg(windows)]
impl Drop for WindowsNativeDisplayHost {
    fn drop(&mut self) {
        // SAFETY: hwnd was created by this object and is destroyed exactly once here. Clear only
        // the cache entries that still point to this exact parent/child pair.
        unsafe {
            crate::windows_titlebar::uncache_windows_native_display_host(
                self.parent_hwnd,
                self.hwnd,
            );
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CStr, CString, c_char, c_void};
    use std::path::PathBuf;

    use hd_core::{NativeDisplayTargetV2, WorkerIdentityV2};
    use raw_window_handle::RawWindowHandle;

    use super::{NativeDisplayBounds, NativeDisplayHost};
    use crate::PlatformError;

    unsafe extern "C" {
        fn hd_macos_native_display_create(
            parent_view: *mut c_void,
            endpoint: *const c_char,
        ) -> *mut c_void;
        fn hd_macos_native_display_destroy(host: *mut c_void);
        fn hd_macos_native_display_set_bounds(
            host: *mut c_void,
            x_px: i32,
            y_px: i32,
            width_px: u32,
            height_px: u32,
            rotation_quarters: u8,
            visible: bool,
        ) -> bool;
        fn hd_macos_native_display_hide(host: *mut c_void) -> bool;
        fn hd_macos_native_display_focus(host: *mut c_void) -> bool;
        fn hd_macos_native_display_root_is_minimized(host: *mut c_void) -> bool;
        fn hd_macos_configure_application_startup() -> bool;
        fn hd_macos_activate_existing_application() -> i32;
        fn hd_macos_choose_apk_file(output: *mut c_char, capacity: usize) -> i32;
        fn hd_macos_choose_location_route_file(output: *mut c_char, capacity: usize) -> i32;
        fn hd_macos_show_error_dialog(
            title: *const c_char,
            message: *const c_char,
            reveal_path: *const c_char,
        ) -> bool;
        fn hd_macos_center_traffic_lights(parent_view: *mut c_void, titlebar_height: f64) -> bool;
        fn hd_macos_set_window_content_aspect_ratio(
            parent_view: *mut c_void,
            width: f64,
            height: f64,
        ) -> bool;
        fn hd_macos_install_titlebar_controls(
            parent_view: *mut c_void,
            context: *mut c_void,
            callback: MacTitlebarCallback,
        ) -> bool;
        fn hd_macos_set_titlebar_fps(
            parent_view: *mut c_void,
            visible: bool,
            fps_milli: u32,
        ) -> bool;
        fn hd_macos_set_titlebar_player_state(
            parent_view: *mut c_void,
            state_json: *const c_char,
        ) -> bool;
        fn hd_macos_titlebar_control_count() -> usize;
        fn hd_macos_titlebar_control_symbol(index: usize) -> *const c_char;
        fn hd_macos_titlebar_control_tooltip(index: usize) -> *const c_char;
        fn hd_macos_titlebar_control_message(index: usize) -> *const c_char;
        fn hd_macos_titlebar_control_placement(index: usize) -> *const c_char;
    }

    pub type MacTitlebarCallback = extern "C" fn(*mut c_void, *const c_char);

    pub(super) fn configure_application_startup() -> Result<(), PlatformError> {
        // SAFETY: the desktop entry point invokes this on the process main thread before any
        // winit event loop is created. The bridge mutates only this process's volatile defaults.
        unsafe { hd_macos_configure_application_startup() }
            .then_some(())
            .ok_or_else(|| {
                PlatformError::Process(
                    "configure macOS application startup policy failed".to_owned(),
                )
            })
    }

    pub(super) fn activate_existing_application() -> Result<bool, PlatformError> {
        // SAFETY: the desktop entry point calls this on the AppKit main thread before creating the
        // winit event loop. The bridge returns only a tri-state integer and retains no Rust data.
        match unsafe { hd_macos_activate_existing_application() } {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PlatformError::Process(
                "query existing macOS application instance failed".to_owned(),
            )),
        }
    }

    pub(super) struct MacNativeDisplayHost {
        raw: *mut c_void,
        endpoint: PathBuf,
        last_bounds: Option<NativeDisplayBounds>,
    }

    impl std::fmt::Debug for MacNativeDisplayHost {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("MacNativeDisplayHost")
                .field("endpoint", &self.endpoint)
                .finish_non_exhaustive()
        }
    }

    impl MacNativeDisplayHost {
        pub(super) fn new(parent: RawWindowHandle) -> Result<Self, PlatformError> {
            let RawWindowHandle::AppKit(handle) = parent else {
                return Err(PlatformError::Unsupported(
                    "macOS NativeDisplayHost requires an AppKit parent view",
                ));
            };
            let user_scope = crate::current_user_scope()?;
            // The display endpoint belongs to one UI process, not to the whole login session.
            // Finder `open -n`, isolated QA data roots, and a runtime upgrade can legitimately
            // overlap with an older UI. A per-user socket makes the newer NativeDisplayHost fail
            // before WebView startup even though every display session already carries its exact
            // endpoint. Include the owner PID so independent UI processes cannot contend for or
            // remove each other's listener.
            let endpoint = std::env::temp_dir().join(format!(
                "bscp-hd-ca-{user_scope}-{}.sock",
                std::process::id()
            ));
            let endpoint_c = CString::new(endpoint.to_string_lossy().as_bytes()).map_err(|_| {
                PlatformError::Process("macOS display endpoint contains NUL".to_owned())
            })?;
            // SAFETY: winit owns the parent NSView for the lifetime of this host. The bridge
            // retains its own host object and copies the endpoint string.
            let raw = unsafe {
                hd_macos_native_display_create(handle.ns_view.as_ptr(), endpoint_c.as_ptr())
            };
            if raw.is_null() {
                return Err(PlatformError::Process(
                    "create macOS CoreAnimation display host failed".to_owned(),
                ));
            }
            Ok(Self {
                raw,
                endpoint,
                last_bounds: None,
            })
        }

        fn call(operation: &'static str, succeeded: bool) -> Result<(), PlatformError> {
            if succeeded {
                Ok(())
            } else {
                Err(PlatformError::Process(format!(
                    "macOS NativeDisplayHost {operation} failed"
                )))
            }
        }
    }

    impl NativeDisplayHost for MacNativeDisplayHost {
        fn handle(&self) -> u64 {
            self.raw as usize as u64
        }

        fn target(&self, owner: WorkerIdentityV2, scanout_id: u32) -> NativeDisplayTargetV2 {
            NativeDisplayTargetV2::MacCaContext {
                endpoint: self.endpoint.to_string_lossy().into_owned(),
                scanout_id,
                owner,
            }
        }

        fn set_bounds(&mut self, bounds: NativeDisplayBounds) -> Result<(), PlatformError> {
            if self.last_bounds == Some(bounds) {
                return Ok(());
            }
            if !bounds.visible {
                return self.hide();
            }
            // SAFETY: raw is retained until Drop and UI calls occur on the AppKit thread.
            let succeeded = unsafe {
                hd_macos_native_display_set_bounds(
                    self.raw,
                    bounds.x_px,
                    bounds.y_px,
                    bounds.width_px,
                    bounds.height_px,
                    bounds.rotation_quarters,
                    bounds.visible,
                )
            };
            Self::call("set bounds", succeeded)?;
            self.last_bounds = Some(bounds);
            Ok(())
        }

        fn hide(&mut self) -> Result<(), PlatformError> {
            if self.last_bounds.is_some_and(|bounds| !bounds.visible) {
                return Ok(());
            }
            // SAFETY: raw is retained until Drop and UI calls occur on the AppKit thread.
            let succeeded = unsafe { hd_macos_native_display_hide(self.raw) };
            Self::call("hide", succeeded)?;
            if let Some(bounds) = self.last_bounds.as_mut() {
                bounds.visible = false;
            }
            Ok(())
        }

        fn focus_guest(&self) -> Result<(), PlatformError> {
            // SAFETY: raw is retained until Drop and UI calls occur on the AppKit thread.
            let succeeded = unsafe { hd_macos_native_display_focus(self.raw) };
            Self::call("focus", succeeded)
        }

        fn root_is_minimized(&self) -> bool {
            // SAFETY: raw is retained until Drop and UI calls occur on the AppKit thread.
            unsafe { hd_macos_native_display_root_is_minimized(self.raw) }
        }
    }

    impl Drop for MacNativeDisplayHost {
        fn drop(&mut self) {
            // SAFETY: raw was returned retained and is released exactly once here.
            unsafe { hd_macos_native_display_destroy(self.raw) };
        }
    }

    pub(super) fn center_traffic_lights(
        parent: RawWindowHandle,
        titlebar_height: f64,
    ) -> Result<(), PlatformError> {
        let RawWindowHandle::AppKit(handle) = parent else {
            return Err(PlatformError::Unsupported(
                "macOS traffic lights require an AppKit parent view",
            ));
        };
        // SAFETY: winit owns the parent NSView; callers run from the AppKit event thread.
        let succeeded =
            unsafe { hd_macos_center_traffic_lights(handle.ns_view.as_ptr(), titlebar_height) };
        if succeeded {
            Ok(())
        } else {
            Err(PlatformError::Process(
                "center macOS traffic lights failed".to_owned(),
            ))
        }
    }

    pub(super) fn set_window_content_aspect_ratio(
        parent: RawWindowHandle,
        width: f64,
        height: f64,
    ) -> Result<(), PlatformError> {
        let RawWindowHandle::AppKit(handle) = parent else {
            return Err(PlatformError::Unsupported(
                "macOS content aspect ratio requires an AppKit parent view",
            ));
        };
        // SAFETY: winit owns the parent NSView and this runs on the AppKit event thread.
        let succeeded = unsafe {
            hd_macos_set_window_content_aspect_ratio(handle.ns_view.as_ptr(), width, height)
        };
        if succeeded {
            Ok(())
        } else {
            Err(PlatformError::Process(
                "set macOS window content aspect ratio failed".to_owned(),
            ))
        }
    }

    pub(super) fn install_titlebar_controls(
        parent: RawWindowHandle,
        context: *mut c_void,
        callback: MacTitlebarCallback,
    ) -> Result<(), PlatformError> {
        let RawWindowHandle::AppKit(handle) = parent else {
            return Err(PlatformError::Unsupported(
                "macOS titlebar controls require an AppKit parent view",
            ));
        };
        // SAFETY: winit owns the parent NSView; callbacks only enqueue UI messages.
        let succeeded = unsafe {
            hd_macos_install_titlebar_controls(handle.ns_view.as_ptr(), context, callback)
        };
        if succeeded {
            Ok(())
        } else {
            Err(PlatformError::Process(
                "install macOS titlebar controls failed".to_owned(),
            ))
        }
    }

    pub(super) fn set_titlebar_fps(
        parent: RawWindowHandle,
        visible: bool,
        fps_milli: u32,
    ) -> Result<(), PlatformError> {
        let RawWindowHandle::AppKit(handle) = parent else {
            return Err(PlatformError::Unsupported(
                "macOS titlebar FPS requires an AppKit parent view",
            ));
        };
        // SAFETY: winit owns the NSView and this function is called from the AppKit UI thread.
        let succeeded =
            unsafe { hd_macos_set_titlebar_fps(handle.ns_view.as_ptr(), visible, fps_milli) };
        if succeeded {
            Ok(())
        } else {
            Err(PlatformError::Process(
                "update macOS titlebar FPS failed".to_owned(),
            ))
        }
    }

    pub(super) fn set_titlebar_player_state(
        parent: RawWindowHandle,
        state_json: &str,
    ) -> Result<(), PlatformError> {
        let RawWindowHandle::AppKit(handle) = parent else {
            return Err(PlatformError::Unsupported(
                "macOS titlebar player state requires an AppKit parent view",
            ));
        };
        let state_json = CString::new(state_json).map_err(|_| {
            PlatformError::Process("macOS titlebar player state contains NUL".to_owned())
        })?;
        // SAFETY: winit owns the NSView, the C string lives for the synchronous call, and this
        // function is invoked from the AppKit UI thread.
        let succeeded = unsafe {
            hd_macos_set_titlebar_player_state(handle.ns_view.as_ptr(), state_json.as_ptr())
        };
        if succeeded {
            Ok(())
        } else {
            Err(PlatformError::Process(
                "update macOS titlebar player state failed".to_owned(),
            ))
        }
    }

    pub(super) fn titlebar_control_contracts()
    -> Result<Vec<super::MacTitlebarControlContract>, PlatformError> {
        // SAFETY: the Objective-C bridge returns process-lifetime static UTF-8 strings.
        let count = unsafe { hd_macos_titlebar_control_count() };
        let mut controls = Vec::with_capacity(count);
        for index in 0..count {
            let field = |pointer: *const c_char, name: &str| -> Result<String, PlatformError> {
                if pointer.is_null() {
                    return Err(PlatformError::Process(format!(
                        "macOS titlebar contract {index} has null {name}"
                    )));
                }
                // SAFETY: pointers come from static NUL-terminated bridge strings.
                unsafe { CStr::from_ptr(pointer) }
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|error| {
                        PlatformError::Process(format!(
                            "macOS titlebar contract {index} has invalid {name}: {error}"
                        ))
                    })
            };
            // SAFETY: every accessor accepts indices below the count returned above.
            controls.push(super::MacTitlebarControlContract {
                symbol: field(unsafe { hd_macos_titlebar_control_symbol(index) }, "symbol")?,
                tooltip: field(
                    unsafe { hd_macos_titlebar_control_tooltip(index) },
                    "tooltip",
                )?,
                message: field(
                    unsafe { hd_macos_titlebar_control_message(index) },
                    "message",
                )?,
                placement: field(
                    unsafe { hd_macos_titlebar_control_placement(index) },
                    "placement",
                )?,
            });
        }
        Ok(controls)
    }

    pub(super) fn choose_apk_file() -> Result<Option<PathBuf>, PlatformError> {
        let mut output = [0 as c_char; 4096];
        // SAFETY: the fixed buffer remains writable for the duration of the synchronous AppKit
        // panel. The bridge always NUL-terminates a selected path and runs on the UI thread.
        let result = unsafe { hd_macos_choose_apk_file(output.as_mut_ptr(), output.len()) };
        match result {
            1 => {
                // SAFETY: a successful bridge call guarantees a NUL-terminated UTF-8 filesystem
                // representation inside `output`.
                let path = unsafe { CStr::from_ptr(output.as_ptr()) }
                    .to_str()
                    .map_err(|error| {
                        PlatformError::Process(format!(
                            "selected APK path is not valid UTF-8: {error}"
                        ))
                    })?;
                Ok(Some(PathBuf::from(path)))
            }
            0 => Ok(None),
            _ => Err(PlatformError::Process(
                "macOS APK file selection failed".to_owned(),
            )),
        }
    }

    pub(super) fn choose_location_route_file() -> Result<Option<PathBuf>, PlatformError> {
        let mut output = [0 as c_char; 4096];
        // SAFETY: the fixed buffer remains writable for the duration of the synchronous AppKit
        // panel. The bridge always NUL-terminates a selected path and runs on the UI thread.
        let result =
            unsafe { hd_macos_choose_location_route_file(output.as_mut_ptr(), output.len()) };
        match result {
            1 => {
                // SAFETY: a successful bridge call guarantees a NUL-terminated UTF-8 filesystem
                // representation inside `output`.
                let path = unsafe { CStr::from_ptr(output.as_ptr()) }
                    .to_str()
                    .map_err(|error| {
                        PlatformError::Process(format!(
                            "selected location route path is not valid UTF-8: {error}"
                        ))
                    })?;
                Ok(Some(PathBuf::from(path)))
            }
            0 => Ok(None),
            _ => Err(PlatformError::Process(
                "macOS location route file selection failed".to_owned(),
            )),
        }
    }

    pub(super) fn show_error_dialog(
        title: &str,
        message: &str,
        reveal_path: Option<&std::path::Path>,
    ) -> Result<(), PlatformError> {
        let title = CString::new(title)
            .map_err(|_| PlatformError::Process("fatal dialog title contains NUL".to_owned()))?;
        let message = CString::new(message)
            .map_err(|_| PlatformError::Process("fatal dialog message contains NUL".to_owned()))?;
        let reveal_path = reveal_path
            .map(|path| CString::new(path.to_string_lossy().as_bytes()))
            .transpose()
            .map_err(|_| {
                PlatformError::Process("fatal dialog reveal path contains NUL".to_owned())
            })?;
        // SAFETY: all C strings remain alive for the synchronous AppKit alert. A null reveal path
        // intentionally omits the optional “show logs” action.
        let succeeded = unsafe {
            hd_macos_show_error_dialog(
                title.as_ptr(),
                message.as_ptr(),
                reveal_path
                    .as_ref()
                    .map_or(std::ptr::null(), |path| path.as_ptr()),
            )
        };
        succeeded.then_some(()).ok_or_else(|| {
            PlatformError::Process("show macOS fatal error dialog failed".to_owned())
        })
    }
}

#[cfg(target_os = "macos")]
use macos::{MacNativeDisplayHost, MacTitlebarCallback};

#[cfg(target_os = "macos")]
pub fn center_macos_traffic_lights(
    parent: RawWindowHandle,
    titlebar_height: f64,
) -> Result<(), PlatformError> {
    macos::center_traffic_lights(parent, titlebar_height)
}

#[cfg(target_os = "macos")]
pub fn set_macos_window_content_aspect_ratio(
    parent: RawWindowHandle,
    width: f64,
    height: f64,
) -> Result<(), PlatformError> {
    macos::set_window_content_aspect_ratio(parent, width, height)
}

#[cfg(target_os = "macos")]
pub fn install_macos_titlebar_controls(
    parent: RawWindowHandle,
    context: *mut std::ffi::c_void,
    callback: MacTitlebarCallback,
) -> Result<(), PlatformError> {
    macos::install_titlebar_controls(parent, context, callback)
}

#[cfg(target_os = "macos")]
pub fn set_macos_titlebar_fps(
    parent: RawWindowHandle,
    visible: bool,
    fps_milli: u32,
) -> Result<(), PlatformError> {
    macos::set_titlebar_fps(parent, visible, fps_milli)
}

#[cfg(target_os = "macos")]
pub fn set_macos_titlebar_player_state(
    parent: RawWindowHandle,
    state_json: &str,
) -> Result<(), PlatformError> {
    macos::set_titlebar_player_state(parent, state_json)
}

#[cfg(target_os = "macos")]
pub fn macos_titlebar_control_contracts() -> Result<Vec<MacTitlebarControlContract>, PlatformError>
{
    macos::titlebar_control_contracts()
}

#[cfg(target_os = "macos")]
pub fn show_macos_error_dialog(
    title: &str,
    message: &str,
    reveal_path: Option<&std::path::Path>,
) -> Result<(), PlatformError> {
    macos::show_error_dialog(title, message, reveal_path)
}
