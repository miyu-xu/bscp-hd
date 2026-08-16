#![allow(unsafe_code)]

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;
use std::time::Duration;

use raw_window_handle::RawWindowHandle;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, RECT};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcessId, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, FindWindowExW, FlashWindow, GW_CHILD, GW_HWNDNEXT,
    GetClassNameW, GetClientRect, GetParent, GetPropW, GetWindow, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, KillTimer, MoveWindow, RemovePropW,
    SIZE_MINIMIZED, SMTO_ABORTIFHUNG, SMTO_BLOCK, SW_RESTORE, SendMessageTimeoutW,
    SetForegroundWindow, SetPropW, SetTimer, ShowWindow, WM_DPICHANGED, WM_ENTERSIZEMOVE,
    WM_EXITSIZEMOVE, WM_SIZE, WM_SIZING, WM_TIMER, WM_USER, WMSZ_BOTTOM, WMSZ_BOTTOMLEFT,
    WMSZ_BOTTOMRIGHT, WMSZ_LEFT, WMSZ_RIGHT, WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT,
};

use crate::PlatformError;

const CROSVM_CLASS_PREFIX_UTF16: [u16; 6] = [
    b'C' as u16,
    b'R' as u16,
    b'O' as u16,
    b'S' as u16,
    b'V' as u16,
    b'M' as u16,
];
const GFXSTREAM_SUBWINDOW_CLASS_UTF16: [u16; 6] = [
    b's' as u16,
    b'u' as u16,
    b'b' as u16,
    b'W' as u16,
    b'i' as u16,
    b'n' as u16,
];
const ROOT_TITLE: &str = "HD Android";
const DEFAULT_PROFILE_MUTEX: &str = "Local\\HD_DEFAULT_UI_PROFILE_V1";
const DEFAULT_PROFILE_OWNER_PREFIX: &str = "Local\\HD_DEFAULT_UI_OWNER_V1_";
const ROOT_LIFECYCLE_SUBCLASS_ID: usize = 0x0048_4450_4f57_4552;
const ROOT_RESIZE_SETTLE_TIMER_ID: usize = 0x4844_5253;
const ROOT_RESIZE_SETTLE_MS: u32 = 120;
const NATIVE_LIFECYCLE_SUSPENDED_MESSAGE: &[u8] =
    b"{\"command\":\"native_lifecycle\",\"state\":\"suspended\"}\0";
const NATIVE_LIFECYCLE_RESUMED_MESSAGE: &[u8] =
    b"{\"command\":\"native_lifecycle\",\"state\":\"resumed\"}\0";
// Must remain aligned with crosvm's private gpu_display viewport message. A regular child resize
// does not update crosvm's host-to-guest pointer transform or gfxstream projection.
const WM_USER_HOST_VIEWPORT_CHANGE_INTERNAL: u32 = WM_USER + 1;
const FORCE_VIEWPORT_COMMIT_WPARAM: usize = 1 << 31;

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

const DISPLAY_HOST_CLASS_W: &[u16] = &wide_ascii_z(b"HD_NATIVE_DISPLAY_HOST_V2\0");
const ROOT_DISPLAY_HOST_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_ROOT_DISPLAY_HOST_V1\0");
const ROOT_DISPLAY_HOST_COOKIE_W: &[u16] = &wide_ascii_z(b"HD_ROOT_DISPLAY_HOST_COOKIE_V1\0");
const CROSVM_INPUT_CHILD_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_CROSVM_INPUT_CHILD_V1\0");
const CROSVM_INPUT_CHILD_COOKIE_W: &[u16] = &wide_ascii_z(b"HD_CROSVM_INPUT_CHILD_COOKIE_V1\0");
const GFXSTREAM_RENDER_CHILD_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_GFXSTREAM_RENDER_CHILD_V1\0");
const GFXSTREAM_RENDER_CHILD_COOKIE_W: &[u16] =
    &wide_ascii_z(b"HD_GFXSTREAM_RENDER_CHILD_COOKIE_V1\0");
const ROOT_LIFECYCLE_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_ROOT_LIFECYCLE_V1\0");
pub(crate) const ROOT_INTERACTIVE_RESIZE_PROPERTY_W: &[u16] =
    &wide_ascii_z(b"HD_ROOT_INTERACTIVE_RESIZE_V1\0");
const LATEST_VIEWPORT_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_LATEST_VIEWPORT_V1\0");
const LATEST_ROTATION_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_LATEST_ROTATION_V1\0");
const FORCED_VIEWPORT_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_FORCED_VIEWPORT_V1\0");
const FORCED_ROTATION_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_FORCED_ROTATION_V1\0");
const FORCE_VIEWPORT_PENDING_PROPERTY_W: &[u16] = &wide_ascii_z(b"HD_FORCE_VIEWPORT_PENDING_V1\0");

// The OS closes this handle on process exit. Storing only the value keeps the process-lifetime
// ownership explicit without claiming that a raw HANDLE itself is thread-safe.
static DEFAULT_PROFILE_MUTEX_HANDLES: OnceLock<[usize; 2]> = OnceLock::new();

struct ExistingWindowSearch {
    window: HWND,
}

unsafe extern "system" fn find_existing_window(hwnd: HWND, lparam: isize) -> i32 {
    // SAFETY: EnumWindows invokes the callback synchronously with this stack-owned search state.
    let search = unsafe { &mut *(lparam as *mut ExistingWindowSearch) };
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    let mut title = [0_u16; 64];
    let capacity = i32::try_from(title.len()).expect("fixed title buffer fits i32");
    // SAFETY: title is writable and the synchronous Win32 call receives its exact capacity.
    let count = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), capacity) };
    let Ok(count) = usize::try_from(count) else {
        return 1;
    };
    if count == 0 || String::from_utf16_lossy(&title[..count]) != ROOT_TITLE {
        return 1;
    }
    let lifecycle_property = ROOT_LIFECYCLE_PROPERTY_W;
    // SAFETY: hwnd came from EnumWindows and the property name is NUL-terminated. The root
    // controller publishes this marker before the window becomes visible. The visible titlebar
    // itself is a WebView and must not be used as the single-instance identity marker.
    if unsafe { GetPropW(hwnd, lifecycle_property.as_ptr()) }.is_null() {
        return 1;
    }
    let mut process_id = 0_u32;
    // SAFETY: hwnd is live for this callback and process_id is writable.
    if unsafe { GetWindowThreadProcessId(hwnd, &raw mut process_id) } == 0 || process_id == 0 {
        return 1;
    }
    let owner_name = wide(&format!("{DEFAULT_PROFILE_OWNER_PREFIX}{process_id}"));
    // SAFETY: owner_name is NUL-terminated; opening the marker does not acquire or mutate it.
    let owner = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, owner_name.as_ptr()) };
    if owner.is_null() {
        return 1;
    }
    // SAFETY: owner was opened by this callback solely for identity validation.
    unsafe { CloseHandle(owner) };
    search.window = hwnd;
    0
}

fn win32_error(operation: &str) -> PlatformError {
    PlatformError::Process(format!(
        "{operation} failed: {}",
        std::io::Error::last_os_error()
    ))
}

fn find_existing_window_handle() -> Result<Option<HWND>, PlatformError> {
    let mut search = ExistingWindowSearch {
        window: std::ptr::null_mut(),
    };
    // SAFETY: the callback only borrows search for the synchronous enumeration.
    let result = unsafe {
        EnumWindows(
            Some(find_existing_window),
            (&raw mut search).cast::<ExistingWindowSearch>() as isize,
        )
    };
    if result == 0 && search.window.is_null() {
        return Err(win32_error("EnumWindows(existing HD window)"));
    }
    Ok((!search.window.is_null()).then_some(search.window))
}

pub(crate) fn activate_existing_application() -> Result<bool, PlatformError> {
    let name = wide(DEFAULT_PROFILE_MUTEX);
    // SAFETY: name is stable NUL-terminated UTF-16 and Local scopes the mutex to this logon
    // session; default security prevents HD from broadening access itself.
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if mutex.is_null() {
        return Err(win32_error("CreateMutexW(default HD profile)"));
    }
    // SAFETY: GetLastError is consumed immediately after CreateMutexW.
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if !already_running {
        // SAFETY: retrieving the current PID has no preconditions.
        let owner_name = wide(&format!("{DEFAULT_PROFILE_OWNER_PREFIX}{}", unsafe {
            GetCurrentProcessId()
        }));
        // SAFETY: owner_name is stable NUL-terminated UTF-16 for the synchronous call.
        let owner = unsafe { CreateMutexW(std::ptr::null(), 0, owner_name.as_ptr()) };
        if owner.is_null() {
            // SAFETY: mutex belongs to this process and was created above.
            unsafe { CloseHandle(mutex) };
            return Err(win32_error("CreateMutexW(default HD profile owner)"));
        }
        DEFAULT_PROFILE_MUTEX_HANDLES
            .set([mutex as usize, owner as usize])
            .map_err(|_| {
                // SAFETY: neither handle was published when OnceLock rejected the value.
                unsafe {
                    CloseHandle(owner);
                    CloseHandle(mutex);
                }
                PlatformError::Process("default HD profile mutex was initialized twice".to_owned())
            })?;
        return Ok(false);
    }
    // SAFETY: close only the handle returned to this second process; the first process retains its
    // own handle and therefore the single-instance claim.
    unsafe { CloseHandle(mutex) };

    // Match the desktop shell's bounded first-paint contract so two rapid launches cannot race the
    // first process while its root window is still intentionally hidden.
    for _ in 0..320 {
        if let Some(window) = find_existing_window_handle()? {
            // SAFETY: the window was just enumerated and has HD's native titlebar child.
            unsafe {
                ShowWindow(window, SW_RESTORE);
                BringWindowToTop(window);
                if SetForegroundWindow(window) == 0 {
                    // Windows can deny foreground transfer to a background launcher. Flash the
                    // taskbar entry instead of silently appearing to ignore the second launch.
                    FlashWindow(window, 1);
                }
            }
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(PlatformError::Process(
        "another HD default-profile UI owns the single-instance lock but did not publish a visible native window within 16 seconds".to_owned(),
    ))
}

pub type WindowsTitlebarCallback = extern "C" fn(*mut c_void, *const c_char);

struct RootLifecycleHook {
    callback_context: *mut c_void,
    callback: WindowsTitlebarCallback,
    content_aspect: Option<WindowsContentAspect>,
    interactive_resize: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowsContentAspect {
    width: u32,
    height: u32,
    titlebar_height: u32,
    rotation_quarters: u8,
    dpi: u32,
}

/// Install only the root-window lifecycle and fixed-content-aspect controller.
///
/// The Windows product titlebar is rendered by the dedicated top `WebView`. Keeping this narrow
/// entry point separate prevents the legacy Win32 button host from being created, hidden and then
/// repeatedly moved during `WebView` titlebar clicks or root resizes.
pub fn install_windows_root_window_controller(
    parent: RawWindowHandle,
    context: *mut c_void,
    callback: WindowsTitlebarCallback,
) -> Result<(), PlatformError> {
    install_windows_root_lifecycle(parent_hwnd(parent)?, context, callback)
}

fn install_windows_root_lifecycle(
    parent: HWND,
    context: *mut c_void,
    callback: WindowsTitlebarCallback,
) -> Result<(), PlatformError> {
    use windows_sys::Win32::UI::Shell::SetWindowSubclass;

    let hook = Box::new(RootLifecycleHook {
        callback_context: context,
        callback,
        content_aspect: None,
        interactive_resize: false,
    });
    let hook = Box::into_raw(hook);
    // SAFETY: installation runs on the winit UI thread. The root subclass owns hook until the
    // root's WM_NCDESTROY and uses a process-unique id that does not collide with WRY's subclass.
    if unsafe {
        SetWindowSubclass(
            parent,
            Some(windows_root_lifecycle_proc),
            ROOT_LIFECYCLE_SUBCLASS_ID,
            hook as usize,
        )
    } == 0
    {
        // SAFETY: SetWindowSubclass rejected the pointer, so it was never published to Win32.
        unsafe { drop(Box::from_raw(hook)) };
        return Err(win32_error("SetWindowSubclass(Windows power lifecycle)"));
    }
    let property = ROOT_LIFECYCLE_PROPERTY_W;
    // SAFETY: the property key is NUL-terminated and the hook remains owned by the installed
    // subclass until WM_NCDESTROY. This avoids depending on GetWindowSubclass, which is absent
    // from the supported MinGW publication import surface.
    if unsafe { SetPropW(parent, property.as_ptr(), hook.cast()) } == 0 {
        // SAFETY: the property was not published; detach this exact subclass before releasing its
        // reference data.
        unsafe {
            windows_sys::Win32::UI::Shell::RemoveWindowSubclass(
                parent,
                Some(windows_root_lifecycle_proc),
                ROOT_LIFECYCLE_SUBCLASS_ID,
            );
            drop(Box::from_raw(hook));
        }
        return Err(win32_error("SetPropW(Windows root lifecycle)"));
    }
    Ok(())
}

unsafe fn commit_current_root_resize(hwnd: HWND, reference_data: usize, force_commit: bool) {
    if let Some(aspect) = (unsafe { (reference_data as *const RootLifecycleHook).as_ref() })
        .and_then(|hook| hook.content_aspect)
    {
        let mut client = RECT::default();
        if unsafe { GetClientRect(hwnd, &raw mut client) } != 0 {
            unsafe {
                synchronize_native_children_during_root_resize(
                    hwnd,
                    rect_width(client),
                    rect_height(client),
                    aspect,
                    false,
                    force_commit,
                );
            }
            // synchronize_native_children_during_root_resize already resolves the live display
            // host and sends the matching crosvm transaction. Falling through used to send a
            // second forced message for the same extent, bypassing crosvm's applied-size dedupe.
            return;
        }
    }
    unsafe { commit_live_display_host_viewport(hwnd, force_commit) };
}

unsafe fn force_commit_current_root_resize(hwnd: HWND, reference_data: usize) {
    unsafe { commit_current_root_resize(hwnd, reference_data, true) };
}

unsafe fn validated_cached_display_child(
    display_host: HWND,
    property: &[u16],
    cookie: &[u16],
) -> HWND {
    let child = unsafe { GetPropW(display_host, property.as_ptr()) };
    if !child.is_null()
        && unsafe { IsWindow(child) } != 0
        && unsafe { GetParent(child) } == display_host
        && unsafe { GetPropW(child, cookie.as_ptr()) } == display_host
    {
        child
    } else {
        std::ptr::null_mut()
    }
}

unsafe fn native_display_children(display_host: HWND) -> (HWND, HWND) {
    let mut input = unsafe {
        validated_cached_display_child(
            display_host,
            CROSVM_INPUT_CHILD_PROPERTY_W,
            CROSVM_INPUT_CHILD_COOKIE_W,
        )
    };
    let mut render = unsafe {
        validated_cached_display_child(
            display_host,
            GFXSTREAM_RENDER_CHILD_PROPERTY_W,
            GFXSTREAM_RENDER_CHILD_COOKIE_W,
        )
    };
    if !input.is_null() && !render.is_null() {
        return (input, render);
    }

    // WM_SIZE can run at pointer cadence during live resize. Scan only when a cached child is
    // absent or was reparented, and compare fixed UTF-16 class buffers without allocating a
    // String. The verified parent relationship makes stale/reused HWND values fail closed.
    let mut child = unsafe { GetWindow(display_host, GW_CHILD) };
    while !child.is_null() && (input.is_null() || render.is_null()) {
        let mut class_name = [0_u16; 64];
        let count = unsafe {
            GetClassNameW(
                child,
                class_name.as_mut_ptr(),
                i32::try_from(class_name.len()).unwrap_or(64),
            )
        };
        if let Ok(count) = usize::try_from(count) {
            if input.is_null()
                && count >= CROSVM_CLASS_PREFIX_UTF16.len()
                && class_name[..CROSVM_CLASS_PREFIX_UTF16.len()] == CROSVM_CLASS_PREFIX_UTF16
            {
                input = child;
                let property = CROSVM_INPUT_CHILD_PROPERTY_W;
                let cookie = CROSVM_INPUT_CHILD_COOKIE_W;
                unsafe { SetPropW(child, cookie.as_ptr(), display_host.cast()) };
                unsafe { SetPropW(display_host, property.as_ptr(), child.cast()) };
            } else if render.is_null()
                && count == GFXSTREAM_SUBWINDOW_CLASS_UTF16.len()
                && class_name[..count] == GFXSTREAM_SUBWINDOW_CLASS_UTF16
            {
                render = child;
                let property = GFXSTREAM_RENDER_CHILD_PROPERTY_W;
                let cookie = GFXSTREAM_RENDER_CHILD_COOKIE_W;
                unsafe { SetPropW(child, cookie.as_ptr(), display_host.cast()) };
                unsafe { SetPropW(display_host, property.as_ptr(), child.cast()) };
            }
        }
        child = unsafe { GetWindow(child, GW_HWNDNEXT) };
    }
    (input, render)
}

unsafe fn validated_cached_display_host(root: HWND) -> HWND {
    let display_host = unsafe { GetPropW(root, ROOT_DISPLAY_HOST_PROPERTY_W.as_ptr()) } as HWND;
    if !display_host.is_null()
        && unsafe { IsWindow(display_host) } != 0
        && unsafe { GetParent(display_host) } == root
        && unsafe { GetPropW(display_host, ROOT_DISPLAY_HOST_COOKIE_W.as_ptr()) } == root
    {
        return display_host;
    }

    let display_host = unsafe {
        FindWindowExW(
            root,
            std::ptr::null_mut(),
            DISPLAY_HOST_CLASS_W.as_ptr(),
            std::ptr::null(),
        )
    };
    if !display_host.is_null() {
        unsafe { cache_windows_native_display_host(root, display_host) };
    }
    display_host
}

pub(crate) unsafe fn cache_windows_native_display_host(root: HWND, display_host: HWND) {
    if root.is_null()
        || display_host.is_null()
        || unsafe { IsWindow(display_host) } == 0
        || unsafe { GetParent(display_host) } != root
    {
        return;
    }
    unsafe {
        SetPropW(
            display_host,
            ROOT_DISPLAY_HOST_COOKIE_W.as_ptr(),
            root.cast(),
        );
        SetPropW(
            root,
            ROOT_DISPLAY_HOST_PROPERTY_W.as_ptr(),
            display_host.cast(),
        );
    }
}

pub(crate) unsafe fn uncache_windows_native_display_host(root: HWND, display_host: HWND) {
    if !root.is_null()
        && unsafe { GetPropW(root, ROOT_DISPLAY_HOST_PROPERTY_W.as_ptr()) } == display_host
    {
        unsafe { RemovePropW(root, ROOT_DISPLAY_HOST_PROPERTY_W.as_ptr()) };
    }
    if !display_host.is_null()
        && unsafe { GetPropW(display_host, ROOT_DISPLAY_HOST_COOKIE_W.as_ptr()) } == root
    {
        unsafe { RemovePropW(display_host, ROOT_DISPLAY_HOST_COOKIE_W.as_ptr()) };
    }
}

unsafe fn commit_live_display_host_viewport(root: HWND, force_commit: bool) {
    let display_host = unsafe { validated_cached_display_host(root) };
    if display_host.is_null() {
        return;
    }
    let mut viewport = RECT::default();
    if unsafe { GetClientRect(display_host, &raw mut viewport) } == 0 {
        return;
    }
    let width = rect_width(viewport).max(1);
    let height = rect_height(viewport).max(1);
    let (Ok(width_word), Ok(height_word)) = (u16::try_from(width), u16::try_from(height)) else {
        return;
    };
    let packed = (u32::from(height_word) << 16) | u32::from(width_word);
    let Ok(lparam) = isize::try_from(packed) else {
        return;
    };
    let (input, _) = unsafe { native_display_children(display_host) };
    if input.is_null() {
        return;
    }
    let property = LATEST_VIEWPORT_PROPERTY_W;
    unsafe { SetPropW(input, property.as_ptr(), lparam as *mut c_void) };
    let rotation_property = LATEST_ROTATION_PROPERTY_W;
    let rotation = unsafe { GetPropW(input, rotation_property.as_ptr()) } as usize;
    if force_commit {
        let force_property = FORCE_VIEWPORT_PENDING_PROPERTY_W;
        unsafe { SetPropW(input, force_property.as_ptr(), lparam as *mut c_void) };
    }
    let mut message_result = 0_usize;
    unsafe {
        SendMessageTimeoutW(
            input,
            WM_USER_HOST_VIEWPORT_CHANGE_INTERNAL,
            rotation
                | if force_commit {
                    FORCE_VIEWPORT_COMMIT_WPARAM
                } else {
                    0
                },
            lparam,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            100,
            &raw mut message_result,
        )
    };
    if message_result == 1 {
        let forced_viewport = FORCED_VIEWPORT_PROPERTY_W;
        unsafe { SetPropW(input, forced_viewport.as_ptr(), lparam as *mut c_void) };
        if rotation != 0 {
            let forced_rotation = FORCED_ROTATION_PROPERTY_W;
            unsafe { SetPropW(input, forced_rotation.as_ptr(), rotation as *mut c_void) };
        }
    }
}

unsafe fn commit_settled_root_resize(hwnd: HWND, reference_data: usize) {
    let property = ROOT_INTERACTIVE_RESIZE_PROPERTY_W;
    // Clear the marker only after the low-priority timer observes a drained resize queue. This
    // keeps delayed winit Resized events from recreating the swapchain at historical drag sizes.
    unsafe { RemovePropW(hwnd, property.as_ptr()) };
    if let Some(hook) = unsafe { (reference_data as *mut RootLifecycleHook).as_mut() } {
        hook.interactive_resize = false;
    }
    // Mouse-up already performed the single forced resize. The settle pass exists only after the
    // queued WM_SIZE/winit tail has drained; send an ordinary latest-value transaction so crosvm
    // repairs a missing acknowledgement but deduplicates an already-applied final extent.
    unsafe { commit_current_root_resize(hwnd, reference_data, false) };
}

unsafe fn handle_root_interactive_resize(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    reference_data: usize,
) {
    let property = ROOT_INTERACTIVE_RESIZE_PROPERTY_W;
    if message == WM_ENTERSIZEMOVE {
        if let Some(hook) = unsafe { (reference_data as *mut RootLifecycleHook).as_mut() } {
            hook.interactive_resize = true;
        }
        // SAFETY: the property stores the already-live root hook as a nonzero marker. Consumers
        // only test it for null and never dereference it through this property.
        unsafe { SetPropW(hwnd, property.as_ptr(), reference_data as *mut c_void) };
        unsafe { KillTimer(hwnd, ROOT_RESIZE_SETTLE_TIMER_ID) };
        return;
    }
    if message == WM_EXITSIZEMOVE {
        // Commit the mouse-up extent immediately so a startup surface cannot remain at gfxstream's
        // 1024x768 bootstrap size. Keep the interactive marker until the timer drains any queued
        // winit/Worker tail; its later force is idempotent when this transaction already applied.
        unsafe { force_commit_current_root_resize(hwnd, reference_data) };
        // WM_TIMER is generated only after higher-priority resize messages have drained. The short
        // settle interval also coalesces cross-thread SetWindowPos tails observed after mouse-up.
        if unsafe {
            SetTimer(
                hwnd,
                ROOT_RESIZE_SETTLE_TIMER_ID,
                ROOT_RESIZE_SETTLE_MS,
                None,
            )
        } != 0
        {
            return;
        }
        unsafe { commit_settled_root_resize(hwnd, reference_data) };
        return;
    }
    if message == WM_TIMER && wparam == ROOT_RESIZE_SETTLE_TIMER_ID {
        unsafe { KillTimer(hwnd, ROOT_RESIZE_SETTLE_TIMER_ID) };
        unsafe { commit_settled_root_resize(hwnd, reference_data) };
    }
}

unsafe fn update_root_content_dpi(reference_data: usize, wparam: usize) {
    let Some(aspect) = (unsafe { (reference_data as *mut RootLifecycleHook).as_mut() })
        .and_then(|hook| hook.content_aspect.as_mut())
    else {
        return;
    };
    let new_dpi = u32::try_from(wparam & 0xffff).unwrap_or_default().max(1);
    let old_dpi = aspect.dpi.max(1);
    aspect.titlebar_height = u32::try_from(
        u64::from(aspect.titlebar_height)
            .saturating_mul(u64::from(new_dpi))
            .saturating_add(u64::from(old_dpi / 2))
            / u64::from(old_dpi),
    )
    .unwrap_or(u32::MAX);
    aspect.dpi = new_dpi;
}

unsafe extern "system" fn windows_root_lifecycle_proc(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
    _subclass_id: usize,
    reference_data: usize,
) -> isize {
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMECRITICAL, PBT_APMRESUMESTANDBY, PBT_APMRESUMESUSPEND,
        PBT_APMSTANDBY, PBT_APMSUSPEND, WM_NCDESTROY, WM_POWERBROADCAST,
    };

    if message == WM_NCDESTROY {
        // Let winit/WRY and the original root procedure finish first while reference_data remains
        // live, then detach this exact subclass and release its process-local callback state.
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        unsafe {
            RemoveWindowSubclass(
                hwnd,
                Some(windows_root_lifecycle_proc),
                ROOT_LIFECYCLE_SUBCLASS_ID,
            )
        };
        let property = ROOT_LIFECYCLE_PROPERTY_W;
        let interactive_resize_property = ROOT_INTERACTIVE_RESIZE_PROPERTY_W;
        // SAFETY: the property belongs to this root and is removed before its hook is released.
        unsafe {
            KillTimer(hwnd, ROOT_RESIZE_SETTLE_TIMER_ID);
            RemovePropW(hwnd, property.as_ptr());
            RemovePropW(hwnd, interactive_resize_property.as_ptr());
        };
        if reference_data != 0 {
            unsafe { drop(Box::from_raw(reference_data as *mut RootLifecycleHook)) };
        }
        return result;
    }
    unsafe { handle_root_interactive_resize(hwnd, message, wparam, reference_data) };

    if message == WM_DPICHANGED {
        unsafe { update_root_content_dpi(reference_data, wparam) };
    }

    if message == WM_POWERBROADCAST
        && let Some(hook) = unsafe { (reference_data as *const RootLifecycleHook).as_ref() }
    {
        let power_event = u32::try_from(wparam).unwrap_or_default();
        let lifecycle_message = if matches!(power_event, PBT_APMSUSPEND | PBT_APMSTANDBY) {
            Some(NATIVE_LIFECYCLE_SUSPENDED_MESSAGE)
        } else if matches!(
            power_event,
            PBT_APMRESUMEAUTOMATIC
                | PBT_APMRESUMECRITICAL
                | PBT_APMRESUMESTANDBY
                | PBT_APMRESUMESUSPEND
        ) {
            Some(NATIVE_LIFECYCLE_RESUMED_MESSAGE)
        } else {
            None
        };
        if let Some(lifecycle_message) = lifecycle_message {
            (hook.callback)(
                hook.callback_context,
                lifecycle_message.as_ptr().cast::<c_char>(),
            );
        }
    }
    if message == WM_SIZE
        && u32::try_from(wparam).unwrap_or_default() != SIZE_MINIMIZED
        && let Some(hook) = unsafe { (reference_data as *const RootLifecycleHook).as_ref() }
        && let Some(aspect) = hook.content_aspect
    {
        let mut client = RECT::default();
        // SAFETY: hwnd is the live root, client is writable, and all native children are owned by
        // this root for the duration of the synchronous resize notification.
        if unsafe { GetClientRect(hwnd, &raw mut client) } != 0 {
            unsafe {
                synchronize_native_children_during_root_resize(
                    hwnd,
                    rect_width(client),
                    rect_height(client),
                    aspect,
                    hook.interactive_resize,
                    false,
                );
            };
        }
    }
    if message == WM_SIZING
        && lparam != 0
        && let Some(hook) = unsafe { (reference_data as *const RootLifecycleHook).as_ref() }
        && let Some(aspect) = hook.content_aspect
    {
        let mut window = RECT::default();
        let mut client = RECT::default();
        // SAFETY: hwnd is the live subclassed root and both rectangles are writable for the
        // duration of these synchronous calls.
        if unsafe { GetWindowRect(hwnd, &raw mut window) } != 0
            && unsafe { GetClientRect(hwnd, &raw mut client) } != 0
        {
            let frame_width = rect_width(window).saturating_sub(rect_width(client));
            let frame_height = rect_height(window).saturating_sub(rect_height(client));
            // SAFETY: WM_SIZING defines lparam as a writable RECT owned by the caller until the
            // subclass returns.
            let proposed = unsafe { &mut *(lparam as *mut RECT) };
            if constrain_sizing_rect(
                proposed,
                u32::try_from(wparam).unwrap_or_default(),
                frame_width,
                frame_height,
                aspect,
            ) {
                return 1;
            }
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

pub fn set_windows_window_content_aspect_ratio(
    parent: RawWindowHandle,
    content_width: u32,
    content_height: u32,
    titlebar_height: u32,
    rotation_quarters: u8,
    dpi: u32,
) -> Result<(), PlatformError> {
    let parent = parent_hwnd(parent)?;
    let property = ROOT_LIFECYCLE_PROPERTY_W;
    // SAFETY: this property is installed with the root subclass and removed before the hook is
    // freed. The setter is called synchronously on the same UI thread as native layout updates.
    let reference_data = unsafe { GetPropW(parent, property.as_ptr()) };
    if reference_data.is_null() {
        return Err(PlatformError::Process(
            "Windows root lifecycle hook was not found".to_owned(),
        ));
    }
    let hook = unsafe { reference_data.cast::<RootLifecycleHook>().as_mut() }.ok_or_else(|| {
        PlatformError::Process("Windows root lifecycle hook state is unavailable".to_owned())
    })?;
    hook.content_aspect =
        (content_width != 0 && content_height != 0).then_some(WindowsContentAspect {
            width: content_width,
            height: content_height,
            titlebar_height,
            rotation_quarters: rotation_quarters & 3,
            dpi: dpi.max(1),
        });
    Ok(())
}

fn rect_width(rect: RECT) -> i32 {
    rect.right.saturating_sub(rect.left).max(0)
}

fn rect_height(rect: RECT) -> i32 {
    rect.bottom.saturating_sub(rect.top).max(0)
}

fn scaled_dimension(value: i32, numerator: u32, denominator: u32) -> i32 {
    let value = i64::from(value.max(1));
    let numerator = i64::from(numerator.max(1));
    let denominator = i64::from(denominator.max(1));
    i32::try_from(
        value
            .saturating_mul(numerator)
            .saturating_add(denominator / 2)
            / denominator,
    )
    .unwrap_or(i32::MAX)
    .max(1)
}

fn aspect_fit_client(client_width: i32, client_height: i32, aspect: WindowsContentAspect) -> RECT {
    let available_width = client_width.max(1);
    let titlebar_height = i32::try_from(aspect.titlebar_height)
        .unwrap_or(i32::MAX)
        .min(client_height.max(0));
    let available_height = client_height.saturating_sub(titlebar_height).max(1);
    let width_from_height = scaled_dimension(available_height, aspect.width, aspect.height);
    let height_from_width = scaled_dimension(available_width, aspect.height, aspect.width);
    let (width, height) = if height_from_width <= available_height {
        (available_width, height_from_width)
    } else {
        (width_from_height.min(available_width), available_height)
    };
    let left = available_width.saturating_sub(width) / 2;
    let top = titlebar_height.saturating_add(available_height.saturating_sub(height) / 2);
    RECT {
        left,
        top,
        right: left.saturating_add(width),
        bottom: top.saturating_add(height),
    }
}

pub(crate) fn current_windows_root_content_bounds(root: HWND) -> Option<(i32, i32, i32, i32)> {
    let property = ROOT_LIFECYCLE_PROPERTY_W;
    // SAFETY: the lifecycle property owns a RootLifecycleHook until WM_NCDESTROY, and callers use
    // this helper only from the same root UI thread while processing a native layout event.
    let hook = unsafe { (GetPropW(root, property.as_ptr()) as *const RootLifecycleHook).as_ref() }?;
    let aspect = hook.content_aspect?;
    let mut client = RECT::default();
    // SAFETY: root is the live winit HWND and client remains writable for the synchronous query.
    if unsafe { GetClientRect(root, &raw mut client) } == 0 {
        return None;
    }
    let bounds = aspect_fit_client(rect_width(client), rect_height(client), aspect);
    Some((
        bounds.left,
        bounds.top,
        rect_width(bounds),
        rect_height(bounds),
    ))
}

pub(crate) fn windows_root_is_interactive_resize(root: HWND) -> bool {
    let property = ROOT_LIFECYCLE_PROPERTY_W;
    // SAFETY: the property is the live hook owned by the root subclass. The Player queries it on
    // the same UI thread that updates the flag from WM_ENTERSIZEMOVE and the settle timer.
    unsafe {
        (GetPropW(root, property.as_ptr()) as *const RootLifecycleHook)
            .as_ref()
            .is_some_and(|hook| hook.interactive_resize)
    }
}

#[allow(clippy::too_many_lines)]
unsafe fn synchronize_native_children_during_root_resize(
    root: HWND,
    client_width: i32,
    client_height: i32,
    aspect: WindowsContentAspect,
    interactive_resize: bool,
    force_commit: bool,
) {
    if client_width <= 0 || client_height <= 0 {
        return;
    }

    // WM_SIZE is delivered synchronously while Windows commits the new root rectangle. Updating
    // the Android native children here prevents one DWM composition from combining the new
    // WebView titlebar with the previous Android position and width. The WebView itself is laid
    // out by WRY from the later winit `Resized` event; no hidden native toolbar participates.
    let display_host = unsafe { validated_cached_display_host(root) };
    if display_host.is_null() {
        return;
    }

    let bounds = aspect_fit_client(client_width, client_height, aspect);
    let width = rect_width(bounds).max(1);
    let height = rect_height(bounds).max(1);
    let (input, render) = unsafe { native_display_children(display_host) };

    // Rotation and extent are one projection transaction. Programmatic orientation changes can
    // synchronously produce WM_SIZE before winit delivers its later Resized event; publishing the
    // hook's new rotation first prevents that WM_SIZE from committing swapped dimensions with
    // the previous input transform.
    let encoded_rotation = usize::from((aspect.rotation_quarters & 3) + 1);
    if !input.is_null() {
        let property = LATEST_ROTATION_PROPERTY_W;
        unsafe { SetPropW(input, property.as_ptr(), encoded_rotation as *mut c_void) };
    }

    // Publish before changing the viewport. A previously queued crosvm notification may run
    // concurrently with this root procedure; the scalar property prevents that old message from
    // restoring the previous input transform and gfxstream extent.
    let latest_viewport = if !input.is_null()
        && let (Ok(width), Ok(height)) = (u16::try_from(width), u16::try_from(height))
    {
        let packed = (u32::from(height) << 16) | u32::from(width);
        isize::try_from(packed).ok().inspect(|lparam| {
            let property = LATEST_VIEWPORT_PROPERTY_W;
            unsafe {
                SetPropW(input, property.as_ptr(), *lparam as *mut c_void);
            }
        })
    } else {
        None
    };

    if interactive_resize {
        // Keep both process-owned children and the Vulkan swapchain completely stationary during
        // live resize. Center/crop the retained frame by moving HD's own parent viewport at the
        // retained render extent. The previous implementation posted one cross-thread
        // SetWindowPos per WM_SIZE; a 125 Hz drag could leave a tail of stale child positions that
        // visibly oscillated after mouse-up. WM_EXITSIZEMOVE performs the only child/projection
        // transaction at the final extent.
        if !render.is_null() {
            let mut retained = RECT::default();
            if unsafe { GetClientRect(render, &raw mut retained) } != 0 {
                let retained_width = rect_width(retained).max(1);
                let retained_height = rect_height(retained).max(1);
                unsafe {
                    MoveWindow(
                        display_host,
                        bounds
                            .left
                            .saturating_add(width.saturating_sub(retained_width) / 2),
                        bounds
                            .top
                            .saturating_add(height.saturating_sub(retained_height) / 2),
                        retained_width,
                        retained_height,
                        0,
                    )
                };
                return;
            }
        }
        unsafe { MoveWindow(display_host, bounds.left, bounds.top, width, height, 0) };
        return;
    }

    // The root owns only this stable clipping viewport. crosvm's window thread is the sole owner
    // of both native children: it updates the input HWND and calls gfxstream's scanout-aware setup
    // exactly once for the same latest size. Moving all three HWNDs here as well raced that path,
    // repeatedly invalidated the Vulkan swapchain, and exposed empty frames during pointer-driven
    // Android animations.
    if unsafe { MoveWindow(display_host, bounds.left, bounds.top, width, height, 0) } == 0 {
        return;
    }

    if let Some(lparam) = latest_viewport {
        if force_commit {
            // Persist the force requirement before crossing into crosvm. Its Surface clears this
            // marker only after the input transform and gfxstream projection commit together; a
            // parked dispatcher or timeout can therefore replay the final mouse-up transaction.
            let force_property = FORCE_VIEWPORT_PENDING_PROPERTY_W;
            unsafe { SetPropW(input, force_property.as_ptr(), lparam as *mut c_void) };
        }
        let mut message_result = 0_usize;
        // A bounded synchronous message makes crosvm apply the input transform and request the
        // matching gfxstream viewport before the deferred winit layout runs. SMTO_ABORTIFHUNG
        // keeps a stalled renderer from permanently blocking the HD window procedure.
        unsafe {
            SendMessageTimeoutW(
                input,
                WM_USER_HOST_VIEWPORT_CHANGE_INTERNAL,
                encoded_rotation
                    | if force_commit {
                        FORCE_VIEWPORT_COMMIT_WPARAM
                    } else {
                        0
                    },
                lparam,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                100,
                &raw mut message_result,
            )
        };
        if message_result == 1 {
            let forced_viewport = FORCED_VIEWPORT_PROPERTY_W;
            unsafe { SetPropW(input, forced_viewport.as_ptr(), lparam as *mut c_void) };
            let forced_rotation = FORCED_ROTATION_PROPERTY_W;
            unsafe {
                SetPropW(
                    input,
                    forced_rotation.as_ptr(),
                    encoded_rotation as *mut c_void,
                )
            };
        }
    }
}

fn constrain_sizing_rect(
    rect: &mut RECT,
    edge: u32,
    frame_width: i32,
    frame_height: i32,
    aspect: WindowsContentAspect,
) -> bool {
    let outer_width = rect_width(*rect).max(1);
    let outer_height = rect_height(*rect).max(1);
    let frame_width = frame_width.max(0);
    let frame_height = frame_height.max(0);
    let titlebar_height = i32::try_from(aspect.titlebar_height).unwrap_or(i32::MAX);
    let client_width = outer_width.saturating_sub(frame_width).max(1);
    let content_height = outer_height
        .saturating_sub(frame_height)
        .saturating_sub(titlebar_height)
        .max(1);
    let height_from_width = scaled_dimension(client_width, aspect.height, aspect.width)
        .saturating_add(frame_height)
        .saturating_add(titlebar_height);
    let width_from_height =
        scaled_dimension(content_height, aspect.width, aspect.height).saturating_add(frame_width);

    let width_drives = match edge {
        WMSZ_LEFT | WMSZ_RIGHT => true,
        WMSZ_TOP | WMSZ_BOTTOM => false,
        WMSZ_TOPLEFT | WMSZ_TOPRIGHT | WMSZ_BOTTOMLEFT | WMSZ_BOTTOMRIGHT => {
            height_from_width.abs_diff(outer_height) <= width_from_height.abs_diff(outer_width)
        }
        _ => return false,
    };
    if width_drives {
        if matches!(edge, WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT) {
            rect.top = rect.bottom.saturating_sub(height_from_width);
        } else {
            rect.bottom = rect.top.saturating_add(height_from_width);
        }
    } else if matches!(edge, WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT) {
        rect.left = rect.right.saturating_sub(width_from_height);
    } else {
        rect.right = rect.left.saturating_add(width_from_height);
    }
    true
}

fn parent_hwnd(parent: RawWindowHandle) -> Result<HWND, PlatformError> {
    let RawWindowHandle::Win32(handle) = parent else {
        return Err(PlatformError::Unsupported(
            "Windows native titlebar requires a Win32 parent window",
        ));
    };
    Ok(handle.hwnd.get() as HWND)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORTRAIT: WindowsContentAspect = WindowsContentAspect {
        width: 1080,
        height: 1920,
        titlebar_height: 32,
        rotation_quarters: 0,
        dpi: 96,
    };

    #[test]
    fn horizontal_resize_preserves_android_content_aspect() {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 540,
            bottom: 928,
        };
        assert!(constrain_sizing_rect(&mut rect, WMSZ_RIGHT, 0, 0, PORTRAIT,));
        assert_eq!(rect_width(rect), 540);
        assert_eq!(rect_height(rect), 992);
        assert_eq!(rect_height(rect) - 32, 960);
    }
}
