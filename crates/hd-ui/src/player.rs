#![cfg_attr(windows, windows_subsystem = "windows")]
// The Player's only unsafe boundary is the documented Win32 child-HWND lifecycle below.
#![allow(unsafe_code)]

// Per-instance Android Player. The egui UI owns only the chrome around the display;
// crosvm/gfxstream continues to present directly to the child HWND.

mod theme;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::Parser;
use eframe::egui;
use hd_core::{
    DisplaySessionV2, DisplayViewportV2, InstanceActionV2, InstanceRecordV2, KeyActionV2,
    NativeDisplayTargetV2, ObservedStateV2, OperationKindV2, OrientationV2, StopModeV2,
};
use hd_platform::{DataPaths, current_process_identity};
use hd_runtime::HostClientV2;
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use tokio::runtime::Runtime;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const SESSION_HEARTBEAT: Duration = Duration::from_secs(5);
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const DISPLAY_ATTACH_BACKOFF: Duration = Duration::from_millis(500);

#[derive(Debug, Parser)]
#[command(name = "hd-player", version, about = "HD Android instance player")]
struct Cli {
    #[arg(long)]
    instance_id: Uuid,
    #[arg(long)]
    data_root: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = match cli.data_root {
        Some(root) => DataPaths::from_root(root),
        None => DataPaths::discover().context("discover HD data directory")?,
    };
    paths.ensure().context("create HD data directories")?;
    let _log_guard = init_logging(&paths)?;
    let title = player_title(cli.instance_id);
    if focus_existing_player(&title) {
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hd-player-runtime")
        .build()
        .context("create Player runtime")?;
    let client = runtime
        .block_on(HostClientV2::connect_or_start(paths.clone()))
        .context("connect to HD host")?;
    let mut options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title(&title)
            .with_decorations(false)
            .with_transparent(true)
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([780.0, 560.0]),
        ..Default::default()
    };
    #[cfg(windows)]
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        setup.instance_descriptor.backends = eframe::wgpu::Backends::VULKAN;
    }
    eframe::run_native(
        &title,
        options,
        Box::new(move |creation| {
            egui_extras::install_image_loaders(&creation.egui_ctx);
            configure_cjk_fonts(&creation.egui_ctx);
            theme::install_player(&creation.egui_ctx);
            Ok(Box::new(PlayerApp::new(
                creation,
                runtime,
                client,
                paths,
                cli.instance_id,
            )?))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn player_title(instance_id: Uuid) -> String {
    format!("HD Player {instance_id}")
}

fn configure_cjk_fonts(context: &egui::Context) {
    #[cfg(windows)]
    let candidates = [r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\simhei.ttf"];
    #[cfg(target_os = "macos")]
    let candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates = [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    ];
    let Some(bytes) = candidates.iter().find_map(|path| std::fs::read(path).ok()) else {
        return;
    };
    let name = "hd-player-cjk".to_owned();
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        name.clone(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push(name.clone());
    }
    context.set_fonts(fonts);
}

fn init_logging(paths: &DataPaths) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let appender = tracing_appender::rolling::daily(&paths.logs, "player-v2.jsonl");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(writer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize Player logging: {error}"))?;
    Ok(guard)
}

#[derive(Debug)]
enum PlayerMessage {
    Record(Result<InstanceRecordV2, String>),
    StartRequested(Result<String, String>),
    Command(Result<String, String>),
    Acquired(Result<DisplaySessionV2, String>),
    Renewed(Result<DisplaySessionV2, String>),
    Reconnected(Result<HostClientV2, String>),
    Shutdown(Result<(), String>),
}

struct PlayerApp {
    runtime: Runtime,
    client: HostClientV2,
    paths: DataPaths,
    instance_id: Uuid,
    display_target: NativeDisplayTargetV2,
    native_viewport: NativeViewport,
    sender: Sender<PlayerMessage>,
    receiver: Receiver<PlayerMessage>,
    record: Option<InstanceRecordV2>,
    session: Option<DisplaySessionV2>,
    viewport: Option<DisplayViewportV2>,
    last_session_viewport: Option<DisplayViewportV2>,
    last_refresh: Instant,
    last_reconnect_attempt: Instant,
    last_session_touch: Instant,
    last_display_attempt: Instant,
    request_in_flight: bool,
    record_in_flight: bool,
    reconnect_in_flight: bool,
    command_in_flight: bool,
    auto_start_decided: bool,
    start_was_requested: bool,
    close_requested: bool,
    status: String,
    window_title: String,
    fullscreen: bool,
    show_power_menu: bool,
    show_apk_entry: bool,
    apk_path: String,
    allow_close: bool,
    settings_open: bool,
}

impl PlayerApp {
    fn new(
        creation: &eframe::CreationContext<'_>,
        runtime: Runtime,
        client: HostClientV2,
        paths: DataPaths,
        instance_id: Uuid,
    ) -> Result<Self> {
        let native_viewport = NativeViewport::new(creation)?;
        let identity = current_process_identity(Uuid::new_v4())
            .map_err(|error| anyhow::anyhow!("identify Player process: {error}"))?;
        let (sender, receiver) = std::sync::mpsc::channel();
        Ok(Self {
            runtime,
            client,
            paths,
            instance_id,
            display_target: NativeDisplayTargetV2::WindowsHwnd {
                hwnd: native_viewport.handle(),
                owner: identity,
            },
            native_viewport,
            sender,
            receiver,
            record: None,
            session: None,
            viewport: None,
            last_session_viewport: None,
            last_refresh: Instant::now() - POLL_INTERVAL,
            last_reconnect_attempt: Instant::now() - RECONNECT_BACKOFF,
            last_session_touch: Instant::now(),
            last_display_attempt: Instant::now() - DISPLAY_ATTACH_BACKOFF,
            request_in_flight: false,
            record_in_flight: false,
            reconnect_in_flight: false,
            command_in_flight: false,
            auto_start_decided: false,
            start_was_requested: false,
            close_requested: false,
            status: "正在连接实例…".to_owned(),
            window_title: player_title(instance_id),
            fullscreen: false,
            show_power_menu: false,
            show_apk_entry: false,
            apk_path: String::new(),
            allow_close: false,
            settings_open: false,
        })
    }

    fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = PlayerMessage> + Send + 'static,
    {
        let sender = self.sender.clone();
        self.runtime.spawn(async move {
            let _ = sender.send(future.await);
        });
    }

    fn refresh_record(&mut self) {
        if self.reconnect_in_flight
            || self.record_in_flight
            || self.last_refresh.elapsed() < POLL_INTERVAL
        {
            return;
        }
        self.record_in_flight = true;
        self.last_refresh = Instant::now();
        let client = self.client.clone();
        let id = self.instance_id;
        self.spawn(async move {
            PlayerMessage::Record(
                client
                    .get_instance(id)
                    .await
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn reconnect_host(&mut self) {
        if self.reconnect_in_flight || self.last_reconnect_attempt.elapsed() < RECONNECT_BACKOFF {
            return;
        }
        self.last_reconnect_attempt = Instant::now();
        self.reconnect_in_flight = true;
        self.record_in_flight = false;
        self.request_in_flight = false;
        self.session = None;
        self.last_session_viewport = None;
        self.status = "HD Host 已重启，正在重新连接…".to_owned();
        let paths = self.paths.clone();
        let instance_id = self.instance_id;
        warn!(%instance_id, "reconnecting Player to HD Host");
        self.spawn(async move {
            PlayerMessage::Reconnected(
                HostClientV2::connect_or_start(paths)
                    .await
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn process_messages(&mut self) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                PlayerMessage::Record(result) => {
                    self.record_in_flight = false;
                    match result {
                        Ok(record) => {
                            let should_start = !self.auto_start_decided
                                && matches!(
                                    record.status.observed,
                                    ObservedStateV2::Defined
                                        | ObservedStateV2::Stopped
                                        | ObservedStateV2::Blocked
                                        | ObservedStateV2::Failed
                                );
                            self.auto_start_decided = true;
                            if self.session.as_ref().is_some_and(|session| {
                                session.generation != record.frame_generation
                            }) {
                                info!(
                                    instance_id = %self.instance_id,
                                    generation = record.frame_generation,
                                    "discarding stale Player display session"
                                );
                                self.session = None;
                                self.last_session_viewport = None;
                            }
                            self.status = status_line(&record);
                            self.record = Some(record);
                            if should_start && !self.close_requested {
                                self.status = "正在启动 Android…".to_owned();
                                self.start_instance_on_open();
                            }
                        }
                        Err(error) => {
                            warn!(instance_id = %self.instance_id, %error, "Player record request failed");
                            self.status = format!("无法连接 HD Host：{error}");
                            self.reconnect_host();
                        }
                    }
                }
                PlayerMessage::StartRequested(result) => {
                    self.command_in_flight = false;
                    match result {
                        Ok(message) => {
                            self.start_was_requested = true;
                            self.status = message;
                        }
                        Err(error) => {
                            self.auto_start_decided = false;
                            warn!(instance_id = %self.instance_id, %error, "automatic Android start request failed");
                            self.status = format!("Android 启动请求失败：{error}");
                        }
                    }
                }
                PlayerMessage::Command(result) => {
                    self.command_in_flight = false;
                    match result {
                        Ok(message) => self.status = message,
                        Err(error) => {
                            warn!(instance_id = %self.instance_id, %error, "Player command failed");
                            self.status = error;
                        }
                    }
                }
                PlayerMessage::Acquired(result) => {
                    self.request_in_flight = false;
                    match result {
                        Ok(session) => {
                            info!(
                                instance_id = %self.instance_id,
                                generation = session.generation,
                                "Android native display attached to Player"
                            );
                            self.status = "Android 原生画面已附加".to_owned();
                            self.last_session_touch = Instant::now();
                            self.session = Some(session);
                            self.last_session_viewport = self.viewport.clone();
                        }
                        Err(error) => {
                            warn!(instance_id = %self.instance_id, %error, "display attach failed");
                            self.status = format!("显示附加失败，正在重试：{error}");
                        }
                    }
                }
                PlayerMessage::Renewed(result) => {
                    self.request_in_flight = false;
                    match result {
                        Ok(session) => {
                            self.last_session_touch = Instant::now();
                            self.session = Some(session);
                            self.last_session_viewport = self.viewport.clone();
                        }
                        Err(error) => {
                            warn!(instance_id = %self.instance_id, %error, "display session renewal failed");
                            self.session = None;
                            self.last_session_viewport = None;
                            self.status = format!("显示会话已失效，将重连：{error}");
                        }
                    }
                }
                PlayerMessage::Reconnected(result) => {
                    self.reconnect_in_flight = false;
                    self.last_refresh = Instant::now() - POLL_INTERVAL;
                    match result {
                        Ok(client) => {
                            info!(instance_id = %self.instance_id, "Player reconnected to HD Host");
                            self.client = client;
                            self.status = "HD Host 已重新连接".to_owned();
                        }
                        Err(error) => {
                            warn!(instance_id = %self.instance_id, %error, "HD Host reconnect failed");
                            self.status = format!("HD Host 重连失败：{error}");
                        }
                    }
                }
                PlayerMessage::Shutdown(result) => match result {
                    Ok(()) => self.allow_close = true,
                    Err(error) => {
                        self.command_in_flight = false;
                        warn!(instance_id = %self.instance_id, %error, "Android shutdown on Player close failed");
                        self.status = format!("关闭 Android 失败，将重连后重试：{error}");
                        self.last_reconnect_attempt = Instant::now() - RECONNECT_BACKOFF;
                        self.reconnect_host();
                    }
                },
            }
        }
    }

    fn start_instance_on_open(&mut self) {
        if self.command_in_flight {
            return;
        }
        self.command_in_flight = true;
        let client = self.client.clone();
        let id = self.instance_id;
        self.spawn(async move {
            PlayerMessage::StartRequested(
                client
                    .create_operation(
                        id,
                        OperationKindV2::Start,
                        &format!("hd-player-open-{}", Uuid::new_v4()),
                    )
                    .await
                    .map(|_| "Android 启动请求已提交".to_owned())
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn run_operation(&mut self, kind: OperationKindV2, timeout: Duration, label: &'static str) {
        if self.command_in_flight {
            return;
        }
        self.command_in_flight = true;
        let client = self.client.clone();
        let id = self.instance_id;
        self.spawn(async move {
            let result = async {
                let operation = client
                    .create_operation(id, kind, &format!("hd-player-{}", Uuid::new_v4()))
                    .await
                    .map_err(|error| error.to_string())?;
                client
                    .wait_operation(operation.id, timeout)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("{label}完成"))
            }
            .await;
            PlayerMessage::Command(result)
        });
    }

    fn send_action(&mut self, action: InstanceActionV2, label: &'static str) {
        if self.command_in_flight {
            return;
        }
        self.native_viewport.focus_guest();
        self.command_in_flight = true;
        let client = self.client.clone();
        let id = self.instance_id;
        self.spawn(async move {
            PlayerMessage::Command(
                client
                    .action(id, action)
                    .await
                    .map(|_| format!("{label}已送达"))
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn capture_screenshot(&mut self) {
        if self.command_in_flight {
            return;
        }
        self.command_in_flight = true;
        let client = self.client.clone();
        let id = self.instance_id;
        self.spawn(async move {
            PlayerMessage::Command(
                client
                    .capture_screenshot(id)
                    .await
                    .map(|record| format!("截图已保存：{}", record.path.display()))
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn install_apk(&mut self) {
        let path = PathBuf::from(self.apk_path.trim());
        if path.as_os_str().is_empty() {
            self.status = "请输入 APK 完整路径".to_owned();
            return;
        }
        self.command_in_flight = true;
        let client = self.client.clone();
        let id = self.instance_id;
        self.spawn(async move {
            let result = async {
                let upload = client
                    .upload_apk(&path)
                    .await
                    .map_err(|error| error.to_string())?;
                let operation = client
                    .create_operation(
                        id,
                        OperationKindV2::InstallApk {
                            upload_id: upload.id,
                            sha256: upload.sha256,
                        },
                        &format!("hd-player-install-{}", Uuid::new_v4()),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                client
                    .wait_operation(operation.id, Duration::from_mins(10))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok("APK 已安装".to_owned())
            }
            .await;
            PlayerMessage::Command(result)
        });
    }

    fn sync_display(&mut self) {
        if self.reconnect_in_flight || self.close_requested {
            return;
        }
        let active = self
            .record
            .as_ref()
            .is_some_and(|record| record.status.observed.is_active() && record.worker.is_some());
        let Some(viewport) = self.viewport.clone() else {
            return;
        };
        if !active {
            return;
        }
        if self.request_in_flight {
            return;
        }
        let client = self.client.clone();
        let id = self.instance_id;
        if let Some(session) = self.session.clone() {
            if self.last_session_viewport.as_ref() == Some(&viewport)
                && self.last_session_touch.elapsed() < SESSION_HEARTBEAT
            {
                return;
            }
            self.request_in_flight = true;
            self.spawn(async move {
                PlayerMessage::Renewed(
                    client
                        .update_display_session(id, session.session_token, viewport)
                        .await
                        .map_err(|error| error.to_string()),
                )
            });
        } else {
            if self.last_display_attempt.elapsed() < DISPLAY_ATTACH_BACKOFF {
                return;
            }
            self.last_display_attempt = Instant::now();
            self.request_in_flight = true;
            let target = self.display_target.clone();
            self.spawn(async move {
                PlayerMessage::Acquired(
                    client
                        .acquire_display_session(id, target, viewport)
                        .await
                        .map_err(|error| error.to_string()),
                )
            });
        }
    }

    fn shutdown_then_close(&mut self) {
        if self.command_in_flight || self.reconnect_in_flight {
            self.status = "正在完成当前操作，随后关闭 Android…".to_owned();
            return;
        }
        if !self.start_was_requested
            && self.record.as_ref().map_or(true, |record| {
                matches!(
                    record.status.observed,
                    ObservedStateV2::Defined
                        | ObservedStateV2::Stopped
                        | ObservedStateV2::Blocked
                        | ObservedStateV2::Failed
                        | ObservedStateV2::Deleted
                )
            })
        {
            self.allow_close = true;
            return;
        }
        self.command_in_flight = true;
        let session = self.session.take();
        self.last_session_viewport = None;
        let client = self.client.clone();
        let id = self.instance_id;
        self.spawn(async move {
            let result = async {
                if let Some(session) = session {
                    if let Err(error) = client
                        .release_display_session(id, session.session_token)
                        .await
                    {
                        warn!(instance_id = %id, %error, "display release failed during Player close");
                    }
                }
                let operation = client
                    .create_operation(
                        id,
                        OperationKindV2::Stop {
                            mode: StopModeV2::Graceful,
                            graceful_timeout_ms: 20_000,
                        },
                        &format!("hd-player-close-{}", Uuid::new_v4()),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                client
                    .wait_operation(operation.id, Duration::from_secs(90))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            .await;
            PlayerMessage::Shutdown(result)
        });
    }

    fn handle_close_request(&mut self, context: &egui::Context) {
        if self.allow_close {
            return;
        }
        if !context.input(|input| input.viewport().close_requested()) {
            return;
        }
        context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        self.close_requested = true;
        self.shutdown_then_close();
    }

    fn sync_window_title(&mut self, context: &egui::Context) {
        let marker = player_title(self.instance_id);
        let title = self.record.as_ref().map_or_else(
            || format!("正在连接实例 | {marker}"),
            |record| {
                let (width, height) = record.spec.display.oriented_size();
                let (state, _) = state_badge(&record.status.observed);
                format!(
                    "{} | {state} | {width} × {height} | {} dpi | 帧 {} | {} | {marker}",
                    record.spec.name,
                    record.spec.display.dpi,
                    record.frame_generation,
                    compact_status(&self.status),
                )
            },
        );
        if title != self.window_title {
            context.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }
    }

    fn central_display(&mut self, ui: &mut egui::Ui) {
        // The display is deliberately inset from the shared chrome material. It reads as a
        // separate black layer while the surrounding titlebar and left tool rail remain one
        // continuous translucent surface.
        let available = ui.available_rect_before_wrap().shrink(6.0);
        let (guest_width, guest_height) = self.record.as_ref().map_or((16.0, 9.0), |record| {
            let (width, height) = record.spec.display.oriented_size();
            (width as f32, height as f32)
        });
        let scale = (available.width() / guest_width).min(available.height() / guest_height);
        let size = egui::vec2(
            guest_width * scale.max(0.01),
            guest_height * scale.max(0.01),
        );
        let rect = egui::Rect::from_center_size(available.center(), size);
        ui.painter().rect_filled(
            available.expand(2.0),
            2.0,
            egui::Color32::from_rgba_unmultiplied(22, 27, 32, 34),
        );
        ui.painter()
            .rect_filled(rect, 0.0, theme::PLAYER_DISPLAY_WELL);
        ui.painter().rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(
                1.0,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 36),
            ),
            egui::StrokeKind::Inside,
        );
        let ppp = ui.ctx().pixels_per_point();
        let width = (rect.width() * ppp).round().max(1.0) as u32;
        let height = (rect.height() * ppp).round().max(1.0) as u32;
        let dpi = (96.0 * ppp).round().clamp(72.0, 960.0) as u32;
        let viewport = DisplayViewportV2 {
            width_px: width,
            height_px: height,
            dpi,
            visible: true,
            revision: next_viewport_revision(self.viewport.as_ref(), width, height, dpi, true),
        };
        // Keep the native child hidden until Worker confirms crosvm has attached. Otherwise the
        // empty Win32 STATIC control would cover the useful boot/connection state painted below.
        self.native_viewport
            .place(rect, ppp, self.session.is_some());
        self.viewport = Some(viewport);
        if self.session.is_none() {
            let (title, detail, color) = self.record.as_ref().map_or_else(
                || {
                    (
                        "正在连接 HD Host".to_owned(),
                        "正在读取实例状态".to_owned(),
                        theme::PLAYER_DISPLAY_TEXT_MUTED,
                    )
                },
                |record| match record.status.observed {
                    ObservedStateV2::Stopped | ObservedStateV2::Defined => (
                        "正在启动 Android".to_owned(),
                        "Player 会在实例就绪后自动附加原生画面".to_owned(),
                        theme::PLAYER_ACCENT,
                    ),
                    ObservedStateV2::Ready => (
                        "正在附加 Android 画面".to_owned(),
                        "原生显示窗口连接中".to_owned(),
                        theme::PLAYER_SUCCESS,
                    ),
                    ObservedStateV2::Failed | ObservedStateV2::Blocked => (
                        "Android 启动失败".to_owned(),
                        compact_status(&self.status),
                        theme::PLAYER_DANGER,
                    ),
                    state => (
                        "Android 正在启动".to_owned(),
                        format!("当前状态：{state:?}"),
                        theme::PLAYER_ACCENT,
                    ),
                },
            );
            ui.painter().text(
                rect.center() - egui::vec2(0.0, 12.0),
                egui::Align2::CENTER_CENTER,
                title,
                egui::FontId::proportional(19.0),
                color,
            );
            ui.painter().text(
                rect.center() + egui::vec2(0.0, 16.0),
                egui::Align2::CENTER_CENTER,
                detail,
                egui::FontId::proportional(13.0),
                theme::PLAYER_DISPLAY_TEXT_MUTED,
            );
        }
    }

    fn titlebar(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        let rect = ui.max_rect();
        let button_width = 40.0;
        let close_rect = egui::Rect::from_min_max(
            egui::pos2(rect.right() - button_width, rect.top()),
            rect.max,
        );
        let maximize_rect = close_rect.translate(egui::vec2(-button_width, 0.0));
        let minimize_rect = maximize_rect.translate(egui::vec2(-button_width, 0.0));
        let drag_rect =
            egui::Rect::from_min_max(rect.min, egui::pos2(minimize_rect.left(), rect.bottom()));

        let maximized = context.input(|input| input.viewport().maximized.unwrap_or(false));
        let drag = ui.interact(
            drag_rect,
            ui.id().with("player-titlebar-drag"),
            egui::Sense::click_and_drag(),
        );
        if drag.double_clicked() {
            context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        } else if drag.drag_started_by(egui::PointerButton::Primary) {
            context.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        let text_rect = drag_rect.shrink2(egui::vec2(8.0, 0.0));
        ui.painter().with_clip_rect(text_rect).text(
            egui::pos2(text_rect.left(), text_rect.center().y),
            egui::Align2::LEFT_CENTER,
            &self.window_title,
            egui::FontId::proportional(12.0),
            theme::PLAYER_TEXT,
        );

        if titlebar_button(
            ui,
            minimize_rect,
            "player-minimize",
            "最小化",
            TitlebarGlyph::Minimize,
        ) {
            context.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        if titlebar_button(
            ui,
            maximize_rect,
            "player-maximize",
            if maximized { "还原" } else { "最大化" },
            if maximized {
                TitlebarGlyph::Restore
            } else {
                TitlebarGlyph::Maximize
            },
        ) {
            context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }
        if titlebar_button(ui, close_rect, "player-close", "关闭", TitlebarGlyph::Close) {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        let ready = self.record.as_ref().is_some_and(|record| {
            matches!(
                record.status.observed,
                ObservedStateV2::Ready | ObservedStateV2::Paused
            )
        });
        let guest_actions_enabled = ready && !self.command_in_flight;
        let toolbar_rect = ui.max_rect();
        let bottom_height = 32.0 * 5.0 + 1.0;
        let bottom_top = (toolbar_rect.bottom() - bottom_height).max(toolbar_rect.top());
        let top_rect = egui::Rect::from_min_max(
            toolbar_rect.min,
            egui::pos2(toolbar_rect.right(), bottom_top),
        );
        let bottom_rect = egui::Rect::from_min_max(
            egui::pos2(toolbar_rect.left(), bottom_top),
            toolbar_rect.max,
        );

        let mut top_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("player-tools-top")
                .max_rect(top_rect)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        top_ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        if icon_button(
            &mut top_ui,
            egui::include_image!("../assets/icons/volume-up.svg"),
            "音量增加",
            guest_actions_enabled,
        ) {
            self.send_action(
                InstanceActionV2::Key {
                    key: KeyActionV2::VolumeUp,
                },
                "音量增加",
            );
        }
        if icon_button(
            &mut top_ui,
            egui::include_image!("../assets/icons/volume-down.svg"),
            "音量降低",
            guest_actions_enabled,
        ) {
            self.send_action(
                InstanceActionV2::Key {
                    key: KeyActionV2::VolumeDown,
                },
                "音量降低",
            );
        }
        if icon_button(
            &mut top_ui,
            egui::include_image!("../assets/icons/rotate.svg"),
            "旋转屏幕",
            guest_actions_enabled,
        ) {
            if let Some(record) = &self.record {
                let orientation = if record.spec.display.orientation.is_portrait() {
                    OrientationV2::Landscape
                } else {
                    OrientationV2::Portrait
                };
                self.send_action(InstanceActionV2::Rotate { orientation }, "旋转");
            }
        }
        if icon_button(
            &mut top_ui,
            egui::include_image!("../assets/icons/fullscreen.svg"),
            "全屏",
            true,
        ) {
            self.fullscreen = !self.fullscreen;
            context.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        toolbar_separator(&mut top_ui);
        if icon_button(
            &mut top_ui,
            egui::include_image!("../assets/icons/screenshot.svg"),
            "截图",
            guest_actions_enabled,
        ) {
            self.capture_screenshot();
        }
        if icon_button(
            &mut top_ui,
            egui::include_image!("../assets/icons/apk.svg"),
            "安装 APK",
            guest_actions_enabled,
        ) {
            self.show_apk_entry = !self.show_apk_entry;
        }

        let mut bottom_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("player-tools-bottom")
                .max_rect(bottom_rect)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        bottom_ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        if icon_button(
            &mut bottom_ui,
            egui::include_image!("../assets/icons/recent.svg"),
            "最近任务",
            guest_actions_enabled,
        ) {
            self.send_action(
                InstanceActionV2::Key {
                    key: KeyActionV2::Recent,
                },
                "最近任务",
            );
        }
        if icon_button(
            &mut bottom_ui,
            egui::include_image!("../assets/icons/home.svg"),
            "主页",
            guest_actions_enabled,
        ) {
            self.send_action(
                InstanceActionV2::Key {
                    key: KeyActionV2::Home,
                },
                "主页",
            );
        }
        if icon_button(
            &mut bottom_ui,
            egui::include_image!("../assets/icons/back.svg"),
            "返回",
            guest_actions_enabled,
        ) {
            self.send_action(
                InstanceActionV2::Key {
                    key: KeyActionV2::Back,
                },
                "返回",
            );
        }
        toolbar_separator(&mut bottom_ui);
        if icon_button(
            &mut bottom_ui,
            egui::include_image!("../assets/icons/settings.svg"),
            "设置",
            true,
        ) {
            self.settings_open = true;
        }
        if icon_button(
            &mut bottom_ui,
            egui::include_image!("../assets/icons/power.svg"),
            "电源菜单",
            true,
        ) {
            self.show_power_menu = !self.show_power_menu;
        }
        if self.show_apk_entry {
            egui::Window::new("安装 APK")
                .collapsible(false)
                .resizable(false)
                .show(context, |ui| {
                    ui.label("APK 文件路径");
                    ui.text_edit_singleline(&mut self.apk_path);
                    if ui.button("安装").clicked() {
                        self.install_apk();
                        self.show_apk_entry = false;
                    }
                });
        }
        if self.show_power_menu {
            egui::Window::new("电源菜单")
                .collapsible(false)
                .resizable(false)
                .show(context, |ui| {
                    if ui.button("启动").clicked() {
                        self.run_operation(OperationKindV2::Start, Duration::from_mins(10), "启动");
                        self.show_power_menu = false;
                    }
                    if ui.button("暂停 / 恢复").clicked() {
                        let operation =
                            self.record
                                .as_ref()
                                .map_or(OperationKindV2::Start, |record| {
                                    if record.status.observed == ObservedStateV2::Paused {
                                        OperationKindV2::Resume
                                    } else {
                                        OperationKindV2::Pause
                                    }
                                });
                        self.run_operation(operation, Duration::from_secs(30), "电源操作");
                        self.show_power_menu = false;
                    }
                    if ui.button("重启").clicked() {
                        self.run_operation(
                            OperationKindV2::Restart,
                            Duration::from_mins(11),
                            "重启",
                        );
                        self.show_power_menu = false;
                    }
                    if ui.button("正常关机").clicked() {
                        self.run_operation(
                            OperationKindV2::Stop {
                                mode: StopModeV2::Graceful,
                                graceful_timeout_ms: 20_000,
                            },
                            Duration::from_secs(90),
                            "关机",
                        );
                        self.show_power_menu = false;
                    }
                });
        }
    }

    fn settings_viewport(&mut self, context: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let record = self.record.clone();
        let still_open = context.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("hd-player-settings", self.instance_id)),
            egui::ViewportBuilder::default()
                .with_title("HD Player 设置")
                .with_inner_size([560.0, 640.0]),
            |ui, _| {
                if ui.ctx().input(|input| input.viewport().close_requested()) {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    return false;
                }
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("HD Player 设置");
                    ui.separator();
                    for (title, detail) in [
                        ("常规", "关闭行为、语言和更新"),
                        ("性能", "CPU、内存和启动模式由 Manager 统一保存"),
                        ("显示", "分辨率、DPI 与刷新率变更需要重启实例"),
                        ("Android / ADB", "ADB 端口与调试选项"),
                        ("制品与启动", "已签名 Guest 与 Host 制品"),
                        ("设备模拟", "位置、传感器、网络、Bluetooth 和 NFC"),
                        ("诊断", "日志、质量门和导出"),
                        ("高级", "实验性运行时选项"),
                        ("Player 偏好", "工具栏和关闭策略"),
                    ] {
                        ui.collapsing(title, |ui| ui.label(detail));
                    }
                    if let Some(record) = &record {
                        ui.separator();
                        ui.monospace(format!("实例：{}", record.spec.name));
                        ui.monospace(format!(
                            "显示：{} × {} @ {} dpi",
                            record.spec.display.oriented_size().0,
                            record.spec.display.oriented_size().1,
                            record.spec.display.dpi
                        ));
                    }
                    ui.separator();
                    ui.label("完整可编辑设置位于 HD Manager；两者共享同一实例配置。");
                });
                true
            },
        );
        self.settings_open = still_open;
    }
}

impl eframe::App for PlayerApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_messages();
        self.sync_window_title(context);
        self.refresh_record();
        self.sync_display();
        self.handle_close_request(context);
        if self.close_requested && !self.allow_close {
            self.shutdown_then_close();
        }
        if self.allow_close {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        context.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let window_rect = ui.max_rect();
        let context = ui.ctx().clone();
        egui::Panel::top("player-custom-titlebar")
            .exact_size(32.0)
            .frame(theme::player_chrome_frame())
            .show(ui, |ui| self.titlebar(ui, &context));
        egui::Panel::left("player-tools")
            .resizable(false)
            .exact_size(44.0)
            .frame(theme::player_chrome_frame())
            .show(ui, |ui| self.toolbar(ui, &context));
        egui::CentralPanel::default()
            .frame(theme::player_content_frame())
            .show(ui, |ui| self.central_display(ui));
        self.settings_viewport(ui.ctx());
        window_resize_handles(ui, window_rect, &context);
    }
}

fn status_line(record: &InstanceRecordV2) -> String {
    let state = match record.status.observed {
        ObservedStateV2::Ready => "Android 已就绪",
        ObservedStateV2::Paused => "Android 已暂停",
        ObservedStateV2::Stopped => "Android 已停止",
        ObservedStateV2::Failed | ObservedStateV2::Blocked => "Android 不可用",
        _ => "Android 正在处理",
    };
    record
        .status
        .reason
        .as_ref()
        .map_or_else(|| state.to_owned(), |reason| format!("{state}：{reason}"))
}

fn state_badge(state: &ObservedStateV2) -> (&'static str, egui::Color32) {
    match state {
        ObservedStateV2::Ready => ("运行中", theme::PLAYER_SUCCESS),
        ObservedStateV2::Paused => ("已暂停", theme::PLAYER_WARNING),
        ObservedStateV2::Stopped | ObservedStateV2::Defined => ("启动中", theme::PLAYER_ACCENT),
        ObservedStateV2::Failed | ObservedStateV2::Blocked => ("需要处理", theme::PLAYER_DANGER),
        ObservedStateV2::Deleted => ("已删除", theme::PLAYER_TEXT_MUTED),
        _ => ("处理中", theme::PLAYER_ACCENT),
    }
}

fn compact_status(value: &str) -> String {
    const MAX_CHARS: usize = 72;
    if value.chars().count() <= MAX_CHARS {
        return value.to_owned();
    }
    let mut compact = value.chars().take(MAX_CHARS - 1).collect::<String>();
    compact.push('…');
    compact
}

fn next_viewport_revision(
    previous: Option<&DisplayViewportV2>,
    width_px: u32,
    height_px: u32,
    dpi: u32,
    visible: bool,
) -> u64 {
    previous.map_or(1, |previous| {
        if previous.width_px == width_px
            && previous.height_px == height_px
            && previous.dpi == dpi
            && previous.visible == visible
        {
            previous.revision
        } else {
            previous.revision.saturating_add(1)
        }
    })
}

#[derive(Clone, Copy)]
enum TitlebarGlyph {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn titlebar_button(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    id_salt: &'static str,
    tooltip: &'static str,
    glyph: TitlebarGlyph,
) -> bool {
    let response = ui.interact(rect, ui.id().with(id_salt), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tooltip));
    let hovered = response.hovered();
    let pressed = response.is_pointer_button_down_on();
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id.with("hover"), hovered, 0.14);
    if pressed {
        ui.painter()
            .rect_filled(rect, 0.0, theme::PLAYER_TOOL_ACTIVE);
    } else if hover > 0.0 {
        let fill = egui::Color32::from_rgba_unmultiplied(
            theme::PLAYER_TOOL_HOVER.r(),
            theme::PLAYER_TOOL_HOVER.g(),
            theme::PLAYER_TOOL_HOVER.b(),
            (242.0 * hover) as u8,
        );
        ui.painter().rect_filled(rect, 0.0, fill);
    }
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.shrink(2.0),
            0.0,
            egui::Stroke::new(1.0, theme::PLAYER_ACCENT),
            egui::StrokeKind::Inside,
        );
    }

    let scale = if pressed { 0.86 } else { 1.0 };
    let center = rect.center();
    let stroke = egui::Stroke::new(1.25, theme::PLAYER_ICON);
    match glyph {
        TitlebarGlyph::Minimize => {
            let half = 5.0 * scale;
            ui.painter().line_segment(
                [
                    egui::pos2(center.x - half, center.y + 2.5),
                    egui::pos2(center.x + half, center.y + 2.5),
                ],
                stroke,
            );
        }
        TitlebarGlyph::Maximize => {
            let glyph = egui::Rect::from_center_size(center, egui::vec2(9.0, 9.0) * scale);
            ui.painter()
                .rect_stroke(glyph, 0.0, stroke, egui::StrokeKind::Inside);
        }
        TitlebarGlyph::Restore => {
            let size = egui::vec2(8.0, 8.0) * scale;
            let back = egui::Rect::from_center_size(center + egui::vec2(2.0, -2.0), size);
            let front = egui::Rect::from_center_size(center + egui::vec2(-2.0, 2.0), size);
            ui.painter()
                .rect_stroke(back, 0.0, stroke, egui::StrokeKind::Inside);
            ui.painter().rect_filled(front, 0.0, theme::PLAYER_PANEL);
            ui.painter()
                .rect_stroke(front, 0.0, stroke, egui::StrokeKind::Inside);
        }
        TitlebarGlyph::Close => {
            let half = 4.5 * scale;
            ui.painter().line_segment(
                [
                    egui::pos2(center.x - half, center.y - half),
                    egui::pos2(center.x + half, center.y + half),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    egui::pos2(center.x + half, center.y - half),
                    egui::pos2(center.x - half, center.y + half),
                ],
                stroke,
            );
        }
    }

    let clicked = response.clicked();
    response.on_hover_text(tooltip);
    clicked
}

fn window_resize_handles(ui: &mut egui::Ui, rect: egui::Rect, context: &egui::Context) {
    let viewport = context.input(|input| input.viewport().clone());
    if viewport.maximized.unwrap_or(false) || viewport.fullscreen.unwrap_or(false) {
        return;
    }
    let edge = 5.0;
    let corner = 10.0;
    let handles = [
        (
            egui::Rect::from_min_size(rect.min, egui::vec2(corner, corner)),
            egui::ResizeDirection::NorthWest,
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            egui::Rect::from_min_size(
                egui::pos2(rect.right() - corner, rect.top()),
                egui::vec2(corner, corner),
            ),
            egui::ResizeDirection::NorthEast,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            egui::Rect::from_min_size(
                egui::pos2(rect.left(), rect.bottom() - corner),
                egui::vec2(corner, corner),
            ),
            egui::ResizeDirection::SouthWest,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            egui::Rect::from_min_size(
                egui::pos2(rect.right() - corner, rect.bottom() - corner),
                egui::vec2(corner, corner),
            ),
            egui::ResizeDirection::SouthEast,
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.left() + corner, rect.top()),
                egui::pos2(rect.right() - corner, rect.top() + edge),
            ),
            egui::ResizeDirection::North,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.left() + corner, rect.bottom() - edge),
                egui::pos2(rect.right() - corner, rect.bottom()),
            ),
            egui::ResizeDirection::South,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.top() + corner),
                egui::pos2(rect.left() + edge, rect.bottom() - corner),
            ),
            egui::ResizeDirection::West,
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.right() - edge, rect.top() + corner),
                egui::pos2(rect.right(), rect.bottom() - corner),
            ),
            egui::ResizeDirection::East,
            egui::CursorIcon::ResizeHorizontal,
        ),
    ];
    for (index, (handle, direction, cursor)) in handles.into_iter().enumerate() {
        let response = ui.interact(
            handle,
            ui.id().with(("player-window-resize", index)),
            egui::Sense::drag(),
        );
        if response.hovered() || response.dragged() {
            context.set_cursor_icon(cursor);
        }
        if response.drag_started_by(egui::PointerButton::Primary) {
            context.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}

fn icon_button(
    ui: &mut egui::Ui,
    icon: egui::ImageSource<'static>,
    tooltip: &str,
    enabled: bool,
) -> bool {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), sense);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tooltip));

    let clicked = enabled && response.clicked();
    let now = ui.input(|input| input.time);
    let click_id = response.id.with("click");
    if clicked {
        ui.ctx().data_mut(|data| data.insert_temp(click_id, now));
    }
    let click_progress = ui
        .ctx()
        .data(|data| data.get_temp::<f64>(click_id))
        .map(|started| ((now - started) / 0.18).clamp(0.0, 1.0) as f32)
        .filter(|progress| *progress < 1.0);
    if click_progress.is_some() {
        ui.ctx().request_repaint();
    }
    let click_wave = click_progress.map_or(0.0, |progress| (std::f32::consts::PI * progress).sin());

    // Disabled guest actions still acknowledge hover so every toolbar glyph feels responsive,
    // while their muted tint and non-clickable sense preserve the disabled semantic.
    let hovered = response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id.with("hover"), hovered, 0.14);
    let icon_scale = if pressed {
        0.86
    } else {
        1.0 - 0.1 * click_wave
    };

    if pressed {
        ui.painter()
            .rect_filled(rect, 0.0, theme::PLAYER_TOOL_ACTIVE);
    } else if hover > 0.0 {
        let fill = egui::Color32::from_rgba_unmultiplied(
            theme::PLAYER_TOOL_HOVER.r(),
            theme::PLAYER_TOOL_HOVER.g(),
            theme::PLAYER_TOOL_HOVER.b(),
            ((if enabled { 242.0 } else { 120.0 }) * hover) as u8,
        );
        ui.painter().rect_filled(rect, 0.0, fill);
    }
    if click_wave > 0.0 {
        let pulse = egui::Color32::from_rgba_unmultiplied(
            theme::PLAYER_ACCENT.r(),
            theme::PLAYER_ACCENT.g(),
            theme::PLAYER_ACCENT.b(),
            (90.0 * click_wave) as u8,
        );
        ui.painter().rect_stroke(
            rect.shrink(1.0 + click_wave),
            0.0,
            egui::Stroke::new(1.0, pulse),
            egui::StrokeKind::Inside,
        );
    }
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.shrink(2.0),
            0.0,
            egui::Stroke::new(1.0, theme::PLAYER_ACCENT),
            egui::StrokeKind::Inside,
        );
    }

    let icon_rect =
        egui::Rect::from_center_size(rect.center(), egui::vec2(16.0, 16.0) * icon_scale);
    egui::Image::new(icon)
        .tint(if enabled {
            theme::PLAYER_ICON
        } else {
            theme::PLAYER_ICON_DISABLED
        })
        .paint_at(ui, icon_rect);

    response.on_hover_text(tooltip);
    clicked
}

fn toolbar_separator(ui: &mut egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), rect.center().y),
            egui::pos2(rect.right(), rect.center().y),
        ],
        egui::Stroke::new(1.0, theme::PLAYER_BORDER),
    );
}

#[cfg(windows)]
struct NativeViewport {
    hwnd: windows_sys::Win32::Foundation::HWND,
}

#[cfg(windows)]
impl NativeViewport {
    fn new(creation: &eframe::CreationContext<'_>) -> Result<Self> {
        use std::ptr;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
        };
        let parent = match creation.window_handle()?.as_raw() {
            RawWindowHandle::Win32(handle) => {
                handle.hwnd.get() as windows_sys::Win32::Foundation::HWND
            }
            _ => anyhow::bail!("HD Player requires a Win32 root window"),
        };
        install_windows_backdrop(parent);
        let class = widestring("STATIC");
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                ptr::null(),
                WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                0,
                0,
                1,
                1,
                parent,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            anyhow::bail!("create Player native display child HWND failed")
        }
        Ok(Self { hwnd })
    }

    fn handle(&self) -> u64 {
        self.hwnd as usize as u64
    }

    fn place(&self, rect: egui::Rect, pixels_per_point: f32, visible: bool) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SetWindowPos,
            ShowWindow,
        };
        let x = (rect.min.x * pixels_per_point).round() as i32;
        let y = (rect.min.y * pixels_per_point).round() as i32;
        let width = (rect.width() * pixels_per_point).round().max(1.0) as i32;
        let height = (rect.height() * pixels_per_point).round().max(1.0) as i32;
        unsafe {
            SetWindowPos(
                self.hwnd,
                std::ptr::null_mut(),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOZORDER,
            );
            ShowWindow(self.hwnd, if visible { SW_SHOW } else { SW_HIDE });
        }
    }

    fn focus_guest(&self) {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        use windows_sys::Win32::UI::WindowsAndMessaging::{GW_CHILD, GetWindow};
        // The crosvm window is the first child after a successful attach. If it has not attached
        // yet, focus the private viewport so the next guest attach receives normal input focus.
        unsafe {
            let child = GetWindow(self.hwnd, GW_CHILD);
            SetFocus(if child.is_null() { self.hwnd } else { child });
        }
    }
}

#[cfg(windows)]
fn install_windows_backdrop(hwnd: windows_sys::Win32::Foundation::HWND) {
    use std::ffi::c_void;
    use windows_sys::Win32::Graphics::Dwm::{
        DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DwmSetWindowAttribute,
    };

    // Windows 11 composes this documented transient backdrop behind our transparent host chrome.
    // Earlier Windows versions reject the attribute; their transparent neutral tint is the
    // intentional visual fallback rather than an error condition.
    let backdrop = DWMSBT_TRANSIENTWINDOW;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE as u32,
            (&raw const backdrop).cast::<c_void>(),
            std::mem::size_of_val(&backdrop) as u32,
        );
    }
}

#[cfg(windows)]
impl Drop for NativeViewport {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd) };
    }
}

#[cfg(not(windows))]
struct NativeViewport;

#[cfg(not(windows))]
impl NativeViewport {
    fn new(_creation: &eframe::CreationContext<'_>) -> Result<Self> {
        anyhow::bail!("HD Player native display embedding is currently Windows-only")
    }
    const fn handle(&self) -> u64 {
        0
    }
    fn place(&self, _rect: egui::Rect, _pixels_per_point: f32, _visible: bool) {}
    fn focus_guest(&self) {}
}

#[cfg(windows)]
fn widestring(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

#[cfg(windows)]
struct ExistingPlayerSearch {
    marker: Vec<u16>,
    hwnd: windows_sys::Win32::Foundation::HWND,
}

#[cfg(windows)]
unsafe extern "system" fn find_existing_player_callback(
    hwnd: windows_sys::Win32::Foundation::HWND,
    lparam: isize,
) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowTextLengthW, GetWindowTextW};

    // SAFETY: EnumWindows synchronously invokes the callback with the live search pointer below.
    let search = unsafe { &mut *(lparam as *mut ExistingPlayerSearch) };
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return 1;
    }
    let mut title = vec![0_u16; length as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
    if copied > 0
        && title[..copied as usize]
            .windows(search.marker.len())
            .any(|candidate| candidate == search.marker)
    {
        search.hwnd = hwnd;
        return 0;
    }
    1
}

#[cfg(windows)]
fn focus_existing_player(title_marker: &str) -> bool {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };
    let mut search = ExistingPlayerSearch {
        marker: std::ffi::OsStr::new(title_marker).encode_wide().collect(),
        hwnd: std::ptr::null_mut(),
    };
    unsafe {
        EnumWindows(
            Some(find_existing_player_callback),
            (&raw mut search) as isize,
        );
    }
    if search.hwnd.is_null() {
        return false;
    }
    unsafe {
        ShowWindow(search.hwnd, SW_RESTORE);
        SetForegroundWindow(search.hwnd);
    }
    true
}

#[cfg(not(windows))]
fn focus_existing_player(_title: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_viewport_keeps_revision_stable() {
        let previous = DisplayViewportV2 {
            width_px: 1280,
            height_px: 720,
            dpi: 120,
            visible: true,
            revision: 17,
        };
        assert_eq!(
            next_viewport_revision(Some(&previous), 1280, 720, 120, true),
            17
        );
    }

    #[test]
    fn viewport_change_advances_revision_once() {
        let previous = DisplayViewportV2 {
            width_px: 1280,
            height_px: 720,
            dpi: 120,
            visible: true,
            revision: 17,
        };
        assert_eq!(
            next_viewport_revision(Some(&previous), 1280, 720, 144, true),
            18
        );
    }
}
