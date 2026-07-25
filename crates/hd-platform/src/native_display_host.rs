#![cfg_attr(windows, allow(unsafe_code))]

use raw_window_handle::RawWindowHandle;

use crate::PlatformError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeDisplayBounds {
    pub x_px: i32,
    pub y_px: i32,
    pub width_px: u32,
    pub height_px: u32,
    pub visible: bool,
}

/// Volatile, platform-owned display container used by gfxstream/crosvm.
///
/// Implementations never persist the native handle. The UI owns this object for exactly as long
/// as its top-level window and sends only the handle plus process identity through a short-lived
/// display session.
pub trait NativeDisplayHost: std::fmt::Debug {
    fn handle(&self) -> u64;
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
    #[cfg(not(windows))]
    {
        let _ = parent;
        Err(PlatformError::Unsupported(
            "NativeDisplayHost is currently implemented only on Windows",
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
