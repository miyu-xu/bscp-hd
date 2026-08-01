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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AspectFitRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
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

/// Centers content inside an available rectangle without changing its aspect ratio.
///
/// The returned dimensions are integral physical pixels, so one axis may differ from
/// the mathematical result by less than one pixel.
pub fn aspect_fit_rect(
    available_width: u32,
    available_height: u32,
    content_width: u32,
    content_height: u32,
) -> AspectFitRect {
    let available_width = available_width.max(1);
    let available_height = available_height.max(1);
    let content_width = content_width.max(1);
    let content_height = content_height.max(1);
    let available_width_64 = u64::from(available_width);
    let available_height_64 = u64::from(available_height);
    let content_width_64 = u64::from(content_width);
    let content_height_64 = u64::from(content_height);

    let (width, height) =
        if available_width_64 * content_height_64 <= available_height_64 * content_width_64 {
            let height = (available_width_64 * content_height_64 / content_width_64)
                .max(1)
                .min(available_height_64);
            (available_width_64, height)
        } else {
            let width = (available_height_64 * content_width_64 / content_height_64)
                .max(1)
                .min(available_width_64);
            (width, available_height_64)
        };
    let width = u32::try_from(width).unwrap_or(available_width);
    let height = u32::try_from(height).unwrap_or(available_height);
    AspectFitRect {
        x: available_width.saturating_sub(width) / 2,
        y: available_height.saturating_sub(height) / 2,
        width,
        height,
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
