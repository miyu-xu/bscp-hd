#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui;
use hd_core::{
    ControlCommandV1, ControlPayloadV1, ControlRequestV1, InstanceAction, InstanceConfigV1,
    InstanceSummaryV1, Orientation, VsyncMode,
};
use hd_platform::{
    DataPaths, DisplayEmbedder, DisplayRect, NativeDisplayEmbedder, NativeWindowBinding,
    PlatformDisplayLease,
};
use hd_runtime::{CrosvmBackend, Supervisor, control_endpoint, run_control_server};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tokio::runtime::Runtime;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

fn main() -> Result<()> {
    let paths = DataPaths::discover().context("discover HD data directory")?;
    paths.ensure().context("create HD data directories")?;
    let _log_guard = init_logging(&paths)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hd-runtime")
        .build()
        .context("create HD runtime")?;
    let repo_root = discover_repo_root();
    let supervisor = Arc::new(
        Supervisor::new(paths, CrosvmBackend::discover(repo_root.as_deref()))
            .context("create HD supervisor")?,
    );
    let endpoint = control_endpoint().context("derive local control endpoint")?;
    let server_supervisor = Arc::clone(&supervisor);
    runtime.spawn(async move {
        if let Err(error) = run_control_server(server_supervisor, &endpoint).await {
            tracing::error!(%error, "control server stopped");
        }
    });

    let mut options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("HD Android")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    #[cfg(windows)]
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    }

    eframe::run_native(
        "HD Android",
        options,
        Box::new(move |creation_context| {
            Ok(Box::new(HdApp::new(creation_context, runtime, supervisor)))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn init_logging(paths: &DataPaths) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let appender = tracing_appender::rolling::daily(&paths.logs, "hd.jsonl");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(writer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize structured logging: {error}"))?;
    Ok(guard)
}

fn discover_repo_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("BSCP_ROOT") {
        return Some(PathBuf::from(root));
    }
    let current = std::env::current_dir().ok()?;
    for candidate in current.ancestors() {
        if candidate.join("build_all.bat").is_file() && candidate.join("external/crosvm").is_dir() {
            return Some(candidate.to_owned());
        }
    }
    None
}

struct HdApp {
    runtime: Runtime,
    supervisor: Arc<Supervisor>,
    embedder: NativeDisplayEmbedder,
    root_window: NativeWindowBinding,
    viewport_leases: HashMap<Uuid, PlatformDisplayLease>,
    viewport_rect: DisplayRect,
    summaries: Vec<InstanceSummaryV1>,
    selected: Option<Uuid>,
    draft: Option<InstanceConfigV1>,
    extra_args: String,
    adb_path: String,
    apk_path: String,
    mock_start: bool,
    status: String,
    last_refresh: Instant,
    shutdown_sent: bool,
}

impl HdApp {
    fn new(
        creation_context: &eframe::CreationContext<'_>,
        runtime: Runtime,
        supervisor: Arc<Supervisor>,
    ) -> Self {
        let root_window = native_binding(creation_context);
        let summaries = runtime.block_on(supervisor.summaries());
        let selected = summaries.first().map(|summary| summary.id);
        let draft = selected.and_then(|id| runtime.block_on(supervisor.config(id)));
        let extra_args = draft
            .as_ref()
            .map(|config| config.extra_kernel_args.join(" "))
            .unwrap_or_default();
        let adb_path = draft
            .as_ref()
            .and_then(|config| config.adb.adb_path.as_ref())
            .map_or_else(String::new, |path| path.display().to_string());
        Self {
            runtime,
            supervisor,
            embedder: NativeDisplayEmbedder::default(),
            root_window,
            viewport_leases: HashMap::new(),
            viewport_rect: DisplayRect {
                x: 280,
                y: 88,
                width: 1080,
                height: 760,
            },
            summaries,
            selected,
            draft,
            extra_args,
            adb_path,
            apk_path: String::new(),
            mock_start: true,
            status: "就绪；阶段 0 可使用 mock 后端验收完整流程".to_owned(),
            last_refresh: Instant::now(),
            shutdown_sent: false,
        }
    }

    fn refresh(&mut self) {
        self.summaries = self.runtime.block_on(self.supervisor.summaries());
        if self
            .selected
            .is_some_and(|id| !self.summaries.iter().any(|summary| summary.id == id))
        {
            self.select(self.summaries.first().map(|summary| summary.id));
        }
        self.last_refresh = Instant::now();
    }

    fn select(&mut self, id: Option<Uuid>) {
        self.selected = id;
        self.draft = id.and_then(|id| self.runtime.block_on(self.supervisor.config(id)));
        self.extra_args = self
            .draft
            .as_ref()
            .map(|config| config.extra_kernel_args.join(" "))
            .unwrap_or_default();
        self.adb_path = self
            .draft
            .as_ref()
            .and_then(|config| config.adb.adb_path.as_ref())
            .map_or_else(String::new, |path| path.display().to_string());
    }

    fn execute(&mut self, command: ControlCommandV1) -> bool {
        let response = self
            .runtime
            .block_on(self.supervisor.handle(ControlRequestV1::new(command)));
        if response.ok {
            self.status = match response.payload {
                Some(ControlPayloadV1::Diagnosis(diagnosis)) => diagnosis
                    .checks
                    .iter()
                    .map(|check| format!("{}: {:?} — {}", check.name, check.status, check.detail))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => "操作成功".to_owned(),
            };
            self.refresh();
            true
        } else {
            self.status = response.error.map_or_else(
                || "未知错误".to_owned(),
                |error| format!("{}: {}", error.code, error.message),
            );
            false
        }
    }

    fn ensure_viewport(&mut self, id: Uuid) -> bool {
        if self.viewport_leases.contains_key(&id) {
            return true;
        }
        match self.embedder.acquire(&self.root_window, self.viewport_rect) {
            Ok(lease) => {
                if let Err(error) = self
                    .runtime
                    .block_on(self.supervisor.set_display_lease(id, Some(lease.clone())))
                {
                    let _ = self.embedder.release(lease.contract.lease_id);
                    self.status = format!("绑定显示窗口失败: {error}");
                    return false;
                }
                self.viewport_leases.insert(id, lease);
                true
            }
            Err(error) => {
                self.status = format!("创建显示窗口失败: {error}");
                false
            }
        }
    }

    fn release_viewport(&mut self, id: Uuid) {
        if let Some(lease) = self.viewport_leases.remove(&id) {
            let _ = self
                .runtime
                .block_on(self.supervisor.set_display_lease(id, None));
            if let Err(error) = self.embedder.release(lease.contract.lease_id) {
                self.status = format!("释放显示窗口失败: {error}");
            }
        }
    }

    fn update_native_viewports(&mut self) {
        for (id, lease) in &self.viewport_leases {
            let rect = if Some(*id) == self.selected {
                self.viewport_rect
            } else {
                DisplayRect {
                    x: -32_000,
                    y: -32_000,
                    width: 1,
                    height: 1,
                }
            };
            if let Err(error) = self.embedder.resize(lease.contract.lease_id, rect) {
                tracing::warn!(%id, %error, "failed to resize native viewport");
            }
        }
    }

    fn save_draft(&mut self) {
        let Some(mut draft) = self.draft.clone() else {
            return;
        };
        draft.extra_kernel_args = self
            .extra_args
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        draft.adb.adb_path = if self.adb_path.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(self.adb_path.trim()))
        };
        if self.execute(ControlCommandV1::Update {
            config: draft.clone(),
        }) {
            self.draft = Some(draft);
        }
    }

    fn start_instance(&mut self, id: Uuid) {
        let mock = self.mock_start;
        if !mock && !self.ensure_viewport(id) {
            return;
        }
        if !self.execute(ControlCommandV1::Start { id, mock }) && !mock {
            self.release_viewport(id);
        }
    }

    fn draw_toolbar(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("toolbar")
            .exact_size(52.0)
            .show(root, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.heading("HD");
                    ui.separator();
                    let selected = self.selected;
                    let start = ui
                        .add_enabled(selected.is_some(), egui::Button::new("启动"))
                        .clicked();
                    let stop = ui
                        .add_enabled(selected.is_some(), egui::Button::new("停止"))
                        .clicked();
                    ui.checkbox(&mut self.mock_start, "Mock 启动");
                    ui.separator();
                    for (label, action) in [
                        ("主页", InstanceAction::Home),
                        ("最近", InstanceAction::Recent),
                        ("返回", InstanceAction::Back),
                        ("旋转", InstanceAction::Rotate),
                        ("电源", InstanceAction::Power),
                        ("音量+", InstanceAction::VolumeUp),
                        ("音量-", InstanceAction::VolumeDown),
                    ] {
                        if ui
                            .add_enabled(selected.is_some(), egui::Button::new(label))
                            .clicked()
                            && let Some(id) = selected
                        {
                            self.execute(ControlCommandV1::Action { id, action });
                        }
                    }
                    if let Some(summary) = selected
                        .and_then(|id| self.summaries.iter().find(|summary| summary.id == id))
                    {
                        ui.separator();
                        ui.label(format!("{:?}", summary.state.state));
                        if let Some(fps) = summary.host_fps_milli {
                            ui.monospace(format!("{:.1} FPS", f64::from(fps) / 1000.0));
                        }
                    }
                    if start && let Some(id) = selected {
                        self.start_instance(id);
                    }
                    if stop
                        && let Some(id) = selected
                        && self.execute(ControlCommandV1::Stop { id })
                    {
                        self.release_viewport(id);
                    }
                });
            });
    }

    fn draw_instances(&mut self, root: &mut egui::Ui) {
        egui::Panel::left("instances")
            .resizable(true)
            .default_size(260.0)
            .min_size(220.0)
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("实例");
                    if ui.button("＋").on_hover_text("新建实例").clicked() {
                        let count = self.summaries.len() + 1;
                        let config = InstanceConfigV1 {
                            name: format!("Android {count}"),
                            ..Default::default()
                        };
                        let id = config.id;
                        if self.execute(ControlCommandV1::Create { config }) {
                            self.select(Some(id));
                        }
                    }
                });
                ui.separator();
                let summaries = self.summaries.clone();
                for summary in summaries {
                    let selected = self.selected == Some(summary.id);
                    let label = format!("{}\n{:?}", summary.name, summary.state.state);
                    if ui.selectable_label(selected, label).clicked() {
                        self.select(Some(summary.id));
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    if let Some(id) = self.selected {
                        if ui.button("诊断").clicked() {
                            self.execute(ControlCommandV1::Diagnose { id });
                        }
                        if ui.button("删除已停止实例").clicked()
                            && self.execute(ControlCommandV1::Delete { id })
                        {
                            self.release_viewport(id);
                            self.select(None);
                        }
                    }
                });
            });
    }

    #[allow(clippy::too_many_lines)]
    fn draw_settings(&mut self, root: &mut egui::Ui) {
        let mut save_requested = false;
        let mut display_requested = None;
        let mut install_requested = false;
        let selected = self.selected;
        let draft_slot = &mut self.draft;
        let adb_path = &mut self.adb_path;
        let extra_args = &mut self.extra_args;
        let apk_path = &mut self.apk_path;
        egui::Panel::right("settings")
            .resizable(true)
            .default_size(330.0)
            .min_size(280.0)
            .show(root, |ui| {
                ui.heading("启动与设备设置");
                ui.separator();
                let Some(draft) = draft_slot.as_mut() else {
                    ui.label("选择一个实例后编辑设置");
                    return;
                };
                egui::ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("settings_grid")
                        .num_columns(2)
                        .spacing([12.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("名称");
                            ui.text_edit_singleline(&mut draft.name);
                            ui.end_row();
                            ui.label("CPU");
                            ui.add(egui::DragValue::new(&mut draft.cpu_count).range(1..=256));
                            ui.end_row();
                            ui.label("内存 MiB");
                            ui.add(
                                egui::DragValue::new(&mut draft.memory_mib)
                                    .speed(128)
                                    .range(512..=1_048_576),
                            );
                            ui.end_row();
                            ui.label("分辨率");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::DragValue::new(&mut draft.display.width)
                                        .range(320..=16_384),
                                );
                                ui.label("×");
                                ui.add(
                                    egui::DragValue::new(&mut draft.display.height)
                                        .range(320..=16_384),
                                );
                            });
                            ui.end_row();
                            ui.label("DPI");
                            ui.add(egui::DragValue::new(&mut draft.display.dpi).range(72..=960));
                            ui.end_row();
                            ui.label("刷新率");
                            ui.add(
                                egui::DragValue::new(&mut draft.display.refresh_rate_hz)
                                    .range(1..=1000)
                                    .suffix(" Hz"),
                            );
                            ui.end_row();
                            ui.label("方向");
                            ui.horizontal(|ui| {
                                ui.selectable_value(
                                    &mut draft.display.orientation,
                                    Orientation::Landscape,
                                    "横屏",
                                );
                                ui.selectable_value(
                                    &mut draft.display.orientation,
                                    Orientation::Portrait,
                                    "竖屏",
                                );
                            });
                            ui.end_row();
                            ui.label("垂直同步");
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut draft.display.vsync, VsyncMode::On, "开");
                                ui.selectable_value(&mut draft.display.vsync, VsyncMode::Off, "关");
                            })
                            .response
                            .on_hover_text("VSync 在下次启动时生效；运行中不会静默伪装为已应用");
                            ui.end_row();
                            ui.label("宿主 FPS");
                            ui.checkbox(&mut draft.display.show_host_fps, "显示");
                            ui.end_row();
                            ui.label("ADB");
                            ui.checkbox(&mut draft.adb.enabled, "启用");
                            ui.end_row();
                            ui.label("ADB 端口");
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut draft.adb.auto_port, "自动");
                                let port = draft.adb.host_port.get_or_insert(6520);
                                ui.add_enabled(
                                    !draft.adb.auto_port,
                                    egui::DragValue::new(port).range(1..=u16::MAX),
                                );
                            });
                            ui.end_row();
                            ui.label("ADB root");
                            ui.checkbox(&mut draft.adb.auto_root, "自动 root");
                            ui.end_row();
                        });
                    ui.label("ADB 路径（空值使用 PATH）");
                    ui.text_edit_singleline(adb_path);
                    ui.label("额外 kernel 参数");
                    ui.text_edit_multiline(extra_args);
                    ui.collapsing("外部构件（HD 不下载、不构建）", |ui| {
                        path_editor(ui, "Kernel", &mut draft.artifacts.kernel);
                        path_editor(ui, "Initrd", &mut draft.artifacts.initrd);
                        path_editor(ui, "Rootfs", &mut draft.artifacts.rootfs);
                        path_editor(ui, "Fstab", &mut draft.artifacts.android_fstab);
                    });
                    ui.horizontal(|ui| {
                        if ui.button("保存启动设置").clicked() {
                            save_requested = true;
                        }
                        if ui.button("仅应用显示").clicked() {
                            display_requested = Some(draft.display.clone());
                        }
                    });
                    ui.separator();
                    ui.label("安装 APK");
                    ui.text_edit_singleline(apk_path);
                    if ui.button("安装").clicked() {
                        install_requested = true;
                    }
                });
            });
        if save_requested {
            self.save_draft();
        }
        if let (Some(id), Some(display)) = (selected, display_requested) {
            self.execute(ControlCommandV1::ApplyDisplay { id, display });
        }
        if install_requested && let Some(id) = selected {
            self.execute(ControlCommandV1::InstallApk {
                id,
                path: PathBuf::from(self.apk_path.trim()),
            });
        }
    }

    fn draw_center(&mut self, root: &mut egui::Ui) {
        let scale = root.ctx().pixels_per_point();
        egui::CentralPanel::default().show(root, |ui| {
            let available = ui.available_rect_before_wrap();
            self.viewport_rect = DisplayRect {
                x: physical_position(available.min.x * scale),
                y: physical_position(available.min.y * scale),
                width: physical_extent(available.width() * scale),
                height: physical_extent(available.height() * scale),
            };
            let painter = ui.painter();
            painter.rect_filled(available, 8.0, egui::Color32::from_rgb(16, 19, 24));
            if self.selected.is_none() {
                painter.text(
                    available.center(),
                    egui::Align2::CENTER_CENTER,
                    "新建或选择实例",
                    egui::FontId::proportional(24.0),
                    egui::Color32::GRAY,
                );
            } else if self
                .selected
                .is_some_and(|id| !self.viewport_leases.contains_key(&id))
            {
                let selected_state = self.selected.and_then(|id| {
                    self.summaries
                        .iter()
                        .find(|summary| summary.id == id)
                        .map(|summary| summary.state.state)
                });
                let message = if selected_state == Some(hd_core::InstanceState::Ready) {
                    "Mock 实例已就绪\n可验证控制、设置、日志和多实例流程"
                } else {
                    "Android 显示区域\n真实启动后由 crosvm/gfxstream 子窗口接管"
                };
                painter.text(
                    available.center(),
                    egui::Align2::CENTER_CENTER,
                    message,
                    egui::FontId::proportional(20.0),
                    egui::Color32::LIGHT_GRAY,
                );
            }
        });
    }

    fn draw_status(&mut self, root: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .resizable(true)
            .default_size(36.0)
            .show(root, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.monospace(&self.status);
                });
            });
    }
}

impl eframe::App for HdApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.supervisor.should_shutdown() {
            self.shutdown_sent = true;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.last_refresh.elapsed() >= Duration::from_millis(500) {
            self.refresh();
        }
        self.draw_toolbar(ui);
        self.draw_instances(ui);
        self.draw_settings(ui);
        self.draw_status(ui);
        self.draw_center(ui);
        self.update_native_viewports();
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}

impl Drop for HdApp {
    fn drop(&mut self) {
        if !self.shutdown_sent {
            let _ = self.runtime.block_on(
                self.supervisor
                    .handle(ControlRequestV1::new(ControlCommandV1::Shutdown)),
            );
            self.shutdown_sent = true;
        }
    }
}

fn path_editor(ui: &mut egui::Ui, label: &str, path: &mut PathBuf) {
    let mut value = path.display().to_string();
    ui.label(label);
    if ui.text_edit_singleline(&mut value).changed() {
        *path = PathBuf::from(value);
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn physical_position(value: f32) -> i32 {
    if value.is_finite() {
        value.round().clamp(i32::MIN as f32, i32::MAX as f32) as i32
    } else {
        0
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn physical_extent(value: f32) -> u32 {
    if value.is_finite() {
        value.round().clamp(1.0, u32::MAX as f32) as u32
    } else {
        1
    }
}

fn native_binding(context: &eframe::CreationContext<'_>) -> NativeWindowBinding {
    let Ok(handle) = context.window_handle() else {
        return NativeWindowBinding::Unsupported;
    };
    match handle.as_raw() {
        #[cfg(windows)]
        RawWindowHandle::Win32(handle) => u64::try_from(handle.hwnd.get()).map_or(
            NativeWindowBinding::Unsupported,
            NativeWindowBinding::Win32Hwnd,
        ),
        #[cfg(target_os = "linux")]
        RawWindowHandle::Xlib(handle) => NativeWindowBinding::X11Window(handle.window),
        #[cfg(target_os = "linux")]
        RawWindowHandle::Wayland(handle) => {
            NativeWindowBinding::WaylandSurface(format!("{:p}", handle.surface.as_ptr()))
        }
        #[cfg(target_os = "macos")]
        RawWindowHandle::AppKit(handle) => {
            NativeWindowBinding::AppKitView(handle.ns_view.as_ptr() as usize as u64)
        }
        _ => NativeWindowBinding::Unsupported,
    }
}
