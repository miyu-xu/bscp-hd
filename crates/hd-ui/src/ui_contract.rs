use hd_core::{DisplayConfigV2, OrientationV2};
use serde::{Deserialize, Serialize};
use winit::dpi::PhysicalSize;

#[cfg(target_os = "macos")]
const TOP_BAR_LOGICAL: f64 = 0.0;
#[cfg(not(target_os = "macos"))]
const TOP_BAR_LOGICAL: f64 = 48.0;
const SIDEBAR_LOGICAL: f64 = 260.0;
const SURFACE_GAP_PX: u32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Page {
    Player,
    Settings,
    Devices,
    Diagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SurfaceLayout {
    pub width: u32,
    pub height: u32,
    pub top_height: u32,
    pub sidebar_width: u32,
    pub page: Page,
    pub sidebar_collapsed: bool,
    pub display_maximized: bool,
    pub android_focused: bool,
}

impl SurfaceLayout {
    pub fn from_window(
        size: PhysicalSize<u32>,
        scale: f64,
        page: Page,
        collapsed: bool,
        display_maximized: bool,
    ) -> Self {
        let top_height = logical_to_physical(TOP_BAR_LOGICAL, scale).min(size.height);
        let android_focused = page == Page::Player && display_maximized;
        let hide_sidebar = collapsed || android_focused;
        let sidebar_width = if hide_sidebar {
            0
        } else {
            logical_to_physical(SIDEBAR_LOGICAL, scale).min(size.width)
        };
        Self {
            width: size.width,
            height: size.height,
            top_height,
            sidebar_width,
            page,
            sidebar_collapsed: collapsed,
            display_maximized,
            android_focused,
        }
    }

    pub fn sidebar_visible(self) -> bool {
        !self.sidebar_collapsed && self.sidebar_width > 0 && self.body_height() > 0
    }

    pub fn body_y(self) -> u32 {
        self.top_height
            .saturating_add(SURFACE_GAP_PX)
            .min(self.height)
    }

    pub fn content_x(self) -> u32 {
        self.sidebar_width
            .saturating_add(SURFACE_GAP_PX)
            .min(self.width)
    }

    pub fn body_height(self) -> u32 {
        self.height.saturating_sub(self.body_y())
    }

    pub fn content_width(self) -> u32 {
        self.width.saturating_sub(self.content_x())
    }

    /// Android always occupies the full content. The sidebar overlays it and never resizes it.
    pub const fn android_bounds(self) -> (u32, u32, u32, u32) {
        (0, 0, self.width, self.height)
    }
}

pub fn oriented_guest_dimensions(display: &DisplayConfigV2) -> (u32, u32) {
    match display.orientation {
        OrientationV2::Portrait | OrientationV2::ReversePortrait => (
            display.width.min(display.height),
            display.width.max(display.height),
        ),
        OrientationV2::Landscape | OrientationV2::ReverseLandscape => (
            display.width.max(display.height),
            display.width.min(display.height),
        ),
    }
}

fn logical_to_physical(value: f64, scale: f64) -> u32 {
    if value <= 0.0 {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        (value * scale).round().clamp(1.0, f64::from(u32::MAX)) as u32
    }
}
