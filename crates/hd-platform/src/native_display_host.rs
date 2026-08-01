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
    fn target(&self, owner: WorkerIdentityV2) -> NativeDisplayTargetV2;
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

pub fn choose_apk_file() -> Result<Option<PathBuf>, PlatformError> {
    #[cfg(target_os = "macos")]
    {
        macos::choose_apk_file()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(PlatformError::Unsupported(
            "native APK file selection is not implemented for this platform",
        ))
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsNativeDisplayHost {
    parent_hwnd: windows_sys::Win32::Foundation::HWND,
    hwnd: windows_sys::Win32::Foundation::HWND,
    last_bounds: Option<NativeDisplayBounds>,
}

#[cfg(windows)]
impl WindowsNativeDisplayHost {
    fn new(parent: RawWindowHandle) -> Result<Self, PlatformError> {
        use windows_sys::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::Graphics::Gdi::{BLACK_BRUSH, GetStockObject};
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, GCLP_HBRBACKGROUND, GWL_STYLE,
            GetWindowLongPtrW, RegisterClassExW, SetClassLongPtrW, SetWindowLongPtrW, WNDCLASSEXW,
            WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
        };

        let RawWindowHandle::Win32(handle) = parent else {
            return Err(PlatformError::Unsupported(
                "Windows NativeDisplayHost requires a Win32 parent window",
            ));
        };
        let parent_hwnd = handle.hwnd.get() as windows_sys::Win32::Foundation::HWND;
        let class = wide("HD_NATIVE_DISPLAY_HOST_V2");
        // SAFETY: parent_hwnd comes from a live winit Window. The registered class stores no
        // instance pointers and uses only DefWindowProc with a stock black brush.
        let hwnd = unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let black_brush = GetStockObject(BLACK_BRUSH);
            let window_class = WNDCLASSEXW {
                cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap_or_default(),
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(DefWindowProcW),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: module,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: black_brush,
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
        Ok(Self {
            parent_hwnd,
            hwnd,
            last_bounds: None,
        })
    }
}

#[cfg(windows)]
impl NativeDisplayHost for WindowsNativeDisplayHost {
    fn handle(&self) -> u64 {
        self.hwnd as usize as u64
    }

    fn target(&self, owner: WorkerIdentityV2) -> NativeDisplayTargetV2 {
        NativeDisplayTargetV2::WindowsHwnd {
            hwnd: self.handle(),
            owner,
        }
    }

    fn set_bounds(&mut self, bounds: NativeDisplayBounds) -> Result<(), PlatformError> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            HWND_BOTTOM, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SetWindowPos,
            ShowWindow,
        };

        if self.last_bounds == Some(bounds) {
            return Ok(());
        }
        if !bounds.visible {
            return self.hide();
        }
        let width = i32::try_from(bounds.width_px.max(1)).unwrap_or(i32::MAX);
        let height = i32::try_from(bounds.height_px.max(1)).unwrap_or(i32::MAX);
        // SAFETY: hwnd is a live child owned by this object. Coordinates are bounded physical
        // pixels supplied by the UI, and no activation or cross-thread ownership transfer occurs.
        let positioned = unsafe {
            let positioned = SetWindowPos(
                self.hwnd,
                HWND_BOTTOM,
                bounds.x_px,
                bounds.y_px,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
            if positioned != 0 {
                // The native viewport stays behind every WebView sibling. The surfaces leave the
                // Player rectangle uncovered, so raising this HWND is unnecessary and can flash
                // it over the title bar while Windows applies a resize transaction.
                ShowWindow(self.hwnd, SW_SHOW);
            }
            positioned
        };
        if positioned == 0 {
            // Keep the child hidden if Windows rejected a transient top-level geometry.
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            return Err(PlatformError::Process(
                "position NativeDisplayHost child HWND failed".to_owned(),
            ));
        }
        self.last_bounds = Some(bounds);
        Ok(())
    }

    fn hide(&mut self) -> Result<(), PlatformError> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
        // SAFETY: hwnd remains owned by this object until Drop.
        unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        if let Some(bounds) = self.last_bounds.as_mut() {
            bounds.visible = false;
        }
        Ok(())
    }

    fn focus_guest(&self) -> Result<(), PlatformError> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        use windows_sys::Win32::UI::WindowsAndMessaging::{GW_CHILD, GetWindow};
        // SAFETY: both the viewport and its optional gfxstream child are live UI-thread windows.
        unsafe {
            let child = GetWindow(self.hwnd, GW_CHILD);
            SetFocus(if child.is_null() { self.hwnd } else { child });
        }
        Ok(())
    }

    fn root_is_minimized(&self) -> bool {
        // SAFETY: parent_hwnd is owned by the still-live winit Window.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsIconic(self.parent_hwnd) != 0 }
    }
}

#[cfg(windows)]
impl Drop for WindowsNativeDisplayHost {
    fn drop(&mut self) {
        // SAFETY: hwnd was created by this object and is destroyed exactly once here.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd) };
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
        fn hd_macos_choose_apk_file(output: *mut c_char, capacity: usize) -> i32;
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
        fn hd_macos_titlebar_control_count() -> usize;
        fn hd_macos_titlebar_control_symbol(index: usize) -> *const c_char;
        fn hd_macos_titlebar_control_tooltip(index: usize) -> *const c_char;
        fn hd_macos_titlebar_control_message(index: usize) -> *const c_char;
        fn hd_macos_titlebar_control_placement(index: usize) -> *const c_char;
    }

    pub type MacTitlebarCallback = extern "C" fn(*mut c_void, *const c_char);

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
            let endpoint = std::env::temp_dir().join(format!("bscp-hd-ca-{user_scope}.sock"));
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

        fn target(&self, owner: WorkerIdentityV2) -> NativeDisplayTargetV2 {
            NativeDisplayTargetV2::MacCaContext {
                endpoint: self.endpoint.to_string_lossy().into_owned(),
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
pub fn macos_titlebar_control_contracts() -> Result<Vec<MacTitlebarControlContract>, PlatformError>
{
    macos::titlebar_control_contracts()
}
