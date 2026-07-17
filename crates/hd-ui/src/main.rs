#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use eframe::egui;
use hd_core::{
    AdbModeV2, ArtifactSelectionV2, CreateInstanceRequestV2, DiagnosticRequestV2,
    HostCapabilitiesV2, InstanceActionV2, InstanceRecordV2, InstanceSpecV2, InstanceSummaryV2,
    KeyActionV2, OperationKindV2, OrientationV2, RestartPolicyV2, StopModeV2,
    UpdateInstanceRequestV2, VsyncModeV2,
};
use hd_platform::DataPaths;
use hd_runtime::HostClientV2;
use tokio::runtime::Runtime;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

fn main() -> Result<()> {
    let paths = DataPaths::discover().context("discover HD data directory")?;
    paths.ensure().context("create HD data directories")?;
    let _log_guard = init_logging(&paths)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hd-ui-runtime")
        .build()
        .context("create HD UI runtime")?;
    let client = runtime
        .block_on(HostClientV2::connect_or_start(paths))
        .context("connect to HD host")?;

    let mut options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("HD Android")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1040.0, 680.0]),
        ..Default::default()
    };
    #[cfg(windows)]
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    }
    eframe::run_native(
        "HD Android",
        options,
        Box::new(move |_context| Ok(Box::new(HdApp::new(runtime, client)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn init_logging(paths: &DataPaths) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let appender = tracing_appender::rolling::daily(&paths.logs, "ui-v2.jsonl");
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

#[derive(Debug)]
struct Snapshot {
    summaries: Vec<InstanceSummaryV2>,
    selected: Option<InstanceRecordV2>,
    capabilities: HostCapabilitiesV2,
}

#[derive(Debug)]
enum UiMessage {
    Snapshot(Result<Box<Snapshot>, String>),
    Completed(Result<String, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelTab {
    Display,
    Settings,
    Devices,
    Diagnostics,
}

struct HdApp {
    runtime: Runtime,
    client: HostClientV2,
    sender: Sender<UiMessage>,
    receiver: Receiver<UiMessage>,
    pending: usize,
    summaries: Vec<InstanceSummaryV2>,
    selected_id: Option<Uuid>,
    selected_record: Option<InstanceRecordV2>,
    draft: Option<InstanceSpecV2>,
    capabilities: Option<HostCapabilitiesV2>,
    tab: PanelTab,
    status: String,
    last_refresh: Instant,
    refresh_requested: bool,
    artifact_store: String,
    guest_digest: String,
    host_digest: String,
    adb_executable: String,
    apk_path: String,
    new_name: String,
}

impl HdApp {
    fn new(runtime: Runtime, client: HostClientV2) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        let now = Instant::now();
        let mut app = Self {
            runtime,
            client,
            sender,
            receiver,
            pending: 0,
            summaries: Vec::new(),
            selected_id: None,
            selected_record: None,
            draft: None,
            capabilities: None,
            tab: PanelTab::Display,
            status: "正在读取宿主状态…".to_owned(),
            last_refresh: now.checked_sub(Duration::from_secs(10)).unwrap_or(now),
            refresh_requested: true,
            artifact_store: String::new(),
            guest_digest: String::new(),
            host_digest: String::new(),
            adb_executable: String::new(),
            apk_path: String::new(),
            new_name: "Android".to_owned(),
        };
        app.refresh();
        app
    }

    fn spawn<F>(&mut self, future: F)
    where
        F: std::future::Future<Output = UiMessage> + Send + 'static,
    {
        self.pending = self.pending.saturating_add(1);
        let sender = self.sender.clone();
        self.runtime.spawn(async move {
            let message = future.await;
            let _ = sender.send(message);
        });
    }

    fn refresh(&mut self) {
        if self.pending > 0 && !self.refresh_requested {
            return;
        }
        self.refresh_requested = false;
        self.last_refresh = Instant::now();
        let client = self.client.clone();
        let selected_id = self.selected_id;
        self.spawn(async move {
            let result = async {
                let summaries = client
                    .list_instances()
                    .await
                    .map_err(|error| error.to_string())?;
                let selected_id = selected_id
                    .filter(|id| summaries.iter().any(|summary| summary.id == *id))
                    .or_else(|| summaries.first().map(|summary| summary.id));
                let selected = match selected_id {
                    Some(id) => Some(
                        client
                            .get_instance(id)
                            .await
                            .map_err(|error| error.to_string())?,
                    ),
                    None => None,
                };
                let capabilities = client
                    .capabilities(selected_id)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(Box::new(Snapshot {
                    summaries,
                    selected,
                    capabilities,
                }))
            }
            .await;
            UiMessage::Snapshot(result)
        });
    }

    fn process_messages(&mut self) {
        while let Ok(message) = self.receiver.try_recv() {
            self.pending = self.pending.saturating_sub(1);
            match message {
                UiMessage::Snapshot(Ok(snapshot)) => {
                    let snapshot = *snapshot;
                    self.summaries = snapshot.summaries;
                    self.capabilities = Some(snapshot.capabilities);
                    let new_id = snapshot.selected.as_ref().map(|record| record.spec.id);
                    let should_replace_draft = self.selected_id != new_id || self.draft.is_none();
                    self.selected_id = new_id;
                    self.selected_record = snapshot.selected;
                    if should_replace_draft {
                        self.reset_draft();
                    }
                    "宿主状态已同步".clone_into(&mut self.status);
                }
                UiMessage::Snapshot(Err(error)) | UiMessage::Completed(Err(error)) => {
                    self.status = error;
                }
                UiMessage::Completed(Ok(message)) => {
                    self.status = message;
                    self.refresh_requested = true;
                }
            }
        }
        if self.refresh_requested && self.pending == 0 {
            self.refresh();
        }
    }

    fn select(&mut self, id: Uuid) {
        if self.selected_id != Some(id) {
            self.selected_id = Some(id);
            self.selected_record = None;
            self.draft = None;
            self.refresh_requested = true;
        }
    }

    fn reset_draft(&mut self) {
        self.draft = self
            .selected_record
            .as_ref()
            .map(|record| record.spec.clone());
        if let Some(spec) = &self.draft {
            self.adb_executable = spec
                .adb
                .executable
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string());
            if let Some(artifacts) = &spec.artifacts {
                self.artifact_store = artifacts.store_root.display().to_string();
                self.guest_digest.clone_from(&artifacts.guest_bundle_digest);
                self.host_digest.clone_from(&artifacts.host_bundle_digest);
            } else {
                self.artifact_store.clear();
                self.guest_digest.clear();
                self.host_digest.clear();
            }
        }
    }

    fn create_instance(&mut self) {
        let spec = InstanceSpecV2 {
            name: self.new_name.trim().to_owned(),
            ..InstanceSpecV2::default()
        };
        if let Err(error) = spec.validate() {
            self.status = error.to_string();
            return;
        }
        let client = self.client.clone();
        self.spawn(async move {
            UiMessage::Completed(
                client
                    .create_instance(&CreateInstanceRequestV2 { spec })
                    .await
                    .map(|record| format!("已创建实例 {}", record.spec.name))
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn save_spec(&mut self) {
        let Some(mut spec) = self.draft.clone() else {
            return;
        };
        let Some(record) = &self.selected_record else {
            return;
        };
        spec.adb.executable = non_empty_path(&self.adb_executable);
        spec.artifacts = if self.artifact_store.trim().is_empty()
            && self.guest_digest.trim().is_empty()
            && self.host_digest.trim().is_empty()
        {
            None
        } else {
            Some(ArtifactSelectionV2 {
                store_root: PathBuf::from(self.artifact_store.trim()),
                guest_bundle_digest: self.guest_digest.trim().to_owned(),
                host_bundle_digest: self.host_digest.trim().to_owned(),
            })
        };
        if let Err(error) = spec.validate() {
            self.status = error.to_string();
            return;
        }
        let expected_revision = record.status.revision;
        let id = spec.id;
        let client = self.client.clone();
        self.spawn(async move {
            UiMessage::Completed(
                client
                    .update_instance(
                        id,
                        &UpdateInstanceRequestV2 {
                            expected_revision,
                            spec,
                        },
                    )
                    .await
                    .map(|_| "实例设置已原子保存".to_owned())
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn operation(&mut self, kind: OperationKindV2, timeout: Duration, label: &'static str) {
        let Some(id) = self.selected_id else {
            return;
        };
        let client = self.client.clone();
        self.spawn(async move {
            let result = async {
                let key = format!("hd-ui-{}", Uuid::new_v4());
                let operation = client
                    .create_operation(id, kind, &key)
                    .await
                    .map_err(|error| error.to_string())?;
                client
                    .wait_operation(operation.id, timeout)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("{label}完成"))
            }
            .await;
            UiMessage::Completed(result)
        });
    }

    fn action(&mut self, action: InstanceActionV2, label: &'static str) {
        let Some(id) = self.selected_id else {
            return;
        };
        let client = self.client.clone();
        self.spawn(async move {
            UiMessage::Completed(
                client
                    .action(id, action)
                    .await
                    .map(|_| format!("{label}已送达并回读"))
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn install_apk(&mut self) {
        let Some(id) = self.selected_id else {
            return;
        };
        let path = PathBuf::from(self.apk_path.trim());
        let client = self.client.clone();
        self.spawn(async move {
            let result = async {
                let upload = client
                    .upload_apk(&path)
                    .await
                    .map_err(|error| error.to_string())?;
                let key = format!("hd-ui-install-{}", Uuid::new_v4());
                let operation = client
                    .create_operation(
                        id,
                        OperationKindV2::InstallApk {
                            upload_id: upload.id,
                            sha256: upload.sha256,
                        },
                        &key,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                client
                    .wait_operation(operation.id, Duration::from_secs(600))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok("APK 已校验、安装并回读包路径".to_owned())
            }
            .await;
            UiMessage::Completed(result)
        });
    }

    fn diagnostics(&mut self, include_guest_logs: bool) {
        let client = self.client.clone();
        let instance_id = self.selected_id;
        self.spawn(async move {
            UiMessage::Completed(
                client
                    .collect_diagnostics(&DiagnosticRequestV2 {
                        instance_id,
                        include_guest_logs,
                    })
                    .await
                    .map(|bundle| format!("诊断包：{}", bundle.path.display()))
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn sidebar(&mut self, root: &mut egui::Ui) {
        egui::Panel::left("instances")
            .resizable(true)
            .default_size(270.0)
            .show(root, |ui| {
                ui.heading("HD 实例");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.new_name);
                    if ui
                        .add_enabled(self.pending == 0, egui::Button::new("新建"))
                        .clicked()
                    {
                        self.create_instance();
                    }
                });
                ui.separator();
                let summaries = self.summaries.clone();
                for summary in summaries {
                    let selected = self.selected_id == Some(summary.id);
                    let text = format!("{}\n{:?}", summary.name, summary.status.observed);
                    if ui.selectable_label(selected, text).clicked() {
                        self.select(summary.id);
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.small(format!("待处理任务：{}", self.pending));
                    if ui.button("立即刷新").clicked() && self.pending == 0 {
                        self.refresh();
                    }
                });
            });
    }

    fn top_bar(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("toolbar").show(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (tab, label) in [
                    (PanelTab::Display, "显示与控制"),
                    (PanelTab::Settings, "启动设置"),
                    (PanelTab::Devices, "设备模拟"),
                    (PanelTab::Diagnostics, "能力与诊断"),
                ] {
                    if ui.selectable_label(self.tab == tab, label).clicked() {
                        self.tab = tab;
                    }
                }
                ui.separator();
                ui.label(&self.status);
            });
        });
    }

    #[allow(clippy::too_many_lines)]
    fn display_panel(&mut self, ui: &mut egui::Ui) {
        let ready = self
            .selected_record
            .as_ref()
            .is_some_and(|record| record.status.observed == hd_core::ObservedStateV2::Ready);
        ui.horizontal_wrapped(|ui| {
            let enabled = self.pending == 0 && self.selected_id.is_some();
            if ui.add_enabled(enabled, egui::Button::new("启动")).clicked() {
                self.operation(OperationKindV2::Start, Duration::from_secs(600), "启动");
            }
            if ui.add_enabled(enabled, egui::Button::new("停止")).clicked() {
                self.operation(
                    OperationKindV2::Stop {
                        mode: StopModeV2::Graceful,
                        graceful_timeout_ms: 20_000,
                    },
                    Duration::from_secs(90),
                    "停止",
                );
            }
            if ui.add_enabled(enabled, egui::Button::new("重启")).clicked() {
                self.operation(OperationKindV2::Restart, Duration::from_secs(660), "重启");
            }
            if ui.add_enabled(enabled, egui::Button::new("暂停")).clicked() {
                self.operation(OperationKindV2::Pause, Duration::from_secs(30), "暂停");
            }
            if ui.add_enabled(enabled, egui::Button::new("恢复")).clicked() {
                self.operation(OperationKindV2::Resume, Duration::from_secs(30), "恢复");
            }
        });
        ui.horizontal_wrapped(|ui| {
            for (label, key) in [
                ("Home", KeyActionV2::Home),
                ("最近任务", KeyActionV2::Recent),
                ("返回", KeyActionV2::Back),
                ("电源", KeyActionV2::Power),
                ("音量 +", KeyActionV2::VolumeUp),
                ("音量 -", KeyActionV2::VolumeDown),
            ] {
                if ui
                    .add_enabled(ready && self.pending == 0, egui::Button::new(label))
                    .clicked()
                {
                    self.action(InstanceActionV2::Key { key }, label);
                }
            }
            if ui
                .add_enabled(ready && self.pending == 0, egui::Button::new("旋转"))
                .clicked()
            {
                let orientation =
                    self.selected_record
                        .as_ref()
                        .map_or(OrientationV2::Landscape, |record| {
                            if record.spec.display.orientation.is_portrait() {
                                OrientationV2::Landscape
                            } else {
                                OrientationV2::Portrait
                            }
                        });
                self.action(InstanceActionV2::Rotate { orientation }, "旋转");
            }
        });
        ui.horizontal(|ui| {
            ui.label("APK");
            ui.text_edit_singleline(&mut self.apk_path);
            if ui
                .add_enabled(ready && self.pending == 0, egui::Button::new("安装并验证"))
                .clicked()
            {
                self.install_apk();
            }
        });
        ui.separator();
        let available = ui.available_size();
        let (rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 8.0, egui::Color32::from_rgb(18, 20, 24));
        let text = match &self.selected_record {
            None => "请选择或创建实例".to_owned(),
            Some(record) if record.status.observed == hd_core::ObservedStateV2::Ready => format!(
                "认证 Guest 已就绪；显示消费者等待原生外部纹理会话\n{} × {} · {} dpi · {} Hz\nframe generation {}",
                record.spec.display.oriented_size().0,
                record.spec.display.oriented_size().1,
                record.spec.display.dpi,
                record.spec.display.refresh_rate_hz,
                record.frame_generation
            ),
            Some(record) => {
                format!(
                    "Android 显示尚未就绪\n状态：{:?}\n{}",
                    record.status.observed,
                    record.status.reason.as_deref().unwrap_or(
                        "启动只会在签名 Guest、正式设备后端和严格零拷贝门禁全部通过后继续"
                    )
                )
            }
        };
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(18.0),
            egui::Color32::LIGHT_GRAY,
        );
    }

    #[allow(clippy::too_many_lines)]
    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        let Some(draft) = self.draft.as_mut() else {
            ui.label("请选择实例");
            return;
        };
        let mut save_clicked = false;
        let mut reset_clicked = false;
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("实例与启动资源");
            egui::Grid::new("resources").num_columns(2).show(ui, |ui| {
                ui.label("名称");
                ui.text_edit_singleline(&mut draft.name);
                ui.end_row();
                ui.label("CPU");
                ui.add(egui::DragValue::new(&mut draft.cpu_count).range(1..=256));
                ui.end_row();
                ui.label("内存 MiB");
                ui.add(egui::DragValue::new(&mut draft.memory_mib).range(2048..=1_048_576));
                ui.end_row();
                ui.label("重启策略");
                egui::ComboBox::from_id_salt("restart-policy")
                    .selected_text(format!("{:?}", draft.restart_policy))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut draft.restart_policy, RestartPolicyV2::OnFailure, "失败恢复");
                        ui.selectable_value(&mut draft.restart_policy, RestartPolicyV2::Never, "不自动恢复");
                    });
                ui.end_row();
            });
            ui.separator();
            ui.heading("显示");
            egui::Grid::new("display-settings").num_columns(2).show(ui, |ui| {
                ui.label("宽度");
                ui.add(egui::DragValue::new(&mut draft.display.width).range(320..=8192));
                ui.end_row();
                ui.label("高度");
                ui.add(egui::DragValue::new(&mut draft.display.height).range(320..=8192));
                ui.end_row();
                ui.label("DPI");
                ui.add(egui::DragValue::new(&mut draft.display.dpi).range(72..=960));
                ui.end_row();
                ui.label("帧率");
                egui::ComboBox::from_id_salt("refresh-rate")
                    .selected_text(draft.display.refresh_rate_hz.to_string())
                    .show_ui(ui, |ui| {
                        for rate in [30, 60, 90, 120] {
                            ui.selectable_value(&mut draft.display.refresh_rate_hz, rate, rate.to_string());
                        }
                    });
                ui.end_row();
                ui.label("方向");
                egui::ComboBox::from_id_salt("orientation")
                    .selected_text(format!("{:?}", draft.display.orientation))
                    .show_ui(ui, |ui| {
                        for (value, label) in [
                            (OrientationV2::Portrait, "竖屏"),
                            (OrientationV2::Landscape, "横屏"),
                            (OrientationV2::ReversePortrait, "反向竖屏"),
                            (OrientationV2::ReverseLandscape, "反向横屏"),
                        ] {
                            ui.selectable_value(&mut draft.display.orientation, value, label);
                        }
                    });
                ui.end_row();
                ui.label("垂直同步");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut draft.display.vsync, VsyncModeV2::On, "开");
                    ui.selectable_value(&mut draft.display.vsync, VsyncModeV2::Off, "关");
                });
                ui.end_row();
                ui.label("显示宿主 FPS");
                ui.checkbox(&mut draft.display.show_host_fps, "启用");
                ui.end_row();
            });
            ui.separator();
            ui.heading("ADB");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut draft.adb.mode, AdbModeV2::Loopback, "仅本机");
                ui.selectable_value(&mut draft.adb.mode, AdbModeV2::Disabled, "关闭");
            });
            if draft.adb.mode == AdbModeV2::Loopback {
                ui.horizontal(|ui| {
                    ui.label("宿主端口（留空自动分配）");
                    let mut port = draft.adb.host_port.unwrap_or(0);
                    if ui.add(egui::DragValue::new(&mut port).range(0..=u16::MAX)).changed() {
                        draft.adb.host_port = (port != 0).then_some(port);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("ADB 可执行文件（留空使用签名 bundle）");
                    ui.text_edit_singleline(&mut self.adb_executable);
                });
            } else {
                draft.adb.host_port = None;
            }
            ui.separator();
            ui.heading("受信制品");
            ui.label("只有 Ed25519 签名、SHA-256 固定且 READY 完整的 Guest/host bundle 才能启动。");
            ui.horizontal(|ui| {
                ui.label("制品仓库");
                ui.text_edit_singleline(&mut self.artifact_store);
            });
            ui.horizontal(|ui| {
                ui.label("Guest digest");
                ui.text_edit_singleline(&mut self.guest_digest);
            });
            ui.horizontal(|ui| {
                ui.label("Host digest");
                ui.text_edit_singleline(&mut self.host_digest);
            });
            ui.separator();
            ui.heading("固定启动参数");
            ui.horizontal(|ui| {
                ui.label("内核日志级别");
                ui.add(egui::DragValue::new(&mut draft.boot.kernel_log_level).range(0..=7));
                ui.label("panic 秒数");
                ui.add(egui::DragValue::new(&mut draft.boot.panic_timeout_seconds).range(0..=300));
                ui.checkbox(&mut draft.boot.boot_animation, "启动动画");
            });
            ui.label("V2 不接受自由拼接的内核或 crosvm 参数；全部参数由类型化配置生成并写入 run manifest。");
            ui.separator();
            if ui
                .add_enabled(self.pending == 0, egui::Button::new("原子保存设置"))
                .clicked()
            {
                save_clicked = true;
            }
            if ui.button("放弃未保存更改").clicked() {
                reset_clicked = true;
            }
        });
        if save_clicked {
            self.save_spec();
        }
        if reset_clicked {
            self.reset_draft();
        }
    }

    fn devices_panel(&mut self, ui: &mut egui::Ui) {
        let Some(draft) = self.draft.as_mut() else {
            ui.label("请选择实例");
            return;
        };
        ui.heading("Android 15 phone profile");
        ui.label(
            "启用的设备必须有正式组件、正式模拟器或明确的软件 Guest 边界；缺失时启动会被阻止。",
        );
        egui::Grid::new("devices").num_columns(3).show(ui, |ui| {
            for (label, enabled) in [
                ("Bluetooth / RootCanal", &mut draft.devices.bluetooth),
                ("NFC / Casimir", &mut draft.devices.nfc),
                ("UWB", &mut draft.devices.uwb),
                ("Modem", &mut draft.devices.modem),
                ("GNSS / Location", &mut draft.devices.gnss),
                ("Sensors", &mut draft.devices.sensors),
                ("Network", &mut draft.devices.network),
                ("Audio", &mut draft.devices.audio),
                ("Camera", &mut draft.devices.camera),
                ("Power / software security", &mut draft.devices.power),
            ] {
                ui.label(label);
                ui.checkbox(enabled, "启用");
                ui.end_row();
            }
        });
        let save_clicked = ui
            .add_enabled(self.pending == 0, egui::Button::new("保存设备配置"))
            .clicked();
        if save_clicked {
            self.save_spec();
        }
        ui.separator();
        if let Some(capabilities) = &self.capabilities {
            for device in &capabilities.devices.devices {
                ui.collapsing(format!("{} · {:?}", device.id, device.backend), |ui| {
                    ui.label(if device.available { "可用" } else { "阻塞" });
                    ui.label(&device.boundary);
                    ui.label(device.features.join(" · "));
                });
            }
        }
    }

    fn diagnostics_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.pending == 0, egui::Button::new("生成诊断包"))
                .clicked()
            {
                self.diagnostics(false);
            }
            if ui
                .add_enabled(self.pending == 0, egui::Button::new("诊断包 + Guest 日志"))
                .clicked()
            {
                self.diagnostics(true);
            }
            if ui
                .add_enabled(
                    self.pending == 0 && self.selected_id.is_some(),
                    egui::Button::new("删除实例"),
                )
                .clicked()
            {
                self.operation(OperationKindV2::Delete, Duration::from_secs(60), "删除");
            }
        });
        ui.separator();
        if let Some(capabilities) = &self.capabilities {
            ui.heading(format!(
                "{} / {} · fingerprint {}",
                capabilities.platform, capabilities.architecture, capabilities.fingerprint
            ));
            ui.label(if capabilities.certified {
                "本机与当前 bundle 已具备签名验收证据"
            } else {
                "当前组合尚无签名 real-guest / zero-copy 验收证据，不允许发布启动"
            });
            egui::ScrollArea::vertical().show(ui, |ui| {
                for probe in &capabilities.probes {
                    ui.collapsing(format!("{:?} · {}", probe.status, probe.id), |ui| {
                        ui.label(&probe.detail);
                        for (key, value) in &probe.properties {
                            ui.monospace(format!("{key}: {value}"));
                        }
                    });
                }
            });
        }
    }
}

impl eframe::App for HdApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_messages();
        if self.pending == 0 && self.last_refresh.elapsed() >= Duration::from_secs(2) {
            self.refresh();
        }
        context.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.sidebar(ui);
        self.top_bar(ui);
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            PanelTab::Display => self.display_panel(ui),
            PanelTab::Settings => self.settings_panel(ui),
            PanelTab::Devices => self.devices_panel(ui),
            PanelTab::Diagnostics => self.diagnostics_panel(ui),
        });
    }
}

fn non_empty_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    (!value.is_empty()).then(|| PathBuf::from(value))
}
