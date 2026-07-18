#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::c_void;

use hd_core::DisplayLeaseV1;
use uuid::Uuid;
use windows_sys::Win32::Foundation::{GetLastError, HWND};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos, WS_CHILD,
    WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE,
};

use crate::{
    DisplayEmbedder, DisplayRect, NativeWindowBinding, PlatformDisplayLease, PlatformError,
};

#[derive(Debug, Default)]
pub struct NativeDisplayEmbedder {
    windows: BTreeMap<Uuid, usize>,
}

impl DisplayEmbedder for NativeDisplayEmbedder {
    fn acquire(
        &mut self,
        parent: &NativeWindowBinding,
        rect: DisplayRect,
    ) -> Result<PlatformDisplayLease, PlatformError> {
        let NativeWindowBinding::Win32Hwnd(parent) = parent else {
            return Err(PlatformError::NativeDisplay(
                "Windows display embedder requires a Win32 HWND".to_owned(),
            ));
        };
        let parent = *parent as HWND;
        if parent.is_null() {
            return Err(PlatformError::NativeDisplay(
                "parent HWND is null".to_owned(),
            ));
        }

        let class = wide("STATIC");
        let title = wide("HD Android viewport");
        // SAFETY: the class/title buffers live across the call, the parent is supplied by winit,
        // and the returned HWND is checked and owned by this embedder until release/drop.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                rect.x,
                rect.y,
                i32::try_from(rect.width).unwrap_or(i32::MAX),
                i32::try_from(rect.height).unwrap_or(i32::MAX),
                parent,
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null::<c_void>(),
            )
        };
        if hwnd.is_null() {
            // SAFETY: GetLastError has no preconditions.
            let code = unsafe { GetLastError() };
            return Err(PlatformError::NativeDisplay(format!(
                "CreateWindowExW failed with error {code}"
            )));
        }

        let lease_id = Uuid::new_v4();
        self.windows.insert(lease_id, hwnd as usize);
        Ok(PlatformDisplayLease {
            contract: DisplayLeaseV1 {
                lease_id,
                platform: "win32".to_owned(),
                binding: format!("win32-hwnd:{}", hwnd as usize),
                width: rect.width,
                height: rect.height,
            },
            vm_parent_handle: Some(hwnd as usize as u64),
        })
    }

    fn resize(&mut self, lease_id: Uuid, rect: DisplayRect) -> Result<(), PlatformError> {
        let hwnd = self
            .windows
            .get(&lease_id)
            .copied()
            .ok_or(PlatformError::UnknownDisplayLease(lease_id))? as HWND;
        // SAFETY: hwnd is an owned live window until release/drop; dimensions are bounded.
        let ok = unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                rect.x,
                rect.y,
                i32::try_from(rect.width).unwrap_or(i32::MAX),
                i32::try_from(rect.height).unwrap_or(i32::MAX),
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        };
        if ok == 0 {
            // SAFETY: GetLastError has no preconditions.
            let code = unsafe { GetLastError() };
            return Err(PlatformError::NativeDisplay(format!(
                "SetWindowPos failed with error {code}"
            )));
        }
        Ok(())
    }

    fn release(&mut self, lease_id: Uuid) -> Result<(), PlatformError> {
        let hwnd = self
            .windows
            .remove(&lease_id)
            .ok_or(PlatformError::UnknownDisplayLease(lease_id))? as HWND;
        // SAFETY: hwnd was created and owned by this embedder and is removed exactly once.
        if unsafe { DestroyWindow(hwnd) } == 0 {
            // SAFETY: GetLastError has no preconditions.
            let code = unsafe { GetLastError() };
            return Err(PlatformError::NativeDisplay(format!(
                "DestroyWindow failed with error {code}"
            )));
        }
        Ok(())
    }
}

impl Drop for NativeDisplayEmbedder {
    fn drop(&mut self) {
        for (_, hwnd) in std::mem::take(&mut self.windows) {
            // SAFETY: each HWND is owned by this embedder and the map contains no duplicates.
            unsafe {
                DestroyWindow(hwnd as HWND);
            }
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
