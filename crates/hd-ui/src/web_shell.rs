#![allow(unsafe_code)]

use std::borrow::Cow;
use std::collections::BTreeSet;
#[cfg(target_os = "macos")]
use std::ffi::{CStr, c_char, c_void};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::Parser;
use hd_core::{
    ArtifactSelectionV2, CreateInstanceRequestV2, DiagnosticRequestV2, DisplaySessionV2,
    DisplayViewportV2, HostCapabilitiesV2, InstanceActionV2, InstanceRecordV2, InstanceSpecV2,
    InstanceSummaryV2, KeyActionV2, NativeDisplayTargetV2, OperationKindV2, OrientationV2,
    StopModeV2, UpdateInstanceRequestV2,
};
use hd_platform::{
    DataPaths, NativeDisplayBounds, NativeDisplayHost, choose_apk_file, create_native_display_host,
    current_process_identity,
};
#[cfg(target_os = "macos")]
use hd_platform::{install_macos_titlebar_controls, set_macos_window_content_aspect_ratio};
use hd_runtime::HostClientV2;
use hd_ui::ui_contract::{Page, SurfaceLayout, oriented_guest_dimensions};
use raw_window_handle::HasWindowHandle as _;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;
use winit::dpi::LogicalSize;
use winit::event::{Event, StartCause, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};
use winit::window::Window;
use wry::dpi::{PhysicalPosition as WebPosition, PhysicalSize as WebSize};
use wry::http::{Request, Response, header::CONTENT_TYPE};
use wry::{Rect, WebView, WebViewBuilder};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const DISPLAY_RETRY: Duration = Duration::from_millis(500);
const DISPLAY_HEARTBEAT: Duration = Duration::from_secs(5);
const DEFAULT_DEV_ARTIFACTS: &str =
    r"C:\workspace\bscp\bscp-vm-artifacts-20260721-aosp-all-targets";
const FALLBACK_DEV_ARTIFACTS: &str =
    r"D:\bscp-vm-artifacts\bscp-vm-artifacts-20260721-aosp-all-targets";
const DEFAULT_ARTIFACT_STORE: &str = r"D:\hd-v2-artifact-store";
const DEFAULT_GUEST_DIGEST: &str =
    "22281c84556c6e865e4f94498968efc2f651ef4c37bd3e871d930414609b2986";
const DEFAULT_HOST_DIGEST: &str =
    "5187de8d05cf29fdbed127c72ab0a96b50fa37f800137079488237208ae1aa7e";
static DIRECT_DEV_SELECTION: OnceLock<ArtifactSelectionV2> = OnceLock::new();

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
    Tick,
    Ipc(String),
    Refreshed(Result<(Vec<InstanceSummaryV2>, Option<InstanceRecordV2>), String>),
    CapabilitiesRefreshed {
        instance_id: Uuid,
        result: Result<HostCapabilitiesV2, String>,
    },
    Created(Result<InstanceRecordV2, String>),
    Saved {
        result: Result<InstanceRecordV2, String>,
        restarted: bool,
    },
    CommandFinished(Result<String, String>),
    DisplayFinished {
        instance_id: Uuid,
        viewport: DisplayViewportV2,
        result: Result<DisplaySessionV2, String>,
    },
    ShutdownFinished(Result<(), String>),
}

struct WebSurfaces {
    top: WebView,
    sidebar: WebView,
    content: WebView,
    last_layout: Option<SurfaceLayout>,
}

impl WebSurfaces {
    fn new(window: &Window, web_root: &Path, proxy: &EventLoopProxy<UserEvent>) -> Result<Self> {
        let tiny = web_rect(0, 0, 1, 1);
        Ok(Self {
            top: build_webview(window, web_root, "top", tiny, proxy)?,
            sidebar: build_webview(window, web_root, "sidebar", tiny, proxy)?,
            content: build_webview(window, web_root, "content", tiny, proxy)?,
            last_layout: None,
        })
    }

    fn apply_layout(&mut self, layout: SurfaceLayout) -> Result<()> {
        if self.last_layout == Some(layout) {
            return Ok(());
        }
        self.top.set_bounds(web_rect(
            0,
            0,
            layout.width.max(1),
            layout.top_height.max(1),
        ))?;
        self.top
            .set_visible(layout.width > 0 && layout.top_height > 0)?;

        let sidebar_visible =
            !layout.sidebar_collapsed && layout.sidebar_width > 0 && layout.body_height() > 0;
        self.sidebar.set_bounds(if sidebar_visible {
            web_rect(
                0,
                layout.body_y(),
                layout.sidebar_width,
                layout.body_height(),
            )
        } else {
            web_rect(0, 0, 1, 1)
        })?;
        self.sidebar.set_visible(sidebar_visible)?;

        let content_visible =
            layout.page != Page::Player && layout.content_width() > 0 && layout.body_height() > 0;
        self.content.set_bounds(if content_visible {
            web_rect(
                layout.content_x(),
                layout.body_y(),
                layout.content_width(),
                layout.body_height(),
            )
        } else {
            web_rect(0, 0, 1, 1)
        })?;
        self.content.set_visible(content_visible)?;
        self.last_layout = Some(layout);
        Ok(())
    }

    fn broadcast(&self, message: &Value) {
        let script = format!("window.__hdReceive?.({message});");
        for surface in [&self.top, &self.sidebar, &self.content] {
            if let Err(error) = surface.evaluate_script(&script) {
                warn!(%error, "send state to HD WebView surface failed");
            }
        }
    }
}

struct ShellState {
    runtime: Runtime,
    client: HostClientV2,
    proxy: EventLoopProxy<UserEvent>,
    native_display: Box<dyn NativeDisplayHost>,
    display_target: NativeDisplayTargetV2,
    summaries: Vec<InstanceSummaryV2>,
    selected_id: Option<Uuid>,
    selected: Option<InstanceRecordV2>,
    capabilities: Option<HostCapabilitiesV2>,
    capabilities_for: Option<Uuid>,
    status: String,
    artifact_hint: Option<String>,
    viewport: Option<DisplayViewportV2>,
    session: Option<DisplaySessionV2>,
    last_session_viewport: Option<DisplayViewportV2>,
    viewport_revision: u64,
    last_refresh: Instant,
    last_display_attempt: Instant,
    last_session_touch: Instant,
    refresh_in_flight: bool,
    capabilities_in_flight: bool,
    command_in_flight: bool,
    display_in_flight: bool,
    reset_display_after_command: bool,
    root_was_minimized: bool,
    auto_start_selected: bool,
    closing: bool,
    display_maximized: bool,
    sidebar_collapsed: bool,
    window_aspect: Option<(u32, u32)>,
    android_focused: bool,
    sidebar_visible: bool,
    page: Page,
    apk_path: Option<PathBuf>,
    include_guest_logs: bool,
    ui_dirty: bool,
    ready_surfaces: BTreeSet<String>,
}

#[allow(deprecated, clippy::too_many_lines)]
pub(crate) fn run() -> Result<()> {
    let cli = Cli::parse();
    let dev_artifacts = cli.dev_artifacts_root.unwrap_or_else(|| {
        [DEFAULT_DEV_ARTIFACTS, FALLBACK_DEV_ARTIFACTS]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_dir())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DEV_ARTIFACTS))
    });
    let artifact_hint = configure_fast_dev_artifacts(&dev_artifacts);
    let paths = cli
        .data_root
        .map_or_else(DataPaths::discover, |root| Ok(DataPaths::from_root(root)))
        .context("discover HD data directory")?;
    paths.ensure().context("create HD data directories")?;
    if artifact_hint.is_some() {
        let selection = hd_runtime::prepare_direct_dev_artifact_store(&paths)
            .context("prepare direct Android development artifacts")?;
        let _ = DIRECT_DEV_SELECTION.set(selection);
    }
    let web_root = resolve_web_root(cli.web_root).context("locate HD WebView assets")?;
    let _log_guard = init_logging(&paths)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hd-web-ui-runtime")
        .build()
        .context("create HD UI runtime")?;
    let client = runtime
        .block_on(HostClientV2::connect_or_start(paths))
        .context("connect to HD Host")?;

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("create HD event loop")?;
    let proxy = event_loop.create_proxy();
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
        event_loop
            .create_window(window_attributes)
            .context("create HD root window")?,
    );
    install_native_titlebar_controls(window.as_ref(), &proxy)
        .context("install HD native titlebar controls")?;
    let raw_parent = window
        .window_handle()
        .context("read HD root window handle")?
        .as_raw();
    let native_display =
        create_native_display_host(raw_parent).context("create HD NativeDisplayHost")?;
    let identity = current_process_identity(Uuid::new_v4())
        .map_err(|error| anyhow::anyhow!("identify HD UI process: {error}"))?;
    let display_target = native_display.target(identity);
    let mut webviews = WebSurfaces::new(window.as_ref(), &web_root, &proxy)?;
    let ticker_proxy = proxy.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if ticker_proxy.send_event(UserEvent::Tick).is_err() {
                break;
            }
        }
    });

    let now = Instant::now();
    let mut shell = ShellState {
        runtime,
        client,
        proxy,
        native_display,
        display_target,
        summaries: Vec::new(),
        selected_id: None,
        selected: None,
        capabilities: None,
        capabilities_for: None,
        status: "正在连接 HD Host…".to_owned(),
        artifact_hint,
        viewport: None,
        session: None,
        last_session_viewport: None,
        viewport_revision: 0,
        last_refresh: now.checked_sub(POLL_INTERVAL).unwrap_or(now),
        last_display_attempt: now.checked_sub(DISPLAY_RETRY).unwrap_or(now),
        last_session_touch: now,
        refresh_in_flight: false,
        capabilities_in_flight: false,
        command_in_flight: false,
        display_in_flight: false,
        reset_display_after_command: false,
        root_was_minimized: false,
        auto_start_selected: true,
        closing: false,
        display_maximized: false,
        sidebar_collapsed: true,
        window_aspect: None,
        android_focused: false,
        sidebar_visible: false,
        page: Page::Player,
        apk_path: None,
        include_guest_logs: true,
        ui_dirty: true,
        ready_surfaces: BTreeSet::new(),
    };
    shell.request_refresh();
    apply_window_layout(&window, &mut webviews, &mut shell);

    event_loop.run(move |event, active| {
        if matches!(&event, Event::NewEvents(StartCause::Init)) {
            active.set_control_flow(ControlFlow::Wait);
        }
        match event {
            Event::WindowEvent { window_id, event } if window_id == window.id() => match event {
                WindowEvent::CloseRequested => shell.begin_close(),
                WindowEvent::DroppedFile(path) => {
                    if path.extension().is_some_and(|extension| {
                        extension.to_string_lossy().eq_ignore_ascii_case("apk")
                    }) {
                        shell.apk_path = Some(path.clone());
                        shell.notice(format!("已选择 APK：{}", path.display()));
                    }
                }
                WindowEvent::Resized(size) => {
                    if size.width == 0 || size.height == 0 {
                        shell.suspend_display_for_minimize();
                    } else {
                        shell.root_was_minimized = false;
                        apply_window_layout(&window, &mut webviews, &mut shell);
                    }
                }
                WindowEvent::ScaleFactorChanged { .. } => {
                    apply_window_layout(&window, &mut webviews, &mut shell);
                }
                WindowEvent::Focused(false) if !shell.sidebar_collapsed => {
                    shell.sidebar_collapsed = true;
                    shell.ui_dirty = true;
                }
                _ => {}
            },
            Event::UserEvent(UserEvent::Ipc(message)) => {
                handle_ipc(&message, &window, &mut shell);
                apply_window_layout(&window, &mut webviews, &mut shell);
            }
            Event::UserEvent(event) => {
                match event {
                    UserEvent::Tick => {
                        shell.poll();
                        shell.flush_web_state(&webviews);
                        return;
                    }
                    UserEvent::Refreshed(result) => shell.on_refresh(result),
                    UserEvent::CapabilitiesRefreshed {
                        instance_id,
                        result,
                    } => shell.on_capabilities_refreshed(instance_id, result),
                    UserEvent::Created(result) => shell.on_created(result),
                    UserEvent::Saved { result, restarted } => {
                        shell.on_saved(result, restarted);
                    }
                    UserEvent::CommandFinished(result) => shell.on_command_finished(result),
                    UserEvent::DisplayFinished {
                        instance_id,
                        viewport,
                        result,
                    } => shell.on_display_finished(instance_id, viewport, result),
                    UserEvent::ShutdownFinished(Ok(())) => active.exit(),
                    UserEvent::ShutdownFinished(Err(error)) => {
                        shell.closing = false;
                        shell.command_in_flight = false;
                        shell.notice(format!("关闭 HD 失败：{error}"));
                    }
                    UserEvent::Ipc(_) => unreachable!(),
                }
                apply_window_layout(&window, &mut webviews, &mut shell);
            }
            _ => {}
        }
    })?;
    Ok(())
}

fn build_webview(
    window: &Window,
    web_root: &Path,
    surface: &str,
    bounds: Rect,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<WebView> {
    let url = format!("hd-ui://localhost/index.html?surface={surface}");
    let proxy = proxy.clone();
    let assets = web_root.to_owned();
    WebViewBuilder::new()
        .with_custom_protocol("hd-ui".to_owned(), move |_id, request| {
            web_asset_response(&assets, &request)
        })
        .with_url(url)
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

fn apply_window_layout(window: &Window, webviews: &mut WebSurfaces, shell: &mut ShellState) {
    shell.sync_window_aspect(window);
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
    if let Err(error) = webviews.apply_layout(layout) {
        shell.notice(format!("WebView 布局失败：{error}"));
    }
    shell.update_display_layout(layout, window.scale_factor());
}

#[cfg(target_os = "macos")]
fn install_native_titlebar_controls(
    window: &Window,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<()> {
    let handle = window
        .window_handle()
        .context("read macOS window handle for native titlebar controls")?
        .as_raw();
    let context = Box::into_raw(Box::new(proxy.clone())).cast::<c_void>();
    install_macos_titlebar_controls(handle, context, native_titlebar_callback)
        .context("install macOS native titlebar controls")
}

#[cfg(target_os = "macos")]
extern "C" fn native_titlebar_callback(context: *mut c_void, message: *const c_char) {
    if context.is_null() || message.is_null() {
        return;
    }
    // SAFETY: context is a leaked Box<EventLoopProxy<UserEvent>> for the UI process lifetime, and
    // AppKit supplies a NUL-terminated JSON command from the native button table.
    let (proxy, message) = unsafe {
        (
            &*context.cast::<EventLoopProxy<UserEvent>>(),
            CStr::from_ptr(message).to_string_lossy().into_owned(),
        )
    };
    let _ = proxy.send_event(UserEvent::Ipc(message));
}

#[cfg(not(target_os = "macos"))]
fn install_native_titlebar_controls(
    _window: &Window,
    _proxy: &EventLoopProxy<UserEvent>,
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
                shell.ready_surfaces.insert(surface.to_owned());
            }
            shell.ui_dirty = true;
            if shell.ready_surfaces.contains("top")
                && (shell.sidebar_collapsed || shell.ready_surfaces.contains("sidebar"))
                && (shell.page == Page::Player || shell.ready_surfaces.contains("content"))
            {
                // Keep the root hidden until both surfaces visible on the initial Player page have
                // painted their first document. The content WebView is parked and hidden on this
                // page; WebView2 is allowed to defer its navigation while hidden, so it must not
                // block the shell from appearing.
                window.set_visible(true);
                window.request_redraw();
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
            shell.create(name);
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
            ),
            Err(error) => shell.notice(format!("设置格式无效：{error}")),
        },
        "operation" => {
            if let Some(kind) = value.get("kind").and_then(Value::as_str) {
                shell.operation(kind);
            }
        }
        "key" => {
            if let Some(key) = value.get("key").and_then(Value::as_str) {
                shell.key(key);
            }
        }
        "rotate" => shell.rotate(),
        "screenshot" => shell.screenshot(),
        "install_apk" => {
            if let Some(path) = value
                .get("path")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
            {
                shell.apk_path = Some(PathBuf::from(path));
            }
            shell.install_selected_apk();
        }
        "choose_apk" => shell.choose_apk(false),
        "choose_install_apk" => shell.choose_apk(true),
        "set_apk_path" => {
            shell.apk_path = value
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from);
            shell.ui_dirty = true;
        }
        "focus_display" => {
            let _ = shell.native_display.focus_guest();
        }
        "diagnostics" => {
            shell.include_guest_logs = value
                .get("include_guest_logs")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            shell.collect_diagnostics();
        }
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
                window.set_maximized(!window.is_maximized());
                shell.ui_dirty = true;
            }
            Some("maximize_display" | "fullscreen") => {
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
    fn navigate(&mut self, page: Page) {
        if self.page == page {
            self.sidebar_collapsed = true;
            self.ui_dirty = true;
            return;
        }
        if self.page == Page::Player || page == Page::Player {
            self.release_display();
            self.last_session_viewport = None;
        }
        if page != Page::Player {
            self.display_maximized = false;
        }
        self.page = page;
        self.sidebar_collapsed = true;
        self.ui_dirty = true;
    }

    fn sync_window_aspect(&mut self, window: &Window) {
        let Some((guest_width, guest_height)) = self
            .selected
            .as_ref()
            .map(|record| oriented_guest_dimensions(&record.spec.display))
        else {
            return;
        };
        let aspect = (guest_width, guest_height);
        if self.window_aspect == Some(aspect) {
            return;
        }

        #[cfg(target_os = "macos")]
        if let Ok(handle) = window.window_handle() {
            let _ = set_macos_window_content_aspect_ratio(
                handle.as_raw(),
                f64::from(guest_width),
                f64::from(guest_height),
            );
        }

        let ratio = f64::from(guest_width) / f64::from(guest_height);
        let (minimum_width, minimum_height) = if ratio >= 1.0 {
            (720.0, (720.0 / ratio).max(360.0))
        } else {
            (480.0, (480.0 / ratio).max(640.0))
        };
        window.set_min_inner_size(Some(LogicalSize::new(minimum_width, minimum_height)));

        if !window.is_maximized() {
            let scale = window.scale_factor().max(1.0);
            let current = window.inner_size();
            let current_width = f64::from(current.width) / scale;
            let current_height = f64::from(current.height) / scale;
            let area = (current_width * current_height).max(1.0);
            let mut target_width = (area * ratio).sqrt();
            let mut target_height = target_width / ratio;
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
            let maximum_height = monitor.height * 0.86;
            let shrink = (maximum_width / target_width)
                .min(maximum_height / target_height)
                .min(1.0);
            target_width = (target_width * shrink).max(minimum_width);
            target_height = (target_height * shrink).max(minimum_height);
            let _ = window.request_inner_size(LogicalSize::new(target_width, target_height));
        }
        self.window_aspect = Some(aspect);
        self.ui_dirty = true;
    }

    fn update_display_layout(&mut self, layout: SurfaceLayout, scale: f64) {
        if self.page != Page::Player
            || layout.width < 64
            || layout.height < 64
            || self.native_display.root_is_minimized()
        {
            self.set_viewport_hidden();
            self.sync_display();
            return;
        }
        let available_width = layout.width;
        let available_height = layout.height;
        let rotation_quarters = self.selected.as_ref().map_or(0, |record| {
            record.spec.display.orientation.android_rotation()
        });
        let bounds = NativeDisplayBounds {
            x_px: 0,
            y_px: 0,
            width_px: available_width,
            height_px: available_height,
            rotation_quarters,
            visible: true,
        };
        if let Err(error) = self.native_display.set_bounds(bounds) {
            self.notice(format!("原生显示区域定位失败：{error}"));
            return;
        }
        let dpi = physical_dimension(96.0 * scale, 960).clamp(72, 960);
        let changed = self.viewport.as_ref().is_none_or(|viewport| {
            viewport.width_px != available_width
                || viewport.height_px != available_height
                || viewport.dpi != dpi
                || !viewport.visible
        });
        if changed {
            self.viewport_revision = self.viewport_revision.saturating_add(1);
            self.viewport = Some(DisplayViewportV2 {
                width_px: available_width,
                height_px: available_height,
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
        let snapshot = json!({
            "type": "snapshot",
            "payload": {
                "summaries": self.summaries,
                "selected": self.selected,
                "status": self.status,
                "artifact_hint": self.artifact_hint,
                "device_capabilities": self.capabilities.as_ref().map_or(&[][..], |capabilities| capabilities.devices.devices.as_slice()),
                "device_capabilities_loading": self.selected_id.is_some()
                    && (self.capabilities_for != self.selected_id || self.capabilities_in_flight),
            }
        });
        webviews.broadcast(&snapshot);
        webviews.broadcast(&json!({
            "type": "layout",
            "payload": {
                "page": self.page,
                "sidebar_collapsed": self.sidebar_collapsed,
                "apk_path": self.apk_path.as_ref().map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
                "display_maximized": self.display_maximized,
                "android_focused": self.android_focused,
                "sidebar_visible": self.sidebar_visible,
            }
        }));
        webviews.broadcast(&json!({ "type": "busy", "payload": self.command_in_flight }));
        webviews.broadcast(&json!({ "type": "notice", "payload": self.status }));
        self.ui_dirty = false;
    }

    fn poll(&mut self) {
        if !self.closing && self.last_refresh.elapsed() >= POLL_INTERVAL {
            self.request_refresh();
        }
        let minimized = self.native_display.root_is_minimized();
        if minimized && !self.root_was_minimized {
            self.root_was_minimized = true;
            self.suspend_display_for_minimize();
        } else if !minimized && self.root_was_minimized {
            self.root_was_minimized = false;
            self.last_session_viewport = None;
        }
        self.sync_display();
    }

    fn request_refresh(&mut self) {
        if self.refresh_in_flight || self.closing {
            return;
        }
        self.refresh_in_flight = true;
        self.last_refresh = Instant::now();
        let client = self.client.clone();
        let selected_id = self.selected_id;
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let summaries = client.list_instances().await.map_err(|e| e.to_string())?;
                let id = selected_id
                    .filter(|id| summaries.iter().any(|item| item.id == *id))
                    .or_else(|| summaries.first().map(|item| item.id));
                let selected = match id {
                    Some(id) => Some(client.get_instance(id).await.map_err(|e| e.to_string())?),
                    None => None,
                };
                Ok((summaries, selected))
            }
            .await;
            let _ = proxy.send_event(UserEvent::Refreshed(result));
        });
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
            self.request_capabilities();
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

    fn on_refresh(
        &mut self,
        result: Result<(Vec<InstanceSummaryV2>, Option<InstanceRecordV2>), String>,
    ) {
        self.refresh_in_flight = false;
        match result {
            Ok((summaries, selected)) => {
                let state_changed = self.summaries != summaries || self.selected != selected;
                self.summaries = summaries;
                self.selected_id = selected.as_ref().map(|record| record.spec.id);
                self.selected = selected;
                if self.capabilities_for != self.selected_id && !self.capabilities_in_flight {
                    self.capabilities = None;
                    self.request_capabilities();
                }
                if self.status == "正在连接 HD Host…" {
                    "HD Host 已同步".clone_into(&mut self.status);
                    self.ui_dirty = true;
                } else if state_changed {
                    // Host polling runs four times per second. Re-evaluating the same React state
                    // in three independent WebView2 composition trees creates needless DComp
                    // commits next to the Vulkan child and can visibly flash on Intel drivers.
                    self.ui_dirty = true;
                }
                if self.auto_start_selected {
                    let inactive = self
                        .selected
                        .as_ref()
                        .is_some_and(|record| !record.status.observed.is_active());
                    if !inactive {
                        self.auto_start_selected = false;
                    } else if self.session.is_some() {
                        self.auto_start_selected = false;
                        self.operation("start");
                    }
                }
            }
            Err(error) => self.notice(error),
        }
    }

    fn select(&mut self, instance_id: Uuid) {
        if self.selected_id == Some(instance_id) {
            self.page = Page::Player;
            self.ui_dirty = true;
            return;
        }
        self.release_display();
        self.selected_id = Some(instance_id);
        self.selected = None;
        self.capabilities = None;
        self.capabilities_for = None;
        self.auto_start_selected = true;
        self.page = Page::Player;
        self.last_refresh = Instant::now()
            .checked_sub(POLL_INTERVAL)
            .unwrap_or_else(Instant::now);
        self.ui_dirty = true;
        self.request_refresh();
    }

    fn create(&mut self, name: String) {
        if self.command_in_flight {
            return;
        }
        let spec = InstanceSpecV2 {
            name,
            artifacts: default_artifact_selection(),
            ..InstanceSpecV2::default()
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
                self.selected_id = Some(record.spec.id);
                self.selected = Some(record);
                self.capabilities = None;
                self.capabilities_for = None;
                self.request_capabilities();
                self.auto_start_selected = true;
                self.page = Page::Player;
                self.notice("实例已创建，正在启动".to_owned());
                self.request_refresh();
            }
            Err(error) => self.notice(error),
        }
    }

    fn save_spec(&mut self, spec: InstanceSpecV2, restart: bool) {
        if self.command_in_flight {
            return;
        }
        let Some(record) = &self.selected else { return };
        if spec.id != record.spec.id {
            self.notice("设置实例与当前实例不一致".to_owned());
            return;
        }
        if let Err(error) = spec.validate() {
            self.notice(error.to_string());
            return;
        }
        let id = record.spec.id;
        let was_active = record.status.observed.is_active();
        let restarting = was_active && restart;
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

                let current = client
                    .get_instance(id)
                    .await
                    .map_err(|error| error.to_string())?;
                let request = UpdateInstanceRequestV2 {
                    expected_revision: current.status.revision,
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
                self.selected = Some(record);
                self.capabilities = None;
                self.capabilities_for = None;
                self.request_capabilities();
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
    fn operation(&mut self, kind: &str) {
        if self.command_in_flight {
            return;
        }
        if kind == "start" && self.session.is_none() {
            self.auto_start_selected = true;
            self.notice("正在准备 Android 原生显示区…".to_owned());
            self.sync_display();
            return;
        }
        let Some(id) = self.selected_id else { return };
        let (operation, timeout, label) = match kind {
            "start" => (
                OperationKindV2::Start,
                Duration::from_mins(3),
                "Android 已启动",
            ),
            "pause" => (
                OperationKindV2::Pause,
                Duration::from_secs(30),
                "Android 已暂停",
            ),
            "resume" => (
                OperationKindV2::Resume,
                Duration::from_secs(30),
                "Android 已恢复",
            ),
            "restart" => (
                OperationKindV2::Restart,
                Duration::from_mins(3),
                "Android 已重启",
            ),
            "stop" => (
                OperationKindV2::Stop {
                    mode: StopModeV2::Graceful,
                    graceful_timeout_ms: 20_000,
                },
                Duration::from_secs(90),
                "Android 已关闭",
            ),
            "delete" => (
                OperationKindV2::Delete,
                Duration::from_secs(90),
                "实例已删除",
            ),
            _ => {
                self.notice(format!("未知实例操作：{kind}"));
                return;
            }
        };
        if matches!(
            operation,
            OperationKindV2::Stop { .. } | OperationKindV2::Delete
        ) {
            self.release_display();
        }
        let artifact_update =
            matches!(operation, OperationKindV2::Start | OperationKindV2::Restart)
                .then(|| self.selected.as_ref())
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
        self.reset_display_after_command = matches!(
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
                Ok(label.to_owned())
            }
            .await;
            let _ = proxy.send_event(UserEvent::CommandFinished(result));
        });
    }

    fn on_command_finished(&mut self, result: Result<String, String>) {
        self.command_in_flight = false;
        let reset_display = std::mem::take(&mut self.reset_display_after_command);
        if reset_display && result.is_ok() {
            self.release_display();
            self.last_session_viewport = None;
        }
        match result {
            Ok(message) => self.notice(message),
            Err(error) => self.notice(error),
        }
        self.last_refresh = Instant::now()
            .checked_sub(POLL_INTERVAL)
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
        self.action(InstanceActionV2::Key { key }, "按键已发送");
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
        let _ = self.native_display.focus_guest();
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
            let _ = proxy.send_event(UserEvent::CommandFinished(result));
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
            let _ = proxy.send_event(UserEvent::CommandFinished(result));
        });
    }

    fn install_selected_apk(&mut self) {
        let Some(path) = self.apk_path.clone() else {
            self.notice("请在设置页选择 APK，或将 APK 文件拖入 HD 窗口".to_owned());
            return;
        };
        self.install_apk(path);
    }

    fn choose_apk(&mut self, install_after_selection: bool) {
        match choose_apk_file() {
            Ok(Some(path)) => {
                self.apk_path = Some(path.clone());
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
            let _ = proxy.send_event(UserEvent::CommandFinished(result));
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
                .map(|bundle| format!("诊断包已保存：{}", bundle.path.display()))
                .map_err(|error| error.to_string());
            let _ = proxy.send_event(UserEvent::CommandFinished(result));
        });
    }

    fn set_viewport_hidden(&mut self) {
        let _ = self.native_display.hide();
        if let Some(viewport) = self.viewport.as_mut()
            && viewport.visible
        {
            self.viewport_revision = self.viewport_revision.saturating_add(1);
            viewport.visible = false;
            viewport.revision = self.viewport_revision;
        }
    }

    fn suspend_display_for_minimize(&mut self) {
        self.set_viewport_hidden();
        self.release_display();
        self.last_session_viewport = None;
    }

    fn sync_display(&mut self) {
        if self.closing || self.display_in_flight {
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
            if session.instance_id != id {
                self.release_display();
                return;
            }
            if self.last_session_viewport.as_ref() == Some(&viewport)
                && self.last_session_touch.elapsed() < DISPLAY_HEARTBEAT
            {
                return;
            }
            self.display_in_flight = true;
            let completed_viewport = viewport.clone();
            self.runtime.spawn(async move {
                let result = client
                    .update_display_session(id, session.session_token, viewport)
                    .await
                    .map_err(|error| error.to_string());
                let _ = proxy.send_event(UserEvent::DisplayFinished {
                    instance_id: id,
                    viewport: completed_viewport,
                    result,
                });
            });
        } else if viewport.visible && self.last_display_attempt.elapsed() >= DISPLAY_RETRY {
            self.last_display_attempt = Instant::now();
            self.display_in_flight = true;
            self.notice("正在附加 Android 原生画面…".to_owned());
            let target = self.display_target.clone();
            let completed_viewport = viewport.clone();
            self.runtime.spawn(async move {
                let result = client
                    .acquire_display_session(id, target, viewport)
                    .await
                    .map_err(|error| error.to_string());
                let _ = proxy.send_event(UserEvent::DisplayFinished {
                    instance_id: id,
                    viewport: completed_viewport,
                    result,
                });
            });
        }
    }

    fn on_display_finished(
        &mut self,
        instance_id: Uuid,
        completed_viewport: DisplayViewportV2,
        result: Result<DisplaySessionV2, String>,
    ) {
        self.display_in_flight = false;
        if self.selected_id != Some(instance_id) {
            if let Ok(session) = result {
                let client = self.client.clone();
                self.runtime.spawn(async move {
                    let _ = client
                        .release_display_session(instance_id, session.session_token)
                        .await;
                });
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
                    self.operation("start");
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

    fn begin_close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.command_in_flight = true;
        let _ = self.native_display.hide();
        self.notice("正在关闭 HD；Android 实例保持运行…".to_owned());
        let session = self.session.take();
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = if let Some(session) = session {
                client
                    .release_display_session(session.instance_id, session.session_token)
                    .await
                    .map_err(|error| error.to_string())
            } else {
                Ok(())
            };
            let _ = proxy.send_event(UserEvent::ShutdownFinished(result));
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
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .map(|root| root.join("web/dist"))
                .filter(|path| path.join("index.html").is_file())
        })
}

fn configure_fast_dev_artifacts(root: &Path) -> Option<String> {
    let direct = [
        root.to_path_buf(),
        root.join("direct-linux"),
        root.join("products/android/vsoc_arm64_only/direct-linux"),
        root.join("products/android/vsoc_x86_64/direct-linux"),
    ]
    .into_iter()
    .find(|candidate| candidate.join("kernel").is_file())?;
    let sparse_rootfs = direct.join("aggregate_android.sparse.img");
    let raw_rootfs = direct.join("aggregate_android.img");
    let rootfs = if sparse_rootfs.is_file() {
        sparse_rootfs
    } else {
        raw_rootfs
    };
    if !direct.join("kernel").is_file()
        || !direct.join("initrd_android.img").is_file()
        || !rootfs.is_file()
        || !direct.join("android_fstab.dt").is_file()
    {
        return None;
    }
    let host_root = direct_host_tools_root()?;
    let sensor = host_root.join("hd-sensor-injector");
    let adb = std::env::var_os("HD_DEV_ADB")
        .map(PathBuf::from)
        .or_else(default_adb_path)?;
    let aapt2 = std::env::var_os("HD_DEV_AAPT2")
        .map(PathBuf::from)
        .or_else(default_aapt2_path);
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
        if std::env::var_os("HD_DEV_ADB").is_none() {
            std::env::set_var("HD_DEV_ADB", &adb);
        }
        if let Some(aapt2) = aapt2.as_ref()
            && std::env::var_os("HD_DEV_AAPT2").is_none()
        {
            std::env::set_var("HD_DEV_AAPT2", aapt2);
        }
    }
    info!(path = %direct.display(), rootfs = %rootfs.display(), "configured Android development artifacts");
    Some(format!("快速开发镜像：{}", rootfs.display()))
}

fn default_artifact_selection() -> Option<ArtifactSelectionV2> {
    if let Some(selection) = DIRECT_DEV_SELECTION.get() {
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
