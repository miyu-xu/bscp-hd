#![allow(unsafe_code)]

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
#[cfg(any(target_os = "macos", windows))]
use std::ffi::{CStr, c_char, c_void};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::Parser;
use hd_core::{
    AndroidBugreportRecordV2, ArtifactSelectionV2, CapabilityProbeV2, CreateInstanceRequestV2,
    DiagnosticBundleResponseV2, DiagnosticRequestV2, DisplayIdV2, DisplaySessionV2,
    DisplayViewportV2, GuestKindV2, HostCapabilitiesV2, InstanceActionV2, InstanceRecordV2,
    InstanceSpecV2, InstanceSummaryV2, KeyActionV2, NativeDisplayTargetV2, ObservedStateV2,
    OperationKindV2, OrientationV2, StopModeV2, TrackpadEventV2, TrackpadPhaseV2, UiSnapshotV2,
    UpdateInstanceRequestV2,
};
use hd_platform::{
    DataPaths, NativeDisplayBounds, NativeDisplayHost, choose_apk_file, choose_location_route_file,
    create_native_display_host, current_process_identity,
};
#[cfg(target_os = "macos")]
use hd_platform::{
    install_macos_titlebar_controls, set_macos_titlebar_fps, set_macos_titlebar_player_state,
    set_macos_window_content_aspect_ratio,
};
#[cfg(windows)]
use hd_platform::{
    install_windows_root_window_controller, set_windows_window_content_aspect_ratio,
    windows_root_has_foreground_focus,
};
use hd_runtime::{HostClientV2, microdroid_platform_supported};
use hd_ui::trackpad_queue::{QueuedTrackpadEvent, TrackpadQueue, TrackpadQueueError};
#[cfg(windows)]
use hd_ui::ui_contract::TOP_BAR_LOGICAL;
use hd_ui::ui_contract::{
    NETWORK_STATUS_REFRESH_INTERVAL, NetworkStatusPollMode, Page, SnapshotPollMode, SurfaceLayout,
    TitlebarPlayerState, TitlebarPresentationState, aspect_fit_rect,
    aspect_size_from_preferred_area, first_paint_timed_out, network_status_poll_mode,
    oriented_guest_dimensions, snapshot_poll_mode, titlebar_player_state,
};
use raw_window_handle::HasWindowHandle as _;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;
use winit::dpi::LogicalSize;
use winit::event::{Event, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::Window;
use wry::dpi::{PhysicalPosition as WebPosition, PhysicalSize as WebSize};
use wry::http::{Request, Response, header::CONTENT_TYPE};
use wry::{PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder};

const MAX_SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const DISPLAY_RETRY: Duration = Duration::from_millis(500);
const DISPLAY_HEARTBEAT: Duration = Duration::from_secs(5);
const DISPLAY_RELEASE_ON_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_DEV_ARTIFACTS: &str =
    r"C:\workspace\bscp\bscp-vm-artifacts-20260721-aosp-all-targets";
const FALLBACK_DEV_ARTIFACTS: &str =
    r"D:\bscp-vm-artifacts\bscp-vm-artifacts-20260721-aosp-all-targets";
const DEFAULT_ARTIFACT_STORE: &str = r"D:\hd-v2-artifact-store";
const DEFAULT_GUEST_DIGEST: &str =
    "22281c84556c6e865e4f94498968efc2f651ef4c37bd3e871d930414609b2986";
const DEFAULT_HOST_DIGEST: &str =
    "5187de8d05cf29fdbed127c72ab0a96b50fa37f800137079488237208ae1aa7e";
#[cfg(windows)]
const BUNDLED_ADB_FILENAME: &str = "adb.exe";
#[cfg(not(windows))]
const BUNDLED_ADB_FILENAME: &str = "adb";
static DIRECT_DEV_SELECTION: OnceLock<ArtifactSelectionV2> = OnceLock::new();
static BUNDLED_SIGNED_SELECTION: OnceLock<ArtifactSelectionV2> = OnceLock::new();

#[derive(Debug, Parser)]
#[allow(clippy::struct_field_names)]
#[command(name = "hd", version, about = "HD Android WebView desktop shell")]
struct Cli {
    #[arg(long)]
    data_root: Option<PathBuf>,
    #[arg(long)]
    web_root: Option<PathBuf>,
    #[arg(long)]
    dev_artifacts_root: Option<PathBuf>,
}

#[derive(Debug)]
enum UserEvent {
    Ipc(String),
    Refreshed {
        generation: u64,
        result: Result<UiSnapshotV2, String>,
    },
    CapabilitiesRefreshed {
        instance_id: Uuid,
        result: Result<HostCapabilitiesV2, String>,
    },
    ResourceAdmissionRefreshed {
        generation: u64,
        instance_id: Uuid,
        result: Result<CapabilityProbeV2, String>,
    },
    Created(Result<InstanceRecordV2, String>),
    Saved {
        result: Result<InstanceRecordV2, String>,
        restarted: bool,
    },
    PayloadImported(Result<InstanceRecordV2, String>),
    ExtraApkImported(Result<InstanceRecordV2, String>),
    DiagnosticsFinished(Result<DiagnosticBundleResponseV2, String>),
    AndroidBugreportFinished(Result<AndroidBugreportRecordV2, String>),
    #[cfg(target_os = "macos")]
    NetworkSetupRefreshed {
        generation: u64,
        result: Result<NetworkSetupSnapshot, String>,
    },
    #[cfg(target_os = "macos")]
    NetworkSetupInstalled(Result<(), String>),
    CommandFinished {
        instance_id: Uuid,
        result: Result<String, String>,
    },
    InputFinished {
        instance_id: Uuid,
        result: Result<(), String>,
    },
    TrackpadFailed {
        instance_id: Uuid,
        error: String,
    },
    DisplayFinished {
        request_epoch: u64,
        instance_id: Uuid,
        viewport: DisplayViewportV2,
        result: Result<DisplaySessionV2, String>,
    },
    ShutdownFinished(Result<(), String>),
}

struct WebSurfaces {
    // Keep the explicitly rooted WebView2 environment alive for every child surface. Without a
    // shared context Windows falls back to an executable-adjacent `hd.exe.WebView2` profile,
    // leaking cache/GPU state across otherwise isolated product regressions.
    _context: Option<WebContext>,
    top: Option<WebView>,
    sidebar: WebView,
    content: WebView,
    last_layout: Option<SurfaceLayout>,
    sidebar_visible: bool,
    content_visible: bool,
}

impl WebSurfaces {
    fn new(
        window: &Window,
        web_root: &Path,
        webview_data_directory: &Path,
        proxy: &Arc<EventLoopProxy<UserEvent>>,
    ) -> Result<Self> {
        let tiny = web_rect(0, 0, 1, 1);
        // Linux rejects registering the same custom protocol more than once on one context. The
        // Windows product path uses one environment so all three WebView2 surfaces share a single
        // data-root-scoped browser process/profile instead of creating executable-local state.
        let mut environment =
            cfg!(windows).then(|| WebContext::new(Some(webview_data_directory.to_owned())));
        #[cfg(target_os = "macos")]
        let top = None;
        #[cfg(not(target_os = "macos"))]
        let top = Some(build_webview(
            window,
            web_root,
            "top",
            tiny,
            environment.as_mut(),
            proxy,
        )?);
        let sidebar = build_webview(
            window,
            web_root,
            "sidebar",
            tiny,
            environment.as_mut(),
            proxy,
        )?;
        let content = build_webview(
            window,
            web_root,
            "content",
            tiny,
            environment.as_mut(),
            proxy,
        )?;
        Ok(Self {
            _context: environment,
            top,
            sidebar,
            content,
            last_layout: None,
            sidebar_visible: false,
            content_visible: false,
        })
    }

    fn apply_layout(&mut self, layout: SurfaceLayout) -> Result<()> {
        if self.last_layout == Some(layout) {
            return Ok(());
        }
        let previous = self.last_layout;
        let top_geometry_changed = previous.is_none_or(|previous| {
            previous.width != layout.width || previous.top_height != layout.top_height
        });
        if top_geometry_changed && let Some(top) = self.top.as_mut() {
            top.set_bounds(web_rect(
                0,
                0,
                layout.width.max(1),
                layout.top_height.max(1),
            ))?;
            top.set_visible(layout.width > 0 && layout.top_height > 0)?;
        }

        let sidebar_visible =
            !layout.sidebar_collapsed && layout.sidebar_width > 0 && layout.body_height() > 0;
        let sidebar_geometry_changed = previous.is_none_or(|previous| {
            previous.sidebar_width != layout.sidebar_width
                || previous.body_y() != layout.body_y()
                || previous.body_height() != layout.body_height()
        });
        if sidebar_visible {
            if !self.sidebar_visible || sidebar_geometry_changed {
                self.sidebar.set_bounds(web_rect(
                    0,
                    layout.body_y(),
                    layout.sidebar_width,
                    layout.body_height(),
                ))?;
            }
            if !self.sidebar_visible {
                self.sidebar.set_visible(true)?;
            }
        } else if self.sidebar_visible {
            // Hide an overlay before parking it at 1x1. Shrinking a still-visible WebView2 child
            // lets DWM expose an intermediate frame over the unchanged Android swapchain.
            self.sidebar.set_visible(false)?;
            self.sidebar.set_bounds(web_rect(0, 0, 1, 1))?;
        } else if previous.is_none() {
            self.sidebar.set_visible(false)?;
        }
        self.sidebar_visible = sidebar_visible;

        let content_visible =
            layout.page != Page::Player && layout.content_width() > 0 && layout.body_height() > 0;
        let content_geometry_changed = previous.is_none_or(|previous| {
            previous.content_x() != layout.content_x()
                || previous.content_width() != layout.content_width()
                || previous.body_y() != layout.body_y()
                || previous.body_height() != layout.body_height()
        });
        if content_visible {
            if !self.content_visible || content_geometry_changed {
                self.content.set_bounds(web_rect(
                    layout.content_x(),
                    layout.body_y(),
                    layout.content_width(),
                    layout.body_height(),
                ))?;
            }
            if !self.content_visible {
                self.content.set_visible(true)?;
            }
        } else if self.content_visible {
            self.content.set_visible(false)?;
            self.content.set_bounds(web_rect(0, 0, 1, 1))?;
        } else if previous.is_none() {
            self.content.set_visible(false)?;
        }
        self.content_visible = content_visible;
        self.last_layout = Some(layout);
        Ok(())
    }

    fn send(surface: &WebView, message: &Value) {
        let script = format!("window.__hdReceive?.({message});");
        if let Err(error) = surface.evaluate_script(&script) {
            warn!(%error, "send state to HD WebView surface failed");
        }
    }

    fn send_top(&self, message: &Value) {
        if let Some(top) = self.top.as_ref() {
            Self::send(top, message);
        }
    }

    fn send_body(&self, message: &Value) {
        // A hidden WebView2 surface still executes scripts and schedules renderer/compositor
        // work. On the Player page that background work competes with the dedicated titlebar
        // WebView and the sibling Vulkan HWND, producing visible DWM hitches even though neither
        // body surface owns any pixels. Only update surfaces that are part of the committed
        // layout; `apply_window_layout` marks the shell dirty when a surface is shown, so it
        // receives the current full snapshot before the user can interact with it.
        if self.sidebar_visible {
            Self::send(&self.sidebar, message);
        }
        if self.content_visible {
            Self::send(&self.content, message);
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct NetworkSetupSnapshot {
    supported: bool,
    health: String,
    service_action: String,
    network_usable: bool,
    installed: bool,
    package_match: bool,
    loaded: bool,
    pf_configured: bool,
    egress: String,
    vpn_nat_required: bool,
    socket_vmnet: bool,
    nat: String,
    unsafe_paths: bool,
    detail: String,
}

impl NetworkSetupSnapshot {
    fn initial() -> Self {
        Self {
            supported: cfg!(target_os = "macos"),
            health: "checking".to_owned(),
            service_action: "none".to_owned(),
            network_usable: false,
            installed: false,
            package_match: false,
            loaded: false,
            pf_configured: false,
            egress: "unknown".to_owned(),
            vpn_nat_required: false,
            socket_vmnet: false,
            nat: "unknown".to_owned(),
            unsafe_paths: false,
            detail: if cfg!(target_os = "macos") {
                "正在检查 macOS 网络兼容服务…".to_owned()
            } else {
                "网络兼容服务仅适用于 macOS".to_owned()
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IpcLayoutSignature {
    page: Page,
    sidebar_collapsed: bool,
    display_maximized: bool,
    native_suspended: bool,
    display_id: DisplayIdV2,
    display_geometry: Option<(u32, u32, u8)>,
}

struct ShellState {
    runtime: Runtime,
    client: HostClientV2,
    proxy: Arc<EventLoopProxy<UserEvent>>,
    native_display: Box<dyn NativeDisplayHost>,
    display_target: NativeDisplayTargetV2,
    summaries: Vec<InstanceSummaryV2>,
    selected_id: Option<Uuid>,
    selected: Option<InstanceRecordV2>,
    selected_displays: BTreeMap<Uuid, DisplayIdV2>,
    capabilities: Option<HostCapabilitiesV2>,
    capabilities_for: Option<Uuid>,
    resource_capability: Option<CapabilityProbeV2>,
    resource_capability_for: Option<Uuid>,
    host_runtime_current: bool,
    status: String,
    artifact_hint: Option<String>,
    diagnostic_artifact: Option<PathBuf>,
    android_bugreport_artifact: Option<(Uuid, PathBuf)>,
    viewport: Option<DisplayViewportV2>,
    session: Option<DisplaySessionV2>,
    last_session_viewport: Option<DisplayViewportV2>,
    viewport_revision: u64,
    last_refresh: Instant,
    last_display_attempt: Instant,
    last_session_touch: Instant,
    refresh_gate: hd_ui::ui_contract::SnapshotRefreshGate,
    resource_capability_gate: hd_ui::ui_contract::SnapshotRefreshGate,
    capabilities_in_flight: bool,
    command_in_flight: bool,
    trackpad_tx: TrackpadDispatchSender,
    display_in_flight: bool,
    display_request_epoch: u64,
    refresh_display_after_command: bool,
    native_suspended: bool,
    root_was_minimized: bool,
    root_occluded: bool,
    window_focused: bool,
    auto_start_selected: bool,
    closing: bool,
    display_maximized: bool,
    window_expanded: bool,
    sidebar_collapsed: bool,
    window_aspect: Option<(u32, u32)>,
    platform_window_aspect: Option<(u32, u32, u32, u8, u32)>,
    preferred_window_content_area: Option<f64>,
    programmatic_window_inner_size: Option<(u32, u32)>,
    last_native_bounds: Option<NativeDisplayBounds>,
    android_focused: bool,
    sidebar_visible: bool,
    #[cfg(target_os = "macos")]
    last_titlebar_fps: Option<(bool, u32)>,
    #[cfg(target_os = "macos")]
    last_titlebar_player_state: Option<String>,
    last_web_titlebar: Option<TitlebarPlayerState>,
    last_web_top_layout: Option<Value>,
    page: Page,
    apk_paths: BTreeMap<Uuid, PathBuf>,
    location_route_paths: BTreeMap<Uuid, PathBuf>,
    settings_dirty: Option<(Uuid, u64)>,
    include_guest_logs: bool,
    network_setup: NetworkSetupSnapshot,
    network_setup_refresh_in_flight: bool,
    network_setup_generation: u64,
    last_network_setup_refresh: Instant,
    ui_dirty: bool,
    ready_surfaces: BTreeSet<String>,
    first_paint_started: Instant,
    root_revealed: bool,
}

struct PendingShell {
    runtime: Runtime,
    client: HostClientV2,
    runtime_current: bool,
    artifact_hint: Option<String>,
    web_root: PathBuf,
    webview_data_directory: PathBuf,
    proxy: Arc<EventLoopProxy<UserEvent>>,
}

struct RunningShell {
    window: Arc<Window>,
    webviews: WebSurfaces,
    shell: ShellState,
}

#[derive(Debug, Default)]
struct TrackpadDispatchState {
    queue: TrackpadQueue,
    closed: bool,
}

#[derive(Debug, Default)]
struct TrackpadDispatchShared {
    state: Mutex<TrackpadDispatchState>,
    notify: tokio::sync::Notify,
}

#[derive(Debug)]
struct TrackpadDispatchSender {
    shared: Arc<TrackpadDispatchShared>,
}

impl TrackpadDispatchSender {
    fn send(
        &self,
        down_instance: Option<Uuid>,
        event: TrackpadEventV2,
    ) -> Result<(), TrackpadQueueError> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(TrackpadQueueError::Saturated);
        }
        state.queue.enqueue(down_instance, event)?;
        drop(state);
        self.shared.notify.notify_one();
        Ok(())
    }
}

impl Drop for TrackpadDispatchSender {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        drop(state);
        self.shared.notify.notify_one();
    }
}

#[derive(Debug)]
struct TrackpadDispatchReceiver {
    shared: Arc<TrackpadDispatchShared>,
}

impl TrackpadDispatchReceiver {
    async fn recv(&self) -> Option<QueuedTrackpadEvent> {
        loop {
            let notified = self.shared.notify.notified();
            {
                let mut state = self
                    .shared
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(event) = state.queue.pop_front() {
                    return Some(event);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}

fn start_trackpad_queue(
    runtime: &Runtime,
    client: &HostClientV2,
    proxy: &Arc<EventLoopProxy<UserEvent>>,
) -> TrackpadDispatchSender {
    let shared = Arc::new(TrackpadDispatchShared::default());
    let sender = TrackpadDispatchSender {
        shared: Arc::clone(&shared),
    };
    let receiver = TrackpadDispatchReceiver { shared };
    let client = client.clone();
    let proxy = Arc::clone(proxy);
    runtime.spawn(async move {
        let mut last_failure: Option<(Uuid, String)> = None;
        while let Some(QueuedTrackpadEvent { instance_id, event }) = receiver.recv().await {
            match client
                .action(instance_id, InstanceActionV2::Trackpad { event })
                .await
            {
                Ok(_) => last_failure = None,
                Err(error) => {
                    let error = error.to_string();
                    if last_failure.as_ref() != Some(&(instance_id, error.clone())) {
                        let _ = proxy.send_event(UserEvent::TrackpadFailed {
                            instance_id,
                            error: error.clone(),
                        });
                    }
                    last_failure = Some((instance_id, error));
                }
            }
        }
    });
    sender
}

#[allow(deprecated, clippy::too_many_lines)]
pub(crate) fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.data_root.is_none()
        && hd_platform::activate_existing_desktop_application()
            .context("activate the existing HD window")?
    {
        return Ok(());
    }
    let bundled_signed = bundled_signed_android_selection()
        .context("load bundled signed Android artifact selection")?;
    let dev_artifacts = cli.dev_artifacts_root.or_else(|| {
        bundled_signed.is_none().then(|| {
            bundled_development_android_root()
                .or_else(|| {
                    [DEFAULT_DEV_ARTIFACTS, FALLBACK_DEV_ARTIFACTS]
                        .into_iter()
                        .map(PathBuf::from)
                        .find(|path| path.is_dir())
                })
                .unwrap_or_else(|| PathBuf::from(DEFAULT_DEV_ARTIFACTS))
        })
    });
    let artifact_hint = bundled_signed
        .as_ref()
        .map(|(_, hint, _)| hint.clone())
        .or_else(|| {
            dev_artifacts
                .as_deref()
                .and_then(configure_fast_dev_artifacts)
        });
    let paths = cli
        .data_root
        .map_or_else(DataPaths::discover, |root| Ok(DataPaths::from_root(root)))
        .context("discover HD data directory")?;
    paths.ensure().context("create HD data directories")?;
    if let Some((_, _, Some(trust_source))) = bundled_signed.as_ref() {
        install_bundled_development_trust(&paths, trust_source)?;
    }
    if let Some((selection, _, _)) = bundled_signed {
        let _ = BUNDLED_SIGNED_SELECTION.set(selection);
    } else if artifact_hint.is_some() {
        let selection = hd_runtime::prepare_direct_dev_artifact_store(&paths)
            .context("prepare direct Android development artifacts")?;
        let _ = DIRECT_DEV_SELECTION.set(selection);
    }
    let web_root = resolve_web_root(cli.web_root).context("locate HD WebView assets")?;
    let webview_data_directory = paths.cache.join("webview2");
    let _log_guard = init_logging(&paths)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hd-web-ui-runtime")
        .build()
        .context("create HD UI runtime")?;
    let client = runtime
        .block_on(HostClientV2::connect_or_start(paths))
        .context("connect to HD Host")?;
    let runtime_current = client.runtime_current();

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("create HD event loop")?;
    // On macOS, cloning winit's EventLoopProxy creates and registers a new CFRunLoopSource.
    // Asynchronous UI work clones this handle on every poll, so share one proxy allocation and
    // clone only the Arc; otherwise the main run loop accumulates sources and eventually consumes
    // a full CPU core while idle.
    let proxy = Arc::new(event_loop.create_proxy());
    let mut pending = Some(PendingShell {
        runtime,
        client,
        runtime_current,
        artifact_hint,
        web_root,
        webview_data_directory,
        proxy,
    });
    let mut running: Option<RunningShell> = None;
    let startup_error = Rc::new(RefCell::new(None));
    let startup_error_for_loop = Rc::clone(&startup_error);

    event_loop.run(move |event, active| {
        if matches!(&event, Event::NewEvents(StartCause::Init)) {
            active.set_control_flow(ControlFlow::Wait);
        }
        if matches!(&event, Event::Resumed) && running.is_none() {
            let Some(inputs) = pending.take() else {
                return;
            };
            match start_running_shell(active, inputs) {
                Ok(state) => running = Some(state),
                Err(error) => {
                    warn!(
                        event = "ui.lifecycle.startup.failed",
                        %error,
                        "failed to initialize HD after the native event loop resumed"
                    );
                    *startup_error_for_loop.borrow_mut() = Some(error);
                    active.exit();
                    return;
                }
            }
        }
        let Some(RunningShell {
            window,
            webviews,
            shell,
        }) = running.as_mut()
        else {
            return;
        };
        let window = window.as_ref();
        match event {
            Event::WindowEvent { window_id, event } if window_id == window.id() => match event {
                WindowEvent::CloseRequested => shell.begin_close(),
                WindowEvent::DroppedFile(path) => {
                    if path.extension().is_some_and(|extension| {
                        extension.to_string_lossy().eq_ignore_ascii_case("apk")
                    }) {
                        if let Some(id) = shell.selected_id {
                            shell.apk_paths.insert(id, path.clone());
                        }
                        shell.notice(format!("已选择 APK：{}", path.display()));
                    } else if path.extension().is_some_and(|extension| {
                        matches!(
                            extension.to_string_lossy().to_ascii_lowercase().as_str(),
                            "gpx" | "kml"
                        )
                    }) {
                        if let Some(id) = shell.selected_id {
                            shell.location_route_paths.insert(id, path.clone());
                        }
                        shell.notice(format!("已选择定位路线：{}", path.display()));
                    }
                }
                WindowEvent::Resized(_size) => {
                    // A queued SIZE_MINIMIZED event can arrive after the root has already been
                    // restored. Trust the platform's current iconic state instead of the event's
                    // historical 0x0 extent; hiding for that stale extent can otherwise leave the
                    // retained Android parent invisible with no later layout event to reveal it.
                    let minimized = shell.native_display.root_is_minimized();
                    if minimized {
                        if !shell.root_was_minimized {
                            shell.root_was_minimized = true;
                            shell.suspend_display_for_minimize();
                        }
                    } else {
                        shell.root_was_minimized = false;
                        #[cfg(not(target_os = "macos"))]
                        {
                            let expanded = window.is_maximized();
                            if shell.window_expanded != expanded {
                                info!(
                                    event = "ui.window.expansion.changed",
                                    previous = shell.window_expanded,
                                    expanded,
                                    "root window expansion state changed"
                                );
                                shell.window_expanded = expanded;
                                shell.ui_dirty = true;
                            }
                        }
                        apply_window_layout(window, webviews, shell, true);
                    }
                }
                WindowEvent::ScaleFactorChanged { .. } => {
                    apply_window_layout(window, webviews, shell, false);
                }
                WindowEvent::Occluded(occluded) => {
                    if shell.root_occluded != occluded {
                        shell.root_occluded = occluded;
                        if occluded && !shell.sidebar_collapsed {
                            shell.sidebar_collapsed = true;
                            shell.ui_dirty = true;
                            // Commit the overlay HWND before AboutToWait publishes the collapsed
                            // titlebar icon. Leaving only the state bit changed keeps a stale
                            // WebView2 sidebar visible until an unrelated later layout event.
                            apply_window_layout(window, webviews, shell, false);
                        }
                        info!(
                            event = "ui.window.occlusion.changed",
                            occluded, "native root window occlusion state changed"
                        );
                    }
                }
                WindowEvent::Focused(focused) => {
                    let focused = effective_root_window_focus(window, focused);
                    if shell.window_focused != focused {
                        shell.window_focused = focused;
                        if !focused && !shell.sidebar_collapsed {
                            shell.sidebar_collapsed = true;
                            shell.ui_dirty = true;
                            // Focus loss is the product close gesture for the overlay. Hide/park
                            // the sidebar in this native event so WebView2 and the titlebar commit
                            // one coherent state instead of waiting for a later Host refresh.
                            apply_window_layout(window, webviews, shell, false);
                        }
                        info!(
                            event = "ui.window.focus.changed",
                            focused, "native root window focus state changed"
                        );
                    }
                }
                _ => {}
            },
            Event::Suspended => shell.suspend_display_for_native_lifecycle(),
            Event::Resumed => {
                if shell.resume_display_after_native_lifecycle() {
                    apply_window_layout(window, webviews, shell, false);
                }
            }
            Event::UserEvent(UserEvent::Ipc(message)) => {
                let layout_before = shell.ipc_layout_signature();
                handle_ipc(&message, window, shell);
                // Most titlebar commands only dispatch an Android action. Re-running the native
                // layout path for those clicks needlessly crosses the WebView/Win32/gfxstream
                // boundary and can make DWM compose an intermediate black frame. Geometry is
                // committed only when an IPC command actually changed a layout input; native
                // window resize events continue to use the unconditional path above.
                if shell.ipc_layout_signature() != layout_before {
                    apply_window_layout(window, webviews, shell, false);
                }
            }
            Event::UserEvent(event) => {
                match event {
                    UserEvent::Refreshed { generation, result } => {
                        shell.on_refresh(generation, result);
                    }
                    UserEvent::CapabilitiesRefreshed {
                        instance_id,
                        result,
                    } => shell.on_capabilities_refreshed(instance_id, result),
                    UserEvent::ResourceAdmissionRefreshed {
                        generation,
                        instance_id,
                        result,
                    } => shell.on_resource_admission_refreshed(generation, instance_id, result),
                    UserEvent::Created(result) => shell.on_created(result),
                    UserEvent::Saved { result, restarted } => {
                        shell.on_saved(result, restarted);
                    }
                    UserEvent::PayloadImported(result) => shell.on_payload_imported(result),
                    UserEvent::ExtraApkImported(result) => shell.on_extra_apk_imported(result),
                    UserEvent::CommandFinished {
                        instance_id,
                        result,
                    } => shell.on_command_finished(instance_id, result),
                    UserEvent::InputFinished {
                        instance_id,
                        result: Err(error),
                    } => {
                        if shell.selected_id == Some(instance_id) {
                            shell.notice(format!("Android 输入失败：{error}"));
                        }
                    }
                    UserEvent::InputFinished { result: Ok(()), .. } => {}
                    UserEvent::TrackpadFailed { instance_id, error } => {
                        if shell.selected_id == Some(instance_id) {
                            shell.notice(format!("触控板输入失败：{error}"));
                        }
                    }
                    UserEvent::DiagnosticsFinished(result) => {
                        shell.on_diagnostics_finished(result);
                    }
                    UserEvent::AndroidBugreportFinished(result) => {
                        shell.on_android_bugreport_finished(result);
                    }
                    #[cfg(target_os = "macos")]
                    UserEvent::NetworkSetupRefreshed { generation, result } => {
                        shell.on_network_setup_refreshed(generation, result);
                    }
                    #[cfg(target_os = "macos")]
                    UserEvent::NetworkSetupInstalled(result) => {
                        shell.on_network_setup_installed(result);
                    }
                    UserEvent::DisplayFinished {
                        request_epoch,
                        instance_id,
                        viewport,
                        result,
                    } => shell.on_display_finished(request_epoch, instance_id, viewport, result),
                    UserEvent::ShutdownFinished(Ok(())) => active.exit(),
                    UserEvent::ShutdownFinished(Err(error)) => {
                        shell.closing = false;
                        shell.command_in_flight = false;
                        shell.notice(format!("关闭 HD 失败：{error}"));
                    }
                    UserEvent::Ipc(_) => unreachable!(),
                }
                apply_window_layout(window, webviews, shell, false);
            }
            Event::AboutToWait => {
                if first_paint_timed_out(
                    shell.root_revealed,
                    shell.closing,
                    shell.first_paint_started.elapsed(),
                ) {
                    let ready_surfaces = shell
                        .ready_surfaces
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(",");
                    let error = anyhow::anyhow!(
                        "HD 首屏初始化超时；WebView 未发布完整 ready surface（已就绪：{}）",
                        if ready_surfaces.is_empty() {
                            "无"
                        } else {
                            ready_surfaces.as_str()
                        }
                    );
                    warn!(
                        event = "ui.lifecycle.first_paint.timed_out",
                        %ready_surfaces,
                        "root window remained hidden past the first-paint deadline"
                    );
                    *startup_error_for_loop.borrow_mut() = Some(error);
                    active.exit();
                    return;
                }
                shell.poll();
                shell.sync_titlebar_fps(window);
                shell.sync_titlebar_player_state(window);
                shell.flush_web_state(webviews);
                active.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + shell.next_wakeup_interval(),
                ));
            }
            _ => {}
        }
    })?;
    let startup_error = startup_error.borrow_mut().take();
    if let Some(error) = startup_error {
        return Err(error);
    }
    Ok(())
}

fn create_root_window(
    active: &ActiveEventLoop,
    _proxy: &Arc<EventLoopProxy<UserEvent>>,
) -> Result<Arc<Window>> {
    let window_attributes = Window::default_attributes()
        .with_title("HD Android")
        .with_visible(false)
        .with_inner_size(LogicalSize::new(1180.0, 820.0))
        .with_min_inner_size(LogicalSize::new(720.0, 520.0));
    #[cfg(target_os = "macos")]
    let window_attributes = window_attributes.with_decorations(true);
    #[cfg(not(target_os = "macos"))]
    let window_attributes = window_attributes.with_decorations(false);
    let window = Arc::new(
        active
            .create_window(window_attributes)
            .context("create HD root window")?,
    );
    Ok(window)
}

#[allow(clippy::too_many_lines)]
fn start_running_shell(active: &ActiveEventLoop, inputs: PendingShell) -> Result<RunningShell> {
    let PendingShell {
        runtime,
        client,
        runtime_current,
        artifact_hint,
        web_root,
        webview_data_directory,
        proxy,
    } = inputs;
    let window = create_root_window(active, &proxy)?;
    let raw_parent = window
        .window_handle()
        .context("read HD root window handle")?
        .as_raw();
    let native_display =
        create_native_display_host(raw_parent).context("create HD NativeDisplayHost")?;
    // Install platform window integration after the native Android host exists. Windows only
    // installs its root lifecycle/aspect controller; the visible titlebar is the dedicated top
    // WebView created below. macOS keeps its AppKit titlebar controls.
    install_platform_window_controls(window.as_ref(), &proxy)
        .context("install HD platform window controls")?;
    let identity = current_process_identity(Uuid::new_v4())
        .map_err(|error| anyhow::anyhow!("identify HD UI process: {error}"))?;
    let display_target = native_display.target(identity, 0);
    let mut webviews =
        WebSurfaces::new(window.as_ref(), &web_root, &webview_data_directory, &proxy)?;
    let now = Instant::now();
    let trackpad_tx = start_trackpad_queue(&runtime, &client, &proxy);
    let mut shell = ShellState {
        runtime,
        client,
        proxy,
        native_display,
        display_target,
        summaries: Vec::new(),
        selected_id: None,
        selected: None,
        selected_displays: BTreeMap::new(),
        capabilities: None,
        capabilities_for: None,
        resource_capability: None,
        resource_capability_for: None,
        host_runtime_current: runtime_current,
        status: if runtime_current {
            "正在连接 HD Host…".to_owned()
        } else {
            "当前实例仍由旧版 HD 运行；停止所有实例并重新打开 HD 以完成运行时升级".to_owned()
        },
        artifact_hint,
        diagnostic_artifact: None,
        android_bugreport_artifact: None,
        viewport: None,
        session: None,
        last_session_viewport: None,
        viewport_revision: 0,
        last_refresh: now.checked_sub(MAX_SNAPSHOT_POLL_INTERVAL).unwrap_or(now),
        last_display_attempt: now.checked_sub(DISPLAY_RETRY).unwrap_or(now),
        last_session_touch: now,
        refresh_gate: hd_ui::ui_contract::SnapshotRefreshGate::default(),
        resource_capability_gate: hd_ui::ui_contract::SnapshotRefreshGate::default(),
        capabilities_in_flight: false,
        command_in_flight: false,
        trackpad_tx,
        display_in_flight: false,
        display_request_epoch: 0,
        refresh_display_after_command: false,
        native_suspended: false,
        root_was_minimized: false,
        root_occluded: false,
        window_focused: false,
        auto_start_selected: true,
        closing: false,
        display_maximized: false,
        window_expanded: false,
        sidebar_collapsed: true,
        window_aspect: None,
        platform_window_aspect: None,
        preferred_window_content_area: None,
        programmatic_window_inner_size: None,
        last_native_bounds: None,
        android_focused: false,
        sidebar_visible: false,
        #[cfg(target_os = "macos")]
        last_titlebar_fps: None,
        #[cfg(target_os = "macos")]
        last_titlebar_player_state: None,
        last_web_titlebar: None,
        last_web_top_layout: None,
        page: Page::Player,
        apk_paths: BTreeMap::new(),
        location_route_paths: BTreeMap::new(),
        settings_dirty: None,
        include_guest_logs: true,
        network_setup: NetworkSetupSnapshot::initial(),
        network_setup_refresh_in_flight: false,
        network_setup_generation: 0,
        last_network_setup_refresh: now
            .checked_sub(NETWORK_STATUS_REFRESH_INTERVAL)
            .unwrap_or(now),
        ui_dirty: true,
        ready_surfaces: if cfg!(target_os = "macos") {
            BTreeSet::from(["top".to_owned()])
        } else {
            BTreeSet::new()
        },
        first_paint_started: now,
        root_revealed: false,
    };
    shell.request_refresh();
    apply_window_layout(window.as_ref(), &mut webviews, &mut shell, false);
    Ok(RunningShell {
        window,
        webviews,
        shell,
    })
}

fn build_webview(
    window: &Window,
    web_root: &Path,
    surface: &str,
    bounds: Rect,
    web_context: Option<&mut WebContext>,
    proxy: &Arc<EventLoopProxy<UserEvent>>,
) -> Result<WebView> {
    let url = format!("hd-ui://localhost/index.html?surface={surface}");
    let proxy = Arc::clone(proxy);
    let assets = web_root.to_owned();
    let protocol_surface = surface.to_owned();
    let page_surface = surface.to_owned();
    let builder = match web_context {
        Some(context) => WebViewBuilder::new_with_web_context(context),
        None => WebViewBuilder::new(),
    };
    builder
        .with_custom_protocol("hd-ui".to_owned(), move |_id, request| {
            let response = web_asset_response(&assets, &request);
            info!(
                event = "ui.webview.asset.response",
                surface = %protocol_surface,
                uri = %request.uri(),
                status = response.status().as_u16(),
                "served a packaged WebView asset"
            );
            response
        })
        .with_url(url)
        .with_on_page_load_handler(move |phase, url| {
            let phase = match phase {
                PageLoadEvent::Started => "started",
                PageLoadEvent::Finished => "finished",
            };
            info!(
                event = "ui.webview.page_load",
                surface = %page_surface,
                phase,
                %url,
                "WebView navigation state changed"
            );
        })
        .with_bounds(bounds)
        .with_focused(false)
        .with_ipc_handler(move |request| {
            let _ = proxy.send_event(UserEvent::Ipc(request.body().clone()));
        })
        .build_as_child(window)
        .with_context(|| format!("create {surface} HD WebView surface"))
}

fn web_asset_response(root: &Path, request: &Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let relative = request.uri().path().trim_start_matches('/');
    let relative = if relative.is_empty() {
        Path::new("index.html")
    } else {
        Path::new(relative)
    };
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return web_error_response(403, "invalid asset path");
    }
    let path = root.join(relative);
    match std::fs::read(&path) {
        Ok(bytes) => Response::builder()
            .status(200)
            .header(CONTENT_TYPE, asset_mime(&path))
            .header("Cache-Control", "no-store")
            .body(Cow::Owned(bytes))
            .unwrap_or_else(|error| web_error_response(500, &error.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            web_error_response(404, "asset not found")
        }
        Err(error) => web_error_response(500, &error.to_string()),
    }
}

fn web_error_response(status: u16, message: &str) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Cow::Owned(message.as_bytes().to_vec()))
        .unwrap_or_else(|_| Response::new(Cow::Owned(b"WebView asset error".to_vec())))
}

fn asset_mime(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn apply_window_layout(
    window: &Window,
    webviews: &mut WebSurfaces,
    shell: &mut ShellState,
    window_resized: bool,
) {
    shell.sync_window_aspect(window, window_resized);
    let size = window.inner_size();
    let layout = SurfaceLayout::from_window(
        size,
        window.scale_factor(),
        shell.page,
        shell.sidebar_collapsed,
        shell.display_maximized,
    );
    let layout_state_changed = shell.android_focused != layout.android_focused
        || shell.sidebar_visible != layout.sidebar_visible();
    shell.android_focused = layout.android_focused;
    shell.sidebar_visible = layout.sidebar_visible();
    if layout_state_changed {
        shell.ui_dirty = true;
    }
    if layout.page == Page::Player {
        // Entering Player must expose the retained native frame before the body WebView is hidden.
        // Otherwise DWM can compose one root-background-only frame between the two independent
        // child-window transactions. Player resize and sidebar overlay changes use the same order:
        // the Android geometry is authoritative and the WebViews reveal it only after it commits.
        shell.update_display_layout(layout, window.scale_factor());
        if let Err(error) = webviews.apply_layout(layout) {
            shell.notice(format!("WebView 布局失败：{error}"));
        }
    } else {
        // When leaving Player, cover the Android child with the destination WebView before hiding
        // the native viewport so the transition cannot expose the black letterbox background.
        if let Err(error) = webviews.apply_layout(layout) {
            shell.notice(format!("WebView 布局失败：{error}"));
        }
        shell.update_display_layout(layout, window.scale_factor());
    }
}

#[cfg(target_os = "macos")]
fn install_platform_window_controls(
    window: &Window,
    proxy: &Arc<EventLoopProxy<UserEvent>>,
) -> Result<()> {
    let handle = window
        .window_handle()
        .context("read macOS window handle for native titlebar controls")?
        .as_raw();
    let context = Box::into_raw(Box::new(Arc::clone(proxy))).cast::<c_void>();
    install_macos_titlebar_controls(handle, context, native_window_callback)
        .context("install macOS native titlebar controls")
}

#[cfg(windows)]
fn install_platform_window_controls(
    window: &Window,
    proxy: &Arc<EventLoopProxy<UserEvent>>,
) -> Result<()> {
    let handle = window
        .window_handle()
        .context("read Windows root window handle")?
        .as_raw();
    let context = Box::into_raw(Box::new(Arc::clone(proxy))).cast::<c_void>();
    install_windows_root_window_controller(handle, context, native_window_callback)
        .context("install Windows root lifecycle/aspect controller")
}

#[cfg(any(target_os = "macos", windows))]
extern "C" fn native_window_callback(context: *mut c_void, message: *const c_char) {
    if context.is_null() || message.is_null() {
        return;
    }
    // SAFETY: context is a leaked Box<Arc<EventLoopProxy<UserEvent>>> for the UI process lifetime,
    // and the platform titlebar supplies a NUL-terminated JSON command from its native controls.
    let (proxy, message) = unsafe {
        (
            &*context.cast::<Arc<EventLoopProxy<UserEvent>>>(),
            CStr::from_ptr(message).to_string_lossy().into_owned(),
        )
    };
    info!(
        event = "ui.native_window.callback",
        command = %message,
        "native window command received"
    );
    if proxy.send_event(UserEvent::Ipc(message)).is_err() {
        warn!(
            event = "ui.native_window.callback.rejected",
            "native window command could not enter the UI event loop"
        );
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
#[allow(clippy::unnecessary_wraps)]
fn install_platform_window_controls(
    _window: &Window,
    _proxy: &Arc<EventLoopProxy<UserEvent>>,
) -> Result<()> {
    Ok(())
}

fn web_rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        position: WebPosition::new(
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        )
        .into(),
        size: WebSize::new(width.max(1), height.max(1)).into(),
    }
}

fn effective_root_window_focus(window: &Window, focused: bool) -> bool {
    if focused {
        return true;
    }
    #[cfg(windows)]
    {
        let Ok(handle) = window.window_handle() else {
            return false;
        };
        // WebView2 is a child HWND. Moving keyboard focus from the root/native Android child to a
        // titlebar button can emit WM_KILLFOCUS for winit even though HD remains the foreground
        // application. Treat only a real foreground-window change as product focus loss; otherwise
        // one titlebar press can collapse the sidebar and enqueue an unrelated display layout.
        return windows_root_has_foreground_focus(handle.as_raw()).unwrap_or(false);
    }
    #[cfg(not(windows))]
    focused
}

#[allow(clippy::too_many_lines)]
fn handle_ipc(message: &str, window: &Window, shell: &mut ShellState) {
    let value: Value = match serde_json::from_str(message) {
        Ok(value) => value,
        Err(error) => {
            shell.notice(format!("WebView 命令格式无效：{error}"));
            return;
        }
    };
    let Some(command) = value.get("command").and_then(Value::as_str) else {
        shell.notice("WebView 命令缺少 command".to_owned());
        return;
    };
    match command {
        "ready" => {
            if let Some(surface) = value
                .get("surface")
                .and_then(Value::as_str)
                .filter(|surface| matches!(*surface, "top" | "sidebar" | "content"))
            {
                if surface == "top" {
                    // WebView2 may recover its renderer without recreating the native shell.
                    // Force the deduplicated state back into a freshly loaded top document.
                    shell.last_web_titlebar = None;
                    shell.last_web_top_layout = None;
                }
                shell.ready_surfaces.insert(surface.to_owned());
            }
            shell.ui_dirty = true;
            if !shell.root_revealed
                && shell.ready_surfaces.contains("top")
                && (shell.sidebar_collapsed || shell.ready_surfaces.contains("sidebar"))
                && (shell.page == Page::Player || shell.ready_surfaces.contains("content"))
            {
                // Keep the root hidden until both surfaces visible on the initial Player page have
                // painted their first document. The content WebView is parked and hidden on this
                // page; WebView2 is allowed to defer its navigation while hidden, so it must not
                // block the shell from appearing.
                window.set_visible(true);
                window.request_redraw();
                if window.is_visible() == Some(false) {
                    warn!(
                        event = "ui.lifecycle.first_paint.visibility_rejected",
                        "native window remained hidden after its first-paint visibility request"
                    );
                } else {
                    shell.root_revealed = true;
                    info!(
                        event = "ui.lifecycle.first_paint.ready",
                        elapsed_ms = u64::try_from(shell.first_paint_started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        "first WebView paint made the native root window visible"
                    );
                }
            }
        }
        "toggle_sidebar" => {
            if shell.display_maximized {
                shell.display_maximized = false;
                shell.sidebar_collapsed = false;
            } else {
                shell.sidebar_collapsed = !shell.sidebar_collapsed;
            }
            shell.ui_dirty = true;
        }
        "close_sidebar" => {
            if !shell.sidebar_collapsed {
                shell.sidebar_collapsed = true;
                shell.ui_dirty = true;
            }
        }
        "resource_admission_refresh" | "capabilities_refresh" => {
            shell.resource_capability_gate.invalidate();
            shell.resource_capability = None;
            shell.resource_capability_for = None;
            shell.request_resource_admission();
        }
        "device_capabilities_refresh" => {
            shell.capabilities = None;
            shell.capabilities_for = None;
            shell.request_capabilities();
        }
        "page" => {
            let page = value
                .get("page")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            if let Some(page) = page {
                shell.navigate(page);
            }
        }
        "select" => {
            if let Some(id) = value
                .get("instance_id")
                .and_then(Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok())
            {
                shell.select(id);
            }
        }
        "create" => {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or("Android")
                .to_owned();
            let guest_kind = value
                .get("guest_kind")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or(GuestKindV2::Android);
            shell.create(name, guest_kind);
        }
        "settings_dirty" => {
            let instance_id = value
                .get("instance_id")
                .and_then(Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok());
            let revision = value.get("revision").and_then(Value::as_u64);
            let dirty = value.get("dirty").and_then(Value::as_bool).unwrap_or(false);
            if dirty {
                shell.settings_dirty = instance_id.zip(revision);
            } else if shell
                .settings_dirty
                .is_some_and(|state| Some(state.0) == instance_id)
            {
                shell.settings_dirty = None;
            }
        }
        "save_spec" => match value
            .get("spec")
            .cloned()
            .ok_or_else(|| "设置命令缺少 spec".to_owned())
            .and_then(|spec| serde_json::from_value(spec).map_err(|error| error.to_string()))
        {
            Ok(spec) => shell.save_spec(
                spec,
                value
                    .get("restart")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                value.get("expected_revision").and_then(Value::as_u64),
            ),
            Err(error) => shell.notice(format!("设置格式无效：{error}")),
        },
        "operation" => {
            if let Some(kind) = value.get("kind").and_then(Value::as_str) {
                let target = value
                    .get("instance_id")
                    .and_then(Value::as_str)
                    .and_then(|id| Uuid::parse_str(id).ok());
                if value.get("instance_id").is_some() && target.is_none() {
                    info!(
                        event = "ui.instance_operation.target_rejected",
                        reason = "invalid_instance_id",
                        "rejected an instance operation with an invalid target"
                    );
                    shell.notice("实例操作包含无效的目标标识，本次操作未执行".to_owned());
                } else if target.is_some_and(|target| {
                    shell.selected.as_ref().map(|record| record.spec.id) != Some(target)
                }) {
                    info!(
                        event = "ui.instance_operation.target_rejected",
                        requested_instance_id = ?target,
                        selected_instance_id = ?shell.selected_id,
                        reason = "target_not_hydrated",
                        "rejected an operation before the target instance snapshot was hydrated"
                    );
                    shell.notice("目标实例仍在同步；为避免控制错误实例，本次操作未执行".to_owned());
                } else {
                    shell.operation(kind, Some(&value));
                }
            }
        }
        "key" => {
            if let Some(key) = value.get("key").and_then(Value::as_str) {
                shell.key(key);
            }
        }
        "trackpad" => {
            let phase = match value.get("phase").and_then(Value::as_str) {
                Some("down") => Some(TrackpadPhaseV2::Down),
                Some("move") => Some(TrackpadPhaseV2::Move),
                Some("up") => Some(TrackpadPhaseV2::Up),
                _ => None,
            };
            let x = value
                .get("x")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok());
            let y = value
                .get("y")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok());
            match (phase, x, y) {
                (Some(phase), Some(x), Some(y)) => {
                    shell.trackpad(TrackpadEventV2 { phase, x, y });
                }
                _ => shell.notice("触控板事件格式无效".to_owned()),
            }
        }
        "rotate" => shell.rotate(),
        "select_display" => {
            let display_id = match value.get("display_id").and_then(Value::as_str) {
                Some("primary") => Ok(DisplayIdV2::Primary),
                Some(id) => Uuid::parse_str(id)
                    .map(|id| DisplayIdV2::Secondary { id })
                    .map_err(|_| "显示选择包含无效标识".to_owned()),
                None => Err("显示选择缺少 display_id".to_owned()),
            };
            match display_id {
                Ok(display_id) => shell.select_display(display_id),
                Err(error) => shell.notice(error),
            }
        }
        "screenshot" => shell.screenshot(),
        "start_screen_recording" => shell.start_screen_recording(),
        "stop_screen_recording" => shell.stop_screen_recording(),
        "install_apk" => {
            if let Some(path) = value
                .get("path")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                && let Some(id) = shell.selected_id
            {
                shell.apk_paths.insert(id, PathBuf::from(path));
            }
            shell.install_selected_apk();
        }
        "choose_apk" => shell.choose_apk(false),
        "choose_install_apk" => shell.choose_apk(true),
        "choose_location_route" => shell.choose_location_route(),
        "set_location_route_path" => {
            let path = value
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from);
            if let Some(id) = shell.selected_id {
                if let Some(path) = path {
                    shell.location_route_paths.insert(id, path);
                } else {
                    shell.location_route_paths.remove(&id);
                }
            }
            shell.ui_dirty = true;
        }
        "start_location_route" => {
            let interval_ms = value
                .get("interval_ms")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            let repeat = value
                .get("repeat")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(interval_ms) = interval_ms {
                shell.start_location_route(interval_ms, repeat);
            } else {
                shell.notice("路线回放间隔无效".to_owned());
            }
        }
        "choose_microdroid_payload" => shell.choose_microdroid_payload(),
        "choose_microdroid_extra_apk" => shell.choose_microdroid_extra_apk(),
        "microdroid_shell" => shell.open_microdroid_shell(),
        "set_apk_path" => {
            let path = value
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from);
            if let Some(id) = shell.selected_id {
                if let Some(path) = path {
                    shell.apk_paths.insert(id, path);
                } else {
                    shell.apk_paths.remove(&id);
                }
            }
            shell.ui_dirty = true;
        }
        "focus_display" => {
            if let Err(error) = shell.native_display.focus_guest() {
                warn!(
                    event = "ui.display.focus.failed",
                    %error,
                    "restore native Android display focus failed"
                );
            }
        }
        "window_expansion" => {
            if let Some(expanded) = value.get("expanded").and_then(Value::as_bool)
                && shell.window_expanded != expanded
            {
                info!(
                    event = "ui.window.expansion.changed",
                    previous = shell.window_expanded,
                    expanded,
                    "root window expansion state changed"
                );
                shell.window_expanded = expanded;
                shell.ui_dirty = true;
            }
        }
        "native_lifecycle" => match value.get("state").and_then(Value::as_str) {
            Some("suspended") => shell.suspend_display_for_native_lifecycle(),
            Some("resumed") => {
                let _ = shell.resume_display_after_native_lifecycle();
            }
            Some(state) => shell.notice(format!("未知原生生命周期状态：{state}")),
            None => shell.notice("原生生命周期命令缺少 state".to_owned()),
        },
        "diagnostics" => {
            shell.include_guest_logs = value
                .get("include_guest_logs")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            shell.collect_diagnostics();
        }
        "reveal_diagnostics" => shell.reveal_diagnostic_artifact(),
        "android_bugreport" => shell.collect_android_bugreport(),
        "reveal_android_bugreport" => shell.reveal_android_bugreport_artifact(),
        "network_setup_refresh" => shell.request_network_setup_status(true),
        "network_setup_install" => shell.install_network_setup(),
        "action" => match value
            .get("action")
            .cloned()
            .ok_or_else(|| "设备命令缺少 action".to_owned())
            .and_then(|action| serde_json::from_value(action).map_err(|error| error.to_string()))
        {
            Ok(action) => shell.action(action, "设备命令已发送"),
            Err(error) => shell.notice(format!("设备命令格式无效：{error}")),
        },
        "window" => match value.get("action").and_then(Value::as_str) {
            Some("drag") => {
                if let Err(error) = window.drag_window() {
                    shell.notice(format!("移动窗口失败：{error}"));
                }
            }
            Some("minimize") => window.set_minimized(true),
            Some("maximize") => {
                let expanded = !window.is_maximized();
                window.set_maximized(expanded);
                #[cfg(not(target_os = "macos"))]
                {
                    shell.window_expanded = expanded;
                }
                shell.ui_dirty = true;
            }
            Some("maximize_display" | "fullscreen") => {
                if shell.page == Page::Settings
                    && shell
                        .settings_dirty
                        .is_some_and(|(instance_id, _)| shell.selected_id == Some(instance_id))
                {
                    shell.notice("当前实例设置尚未保存；请先保存或放弃更改，再进入全屏".to_owned());
                    return;
                }
                shell.display_maximized = !shell.display_maximized;
                shell.page = Page::Player;
                shell.ui_dirty = true;
            }
            Some("close") => shell.begin_close(),
            Some(action) => shell.notice(format!("未知窗口操作：{action}")),
            None => shell.notice("窗口命令缺少 action".to_owned()),
        },
        other => shell.notice(format!("未知 WebView 命令：{other}")),
    }
}

impl ShellState {
    fn ipc_layout_signature(&self) -> IpcLayoutSignature {
        IpcLayoutSignature {
            page: self.page,
            sidebar_collapsed: self.sidebar_collapsed,
            display_maximized: self.display_maximized,
            // `window_expanded` is presentation state, not a SurfaceLayout input. The authoritative
            // WM_SIZE/Resized event applies maximized geometry; including the optimistic titlebar
            // state here would first submit a redundant layout at the old window extent.
            native_suspended: self.native_suspended,
            // Two scanouts can expose identical geometry. The ID is still a layout input because
            // select_display clears the old viewport/session and the new scanout must receive a
            // freshly committed native target before it can attach.
            display_id: self.selected_display_id(),
            display_geometry: self.active_display_geometry(),
        }
    }

    fn selected_display_id(&self) -> DisplayIdV2 {
        self.selected_id
            .and_then(|id| self.selected_displays.get(&id).copied())
            .unwrap_or_default()
    }

    fn active_display_geometry(&self) -> Option<(u32, u32, u8)> {
        let record = self.selected.as_ref()?;
        match self.selected_display_id() {
            DisplayIdV2::Primary => {
                let (width, height) = oriented_guest_dimensions(&record.spec.display);
                Some((
                    width,
                    height,
                    record.spec.display.orientation.android_rotation(),
                ))
            }
            display_id @ DisplayIdV2::Secondary { .. } => record
                .runtime_displays
                .iter()
                .find(|display| display.display_id == display_id)
                .map(|display| (display.width, display.height, 0)),
        }
    }

    fn reset_display_attachment_for_scanout(&mut self, scanout_id: u32) {
        self.release_display();
        let owner = self.display_target.owner().clone();
        self.display_target = self.native_display.target(owner, scanout_id);
        self.viewport = None;
        self.last_session_viewport = None;
        self.last_native_bounds = None;
        self.window_aspect = None;
        self.programmatic_window_inner_size = None;
        self.last_display_attempt = Instant::now()
            .checked_sub(DISPLAY_RETRY)
            .unwrap_or_else(Instant::now);
    }

    fn reconcile_selected_display_target(&mut self) {
        let Some(record) = self
            .selected
            .as_ref()
            .filter(|record| record.spec.guest_kind == GuestKindV2::Android)
        else {
            return;
        };
        let instance_id = record.spec.id;
        let requested = self
            .selected_displays
            .get(&instance_id)
            .copied()
            .unwrap_or_default();
        // A stopped or starting instance may not have published its runtime display mapping yet.
        // Preserve the per-instance selection until a real mapping is available instead of
        // silently replacing a secondary display with the primary one during a transient refresh.
        if record.runtime_displays.is_empty() {
            return;
        }
        let resolved = record
            .runtime_displays
            .iter()
            .find(|display| display.display_id == requested)
            .or_else(|| {
                record
                    .runtime_displays
                    .iter()
                    .find(|display| display.display_id == DisplayIdV2::Primary)
            });
        let Some(resolved) = resolved else {
            return;
        };
        let resolved_id = resolved.display_id;
        let scanout_id = resolved.scanout_id;
        self.selected_displays.insert(instance_id, resolved_id);
        if self.display_target.scanout_id() != scanout_id {
            self.reset_display_attachment_for_scanout(scanout_id);
            self.ui_dirty = true;
        }
    }

    fn select_display(&mut self, display_id: DisplayIdV2) {
        let Some(instance_id) = self.selected_id else {
            self.notice("请先选择 Android 实例".to_owned());
            return;
        };
        let Some(runtime_display) = self
            .selected
            .as_ref()
            .and_then(|record| (record.spec.guest_kind == GuestKindV2::Android).then_some(record))
            .and_then(|record| {
                record
                    .runtime_displays
                    .iter()
                    .find(|display| display.display_id == display_id)
            })
            .cloned()
        else {
            self.notice("所选显示器不属于当前 Android 运行".to_owned());
            return;
        };
        if self.selected_display_id() == display_id {
            return;
        }
        self.selected_displays.insert(instance_id, display_id);
        self.reset_display_attachment_for_scanout(runtime_display.scanout_id);
        self.notice(format!("正在切换到{}…", runtime_display.name));
        self.ui_dirty = true;
    }

    fn navigate(&mut self, page: Page) {
        if self.page == page {
            self.sidebar_collapsed = true;
            self.ui_dirty = true;
            if page == Page::Devices {
                self.request_capabilities();
                self.request_network_setup_status(false);
            }
            return;
        }
        if self.page == Page::Settings
            && self
                .settings_dirty
                .is_some_and(|(instance_id, _)| self.selected_id == Some(instance_id))
        {
            self.notice("当前实例设置尚未保存；请先保存或放弃更改，再离开设置页".to_owned());
            return;
        }
        // A page change only changes HD's local composition. Keep the Player-owned display
        // session and Worker children alive so returning to Player can reveal the retained frame
        // synchronously instead of reacquiring gfxstream and presenting a black transition.
        if page != Page::Player {
            self.display_maximized = false;
        }
        if self.page == Page::Devices {
            self.network_setup_generation = self.network_setup_generation.wrapping_add(1);
        }
        self.page = page;
        self.sidebar_collapsed = true;
        self.ui_dirty = true;
        if page == Page::Devices {
            self.request_capabilities();
            self.request_network_setup_status(false);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn sync_window_aspect(&mut self, window: &Window, window_resized: bool) {
        if self
            .selected
            .as_ref()
            .is_some_and(|record| record.spec.guest_kind == GuestKindV2::Microdroid)
        {
            self.window_aspect = None;
            self.programmatic_window_inner_size = None;
            self.sync_platform_window_aspect(window, None);
            return;
        }
        let Some((guest_width, guest_height, rotation_quarters)) = self.active_display_geometry()
        else {
            self.window_aspect = None;
            self.programmatic_window_inner_size = None;
            self.sync_platform_window_aspect(window, None);
            return;
        };
        let aspect = (guest_width, guest_height);
        // The native WM_SIZING/WM_SIZE controller must track rotation even while the root is
        // maximized. Keep `window_aspect` unchanged in that state so restoring the window still
        // performs the orientation-aware size transition using the preserved preferred area.
        self.sync_platform_window_aspect(
            window,
            Some((guest_width, guest_height, rotation_quarters)),
        );
        // Query the native window directly: a rotate IPC can arrive after ShowWindow(SW_MAXIMIZE)
        // but before winit's queued Resized event updates the presentation-only cached flag.
        if window.is_maximized() {
            return;
        }
        let aspect_changed = self.window_aspect != Some(aspect);
        #[cfg(windows)]
        let titlebar_height = TOP_BAR_LOGICAL;
        #[cfg(not(windows))]
        let titlebar_height = 0.0;
        let scale = window.scale_factor().max(1.0);
        let current = window.inner_size();
        let current_width = f64::from(current.width) / scale;
        let current_height = f64::from(current.height) / scale;
        let current_content_height = (current_height - titlebar_height).max(1.0);
        let current_area = (current_width * current_content_height).max(1.0);
        if self.preferred_window_content_area.is_none() {
            self.preferred_window_content_area = Some(current_area);
        }
        if window_resized && !aspect_changed {
            let matches_programmatic_size = self.programmatic_window_inner_size.is_some_and(
                |(expected_width, expected_height)| {
                    current.width.abs_diff(expected_width) <= 2
                        && current.height.abs_diff(expected_height) <= 2
                },
            );
            if !matches_programmatic_size {
                // Only a real root resize changes the user's preferred emulator scale. A rotate
                // may temporarily shrink one orientation to fit the monitor, but that fitted area
                // must not become the next orientation's starting size.
                self.preferred_window_content_area = Some(current_area);
                self.programmatic_window_inner_size = None;
            }
        }
        if !aspect_changed {
            return;
        }

        let ratio = f64::from(guest_width) / f64::from(guest_height);
        let (minimum_width, minimum_content_height) = if ratio >= 1.0 {
            (720.0, (720.0 / ratio).max(360.0))
        } else {
            (480.0, (480.0 / ratio).max(640.0))
        };
        let minimum_height = minimum_content_height + titlebar_height;
        window.set_min_inner_size(Some(LogicalSize::new(minimum_width, minimum_height)));

        if !window.is_maximized() {
            let monitor =
                window
                    .current_monitor()
                    .map_or(LogicalSize::new(1440.0, 900.0), |monitor| {
                        let size = monitor.size();
                        LogicalSize::new(
                            f64::from(size.width) / scale,
                            f64::from(size.height) / scale,
                        )
                    });
            let maximum_width = monitor.width * 0.90;
            let maximum_content_height = (monitor.height * 0.86 - titlebar_height).max(1.0);
            let preferred_area = self.preferred_window_content_area.unwrap_or(current_area);
            let (target_width, _) = aspect_size_from_preferred_area(
                preferred_area,
                guest_width,
                guest_height,
                minimum_width,
                minimum_content_height,
                maximum_width,
                maximum_content_height,
            );
            let target_width_px = physical_dimension(target_width * scale, u32::MAX);
            let target_content_height_px =
                aspect_fit_rect(target_width_px, u32::MAX, guest_width, guest_height).height;
            let titlebar_height_px = if titlebar_height == 0.0 {
                0
            } else {
                physical_dimension(titlebar_height * scale, u32::MAX)
            };
            let target_height_px = target_content_height_px.saturating_add(titlebar_height_px);
            if current.width != target_width_px || current.height != target_height_px {
                let target_size = LogicalSize::new(
                    f64::from(target_width_px) / scale,
                    f64::from(target_height_px) / scale,
                );
                self.programmatic_window_inner_size = Some((target_width_px, target_height_px));
                let _ = window.request_inner_size(target_size);
            }
        }
        self.window_aspect = Some(aspect);
        self.ui_dirty = true;
    }

    fn sync_platform_window_aspect(&mut self, window: &Window, aspect: Option<(u32, u32, u8)>) {
        #[cfg(target_os = "macos")]
        {
            let platform_aspect =
                aspect.map(|(width, height, rotation)| (width, height, 0, rotation, 0));
            if self.platform_window_aspect == platform_aspect {
                return;
            }
            let Ok(handle) = window.window_handle() else {
                return;
            };
            let (width, height) = aspect.map_or((0.0, 0.0), |(width, height, _)| {
                (f64::from(width), f64::from(height))
            });
            if let Err(error) =
                set_macos_window_content_aspect_ratio(handle.as_raw(), width, height)
            {
                warn!(%error, "update macOS content aspect ratio failed");
                return;
            }
            self.platform_window_aspect = platform_aspect;
        }
        #[cfg(windows)]
        {
            let Ok(handle) = window.window_handle() else {
                return;
            };
            let (width, height, rotation_quarters) = aspect.unwrap_or_default();
            let titlebar_height = aspect.map_or(0, |_| {
                physical_dimension(TOP_BAR_LOGICAL * window.scale_factor(), u32::MAX)
            });
            let dpi = aspect.map_or(0, |_| {
                physical_dimension(96.0 * window.scale_factor(), 960).clamp(72, 960)
            });
            let platform_aspect =
                aspect.map(|_| (width, height, titlebar_height, rotation_quarters, dpi));
            if self.platform_window_aspect == platform_aspect {
                return;
            }
            if let Err(error) = set_windows_window_content_aspect_ratio(
                handle.as_raw(),
                width,
                height,
                titlebar_height,
                rotation_quarters,
                dpi,
            ) {
                warn!(%error, "update Windows content aspect ratio failed");
                return;
            }
            self.platform_window_aspect = platform_aspect;
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = window;
            self.platform_window_aspect =
                aspect.map(|(width, height, rotation)| (width, height, 0, rotation, 0));
        }
    }

    fn update_display_layout(&mut self, layout: SurfaceLayout, scale: f64) {
        if self.page != Page::Player
            || layout.width < 64
            || layout.height < 64
            || self.native_display.root_is_minimized()
        {
            // Page replacement, tiny transient layouts and minimize are local composition states.
            // Hide only the HD-owned parent viewport and retain the live display session plus its
            // gfxstream/input children so returning to Player reveals the retained frame without
            // an asynchronous visibility round-trip or swapchain rebuild.
            self.set_viewport_locally_hidden();
            self.sync_display();
            return;
        }
        if self.native_suspended
            || self
                .selected
                .as_ref()
                .is_some_and(|record| record.spec.guest_kind == GuestKindV2::Microdroid)
        {
            self.set_viewport_hidden();
            self.sync_display();
            return;
        }
        let available_width = layout.width;
        #[cfg(windows)]
        let (display_origin_y, available_height) = (
            layout.top_height,
            layout.height.saturating_sub(layout.top_height),
        );
        #[cfg(not(windows))]
        let (display_origin_y, available_height) = (0_u32, layout.height);
        let (rotation_quarters, guest_dimensions) = self
            .active_display_geometry()
            .map_or((0, None), |(width, height, rotation)| {
                (rotation, Some((width, height)))
            });
        let fitted = guest_dimensions.map_or_else(
            || {
                aspect_fit_rect(
                    available_width,
                    available_height,
                    available_width,
                    available_height,
                )
            },
            |(guest_width, guest_height)| {
                aspect_fit_rect(available_width, available_height, guest_width, guest_height)
            },
        );
        let bounds = NativeDisplayBounds {
            x_px: i32::try_from(fitted.x).unwrap_or(i32::MAX),
            y_px: i32::try_from(display_origin_y.saturating_add(fitted.y)).unwrap_or(i32::MAX),
            width_px: fitted.width,
            height_px: fitted.height,
            rotation_quarters,
            visible: true,
        };
        let layout_changed = self.last_native_bounds != Some(bounds);
        if layout_changed {
            // The sidebar is an overlay on the Android surface. Toggling it changes only WebView
            // visibility; resubmitting identical Win32/gfxstream bounds needlessly crosses the
            // compositor boundary and can expose an intermediate black frame on titlebar clicks.
            if let Err(error) = self.native_display.set_bounds(bounds) {
                self.notice(format!("原生显示区域定位失败：{error}"));
                return;
            }
            info!(
                event = "ui.display.layout.changed",
                expanded = self.window_expanded,
                available_width,
                available_height,
                render_x = bounds.x_px,
                render_y = bounds.y_px,
                render_width = bounds.width_px,
                render_height = bounds.height_px,
                rotation_quarters,
                "native Android display layout changed"
            );
            self.last_native_bounds = Some(bounds);
        }
        let dpi = physical_dimension(96.0 * scale, 960).clamp(72, 960);
        let changed = self.viewport.as_ref().is_none_or(|viewport| {
            viewport.width_px != fitted.width
                || viewport.height_px != fitted.height
                || viewport.dpi != dpi
                || !viewport.visible
        });
        if changed {
            self.viewport_revision = self.viewport_revision.saturating_add(1);
            self.viewport = Some(DisplayViewportV2 {
                width_px: fitted.width,
                height_px: fitted.height,
                dpi,
                visible: true,
                revision: self.viewport_revision,
            });
        }
        self.sync_display();
    }

    fn flush_web_state(&mut self, webviews: &WebSurfaces) {
        if !self.ui_dirty {
            return;
        }
        let titlebar = titlebar_player_state(
            self.selected.as_ref(),
            TitlebarPresentationState {
                command_in_flight: self.command_in_flight,
                screen_recording_supported: true,
                player_page: self.page == Page::Player,
                sidebar_visible: self.sidebar_visible,
            },
        );
        // The Windows titlebar is its own WebView surface. Keep it off the full snapshot/busy
        // broadcast used by the sidebar and settings surface: toggling the global busy flag used
        // to disable and fade every titlebar icon for a short command, visibly flashing the whole
        // strip. The host still serializes commands; the top surface only needs stable capability
        // state, recording/FPS changes, and actual layout changes.
        let mut web_titlebar = titlebar_player_state(
            self.selected.as_ref(),
            TitlebarPresentationState {
                command_in_flight: false,
                screen_recording_supported: true,
                player_page: self.page == Page::Player,
                sidebar_visible: self.sidebar_visible,
            },
        );
        // The Windows top surface reads sidebar visibility exclusively from the compact layout
        // message. Normalizing the duplicate field keeps one sidebar click to one React update;
        // otherwise both titlebar and layout messages invalidate the same WebView2 frame and DWM
        // can compose the intermediate titlebar state over the unchanged Android child.
        web_titlebar.sidebar_visible = false;
        let snapshot = json!({
            "type": "snapshot",
            "payload": {
                "summaries": self.summaries,
                "selected": self.selected,
                "selected_display": self.selected_display_id(),
                "host_runtime_current": self.host_runtime_current,
                "screen_recording_supported": true,
                "microdroid_supported": microdroid_platform_supported(),
                "titlebar": &titlebar,
                "status": self.status,
                "artifact_hint": self.artifact_hint,
                "diagnostic_artifact": self
                    .diagnostic_artifact
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                "android_bugreport_artifact": self
                    .android_bugreport_artifact
                    .as_ref()
                    .filter(|(instance_id, _)| Some(*instance_id) == self.selected_id)
                    .map(|(_, path)| path.to_string_lossy().into_owned()),
                "network_setup": &self.network_setup,
                "resource_capability": self.resource_capability,
                "resource_capability_loading": self.selected_id.is_some()
                    && (self.resource_capability_for != self.selected_id
                        || self.resource_capability_gate.is_in_flight()),
                "device_capabilities": self.capabilities.as_ref().map_or(&[][..], |capabilities| capabilities.devices.devices.as_slice()),
                "device_capabilities_loading": self.capabilities_in_flight,
            }
        });
        let layout = json!({
            "type": "layout",
            "payload": {
                "page": self.page,
                "sidebar_collapsed": self.sidebar_collapsed,
                "apk_path": self.selected_id
                    .and_then(|id| self.apk_paths.get(&id))
                    .map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
                "location_route_path": self.selected_id
                    .and_then(|id| self.location_route_paths.get(&id))
                    .map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
                "display_maximized": self.display_maximized,
                "window_expanded": self.window_expanded,
                "android_focused": self.android_focused,
                "sidebar_visible": self.sidebar_visible,
            }
        });
        // The top surface consumes only these two layout fields. Comparing or publishing the body
        // layout here would wake WebView2 for unrelated APK paths, location routes, window sizing,
        // or fullscreen bookkeeping even though none of those values can change a titlebar pixel.
        let top_layout = json!({
            "type": "layout",
            "payload": {
                "page": self.page,
                "sidebar_visible": self.sidebar_visible,
            }
        });
        webviews.send_body(&snapshot);
        webviews.send_body(&layout);
        webviews.send_body(&json!({ "type": "busy", "payload": self.command_in_flight }));
        webviews.send_body(&json!({ "type": "notice", "payload": self.status }));
        if self.last_web_titlebar.as_ref() != Some(&web_titlebar) {
            webviews.send_top(&json!({ "type": "titlebar", "payload": &web_titlebar }));
            self.last_web_titlebar = Some(web_titlebar);
        }
        if self.last_web_top_layout.as_ref() != Some(&top_layout) {
            webviews.send_top(&top_layout);
            self.last_web_top_layout = Some(top_layout);
        }
        self.ui_dirty = false;
    }

    fn sync_titlebar_fps(&mut self, window: &Window) {
        #[cfg(target_os = "macos")]
        {
            let titlebar = titlebar_player_state(
                self.selected.as_ref(),
                TitlebarPresentationState {
                    command_in_flight: self.command_in_flight,
                    screen_recording_supported: true,
                    player_page: self.page == Page::Player,
                    sidebar_visible: self.sidebar_visible,
                },
            );
            let state = (titlebar.show_host_fps, titlebar.host_fps_milli);
            if self.last_titlebar_fps == Some(state) {
                return;
            }
            let result = window
                .window_handle()
                .map_err(|error| error.to_string())
                .and_then(|handle| {
                    set_macos_titlebar_fps(handle.as_raw(), state.0, state.1)
                        .map_err(|error| error.to_string())
                });
            if let Err(error) = result {
                warn!(%error, "update macOS titlebar FPS failed");
                return;
            }
            self.last_titlebar_fps = Some(state);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (self, window);
    }

    fn sync_titlebar_player_state(&mut self, window: &Window) {
        #[cfg(target_os = "macos")]
        {
            let state = serde_json::to_string(&titlebar_player_state(
                self.selected.as_ref(),
                TitlebarPresentationState {
                    command_in_flight: self.command_in_flight,
                    screen_recording_supported: true,
                    player_page: self.page == Page::Player,
                    sidebar_visible: self.sidebar_visible,
                },
            ))
            .expect("serialize titlebar player state");
            if self.last_titlebar_player_state.as_deref() == Some(state.as_str()) {
                return;
            }
            let result = window
                .window_handle()
                .map_err(|error| error.to_string())
                .and_then(|handle| {
                    set_macos_titlebar_player_state(handle.as_raw(), &state)
                        .map_err(|error| error.to_string())
                });
            if let Err(error) = result {
                warn!(%error, "update native titlebar player state failed");
                return;
            }
            self.last_titlebar_player_state = Some(state);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (self, window);
    }

    fn poll(&mut self) {
        if self.native_suspended {
            return;
        }
        if !self.closing && self.last_refresh.elapsed() >= self.snapshot_poll_mode().interval() {
            self.request_refresh();
        }
        let minimized = self.native_display.root_is_minimized();
        if minimized && !self.root_was_minimized {
            self.root_was_minimized = true;
            self.suspend_display_for_minimize();
        } else if !minimized && self.root_was_minimized {
            self.root_was_minimized = false;
        }
        self.request_network_setup_status(false);
        self.sync_display();
    }

    fn network_status_poll_mode(&self) -> NetworkStatusPollMode {
        network_status_poll_mode(
            self.page,
            self.window_focused,
            self.native_suspended || self.root_was_minimized || self.root_occluded,
        )
    }

    fn snapshot_poll_mode(&self) -> SnapshotPollMode {
        let lifecycle_transition = self.command_in_flight
            || self.display_in_flight
            || self.selected.as_ref().is_some_and(|record| {
                !matches!(
                    record.status.observed,
                    ObservedStateV2::Defined
                        | ObservedStateV2::Stopped
                        | ObservedStateV2::Ready
                        | ObservedStateV2::Paused
                        | ObservedStateV2::Blocked
                        | ObservedStateV2::Failed
                        | ObservedStateV2::Deleted
                )
            });
        snapshot_poll_mode(
            self.page,
            self.window_focused,
            self.native_suspended || self.root_was_minimized || self.root_occluded,
            lifecycle_transition,
        )
    }

    fn next_wakeup_interval(&self) -> Duration {
        self.snapshot_poll_mode().interval().min(DISPLAY_HEARTBEAT)
    }

    fn request_refresh(&mut self) {
        if self.closing {
            return;
        }
        let Some(generation) = self.refresh_gate.begin() else {
            return;
        };
        self.last_refresh = Instant::now();
        let client = self.client.clone();
        let selected_id = self.selected_id;
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = client
                .ui_snapshot(selected_id)
                .await
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::Refreshed { generation, result });
        });
    }

    fn request_resource_admission(&mut self) {
        if self.closing {
            return;
        }
        let Some(instance_id) = self.selected_id else {
            self.resource_capability = None;
            self.resource_capability_for = None;
            return;
        };
        let Some(generation) = self.resource_capability_gate.begin() else {
            return;
        };
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = client
                .resource_admission(instance_id)
                .await
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::ResourceAdmissionRefreshed {
                generation,
                instance_id,
                result,
            });
        });
    }

    fn on_resource_admission_refreshed(
        &mut self,
        generation: u64,
        instance_id: Uuid,
        result: Result<CapabilityProbeV2, String>,
    ) {
        if !self.resource_capability_gate.complete(generation) {
            info!(
                event = "ui.resource_admission.stale_rejected",
                requested_generation = generation,
                current_generation = self.resource_capability_gate.generation(),
                %instance_id,
                "rejected stale or duplicate resource admission response"
            );
            self.request_resource_admission();
            return;
        }
        if self.selected_id != Some(instance_id) {
            self.request_resource_admission();
            return;
        }
        match result {
            Ok(capability) => {
                self.resource_capability = Some(capability);
                self.resource_capability_for = Some(instance_id);
                self.ui_dirty = true;
            }
            Err(error) => {
                self.resource_capability = None;
                self.resource_capability_for = Some(instance_id);
                self.ui_dirty = true;
                warn!(%error, %instance_id, "refresh HD resource admission failed");
            }
        }
    }

    fn request_capabilities(&mut self) {
        if self.capabilities_in_flight || self.closing {
            return;
        }
        let Some(instance_id) = self.selected_id else {
            self.capabilities = None;
            self.capabilities_for = None;
            return;
        };
        self.capabilities_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = client
                .capabilities(Some(instance_id))
                .await
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::CapabilitiesRefreshed {
                instance_id,
                result,
            });
        });
    }

    fn on_capabilities_refreshed(
        &mut self,
        instance_id: Uuid,
        result: Result<HostCapabilitiesV2, String>,
    ) {
        self.capabilities_in_flight = false;
        if self.selected_id != Some(instance_id) {
            return;
        }
        match result {
            Ok(capabilities) => {
                self.capabilities = Some(capabilities);
                self.capabilities_for = Some(instance_id);
                self.ui_dirty = true;
            }
            Err(error) => {
                self.capabilities = None;
                self.capabilities_for = Some(instance_id);
                self.ui_dirty = true;
                warn!(%error, %instance_id, "refresh HD device capabilities failed");
            }
        }
    }

    fn on_refresh(&mut self, generation: u64, result: Result<UiSnapshotV2, String>) {
        if !self.refresh_gate.complete(generation) {
            info!(
                event = "ui.snapshot.stale_rejected",
                requested_generation = generation,
                current_generation = self.refresh_gate.generation(),
                "rejected a stale or duplicate Host snapshot"
            );
            self.request_refresh();
            return;
        }
        match result {
            Ok(UiSnapshotV2 {
                summaries,
                selected,
            }) => {
                let state_changed = self.summaries != summaries || self.selected != selected;
                self.summaries = summaries;
                let refreshed_selected_id = selected.as_ref().map(|record| record.spec.id);
                if self.selected_id != refreshed_selected_id {
                    self.resource_capability_gate.invalidate();
                    self.resource_capability = None;
                    self.resource_capability_for = None;
                    self.capabilities = None;
                    self.capabilities_for = None;
                    self.reset_display_attachment_for_scanout(0);
                }
                self.selected_id = refreshed_selected_id;
                self.selected = selected;
                self.reconcile_selected_display_target();
                if self.resource_capability_for != self.selected_id
                    && !self.resource_capability_gate.is_in_flight()
                {
                    self.request_resource_admission();
                }
                if self.status == "正在连接 HD Host…" {
                    "HD Host 已同步".clone_into(&mut self.status);
                    self.ui_dirty = true;
                } else if state_changed {
                    // Only changed snapshots are committed to the sidebar/page WebView trees,
                    // avoiding needless composition next to the native Vulkan child.
                    self.ui_dirty = true;
                }
                if self.auto_start_selected {
                    let inactive = self
                        .selected
                        .as_ref()
                        .is_some_and(|record| !record.status.observed.is_active());
                    let microdroid = self
                        .selected
                        .as_ref()
                        .is_some_and(|record| record.spec.guest_kind == GuestKindV2::Microdroid);
                    if !inactive {
                        self.auto_start_selected = false;
                    } else if microdroid || self.session.is_some() {
                        self.auto_start_selected = false;
                        self.operation("start", None);
                    }
                }
            }
            Err(error) => self.notice(error),
        }
    }

    fn select(&mut self, instance_id: Uuid) {
        if let Some((dirty_id, _)) = self.settings_dirty
            && self.selected_id == Some(dirty_id)
        {
            self.notice("当前实例设置尚未保存；请先保存或放弃更改，再切换页面或实例".to_owned());
            return;
        }
        if self.selected_id == Some(instance_id) {
            self.page = Page::Player;
            self.ui_dirty = true;
            return;
        }
        self.refresh_gate.invalidate();
        self.reset_display_attachment_for_scanout(0);
        self.selected_id = Some(instance_id);
        self.selected_displays
            .entry(instance_id)
            .or_insert(DisplayIdV2::Primary);
        self.selected = None;
        self.capabilities = None;
        self.capabilities_for = None;
        self.resource_capability_gate.invalidate();
        self.resource_capability = None;
        self.resource_capability_for = None;
        self.auto_start_selected = true;
        self.page = Page::Player;
        self.last_refresh = Instant::now()
            .checked_sub(MAX_SNAPSHOT_POLL_INTERVAL)
            .unwrap_or_else(Instant::now);
        self.ui_dirty = true;
        self.request_resource_admission();
        self.request_refresh();
    }

    fn create(&mut self, name: String, guest_kind: GuestKindV2) {
        if self.command_in_flight {
            return;
        }
        if guest_kind == GuestKindV2::Microdroid && !microdroid_platform_supported() {
            self.notice("Microdroid 仅支持 macOS Apple Silicon 或 Windows x86_64".to_owned());
            return;
        }
        let spec = match guest_kind {
            GuestKindV2::Android => InstanceSpecV2 {
                name,
                artifacts: default_artifact_selection(),
                ..InstanceSpecV2::default()
            },
            GuestKindV2::Microdroid => InstanceSpecV2::microdroid(name),
        };
        if let Err(error) = spec.validate() {
            self.notice(error.to_string());
            return;
        }
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = client
                .create_instance(&CreateInstanceRequestV2 { spec })
                .await
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::Created(result));
        });
    }

    fn on_created(&mut self, result: Result<InstanceRecordV2, String>) {
        self.command_in_flight = false;
        match result {
            Ok(record) => {
                self.refresh_gate.invalidate();
                let instance_id = record.spec.id;
                self.reset_display_attachment_for_scanout(0);
                self.selected_displays
                    .insert(instance_id, DisplayIdV2::Primary);
                self.selected_id = Some(instance_id);
                self.selected = Some(record);
                self.capabilities = None;
                self.capabilities_for = None;
                self.resource_capability_gate.invalidate();
                self.resource_capability = None;
                self.resource_capability_for = None;
                self.request_resource_admission();
                self.auto_start_selected = true;
                self.page = Page::Player;
                self.notice(
                    if self
                        .selected
                        .as_ref()
                        .is_some_and(|record| record.spec.guest_kind == GuestKindV2::Microdroid)
                    {
                        "Microdroid 实例已创建，正在启动工作负载".to_owned()
                    } else {
                        "Android 实例已创建，正在启动".to_owned()
                    },
                );
                self.request_refresh();
            }
            Err(error) => self.notice(error),
        }
    }

    fn save_spec(&mut self, spec: InstanceSpecV2, restart: bool, expected_revision: Option<u64>) {
        if self.command_in_flight {
            return;
        }
        let Some(record) = &self.selected else { return };
        if spec.id != record.spec.id {
            self.notice("设置实例与当前实例不一致".to_owned());
            return;
        }
        let Some(expected_revision) = expected_revision else {
            self.notice("设置缺少实例版本，请重新打开设置页".to_owned());
            return;
        };
        if expected_revision != record.status.revision {
            self.notice("实例已在其他位置更新，请重新加载后再保存".to_owned());
            return;
        }
        if let Err(error) = spec.validate() {
            self.notice(error.to_string());
            return;
        }
        let id = record.spec.id;
        let was_active = record.status.observed.is_active();
        let restarting = was_active && restart;
        let original_spec = record.spec.clone();
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                if restarting {
                    let operation = client
                        .create_operation(
                            id,
                            OperationKindV2::Stop {
                                mode: StopModeV2::Graceful,
                                graceful_timeout_ms: 20_000,
                            },
                            &format!("hd-web-ui-save-stop-{}", Uuid::new_v4()),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    client
                        .wait_operation(operation.id, Duration::from_secs(90))
                        .await
                        .map_err(|error| error.to_string())?;
                }

                let update_revision = if restarting {
                    let current = client
                        .get_instance(id)
                        .await
                        .map_err(|error| error.to_string())?;
                    if current.spec != original_spec {
                        return Err("实例设置已在停机期间被其他位置更新；本次更改未保存".to_owned());
                    }
                    current.status.revision
                } else {
                    expected_revision
                };
                let request = UpdateInstanceRequestV2 {
                    expected_revision: update_revision,
                    spec,
                };
                let mut saved = client
                    .update_instance(id, &request)
                    .await
                    .map_err(|error| error.to_string())?;

                if restarting {
                    let operation = client
                        .create_operation(
                            id,
                            OperationKindV2::Start,
                            &format!("hd-web-ui-save-start-{}", Uuid::new_v4()),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    client
                        .wait_operation(operation.id, Duration::from_mins(3))
                        .await
                        .map_err(|error| error.to_string())?;
                    saved = client
                        .get_instance(id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                Ok(saved)
            }
            .await;
            let _ = proxy.send_event(UserEvent::Saved {
                result,
                restarted: restarting,
            });
        });
    }

    fn on_saved(&mut self, result: Result<InstanceRecordV2, String>, restarted: bool) {
        self.command_in_flight = false;
        match result {
            Ok(record) => {
                self.refresh_gate.invalidate();
                self.settings_dirty = None;
                if self.selected_id != Some(record.spec.id) {
                    info!(
                        event = "ui.async_result.selection_changed",
                        result_kind = "settings_save",
                        result_instance_id = %record.spec.id,
                        selected_instance_id = ?self.selected_id,
                        "kept an asynchronous result from overwriting the current selection"
                    );
                    self.notice(format!(
                        "“{}”的设置已保存；当前选择已切换，未覆盖当前实例界面",
                        record.spec.name
                    ));
                    self.request_refresh();
                    return;
                }
                let refresh_device_capabilities =
                    self.capabilities_for == Some(record.spec.id) || self.page == Page::Devices;
                self.selected = Some(record);
                self.capabilities = None;
                self.capabilities_for = None;
                self.resource_capability_gate.invalidate();
                self.resource_capability = None;
                self.resource_capability_for = None;
                self.request_resource_admission();
                if refresh_device_capabilities {
                    self.request_capabilities();
                }
                if restarted {
                    // Keep the Player-owned display session alive across the graceful restart.
                    // The Host passes this session to the replacement crosvm process before
                    // gfxstream creates its CAMetalLayer. Force an immediate heartbeat so the
                    // shell observes the new frame generation without exposing crosvm's fallback
                    // top-level window or leaving the Player black.
                    self.last_session_viewport = None;
                    self.last_display_attempt = Instant::now()
                        .checked_sub(DISPLAY_RETRY)
                        .unwrap_or_else(Instant::now);
                }
                self.notice(if restarted {
                    "设置已保存，Android 已重新启动".to_owned()
                } else {
                    "设置已保存".to_owned()
                });
                self.request_refresh();
            }
            Err(error) => self.notice(error),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn operation(&mut self, kind: &str, parameters: Option<&Value>) {
        if self.command_in_flight {
            return;
        }
        let is_microdroid = self
            .selected
            .as_ref()
            .is_some_and(|record| record.spec.guest_kind == GuestKindV2::Microdroid);
        if kind == "start" && !is_microdroid && self.session.is_none() {
            self.auto_start_selected = true;
            self.notice("正在准备 Android 原生显示区…".to_owned());
            self.sync_display();
            return;
        }
        let Some(id) = self.selected_id else { return };
        let guest_label = if is_microdroid {
            "Microdroid"
        } else {
            "Android"
        };
        let (operation, timeout, label) = match kind {
            "start" => (
                OperationKindV2::Start,
                Duration::from_mins(3),
                format!("{guest_label} 已启动"),
            ),
            "pause" => (
                OperationKindV2::Pause,
                Duration::from_secs(30),
                format!("{guest_label} 已暂停"),
            ),
            "resume" => (
                OperationKindV2::Resume,
                Duration::from_secs(30),
                format!("{guest_label} 已恢复"),
            ),
            "restart" => (
                OperationKindV2::Restart,
                Duration::from_mins(3),
                format!("{guest_label} 已重启"),
            ),
            "stop" => (
                OperationKindV2::Stop {
                    mode: StopModeV2::Graceful,
                    graceful_timeout_ms: 20_000,
                },
                Duration::from_secs(90),
                format!("{guest_label} 已关闭"),
            ),
            "force_stop" => (
                OperationKindV2::Stop {
                    mode: StopModeV2::Force,
                    graceful_timeout_ms: 0,
                },
                Duration::from_secs(90),
                format!("{guest_label} 已强制停止"),
            ),
            "delete" => (
                OperationKindV2::Delete,
                Duration::from_secs(90),
                "实例已删除".to_owned(),
            ),
            "powerwash" => {
                let Some(expected_revision) = parameters
                    .and_then(|value| value.get("expected_revision"))
                    .and_then(Value::as_u64)
                else {
                    self.notice("恢复出厂数据请求缺少实例 revision".to_owned());
                    return;
                };
                let Some(confirmation_name) = parameters
                    .and_then(|value| value.get("confirmation_name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    self.notice("恢复出厂数据请求缺少实例名确认".to_owned());
                    return;
                };
                (
                    OperationKindV2::Powerwash {
                        expected_revision,
                        confirmation_name,
                    },
                    Duration::from_mins(10),
                    "Android userdata 已恢复出厂状态，旧数据备份已保留".to_owned(),
                )
            }
            "restore_powerwash" | "discard_powerwash_backup" => {
                let backup_id = parameters
                    .and_then(|value| value.get("backup_id"))
                    .and_then(Value::as_str)
                    .and_then(|id| Uuid::parse_str(id).ok());
                let expected_revision = parameters
                    .and_then(|value| value.get("expected_revision"))
                    .and_then(Value::as_u64);
                let confirmation_name = parameters
                    .and_then(|value| value.get("confirmation_name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let (Some(backup_id), Some(expected_revision), Some(confirmation_name)) =
                    (backup_id, expected_revision, confirmation_name)
                else {
                    self.notice("数据备份操作缺少备份、revision 或实例名确认".to_owned());
                    return;
                };
                if kind == "restore_powerwash" {
                    (
                        OperationKindV2::RestorePowerwash {
                            backup_id,
                            expected_revision,
                            confirmation_name,
                        },
                        Duration::from_mins(10),
                        "旧 userdata 已恢复；此前的当前数据已保留为回滚备份".to_owned(),
                    )
                } else {
                    (
                        OperationKindV2::DiscardPowerwashBackup {
                            backup_id,
                            expected_revision,
                            confirmation_name,
                        },
                        Duration::from_mins(10),
                        "userdata 备份已永久丢弃".to_owned(),
                    )
                }
            }
            _ => {
                self.notice(format!("未知实例操作：{kind}"));
                return;
            }
        };
        if matches!(
            operation,
            OperationKindV2::Stop { .. }
                | OperationKindV2::Delete
                | OperationKindV2::Powerwash { .. }
                | OperationKindV2::RestorePowerwash { .. }
                | OperationKindV2::DiscardPowerwashBackup { .. }
        ) {
            self.release_display();
        }
        let artifact_update = (!is_microdroid
            && matches!(operation, OperationKindV2::Start | OperationKindV2::Restart))
        .then_some(self.selected.as_ref())
        .flatten()
        .and_then(|record| {
            let artifacts = DIRECT_DEV_SELECTION.get().cloned().or_else(|| {
                record
                    .spec
                    .artifacts
                    .is_none()
                    .then(default_artifact_selection)
                    .flatten()
            })?;
            if record.spec.artifacts.as_ref() == Some(&artifacts) {
                return None;
            }
            let mut spec = record.spec.clone();
            spec.artifacts = Some(artifacts);
            Some(UpdateInstanceRequestV2 {
                expected_revision: record.status.revision,
                spec,
            })
        });
        self.command_in_flight = true;
        self.refresh_display_after_command = matches!(
            &operation,
            OperationKindV2::Resume | OperationKindV2::Restart
        );
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        let key = format!("hd-web-ui-{}", Uuid::new_v4());
        self.runtime.spawn(async move {
            let result = async {
                if let Some(request) = artifact_update {
                    client
                        .update_instance(id, &request)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                let operation = client
                    .create_operation(id, operation, &key)
                    .await
                    .map_err(|e| e.to_string())?;
                client
                    .wait_operation(operation.id, timeout)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(label)
            }
            .await;
            let _ = proxy.send_event(UserEvent::CommandFinished {
                instance_id: id,
                result,
            });
        });
    }

    fn on_command_finished(&mut self, instance_id: Uuid, result: Result<String, String>) {
        self.refresh_gate.invalidate();
        self.resource_capability_gate.invalidate();
        self.resource_capability = None;
        self.resource_capability_for = None;
        self.command_in_flight = false;
        let refresh_display = std::mem::take(&mut self.refresh_display_after_command);
        if refresh_display && result.is_ok() {
            // Resume and restart preserve the Host-owned session. An immediate equal-viewport
            // heartbeat refreshes its frame generation and repairs the retained child hierarchy
            // without detaching gfxstream or exposing a black reacquisition interval.
            self.last_session_viewport = None;
        }
        if self.selected_id == Some(instance_id) {
            match result {
                Ok(message) => self.notice(message),
                Err(error) => self.notice(error),
            }
        } else {
            info!(
                event = "ui.async_result.selection_rejected",
                %instance_id,
                selected_instance_id = ?self.selected_id,
                "ignored an instance command completion after the selection changed"
            );
        }
        self.last_refresh = Instant::now()
            .checked_sub(MAX_SNAPSHOT_POLL_INTERVAL)
            .unwrap_or_else(Instant::now);
        self.request_resource_admission();
        self.request_refresh();
    }

    fn on_diagnostics_finished(&mut self, result: Result<DiagnosticBundleResponseV2, String>) {
        self.command_in_flight = false;
        match result {
            Ok(bundle) => {
                self.status = format!(
                    "诊断包已生成（{} KiB）",
                    bundle.size_bytes.saturating_add(1023) / 1024
                );
                self.diagnostic_artifact = Some(bundle.path);
            }
            Err(error) => self.status = format!("生成诊断包失败：{error}"),
        }
        self.ui_dirty = true;
        self.last_refresh = Instant::now()
            .checked_sub(MAX_SNAPSHOT_POLL_INTERVAL)
            .unwrap_or_else(Instant::now);
        self.request_refresh();
    }

    fn on_android_bugreport_finished(&mut self, result: Result<AndroidBugreportRecordV2, String>) {
        self.command_in_flight = false;
        match result {
            Ok(bugreport) => {
                self.status = format!(
                    "Android bugreport 已生成（{} MiB，包含敏感设备数据）",
                    bugreport.size_bytes.saturating_add(1024 * 1024 - 1) / (1024 * 1024)
                );
                self.android_bugreport_artifact = Some((bugreport.instance_id, bugreport.path));
            }
            Err(error) => self.status = format!("生成 Android bugreport 失败：{error}"),
        }
        self.ui_dirty = true;
        self.last_refresh = Instant::now()
            .checked_sub(MAX_SNAPSHOT_POLL_INTERVAL)
            .unwrap_or_else(Instant::now);
        self.request_refresh();
    }

    fn key(&mut self, key: &str) {
        let key = match key {
            "home" => KeyActionV2::Home,
            "recent" => KeyActionV2::Recent,
            "back" => KeyActionV2::Back,
            "power" => KeyActionV2::Power,
            "volume_up" => KeyActionV2::VolumeUp,
            "volume_down" => KeyActionV2::VolumeDown,
            _ => {
                self.notice(format!("未知 Android 按键：{key}"));
                return;
            }
        };
        let action = InstanceActionV2::Key { key };
        if let Err(error) = action.validate() {
            self.notice(error.to_string());
            return;
        }
        let Some(id) = self.selected_id else { return };
        if let Err(error) = self.native_display.focus_guest() {
            warn!(
                event = "ui.display.focus.failed",
                %error,
                "restore native Android display focus before key input failed"
            );
        }
        // Android navigation, volume and power keys are input events, not long-running instance
        // commands. Do not toggle the global busy state or broadcast a new WebView snapshot for
        // every click: doing so disables/re-enables the titlebar DOM around the pointer and makes
        // WebView2 visibly flash. Successful input has no UI state to publish; only failures enter
        // the event loop and surface a user-visible error.
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = client
                .action(id, action)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            if result.is_err() {
                let _ = proxy.send_event(UserEvent::InputFinished {
                    instance_id: id,
                    result,
                });
            }
        });
    }

    fn trackpad(&mut self, event: TrackpadEventV2) {
        let action = InstanceActionV2::Trackpad { event };
        if let Err(error) = action.validate() {
            self.notice(error.to_string());
            return;
        }
        let down_instance = if event.phase == TrackpadPhaseV2::Down {
            let Some(record) = self.selected.as_ref() else {
                return;
            };
            if record.spec.guest_kind != GuestKindV2::Android
                || !record.spec.devices.touchpad
                || record.status.observed != ObservedStateV2::Ready
            {
                return;
            }
            Some(record.spec.id)
        } else {
            None
        };
        if let Err(error) = self.trackpad_tx.send(down_instance, event) {
            match error {
                TrackpadQueueError::NoActiveGesture | TrackpadQueueError::GestureAlreadyActive => {}
                TrackpadQueueError::MissingDownInstance | TrackpadQueueError::Saturated => {
                    self.notice(format!("触控板输入暂不可用：{error}"));
                }
            }
        }
    }

    fn rotate(&mut self) {
        let orientation = self
            .selected
            .as_ref()
            .map_or(OrientationV2::Landscape, |record| {
                match record.spec.display.orientation {
                    OrientationV2::Portrait => OrientationV2::Landscape,
                    OrientationV2::Landscape => OrientationV2::Portrait,
                    OrientationV2::ReversePortrait => OrientationV2::ReverseLandscape,
                    OrientationV2::ReverseLandscape => OrientationV2::ReversePortrait,
                }
            });
        self.action(InstanceActionV2::Rotate { orientation }, "旋转命令已发送");
    }

    fn action(&mut self, action: InstanceActionV2, label: &'static str) {
        if self.command_in_flight {
            return;
        }
        if let Err(error) = action.validate() {
            self.notice(error.to_string());
            return;
        }
        let Some(id) = self.selected_id else { return };
        if let Err(error) = self.native_display.focus_guest() {
            warn!(
                event = "ui.display.focus.failed",
                %error,
                "restore native Android display focus before action failed"
            );
        }
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = client
                .action(id, action)
                .await
                .map(|_| label.to_owned())
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::CommandFinished {
                instance_id: id,
                result,
            });
        });
    }

    fn screenshot(&mut self) {
        if self.command_in_flight {
            return;
        }
        let Some(id) = self.selected_id else { return };
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = client
                .capture_screenshot(id)
                .await
                .map(|record| format!("截图已保存：{}", record.path.display()))
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::CommandFinished {
                instance_id: id,
                result,
            });
        });
    }

    fn start_screen_recording(&mut self) {
        if self.command_in_flight {
            return;
        }
        let Some(record) = self.selected.as_ref() else {
            return;
        };
        if record.spec.guest_kind != GuestKindV2::Android
            || record.status.observed != ObservedStateV2::Ready
            || !record.adb_ready
        {
            self.notice("录屏需要已就绪且 ADB 可用的 Android 实例".to_owned());
            return;
        }
        if record.screen_recording.is_some() {
            self.notice("当前实例已有录屏任务，请先停止并保存".to_owned());
            return;
        }
        let id = record.spec.id;
        let display_id = self.selected_display_id();
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = client
                .start_display_screen_recording(id, display_id, 180)
                .await
                .map(|_| "录屏已开始，最长 3 分钟；再次点击录屏按钮停止并保存".to_owned())
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::CommandFinished {
                instance_id: id,
                result,
            });
        });
    }

    fn stop_screen_recording(&mut self) {
        if self.command_in_flight {
            return;
        }
        let Some(record) = self.selected.as_ref() else {
            return;
        };
        if record.screen_recording.is_none() {
            self.notice("当前实例没有正在进行的录屏".to_owned());
            return;
        }
        let id = record.spec.id;
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = client
                .stop_screen_recording(id)
                .await
                .map(|record| format!("录屏已保存：{}", record.path.display()))
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::CommandFinished {
                instance_id: id,
                result,
            });
        });
    }

    fn install_selected_apk(&mut self) {
        if self
            .selected
            .as_ref()
            .is_some_and(|record| record.spec.guest_kind == GuestKindV2::Microdroid)
        {
            self.notice(
                "Microdroid Payload 需要在实例设置中导入，不能作为 Android APK 安装".to_owned(),
            );
            return;
        }
        let Some(path) = self
            .selected_id
            .and_then(|id| self.apk_paths.get(&id))
            .cloned()
        else {
            self.notice("请在设置页选择 APK，或将 APK 文件拖入 HD 窗口".to_owned());
            return;
        };
        self.install_apk(path);
    }

    fn choose_apk(&mut self, install_after_selection: bool) {
        match choose_apk_file() {
            Ok(Some(path)) => {
                if let Some(id) = self.selected_id {
                    self.apk_paths.insert(id, path.clone());
                }
                self.ui_dirty = true;
                if install_after_selection {
                    self.install_apk(path);
                } else {
                    self.notice(format!("已选择 APK：{}", path.display()));
                }
            }
            Ok(None) => {}
            Err(error) => self.notice(format!("选择 APK 失败：{error}")),
        }
    }

    fn choose_location_route(&mut self) {
        match choose_location_route_file() {
            Ok(Some(path)) => {
                if let Some(id) = self.selected_id {
                    self.location_route_paths.insert(id, path.clone());
                }
                self.ui_dirty = true;
                self.notice(format!("已选择定位路线：{}", path.display()));
            }
            Ok(None) => {}
            Err(error) => self.notice(format!("选择定位路线失败：{error}")),
        }
    }

    fn start_location_route(&mut self, interval_ms: u32, repeat: bool) {
        if self.command_in_flight {
            return;
        }
        let Some(record) = self.selected.as_ref() else {
            self.notice("请先选择 Android 实例".to_owned());
            return;
        };
        if record.spec.guest_kind != GuestKindV2::Android {
            self.notice("定位路线仅用于 Android 实例".to_owned());
            return;
        }
        if !record.adb_ready || record.status.observed != ObservedStateV2::Ready {
            self.notice("Android 尚未就绪，不能开始路线回放".to_owned());
            return;
        }
        let id = record.spec.id;
        let Some(path) = self.location_route_paths.get(&id).cloned() else {
            self.notice("请先选择 GPX 或 KML 路线文件".to_owned());
            return;
        };
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let route = tokio::task::spawn_blocking(move || {
                    hd_runtime::load_location_route(&path, interval_ms, repeat)
                })
                .await
                .map_err(|error| format!("路线解析任务失败：{error}"))?
                .map_err(|error| error.to_string())?;
                let description = format!(
                    "正在回放 {}（{} 个点，间隔 {} ms{}）",
                    route.name,
                    route.points.len(),
                    route.interval_ms,
                    if route.repeat { "，循环" } else { "" }
                );
                client
                    .action(id, InstanceActionV2::StartLocationRoute { route })
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(description)
            }
            .await;
            let _ = proxy.send_event(UserEvent::CommandFinished {
                instance_id: id,
                result,
            });
        });
    }

    fn choose_microdroid_payload(&mut self) {
        if self.command_in_flight {
            return;
        }
        let Some(record) = self.selected.as_ref() else {
            self.notice("请先选择 Microdroid 实例".to_owned());
            return;
        };
        if record.spec.guest_kind != GuestKindV2::Microdroid {
            self.notice("Payload APK 仅用于 Microdroid 实例".to_owned());
            return;
        }
        if record.status.observed.is_active() {
            self.notice("更换 Payload 前请先停止当前 Microdroid 实例".to_owned());
            return;
        }
        if self
            .settings_dirty
            .is_some_and(|(instance_id, _)| instance_id == record.spec.id)
        {
            self.notice("请先保存或放弃当前设置修改，再导入 Payload".to_owned());
            return;
        }
        let path = match choose_apk_file() {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                self.notice(format!("选择 Payload APK 失败：{error}"));
                return;
            }
        };
        let mut spec = record.spec.clone();
        let expected_revision = record.status.revision;
        let id = spec.id;
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let inspection = hd_runtime::inspect_microdroid_payload_apk(&path)
                    .await
                    .map_err(|error| error.to_string())?;
                let upload = client
                    .upload_apk(&path)
                    .await
                    .map_err(|error| error.to_string())?;
                let microdroid = spec
                    .microdroid
                    .as_mut()
                    .ok_or_else(|| "Microdroid 配置缺失".to_owned())?;
                microdroid.payload = hd_core::MicrodroidPayloadV2::Uploaded {
                    upload_id: upload.id,
                    sha256: upload.sha256,
                    config_path: "assets/vm_config.json".to_owned(),
                };
                microdroid.payload_extra_apk_count = Some(inspection.declared_extra_apk_count);
                microdroid.extra_apks.clear();
                spec.validate().map_err(|error| error.to_string())?;
                client
                    .update_instance(
                        id,
                        &UpdateInstanceRequestV2 {
                            expected_revision,
                            spec,
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            let _ = proxy.send_event(UserEvent::PayloadImported(result));
        });
    }

    fn on_payload_imported(&mut self, result: Result<InstanceRecordV2, String>) {
        self.command_in_flight = false;
        match result {
            Ok(record) => {
                self.refresh_gate.invalidate();
                if self.selected_id != Some(record.spec.id) {
                    info!(
                        event = "ui.async_result.selection_changed",
                        result_kind = "payload_import",
                        result_instance_id = %record.spec.id,
                        selected_instance_id = ?self.selected_id,
                        "kept an asynchronous result from overwriting the current selection"
                    );
                    self.notice(format!(
                        "Payload 已保存到“{}”；当前选择已切换，未覆盖当前实例界面",
                        record.spec.name
                    ));
                    self.request_refresh();
                    return;
                }
                let declared_extra_apks = record
                    .spec
                    .microdroid
                    .as_ref()
                    .and_then(|config| config.payload_extra_apk_count)
                    .unwrap_or(0);
                self.selected = Some(record);
                self.capabilities = None;
                self.capabilities_for = None;
                self.resource_capability_gate.invalidate();
                self.resource_capability = None;
                self.resource_capability_for = None;
                self.notice(format!(
                    "Payload APK 已校验并保存；配置声明 {declared_extra_apks} 个额外 APK"
                ));
                self.request_resource_admission();
                self.request_refresh();
            }
            Err(error) => self.notice(format!("导入 Payload 失败：{error}")),
        }
    }

    fn choose_microdroid_extra_apk(&mut self) {
        if self.command_in_flight {
            return;
        }
        let Some(record) = self.selected.as_ref() else {
            self.notice("请先选择 Microdroid 实例".to_owned());
            return;
        };
        let Some(microdroid) = record.spec.microdroid.as_ref() else {
            self.notice("额外 APK 仅用于 Microdroid 实例".to_owned());
            return;
        };
        if !matches!(
            microdroid.payload,
            hd_core::MicrodroidPayloadV2::Uploaded { .. }
        ) {
            self.notice("请先导入主 Payload APK".to_owned());
            return;
        }
        if microdroid.extra_apks.len() >= hd_core::MAX_MICRODROID_EXTRA_APKS {
            self.notice(format!(
                "额外 APK 最多 {} 个",
                hd_core::MAX_MICRODROID_EXTRA_APKS
            ));
            return;
        }
        if microdroid
            .payload_extra_apk_count
            .is_some_and(|declared| microdroid.extra_apks.len() >= usize::from(declared))
        {
            self.notice(format!(
                "主 Payload 只声明了 {} 个额外 APK，不能继续添加",
                microdroid
                    .payload_extra_apk_count
                    .expect("declared count was checked")
            ));
            return;
        }
        if record.status.observed.is_active() {
            self.notice("添加额外 APK 前请先停止当前 Microdroid 实例".to_owned());
            return;
        }
        if self
            .settings_dirty
            .is_some_and(|(instance_id, _)| instance_id == record.spec.id)
        {
            self.notice("请先保存或放弃当前设置修改，再添加额外 APK".to_owned());
            return;
        }
        let path = match choose_apk_file() {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                self.notice(format!("选择额外 APK 失败：{error}"));
                return;
            }
        };
        let mut spec = record.spec.clone();
        let expected_revision = record.status.revision;
        let id = spec.id;
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                hd_runtime::validate_microdroid_extra_apk(&path)
                    .await
                    .map_err(|error| error.to_string())?;
                let upload = client
                    .upload_apk(&path)
                    .await
                    .map_err(|error| error.to_string())?;
                let microdroid = spec
                    .microdroid
                    .as_mut()
                    .ok_or_else(|| "Microdroid 配置缺失".to_owned())?;
                microdroid.extra_apks.push(hd_core::MicrodroidExtraApkV2 {
                    upload_id: upload.id,
                    sha256: upload.sha256,
                });
                spec.validate().map_err(|error| error.to_string())?;
                client
                    .update_instance(
                        id,
                        &UpdateInstanceRequestV2 {
                            expected_revision,
                            spec,
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            let _ = proxy.send_event(UserEvent::ExtraApkImported(result));
        });
    }

    fn on_extra_apk_imported(&mut self, result: Result<InstanceRecordV2, String>) {
        self.command_in_flight = false;
        match result {
            Ok(record) => {
                self.refresh_gate.invalidate();
                if self.selected_id == Some(record.spec.id) {
                    self.selected = Some(record);
                    self.capabilities = None;
                    self.capabilities_for = None;
                    self.resource_capability_gate.invalidate();
                    self.resource_capability = None;
                    self.resource_capability_for = None;
                    self.notice("额外 APK 已校验、上传并按顺序保存到当前实例".to_owned());
                    self.request_resource_admission();
                } else {
                    self.notice(format!(
                        "额外 APK 已保存到“{}”；当前选择已切换，未覆盖当前实例界面",
                        record.spec.name
                    ));
                }
                self.request_refresh();
            }
            Err(error) => self.notice(format!("添加额外 APK 失败：{error}")),
        }
        self.ui_dirty = true;
    }

    fn open_microdroid_shell(&mut self) {
        let Some(record) = self.selected.as_ref() else {
            self.notice("请先选择 Microdroid 实例".to_owned());
            return;
        };
        if record.spec.guest_kind != GuestKindV2::Microdroid {
            self.notice("Shell 操作仅用于 Microdroid 实例".to_owned());
            return;
        }
        if !record.adb_ready {
            self.notice("Microdroid ADB 尚未就绪".to_owned());
            return;
        }
        let Some(serial) = record.adb_serial.as_deref() else {
            self.notice("Microdroid 实例没有可用的 ADB 端点".to_owned());
            return;
        };
        let adb = record
            .spec
            .adb
            .executable
            .clone()
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|path| {
                        path.parent()
                            .map(|parent| parent.join(BUNDLED_ADB_FILENAME))
                    })
                    .filter(|path| path.is_file())
            })
            .unwrap_or_else(|| PathBuf::from("adb"));
        #[cfg(target_os = "macos")]
        {
            let command = format!(
                "{} -s {} shell",
                shell_quote(&adb.to_string_lossy()),
                shell_quote(serial)
            );
            let script = format!(
                "tell application \"Terminal\" to activate\ntell application \"Terminal\" to do script {}",
                apple_script_string(&command)
            );
            match std::process::Command::new("/usr/bin/osascript")
                .args(["-e", &script])
                .spawn()
            {
                Ok(_) => self.notice(format!("已打开 Microdroid Shell：{serial}")),
                Err(error) => self.notice(format!("无法打开 Terminal：{error}")),
            }
        }
        #[cfg(windows)]
        {
            let terminal = std::process::Command::new("wt.exe")
                .args(["new-tab", "--"])
                .arg(&adb)
                .args(["-s", serial, "shell"])
                .spawn();
            let result = terminal.or_else(|terminal_error| {
                std::process::Command::new("powershell.exe")
                    .args(["-NoLogo", "-NoExit", "-Command"])
                    .arg("& $args[0] -s $args[1] shell")
                    .arg(&adb)
                    .arg(serial)
                    .spawn()
                    .map_err(|powershell_error| {
                        std::io::Error::new(
                            powershell_error.kind(),
                            format!(
                                "Windows Terminal: {terminal_error}; PowerShell: {powershell_error}"
                            ),
                        )
                    })
            });
            match result {
                Ok(_) => self.notice(format!("已打开 Microdroid Shell：{serial}")),
                Err(error) => self.notice(format!("无法打开 Windows 终端：{error}")),
            }
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        self.notice(format!("请运行 adb -s {serial} shell"));
    }

    fn install_apk(&mut self, path: PathBuf) {
        if self.command_in_flight {
            return;
        }
        if !path.is_file() {
            self.notice(format!("APK 文件不存在：{}", path.display()));
            return;
        }
        let Some(id) = self.selected_id else { return };
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let upload = client.upload_apk(&path).await.map_err(|e| e.to_string())?;
                let operation = client
                    .create_operation(
                        id,
                        OperationKindV2::InstallApk {
                            upload_id: upload.id,
                            sha256: upload.sha256,
                        },
                        &format!("hd-web-ui-apk-{}", Uuid::new_v4()),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                client
                    .wait_operation(operation.id, Duration::from_mins(10))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok("APK 已安装".to_owned())
            }
            .await;
            let _ = proxy.send_event(UserEvent::CommandFinished {
                instance_id: id,
                result,
            });
        });
    }

    fn collect_diagnostics(&mut self) {
        if self.command_in_flight {
            return;
        }
        self.command_in_flight = true;
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        let request = DiagnosticRequestV2 {
            instance_id: self.selected_id,
            include_guest_logs: self.include_guest_logs,
        };
        self.runtime.spawn(async move {
            let result = client
                .collect_diagnostics(&request)
                .await
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::DiagnosticsFinished(result));
        });
    }

    fn collect_android_bugreport(&mut self) {
        if self.command_in_flight {
            return;
        }
        let Some(record) = self.selected.as_ref() else {
            self.notice("请先选择 Android 实例".to_owned());
            return;
        };
        if record.spec.guest_kind != GuestKindV2::Android {
            self.notice("Microdroid 不支持 Android bugreport".to_owned());
            return;
        }
        if record.status.observed != ObservedStateV2::Ready || !record.adb_ready {
            self.notice("Android bugreport 需要实例处于 Ready 且 ADB 可用".to_owned());
            return;
        }
        let instance_id = record.spec.id;
        self.command_in_flight = true;
        "正在生成完整 Android bugreport，可能需要数分钟…".clone_into(&mut self.status);
        self.ui_dirty = true;
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = client
                .collect_android_bugreport(instance_id)
                .await
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::AndroidBugreportFinished(result));
        });
    }

    fn request_network_setup_status(&mut self, force: bool) {
        if self.page != Page::Devices {
            if force {
                warn!(
                    event = "ui.network_status.hidden_refresh_rejected",
                    page = ?self.page,
                    "ignored network status refresh outside the Devices page"
                );
            }
            return;
        }
        if self.network_setup_refresh_in_flight || self.command_in_flight || self.closing {
            return;
        }
        if !force
            && !self
                .network_status_poll_mode()
                .refresh_due(self.last_network_setup_refresh.elapsed())
        {
            return;
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.last_network_setup_refresh = Instant::now();
        }
        #[cfg(target_os = "macos")]
        {
            self.network_setup_refresh_in_flight = true;
            self.last_network_setup_refresh = Instant::now();
            let generation = self.network_setup_generation;
            info!(
                event = "ui.network_status.probe.started",
                generation, force, "started visible Devices-page network status probe"
            );
            let proxy = self.proxy.clone();
            self.runtime.spawn(async move {
                let result = tokio::task::spawn_blocking(read_network_setup_status)
                    .await
                    .map_err(|error| format!("等待网络服务状态检查失败：{error}"))
                    .and_then(|result| result);
                let _ = proxy.send_event(UserEvent::NetworkSetupRefreshed { generation, result });
            });
        }
    }

    #[cfg(target_os = "macos")]
    fn on_network_setup_refreshed(
        &mut self,
        generation: u64,
        result: Result<NetworkSetupSnapshot, String>,
    ) {
        self.network_setup_refresh_in_flight = false;
        if generation != self.network_setup_generation || self.command_in_flight {
            if !self.command_in_flight && self.page == Page::Devices {
                self.last_network_setup_refresh = Instant::now()
                    .checked_sub(NETWORK_STATUS_REFRESH_INTERVAL)
                    .unwrap_or_else(Instant::now);
                self.request_network_setup_status(true);
            }
            return;
        }
        match result {
            Ok(state) => self.network_setup = state,
            Err(error) => {
                "unknown".clone_into(&mut self.network_setup.health);
                self.network_setup.detail = error;
            }
        }
        self.ui_dirty = true;
    }

    fn install_network_setup(&mut self) {
        if self.page != Page::Devices {
            warn!(
                event = "ui.network_status.hidden_install_rejected",
                page = ?self.page,
                "ignored network service installation outside the Devices page"
            );
            return;
        }
        if self.command_in_flight {
            return;
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.notice("网络兼容服务仅适用于 macOS".to_owned());
        }
        #[cfg(target_os = "macos")]
        {
            if resolve_network_setup_script().is_none() {
                self.notice("当前 HD 安装缺少网络兼容服务脚本，请重新安装应用".to_owned());
                return;
            }
            self.network_setup_generation = self.network_setup_generation.wrapping_add(1);
            self.command_in_flight = true;
            self.ui_dirty = true;
            let proxy = self.proxy.clone();
            self.runtime.spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let apple_script = hd_ui::macos_network::administrator_install_apple_script();
                    let output = std::process::Command::new("/usr/bin/osascript")
                        .args(["-e", &apple_script])
                        .output()
                        .map_err(|error| format!("启动管理员授权失败：{error}"))?;
                    if output.status.success() {
                        Ok(())
                    } else {
                        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                        Err(if detail.is_empty() {
                            "网络服务安装未完成".to_owned()
                        } else {
                            format!("网络服务安装未完成：{detail}")
                        })
                    }
                })
                .await
                .map_err(|error| format!("等待网络服务安装失败：{error}"))
                .and_then(|result| result);
                let _ = proxy.send_event(UserEvent::NetworkSetupInstalled(result));
            });
        }
    }

    #[cfg(target_os = "macos")]
    fn on_network_setup_installed(&mut self, result: Result<(), String>) {
        self.command_in_flight = false;
        match result {
            Ok(()) => self.notice("macOS 网络兼容服务已安装并启动".to_owned()),
            Err(error) => self.notice(error),
        }
        self.last_network_setup_refresh = Instant::now()
            .checked_sub(NETWORK_STATUS_REFRESH_INTERVAL)
            .unwrap_or_else(Instant::now);
        self.request_network_setup_status(true);
    }

    fn reveal_diagnostic_artifact(&mut self) {
        let Some(path) = self.diagnostic_artifact.as_ref() else {
            self.notice("请先生成诊断包".to_owned());
            return;
        };
        if !path.is_file() {
            self.diagnostic_artifact = None;
            self.notice("诊断包已被移动或按保留策略清理，请重新生成".to_owned());
            return;
        }
        #[cfg(target_os = "macos")]
        let result = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
        #[cfg(target_os = "windows")]
        let result = std::process::Command::new("explorer.exe")
            .arg(format!("/select,{}", path.display()))
            .spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let result = std::process::Command::new("xdg-open")
            .arg(path.parent().unwrap_or(path))
            .spawn();
        match result {
            Ok(_) => self.notice("已在文件管理器中定位诊断包".to_owned()),
            Err(error) => self.notice(format!("无法打开文件管理器：{error}")),
        }
    }

    fn reveal_android_bugreport_artifact(&mut self) {
        let Some((instance_id, path)) = self.android_bugreport_artifact.as_ref() else {
            self.notice("请先生成 Android bugreport".to_owned());
            return;
        };
        if Some(*instance_id) != self.selected_id {
            self.notice("当前实例尚未生成 Android bugreport".to_owned());
            return;
        }
        if !path.is_file() {
            self.android_bugreport_artifact = None;
            self.notice("Android bugreport 已被移动或按保留策略清理，请重新生成".to_owned());
            return;
        }
        #[cfg(target_os = "macos")]
        let result = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
        #[cfg(target_os = "windows")]
        let result = std::process::Command::new("explorer.exe")
            .arg(format!("/select,{}", path.display()))
            .spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let result = std::process::Command::new("xdg-open")
            .arg(path.parent().unwrap_or(path))
            .spawn();
        match result {
            Ok(_) => self.notice("已在文件管理器中定位 Android bugreport".to_owned()),
            Err(error) => self.notice(format!("无法打开文件管理器：{error}")),
        }
    }

    fn set_viewport_hidden(&mut self) {
        let _ = self.native_display.hide();
        // `hide` invalidates the last committed visible bounds. Force exactly one bounds commit
        // when the Player becomes visible again, even if its geometry did not otherwise change.
        self.last_native_bounds = None;
        if let Some(viewport) = self.viewport.as_mut()
            && viewport.visible
        {
            self.viewport_revision = self.viewport_revision.saturating_add(1);
            viewport.visible = false;
            viewport.revision = self.viewport_revision;
        }
    }

    fn set_viewport_locally_hidden(&mut self) {
        let _ = self.native_display.hide();
        // Preserve the versioned session viewport and the Worker-owned sibling visibility. Only
        // this parent is outside DWM composition; a same-geometry restore can expose the retained
        // Vulkan frame synchronously without a Host/Worker transaction.
        self.last_native_bounds = None;
    }

    fn suspend_display_for_minimize(&mut self) {
        self.set_viewport_locally_hidden();
        self.sync_display();
    }

    fn suspend_display_for_native_lifecycle(&mut self) {
        if self.native_suspended {
            return;
        }
        self.native_suspended = true;
        self.set_viewport_hidden();
        self.release_display();
        self.last_session_viewport = None;
        info!(
            event = "ui.lifecycle.suspended",
            "released the Android display session for native suspension"
        );
    }

    fn resume_display_after_native_lifecycle(&mut self) -> bool {
        if !self.native_suspended {
            return false;
        }
        self.native_suspended = false;
        self.last_session_viewport = None;
        self.last_display_attempt = Instant::now()
            .checked_sub(DISPLAY_RETRY)
            .unwrap_or_else(Instant::now);
        self.ui_dirty = true;
        info!(
            event = "ui.lifecycle.resumed",
            "scheduled Android display reattachment after native resume"
        );
        true
    }

    fn sync_display(&mut self) {
        if self.closing || self.display_in_flight {
            return;
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|record| record.spec.guest_kind == GuestKindV2::Microdroid)
        {
            self.release_display();
            return;
        }
        let Some(id) = self.selected_id else { return };
        let Some(viewport) = self.viewport.clone() else {
            return;
        };
        if !viewport.visible && self.session.is_none() {
            return;
        }
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        if let Some(session) = self.session.clone() {
            if session.instance_id != id || session.display_id != self.selected_display_id() {
                self.release_display();
                return;
            }
            if self.last_session_viewport.as_ref() == Some(&viewport)
                && self.last_session_touch.elapsed() < DISPLAY_HEARTBEAT
            {
                return;
            }
            self.display_in_flight = true;
            self.display_request_epoch = self.display_request_epoch.wrapping_add(1);
            let request_epoch = self.display_request_epoch;
            let completed_viewport = viewport.clone();
            self.runtime.spawn(async move {
                let result = client
                    .update_display_session(id, session.session_token, viewport)
                    .await
                    .map_err(|error| error.to_string());
                let _ = proxy.send_event(UserEvent::DisplayFinished {
                    request_epoch,
                    instance_id: id,
                    viewport: completed_viewport,
                    result,
                });
            });
        } else if viewport.visible
            && self.last_native_bounds.is_some()
            && self.last_display_attempt.elapsed() >= DISPLAY_RETRY
        {
            // A retained visible viewport is also used to keep an existing session alive while
            // HD locally covers or minimizes Player. Do not turn that retained state into a new
            // hidden-page attachment; acquisition begins only after a valid native layout has
            // been committed for the visible Player.
            self.last_display_attempt = Instant::now();
            self.display_in_flight = true;
            self.display_request_epoch = self.display_request_epoch.wrapping_add(1);
            let request_epoch = self.display_request_epoch;
            self.notice("正在附加 Android 原生画面…".to_owned());
            let target = self.display_target.clone();
            let display_id = self.selected_display_id();
            let completed_viewport = viewport.clone();
            self.runtime.spawn(async move {
                let result = client
                    .acquire_display_session(id, display_id, target, viewport)
                    .await
                    .map_err(|error| error.to_string());
                let _ = proxy.send_event(UserEvent::DisplayFinished {
                    request_epoch,
                    instance_id: id,
                    viewport: completed_viewport,
                    result,
                });
            });
        }
    }

    fn on_display_finished(
        &mut self,
        request_epoch: u64,
        instance_id: Uuid,
        completed_viewport: DisplayViewportV2,
        result: Result<DisplaySessionV2, String>,
    ) {
        if request_epoch != self.display_request_epoch {
            // Release or target replacement invalidates an already dispatched Host request. A
            // late success must never reinstall that session into the current Player state.
            if let Ok(session) = result {
                self.release_returned_display_session(session);
            }
            return;
        }
        self.display_in_flight = false;
        if self.selected_id != Some(instance_id) {
            if let Ok(session) = result {
                self.release_returned_display_session(session);
            }
            return;
        }
        match result {
            Ok(session) => {
                self.session = Some(session);
                let visible = completed_viewport.visible;
                self.last_session_viewport = Some(completed_viewport);
                self.last_session_touch = Instant::now();
                if visible {
                    self.notice("Android 原生画面已附加".to_owned());
                }
                let inactive = self
                    .selected
                    .as_ref()
                    .is_some_and(|record| !record.status.observed.is_active());
                if self.auto_start_selected && inactive {
                    self.auto_start_selected = false;
                    self.operation("start", None);
                }
            }
            Err(error) => {
                self.session = None;
                self.last_session_viewport = None;
                self.notice(format!("显示附加失败，正在重试：{error}"));
            }
        }
    }

    fn release_display(&mut self) {
        self.display_request_epoch = self.display_request_epoch.wrapping_add(1);
        self.display_in_flight = false;
        self.last_session_viewport = None;
        let Some(session) = self.session.take() else {
            return;
        };
        let client = self.client.clone();
        self.runtime.spawn(async move {
            if let Err(error) = client
                .release_display_session(session.instance_id, session.session_token)
                .await
            {
                warn!(%error, "release HD display session failed");
            }
        });
    }

    fn release_returned_display_session(&self, session: DisplaySessionV2) {
        let client = self.client.clone();
        self.runtime.spawn(async move {
            if let Err(error) = client
                .release_display_session(session.instance_id, session.session_token)
                .await
            {
                debug!(%error, "release stale HD display session failed");
            }
        });
    }

    fn begin_close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.command_in_flight = true;
        self.display_request_epoch = self.display_request_epoch.wrapping_add(1);
        self.display_in_flight = false;
        let _ = self.native_display.hide();
        self.notice("正在关闭 HD；Android 实例保持运行…".to_owned());
        let session = self.session.take();
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            if let Some(session) = session {
                match tokio::time::timeout(
                    DISPLAY_RELEASE_ON_CLOSE_TIMEOUT,
                    client.release_display_session(session.instance_id, session.session_token),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => warn!(
                        event = "ui.lifecycle.display_release.failed",
                        %error,
                        "display session release failed while closing; the Host lease will reclaim it"
                    ),
                    Err(_) => warn!(
                        event = "ui.lifecycle.display_release.timed_out",
                        timeout_ms = DISPLAY_RELEASE_ON_CLOSE_TIMEOUT.as_millis(),
                        "display session release timed out while closing; the Host lease will reclaim it"
                    ),
                }
            }
            let _ = proxy.send_event(UserEvent::ShutdownFinished(Ok(())));
        });
    }

    fn notice(&mut self, message: String) {
        self.status = message;
        self.ui_dirty = true;
    }
}

fn physical_dimension(value: f64, maximum: u32) -> u32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        value.round().clamp(1.0, f64::from(maximum.max(1))) as u32
    }
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(target_os = "macos")]
fn apple_script_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "macos")]
fn resolve_network_setup_script() -> Option<PathBuf> {
    let packaged = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent()?.parent().map(Path::to_owned))
        .map(|contents| contents.join("Resources/scripts/macos-network-setup.sh"));
    let development = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(|root| root.join("scripts/macos-network-setup.sh"));
    packaged
        .into_iter()
        .chain(development)
        .find(|path| hd_ui::macos_network::script_matches_embedded(path))
}

#[cfg(target_os = "macos")]
fn read_network_setup_status() -> Result<NetworkSetupSnapshot, String> {
    let script = resolve_network_setup_script()
        .ok_or_else(|| "当前 HD 安装缺少网络兼容服务脚本".to_owned())?;
    let output = hd_ui::macos_network::run_status_script(
        &script,
        hd_ui::macos_network::NETWORK_STATUS_TIMEOUT,
    )?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            "网络兼容服务状态检查失败".to_owned()
        } else {
            format!("网络兼容服务状态检查失败：{detail}")
        });
    }
    let text =
        String::from_utf8(output.stdout).map_err(|_| "网络兼容服务返回了无效 UTF-8".to_owned())?;
    let fields = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect::<BTreeMap<_, _>>();
    if fields.get("schema_version").map(String::as_str) != Some("2") {
        return Err("网络兼容服务状态版本不受支持，请升级 HD".to_owned());
    }
    let required = |key: &str| {
        fields
            .get(key)
            .cloned()
            .ok_or_else(|| format!("网络兼容服务状态缺少 {key}"))
    };
    let boolean = |key: &str| -> Result<bool, String> {
        match required(key)?.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!("网络兼容服务状态 {key} 无效")),
        }
    };
    Ok(NetworkSetupSnapshot {
        supported: true,
        health: required("health")?,
        service_action: required("service_action")?,
        network_usable: boolean("network_usable")?,
        installed: boolean("installed")?,
        package_match: boolean("package_match")?,
        loaded: boolean("loaded")?,
        pf_configured: boolean("pf_configured")?,
        egress: required("egress")?,
        vpn_nat_required: boolean("vpn_nat_required")?,
        socket_vmnet: boolean("socket_vmnet")?,
        nat: required("nat")?,
        unsafe_paths: boolean("unsafe")?,
        detail: required("detail")?,
    })
}

fn resolve_web_root(explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit
        .filter(|path| path.join("index.html").is_file())
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(Path::to_owned))
                .map(|path| path.join("ui"))
                .filter(|path| path.join("index.html").is_file())
        })
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent()?.parent().map(Path::to_owned))
                .map(|contents| contents.join("Resources/ui"))
                .filter(|path| path.join("index.html").is_file())
        })
        .or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .map(|root| root.join("web/dist"))
                .filter(|path| path.join("index.html").is_file())
        })
}

fn configure_fast_dev_artifacts(root: &Path) -> Option<String> {
    let direct = hd_ui::ui_contract::resolve_direct_android_artifact_root(
        root,
        hd_platform::architecture_name(),
    )?;
    let sparse_rootfs = direct.join("aggregate_android.sparse.img");
    let raw_rootfs = direct.join("aggregate_android.img");
    let rootfs = if sparse_rootfs.is_file() {
        sparse_rootfs
    } else {
        raw_rootfs
    };
    let host_root = direct_host_tools_root()?;
    let sensor = host_root.join("hd-sensor-injector");
    let packaged_gfxstream = host_root.join("libgfxstream_backend.dylib");
    let packaged_adb = host_root.join(hd_platform::executable_name("adb"));
    let packaged_aapt2 = host_root.join(hd_platform::executable_name("aapt2"));
    let adb = std::env::var_os("HD_DEV_ADB")
        .map(PathBuf::from)
        .or_else(|| packaged_adb.is_file().then_some(packaged_adb))
        .or_else(default_adb_path)?;
    let aapt2 = std::env::var_os("HD_DEV_AAPT2")
        .map(PathBuf::from)
        .or_else(|| packaged_aapt2.is_file().then_some(packaged_aapt2))
        .or_else(default_aapt2_path);
    let microdroid = resolve_microdroid_dev_artifact_root(root);
    // SAFETY: initialization runs before Tokio, Host, Worker, or any other process is started.
    unsafe {
        if std::env::var_os("HD_DEV_FAST_ARTIFACTS").is_none() {
            std::env::set_var("HD_DEV_FAST_ARTIFACTS", "1");
        }
        if std::env::var_os("HD_DEV_GUEST_BUNDLE_ROOT").is_none() {
            std::env::set_var("HD_DEV_GUEST_BUNDLE_ROOT", &direct);
        }
        if std::env::var_os("HD_DEV_GUEST_ROOTFS").is_none() {
            std::env::set_var("HD_DEV_GUEST_ROOTFS", &rootfs);
        }
        if std::env::var_os("HD_DEV_HOST_TOOLS_ROOT").is_none() {
            std::env::set_var("HD_DEV_HOST_TOOLS_ROOT", &host_root);
        }
        if sensor.is_file() && std::env::var_os("HD_DEV_SENSOR_INJECTOR").is_none() {
            std::env::set_var("HD_DEV_SENSOR_INJECTOR", &sensor);
        }
        if packaged_gfxstream.is_file() && std::env::var_os("HD_DEV_GFXSTREAM_BACKEND").is_none() {
            std::env::set_var("HD_DEV_GFXSTREAM_BACKEND", &packaged_gfxstream);
        }
        if std::env::var_os("HD_DEV_ADB").is_none() {
            std::env::set_var("HD_DEV_ADB", &adb);
        }
        if let Some(aapt2) = aapt2.as_ref()
            && std::env::var_os("HD_DEV_AAPT2").is_none()
        {
            std::env::set_var("HD_DEV_AAPT2", aapt2);
        }
        if let Some(microdroid) = microdroid.as_ref() {
            if std::env::var_os("HD_MICRODROID_ARTIFACTS_ROOT").is_none() {
                std::env::set_var("HD_MICRODROID_ARTIFACTS_ROOT", microdroid);
            }
            if std::env::var_os("HD_MICRODROID_DEV_BYPASS").is_none() {
                std::env::set_var("HD_MICRODROID_DEV_BYPASS", "1");
            }
        }
    }
    info!(path = %direct.display(), rootfs = %rootfs.display(), "configured Android development artifacts");
    Some(format!("快速开发镜像：{}", rootfs.display()))
}

fn resolve_microdroid_dev_artifact_root(root: &Path) -> Option<PathBuf> {
    let product = if hd_platform::architecture_name() == "x86_64" {
        "vsoc_x86_64"
    } else {
        "vsoc_arm64_only"
    };
    [
        root.join("products/microdroid").join(product),
        root.join("products/microdroid/vsoc_arm64"),
        root.join("products/microdroid"),
        root.join("microdroid").join(product),
        root.join(product),
        root.to_path_buf(),
    ]
    .into_iter()
    .find(|candidate| {
        candidate
            .join("apex_dir/apex/com.android.virt/etc/microdroid.json")
            .is_file()
    })
}

fn bundled_development_android_root() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    if macos.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    let android = contents.join("Resources/products/android");
    let marker = android.join("development-direct-v1.plist");
    let marker_metadata = std::fs::symlink_metadata(&marker).ok()?;
    if !marker_metadata.file_type().is_file() || marker_metadata.file_type().is_symlink() {
        return None;
    }
    let marker_contents = std::fs::read_to_string(&marker).ok()?;
    if !marker_contents.contains("<key>channel</key><string>development</string>")
        || !marker_contents
            .contains("<key>data_profile</key><string>development-unencrypted</string>")
    {
        return None;
    }
    let product = android.join("vsoc_arm64_only");
    let direct = product.join("direct-linux");
    let required = [
        direct.join("kernel"),
        direct.join("initrd_android.img"),
        direct.join("aggregate_android.img"),
        direct.join("android_fstab.dt"),
        direct.join("runtime-files-v1.sha256"),
    ];
    required
        .into_iter()
        .all(|path| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
        })
        .then_some(product)
}

fn bundled_signed_android_selection()
-> Result<Option<(ArtifactSelectionV2, String, Option<PathBuf>)>> {
    let Some(executable) = std::env::current_exe().ok() else {
        return Ok(None);
    };
    let Some(macos) = executable.parent() else {
        return Ok(None);
    };
    let Some(contents) = macos.parent() else {
        return Ok(None);
    };
    if macos.file_name().and_then(|value| value.to_str()) != Some("MacOS")
        || contents.file_name().and_then(|value| value.to_str()) != Some("Contents")
    {
        return Ok(None);
    }
    let store = contents.join("Resources/products/android/artifact-store-v2");
    if !store
        .join(hd_core::PACKAGED_ANDROID_ARTIFACT_INDEX_FILE)
        .is_file()
    {
        return Ok(None);
    }
    let (index, selection) = hd_runtime::load_packaged_android_artifact_selection(&store)
        .context("validate bundled Android artifact-store index")?;
    let channel = match index.channel {
        hd_core::PackagedArtifactChannelV2::Development => "development",
        hd_core::PackagedArtifactChannelV2::Release => "release",
    };
    let development_trust = (index.channel == hd_core::PackagedArtifactChannelV2::Development)
        .then(|| store.join("trusted-keys-v2.json"));
    Ok(Some((
        selection,
        format!("签名 Android {}：{}", channel, index.guest_bundle_digest),
        development_trust,
    )))
}

fn install_bundled_development_trust(paths: &DataPaths, source: &Path) -> Result<()> {
    hd_runtime::ArtifactTrustStore::load(source)
        .context("verify bundled development Android trust store")?;
    let source_bytes = hd_platform::read_regular_nofollow_limited(source, 4 * 1024 * 1024)
        .context("read bundled development Android trust store")?;
    let destination = paths.root.join("trusted-keys-v2.json");
    if destination.exists() {
        let installed = hd_platform::read_regular_nofollow_limited(&destination, 4 * 1024 * 1024)
            .context("read installed Android trust store")?;
        anyhow::ensure!(
            installed == source_bytes,
            "已安装的制品信任根与签名 development 包不一致；请使用隔离数据目录"
        );
        return Ok(());
    }
    hd_platform::write_owner_only(&destination, &source_bytes)
        .context("install bundled development Android trust store")?;
    Ok(())
}

fn default_artifact_selection() -> Option<ArtifactSelectionV2> {
    if let Some(selection) = DIRECT_DEV_SELECTION.get() {
        return Some(selection.clone());
    }
    if let Some(selection) = BUNDLED_SIGNED_SELECTION.get() {
        return Some(selection.clone());
    }
    let store_root = PathBuf::from(DEFAULT_ARTIFACT_STORE);
    let bundles = store_root.join("bundles");
    (bundles.join(DEFAULT_GUEST_DIGEST).is_dir() && bundles.join(DEFAULT_HOST_DIGEST).is_dir())
        .then(|| ArtifactSelectionV2 {
            store_root,
            guest_bundle_digest: DEFAULT_GUEST_DIGEST.to_owned(),
            host_bundle_digest: DEFAULT_HOST_DIGEST.to_owned(),
        })
}

fn direct_host_tools_root() -> Option<PathBuf> {
    let executable = hd_platform::executable_name("crosvm");
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_owned))
        .filter(|path| path.join(&executable).is_file())
        .or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(3)
                .map(|root| {
                    root.join("out/dist")
                        .join(hd_platform::platform_name())
                        .join("bin")
                })
                .filter(|path| path.join(&executable).is_file())
        })
}

fn default_adb_path() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return std::env::var_os("ANDROID_HOME")
            .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
            .map(PathBuf::from)
            .map(|root| root.join("platform-tools/adb"))
            .filter(|path| path.is_file())
            .or_else(|| {
                let path = PathBuf::from("/opt/homebrew/bin/adb");
                path.is_file().then_some(path)
            })
            .and_then(|path| path.canonicalize().ok())
            .filter(|path| path.is_file());
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("Android/Sdk/platform-tools/adb.exe"))
        .filter(|path| path.is_file())
}

fn default_aapt2_path() -> Option<PathBuf> {
    let sdk = if cfg!(target_os = "macos") {
        std::env::var_os("ANDROID_HOME").or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
    } else {
        std::env::var_os("LOCALAPPDATA")
    };
    let root = sdk.map(PathBuf::from)?.join(if cfg!(target_os = "macos") {
        "build-tools"
    } else {
        "Android/Sdk/build-tools"
    });
    let mut versions = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .collect::<Vec<_>>();
    versions.sort_by_key(|entry| std::cmp::Reverse(entry.file_name()));
    versions
        .into_iter()
        .map(|entry| {
            entry.path().join(if cfg!(target_os = "macos") {
                "aapt2"
            } else {
                "aapt2.exe"
            })
        })
        .find(|path| path.is_file())
}

fn init_logging(paths: &DataPaths) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let appender = tracing_appender::rolling::daily(&paths.logs, "ui-web.jsonl");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(writer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize HD WebView UI logging: {error}"))?;
    Ok(guard)
}
