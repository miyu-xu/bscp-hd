use std::{path::PathBuf, time::Duration};

use hd_core::{DisplayConfigV2, GuestKindV2, InstanceRecordV2, ObservedStateV2, OrientationV2};
use serde::{Deserialize, Serialize};
use winit::dpi::PhysicalSize;

#[cfg(target_os = "macos")]
pub const TOP_BAR_LOGICAL: f64 = 0.0;
#[cfg(not(target_os = "macos"))]
pub const TOP_BAR_LOGICAL: f64 = 30.0;
const SIDEBAR_LOGICAL: f64 = 260.0;
const SURFACE_GAP_PX: u32 = 0;
pub const NETWORK_STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
pub const FIRST_PAINT_TIMEOUT: Duration = Duration::from_secs(15);

/// Canonical controls exposed by every HD desktop titlebar. The first action toggles the global
/// sidebar; the remaining actions operate on the selected Android Player. `AppKit` renders this
/// contract on macOS and the dedicated top `WebView` renders it on Windows. Display selection stays
/// in the overlay sidebar; host window controls remain separate from Android actions.
pub const TITLEBAR_ACTION_CONTRACTS: [(&str, &str); 11] = [
    ("sidebar", r#"{"command":"toggle_sidebar"}"#),
    ("power", r#"{"command":"key","key":"power"}"#),
    ("volume_down", r#"{"command":"key","key":"volume_down"}"#),
    ("volume_up", r#"{"command":"key","key":"volume_up"}"#),
    ("rotate", r#"{"command":"rotate"}"#),
    ("screenshot", r#"{"command":"screenshot"}"#),
    ("record", r#"{"command":"start_screen_recording"}"#),
    ("install_apk", r#"{"command":"choose_install_apk"}"#),
    ("recent", r#"{"command":"key","key":"recent"}"#),
    ("home", r#"{"command":"key","key":"home"}"#),
    ("back", r#"{"command":"key","key":"back"}"#),
];

/// Platform-neutral titlebar state. The Windows `WebView` and macOS `AppKit` accessory consume this
/// value so readiness, recording and FPS behavior cannot drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TitlebarPlayerState {
    pub android_selected: bool,
    pub controls_visible: bool,
    pub sidebar_visible: bool,
    pub power_enabled: bool,
    pub actions_enabled: bool,
    pub recording_supported: bool,
    pub recording_active: bool,
    pub recording_enabled: bool,
    pub show_host_fps: bool,
    pub host_fps_milli: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitlebarPresentationState {
    pub command_in_flight: bool,
    pub screen_recording_supported: bool,
    pub player_page: bool,
    pub sidebar_visible: bool,
}

pub fn titlebar_player_state(
    record: Option<&InstanceRecordV2>,
    presentation: TitlebarPresentationState,
) -> TitlebarPlayerState {
    let TitlebarPresentationState {
        command_in_flight,
        screen_recording_supported,
        player_page,
        sidebar_visible,
    } = presentation;
    let android = record.filter(|record| record.spec.guest_kind == GuestKindV2::Android);
    let controls_visible = player_page && android.is_some();
    let ready = android.is_some_and(|record| record.status.observed == ObservedStateV2::Ready);
    let adb_ready = android.is_some_and(|record| record.adb_ready);
    let recording_active = android.is_some_and(|record| record.screen_recording.is_some());
    let show_host_fps =
        controls_visible && android.is_some_and(|record| record.spec.display.show_host_fps);
    // Host cadence is sampled continuously, but an invisible value must not participate in the
    // titlebar equality contract. Otherwise every sample wakes and recomposites the dedicated
    // Windows WebView2 titlebar even when the user has not enabled the FPS indicator.
    let host_fps_milli = if show_host_fps {
        android
            .and_then(|record| record.host_fps_milli)
            .unwrap_or(0)
    } else {
        0
    };
    let recording_supported = android.is_some() && screen_recording_supported;
    TitlebarPlayerState {
        android_selected: android.is_some(),
        controls_visible,
        sidebar_visible,
        power_enabled: ready && !command_in_flight,
        actions_enabled: ready && adb_ready && !command_in_flight,
        recording_supported,
        recording_active,
        recording_enabled: recording_supported
            && !command_in_flight
            && (recording_active || (ready && adb_ready)),
        show_host_fps,
        host_fps_milli,
    }
}

pub fn direct_android_artifact_candidates(
    root: &std::path::Path,
    host_architecture: &str,
) -> Vec<PathBuf> {
    let mut candidates = vec![root.to_owned(), root.join("direct-linux")];
    match host_architecture {
        "x86_64" => candidates.push(root.join("products/android/vsoc_x86_64/direct-linux")),
        "arm64" => {
            candidates.push(root.join("products/android/vsoc_arm64_only/direct-linux"));
            candidates.push(root.join("products/android/vsoc_arm64/direct-linux"));
        }
        _ => {}
    }
    candidates
}

pub fn resolve_direct_android_artifact_root(
    root: &std::path::Path,
    host_architecture: &str,
) -> Option<PathBuf> {
    direct_android_artifact_candidates(root, host_architecture)
        .into_iter()
        .find(|candidate| {
            candidate.join("kernel").is_file()
                && candidate.join("initrd_android.img").is_file()
                && (candidate.join("aggregate_android.sparse.img").is_file()
                    || candidate.join("aggregate_android.img").is_file())
                && candidate.join("android_fstab.dt").is_file()
        })
}

/// Prevents a `WebView` that never publishes its first ready surface from leaving a healthy HD
/// process permanently invisible. Closing takes precedence so an intentional shutdown cannot be
/// converted into a startup failure at the deadline.
pub fn first_paint_timed_out(root_revealed: bool, closing: bool, elapsed: Duration) -> bool {
    !root_revealed && !closing && elapsed >= FIRST_PAINT_TIMEOUT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Page {
    Player,
    Settings,
    Devices,
    Diagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotPollMode {
    LifecycleTransition,
    ForegroundPlayer,
    ForegroundAuxiliary,
    Background,
    Minimized,
}

impl SnapshotPollMode {
    pub const fn interval(self) -> Duration {
        match self {
            Self::LifecycleTransition => Duration::from_millis(250),
            Self::ForegroundPlayer => Duration::from_secs(1),
            Self::ForegroundAuxiliary | Self::Background => Duration::from_secs(2),
            Self::Minimized => Duration::from_secs(5),
        }
    }
}

pub const fn snapshot_poll_mode(
    page: Page,
    window_focused: bool,
    minimized: bool,
    lifecycle_transition: bool,
) -> SnapshotPollMode {
    if lifecycle_transition {
        SnapshotPollMode::LifecycleTransition
    } else if minimized {
        SnapshotPollMode::Minimized
    } else if !window_focused {
        SnapshotPollMode::Background
    } else if matches!(page, Page::Player) {
        SnapshotPollMode::ForegroundPlayer
    } else {
        SnapshotPollMode::ForegroundAuxiliary
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkStatusPollMode {
    ForegroundDevices,
    Suspended,
}

impl NetworkStatusPollMode {
    pub const fn interval(self) -> Option<Duration> {
        match self {
            Self::ForegroundDevices => Some(NETWORK_STATUS_REFRESH_INTERVAL),
            Self::Suspended => None,
        }
    }

    pub fn refresh_due(self, elapsed: Duration) -> bool {
        self.interval().is_some_and(|interval| elapsed >= interval)
    }
}

pub const fn network_status_poll_mode(
    page: Page,
    window_focused: bool,
    minimized: bool,
) -> NetworkStatusPollMode {
    if matches!(page, Page::Devices) && window_focused && !minimized {
        NetworkStatusPollMode::ForegroundDevices
    } else {
        NetworkStatusPollMode::Suspended
    }
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

/// Serializes Host snapshot refreshes and rejects responses that were issued before a local
/// selection or mutation changed the UI's desired state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRefreshGate {
    generation: u64,
    in_flight: Option<u64>,
}

impl SnapshotRefreshGate {
    /// Returns the latest request or invalidation generation for structured diagnostics.
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// Reports whether a request is active without exposing its generation to presentation code.
    pub const fn is_in_flight(self) -> bool {
        self.in_flight.is_some()
    }

    /// Invalidates an outstanding snapshot after a local state transition.
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Begins one refresh. Only one Host request may be active at a time.
    pub fn begin(&mut self) -> Option<u64> {
        if self.in_flight.is_some() {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.in_flight = Some(self.generation);
        self.in_flight
    }

    /// Completes the active refresh and reports whether its response still matches the desired
    /// local generation. Duplicate or stale completions are always rejected.
    pub fn complete(&mut self, generation: u64) -> bool {
        if self.in_flight != Some(generation) {
            return false;
        }
        self.in_flight = None;
        generation == self.generation
    }
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
            // Choose the nearest integral pixel instead of always flooring. Flooring both the
            // root resize and native child resize compounds into a visibly narrow Android image
            // at small portrait sizes (for example 506x901 for a 9:16 Guest).
            let height =
                (available_width_64 * content_height_64 + content_width_64 / 2) / content_width_64;
            let height = height.max(1).min(available_height_64);
            (available_width_64, height)
        } else {
            let width = (available_height_64 * content_width_64 + content_height_64 / 2)
                / content_height_64;
            let width = width.max(1).min(available_width_64);
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

/// Chooses an aspect-correct content size from a remembered, unconstrained window area.
///
/// `maximum_width` and `maximum_height` may temporarily reduce the returned size so an
/// orientation fits the current monitor. Callers must keep `preferred_area` unchanged: using the
/// reduced result as the next preference makes a portrait-to-landscape round trip progressively
/// shrink the emulator window.
pub fn aspect_size_from_preferred_area(
    preferred_area: f64,
    aspect_width: u32,
    aspect_height: u32,
    minimum_width: f64,
    minimum_height: f64,
    maximum_width: f64,
    maximum_height: f64,
) -> (f64, f64) {
    let preferred_area = if preferred_area.is_finite() {
        preferred_area.max(1.0)
    } else {
        1.0
    };
    let ratio = f64::from(aspect_width.max(1)) / f64::from(aspect_height.max(1));
    let mut width = (preferred_area * ratio).sqrt();
    let mut height = width / ratio;
    let shrink = (maximum_width.max(1.0) / width)
        .min(maximum_height.max(1.0) / height)
        .min(1.0);
    width *= shrink;
    height *= shrink;
    let grow = (minimum_width.max(1.0) / width)
        .max(minimum_height.max(1.0) / height)
        .max(1.0);
    (width * grow, height * grow)
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
