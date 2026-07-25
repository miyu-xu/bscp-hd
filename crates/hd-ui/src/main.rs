#![cfg_attr(windows, windows_subsystem = "windows")]

mod theme;

use std::path::PathBuf;
use std::sync::{
    Arc,
    mpsc::{Receiver, Sender},
};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::Parser;
use eframe::egui;
use hd_core::{
    AdbModeV2, ArtifactSelectionV2, BatteryStateV2, BluetoothPeerActionV2, CreateInstanceRequestV2,
    DiagnosticRequestV2, HostCapabilitiesV2, InstanceActionV2, InstanceRecordV2, InstanceSpecV2,
    InstanceSummaryV2, KeyActionV2, LocationV2, NetworkConditionV2, NfcTagActionV2,
    OperationKindV2, OrientationV2, RestartPolicyV2, SensorInjectionV2, StopModeV2,
    UpdateInstanceRequestV2, VsyncModeV2,
};
use hd_platform::DataPaths;
use hd_runtime::HostClientV2;
use tokio::runtime::Runtime;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "hd", version, about = "HD Android desktop manager")]
struct Cli {
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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hd-ui-runtime")
        .build()
        .context("create HD UI runtime")?;
    let client = runtime
        .block_on(HostClientV2::connect_or_start(paths.clone()))
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
        // HD's Windows release requires Vulkan for the crosvm/gfxstream display path. Keeping the
        // control UI on Vulkan also avoids wgpu's DXIL signing dependency on dxil.dll.
        setup.instance_descriptor.backends = eframe::wgpu::Backends::VULKAN;
    }
    eframe::run_native(
        "HD Android",
        options,
        Box::new(move |context| {
            configure_cjk_fonts(&context.egui_ctx);
            theme::install(&context.egui_ctx);
            Ok(Box::new(HdApp::new(runtime, client, paths)))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
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

    let Some((path, bytes)) = candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok().map(|bytes| (*path, bytes)))
    else {
        tracing::warn!("no system CJK font was found; non-Latin labels may not render");
        return;
    };

    let name = "hd-system-cjk".to_owned();
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert(name.clone(), Arc::new(egui::FontData::from_owned(bytes)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push(name.clone());
    }
    context.set_fonts(fonts);
    tracing::info!(font_path = path, "configured system CJK font fallback");
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
}

#[derive(Debug)]
enum UiMessage {
    Snapshot(Result<Box<Snapshot>, String>),
    Capabilities(Result<HostCapabilitiesV2, String>),
    Completed(Result<String, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelTab {
    Display,
    Settings,
    Devices,
    Diagnostics,
}

#[derive(Debug)]
struct DeviceActionDraft {
    latitude_e7: i32,
    longitude_e7: i32,
    altitude_mm: i32,
    accuracy_mm: u32,
    battery_level_percent: u8,
    battery_charging: bool,
    battery_temperature_deci_celsius: i16,
    network_latency_ms: u32,
    network_loss_basis_points: u16,
    network_bandwidth_enabled: bool,
    network_bandwidth_kbps: u32,
    sensor: String,
    sensor_values_microunits: String,
    sensor_duration_ms: u32,
    bluetooth_peer_id: String,
    bluetooth_peer_name: String,
    nfc_ndef_hex: String,
}

impl Default for DeviceActionDraft {
    fn default() -> Self {
        Self {
            latitude_e7: 374_219_999,
            longitude_e7: -1_220_840_577,
            altitude_mm: 5_000,
            accuracy_mm: 5_000,
            battery_level_percent: 100,
            battery_charging: true,
            battery_temperature_deci_celsius: 250,
            network_latency_ms: 0,
            network_loss_basis_points: 0,
            network_bandwidth_enabled: false,
            network_bandwidth_kbps: 10_000,
            sensor: "accelerometer".to_owned(),
            sensor_values_microunits: "0,0,9806650".to_owned(),
            sensor_duration_ms: 0,
            bluetooth_peer_id: Uuid::new_v4().to_string(),
            bluetooth_peer_name: "HD GATT peer".to_owned(),
            nfc_ndef_hex: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DeviceUiCommand {
    SetLocation,
    SetBattery,
    SetNetwork,
    InjectSensor,
    BluetoothCreate,
    BluetoothRemove,
    BluetoothAdvertise(bool),
    NfcPresentType2,
    NfcPresentType4,
    NfcRemove,
}

struct HdApp {
    runtime: Runtime,
    client: HostClientV2,
    paths: DataPaths,
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
    device_actions: DeviceActionDraft,
}

impl HdApp {
    fn new(runtime: Runtime, client: HostClientV2, paths: DataPaths) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        let now = Instant::now();
        let mut app = Self {
            runtime,
            client,
            paths,
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
            device_actions: DeviceActionDraft::default(),
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
                Ok(Box::new(Snapshot {
                    summaries,
                    selected,
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
                    let new_id = snapshot.selected.as_ref().map(|record| record.spec.id);
                    let should_replace_draft = self.selected_id != new_id || self.draft.is_none();
                    self.selected_id = new_id;
                    self.selected_record = snapshot.selected;
                    if should_replace_draft {
                        self.reset_draft();
                    }
                    "宿主状态已同步".clone_into(&mut self.status);
                }
                UiMessage::Capabilities(Ok(capabilities)) => {
                    self.capabilities = Some(capabilities);
                    self.status = "能力与制品诊断已刷新".to_owned();
                }
                UiMessage::Snapshot(Err(error))
                | UiMessage::Capabilities(Err(error))
                | UiMessage::Completed(Err(error)) => {
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

    fn refresh_capabilities(&mut self) {
        if self.pending > 0 {
            return;
        }
        let client = self.client.clone();
        let instance_id = self.selected_id;
        self.status = "正在执行显式能力与制品诊断…".to_owned();
        self.spawn(async move {
            UiMessage::Capabilities(
                client
                    .capabilities(instance_id)
                    .await
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn open_player(&mut self) {
        let Some(id) = self.selected_id else {
            return;
        };
        let player_name = if cfg!(windows) {
            "hd-player.exe"
        } else {
            "hd-player"
        };
        let executable = std::env::current_exe()
            .ok()
            .map(|path| path.with_file_name(player_name))
            .unwrap_or_else(|| PathBuf::from(player_name));
        match std::process::Command::new(&executable)
            .arg("--instance-id")
            .arg(id.to_string())
            .arg("--data-root")
            .arg(&self.paths.root)
            .spawn()
        {
            Ok(_) => self.status = "正在打开 Player…".to_owned(),
            Err(error) => {
                self.status = format!("启动 Player 失败（{}）：{error}", executable.display());
            }
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

    fn device_action(&mut self, command: DeviceUiCommand) {
        match build_device_action(&self.device_actions, command) {
            Ok((action, label)) => self.action(action, label),
            Err(error) => self.status = error,
        }
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
                    .wait_operation(operation.id, Duration::from_mins(10))
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
                ui.heading("HD 多实例");
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
                    let response = ui.selectable_label(selected, text);
                    if response.clicked() {
                        self.select(summary.id);
                    }
                    if response.double_clicked() {
                        self.selected_id = Some(summary.id);
                        self.open_player();
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
            if ui
                .add_enabled(enabled, egui::Button::new("打开 Player"))
                .clicked()
            {
                self.open_player();
            }
            if ui.add_enabled(enabled, egui::Button::new("启动")).clicked() {
                self.open_player();
                self.operation(OperationKindV2::Start, Duration::from_mins(10), "启动");
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
                self.operation(OperationKindV2::Restart, Duration::from_mins(11), "重启");
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
                "认证 Guest 已就绪；Android 画面由 crosvm/gfxstream 原生 Vulkan 窗口呈现\n{} × {} · {} dpi · {} Hz\n严格外部纹理 frame generation {}",
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

    #[allow(clippy::too_many_lines)]
    fn devices_panel(&mut self, ui: &mut egui::Ui) {
        let Some(draft) = self.draft.as_mut() else {
            ui.label("请选择实例");
            return;
        };
        let can_action = self.pending == 0
            && self
                .selected_record
                .as_ref()
                .is_some_and(|record| record.status.observed == hd_core::ObservedStateV2::Ready);
        let pending = self.pending;
        let actions = &mut self.device_actions;
        let capabilities = self.capabilities.as_ref();
        let mut requested_action = None;
        let mut save_clicked = false;
        egui::ScrollArea::vertical().show(ui, |ui| {
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
            save_clicked = ui
                .add_enabled(pending == 0, egui::Button::new("保存设备配置"))
                .clicked();
            ui.separator();
            ui.heading("运行时设备控制");
            ui.label(
                "控制只在实例 Ready 后发送，并由对应签名组件回读确认。数值采用 V2 协议的整数单位。",
            );

            ui.collapsing("定位", |ui| {
                egui::Grid::new("location-action")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("纬度 E7");
                        ui.add(egui::DragValue::new(&mut actions.latitude_e7));
                        ui.end_row();
                        ui.label("经度 E7");
                        ui.add(egui::DragValue::new(&mut actions.longitude_e7));
                        ui.end_row();
                        ui.label("高度 mm");
                        ui.add(egui::DragValue::new(&mut actions.altitude_mm));
                        ui.end_row();
                        ui.label("精度 mm");
                        ui.add(egui::DragValue::new(&mut actions.accuracy_mm).range(0..=1_000_000));
                        ui.end_row();
                    });
                if ui
                    .add_enabled(
                        can_action && draft.devices.gnss,
                        egui::Button::new("注入定位"),
                    )
                    .clicked()
                {
                    requested_action = Some(DeviceUiCommand::SetLocation);
                }
            });

            ui.collapsing("电池与电源", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("电量 %");
                    ui.add(egui::DragValue::new(&mut actions.battery_level_percent).range(0..=100));
                    ui.checkbox(&mut actions.battery_charging, "充电中");
                    ui.label("温度 0.1°C");
                    ui.add(
                        egui::DragValue::new(&mut actions.battery_temperature_deci_celsius)
                            .range(-500..=1_500),
                    );
                    if ui
                        .add_enabled(
                            can_action && draft.devices.power,
                            egui::Button::new("应用电池状态"),
                        )
                        .clicked()
                    {
                        requested_action = Some(DeviceUiCommand::SetBattery);
                    }
                });
            });

            ui.collapsing("网络条件", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("延迟 ms");
                    ui.add(egui::DragValue::new(&mut actions.network_latency_ms).range(0..=60_000));
                    ui.label("丢包 bp");
                    ui.add(
                        egui::DragValue::new(&mut actions.network_loss_basis_points)
                            .range(0..=10_000),
                    );
                    ui.checkbox(&mut actions.network_bandwidth_enabled, "限制带宽");
                    ui.add_enabled(
                        actions.network_bandwidth_enabled,
                        egui::DragValue::new(&mut actions.network_bandwidth_kbps)
                            .range(1..=u32::MAX)
                            .suffix(" kbps"),
                    );
                    if ui
                        .add_enabled(
                            can_action && draft.devices.network,
                            egui::Button::new("应用网络条件"),
                        )
                        .clicked()
                    {
                        requested_action = Some(DeviceUiCommand::SetNetwork);
                    }
                });
            });

            ui.collapsing("传感器", |ui| {
                ui.horizontal_wrapped(|ui| {
                    egui::ComboBox::from_id_salt("sensor-kind")
                        .selected_text(&actions.sensor)
                        .show_ui(ui, |ui| {
                            for sensor in [
                                "accelerometer",
                                "gyroscope",
                                "magnetometer",
                                "light",
                                "proximity",
                            ] {
                                ui.selectable_value(&mut actions.sensor, sensor.to_owned(), sensor);
                            }
                        });
                    ui.label("微单位值（逗号分隔）");
                    ui.text_edit_singleline(&mut actions.sensor_values_microunits);
                    ui.label("持续 ms");
                    ui.add(
                        egui::DragValue::new(&mut actions.sensor_duration_ms).range(0..=3_600_000),
                    );
                    if ui
                        .add_enabled(
                            can_action && draft.devices.sensors,
                            egui::Button::new("注入传感器"),
                        )
                        .clicked()
                    {
                        requested_action = Some(DeviceUiCommand::InjectSensor);
                    }
                });
            });

            ui.collapsing("Bluetooth / RootCanal", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Peer UUID");
                    ui.text_edit_singleline(&mut actions.bluetooth_peer_id);
                    if ui.button("新 UUID").clicked() {
                        actions.bluetooth_peer_id = Uuid::new_v4().to_string();
                    }
                    ui.label("名称");
                    ui.text_edit_singleline(&mut actions.bluetooth_peer_name);
                });
                ui.horizontal_wrapped(|ui| {
                    let enabled = can_action && draft.devices.bluetooth;
                    if ui
                        .add_enabled(enabled, egui::Button::new("创建 GATT Peer"))
                        .clicked()
                    {
                        requested_action = Some(DeviceUiCommand::BluetoothCreate);
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("移除 Peer"))
                        .clicked()
                    {
                        requested_action = Some(DeviceUiCommand::BluetoothRemove);
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("开始广播"))
                        .clicked()
                    {
                        requested_action = Some(DeviceUiCommand::BluetoothAdvertise(true));
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("停止广播"))
                        .clicked()
                    {
                        requested_action = Some(DeviceUiCommand::BluetoothAdvertise(false));
                    }
                });
            });

            ui.collapsing("NFC / Casimir", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("NDEF hex");
                    ui.text_edit_singleline(&mut actions.nfc_ndef_hex);
                    let enabled = can_action && draft.devices.nfc;
                    if ui
                        .add_enabled(enabled, egui::Button::new("呈现 Type 2"))
                        .clicked()
                    {
                        requested_action = Some(DeviceUiCommand::NfcPresentType2);
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("呈现 Type 4"))
                        .clicked()
                    {
                        requested_action = Some(DeviceUiCommand::NfcPresentType4);
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("移除标签"))
                        .clicked()
                    {
                        requested_action = Some(DeviceUiCommand::NfcRemove);
                    }
                });
            });

            ui.separator();
            if let Some(capabilities) = capabilities {
                for device in &capabilities.devices.devices {
                    ui.collapsing(format!("{} · {:?}", device.id, device.backend), |ui| {
                        ui.label(if device.available { "可用" } else { "阻塞" });
                        ui.label(&device.boundary);
                        ui.label(device.features.join(" · "));
                    });
                }
            }
        });
        if save_clicked {
            self.save_spec();
        }
        if let Some(command) = requested_action {
            self.device_action(command);
        }
    }

    fn diagnostics_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.pending == 0, egui::Button::new("刷新能力与制品诊断"))
                .clicked()
            {
                self.refresh_capabilities();
            }
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
                self.operation(OperationKindV2::Delete, Duration::from_mins(1), "删除");
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

#[allow(clippy::too_many_lines)]
fn build_device_action(
    draft: &DeviceActionDraft,
    command: DeviceUiCommand,
) -> Result<(InstanceActionV2, &'static str), String> {
    let action = match command {
        DeviceUiCommand::SetLocation => {
            let location = LocationV2 {
                latitude_e7: draft.latitude_e7,
                longitude_e7: draft.longitude_e7,
                altitude_mm: draft.altitude_mm,
                accuracy_mm: draft.accuracy_mm,
            };
            if !location.is_valid() {
                return Err("定位超出 V2 支持范围".to_owned());
            }
            (InstanceActionV2::SetLocation { location }, "定位")
        }
        DeviceUiCommand::SetBattery => {
            if draft.battery_level_percent > 100
                || !(-500..=1_500).contains(&draft.battery_temperature_deci_celsius)
            {
                return Err("电池状态超出 V2 支持范围".to_owned());
            }
            (
                InstanceActionV2::SetBattery {
                    battery: BatteryStateV2 {
                        level_percent: draft.battery_level_percent,
                        charging: draft.battery_charging,
                        temperature_deci_celsius: draft.battery_temperature_deci_celsius,
                    },
                },
                "电池状态",
            )
        }
        DeviceUiCommand::SetNetwork => {
            let bandwidth_kbps = draft
                .network_bandwidth_enabled
                .then_some(draft.network_bandwidth_kbps);
            if draft.network_latency_ms > 60_000
                || draft.network_loss_basis_points > 10_000
                || bandwidth_kbps == Some(0)
            {
                return Err("网络条件超出 V2 支持范围".to_owned());
            }
            (
                InstanceActionV2::SetNetworkCondition {
                    condition: NetworkConditionV2 {
                        latency_ms: draft.network_latency_ms,
                        loss_basis_points: draft.network_loss_basis_points,
                        bandwidth_kbps,
                    },
                },
                "网络条件",
            )
        }
        DeviceUiCommand::InjectSensor => {
            let values_microunits = draft
                .sensor_values_microunits
                .split(',')
                .map(str::trim)
                .map(str::parse::<i64>)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "传感器值必须是逗号分隔的整数".to_owned())?;
            let expected_values = match draft.sensor.as_str() {
                "accelerometer" | "gyroscope" | "magnetometer" => 3,
                "light" | "proximity" => 1,
                _ => return Err("不支持该传感器".to_owned()),
            };
            if values_microunits.len() != expected_values
                || values_microunits
                    .iter()
                    .any(|value| value.unsigned_abs() > 1_000_000_000_000)
                || draft.sensor_duration_ms > 3_600_000
            {
                return Err("传感器值数量或范围无效".to_owned());
            }
            (
                InstanceActionV2::InjectSensor {
                    injection: SensorInjectionV2 {
                        sensor: draft.sensor.clone(),
                        values_microunits,
                        duration_ms: draft.sensor_duration_ms,
                    },
                },
                "传感器数据",
            )
        }
        DeviceUiCommand::BluetoothCreate => {
            let peer_id = parse_peer_id(&draft.bluetooth_peer_id)?;
            let name = draft.bluetooth_peer_name.trim();
            if name.is_empty() || name.len() > 128 {
                return Err("Bluetooth Peer 名称必须为 1..=128 字节".to_owned());
            }
            (
                InstanceActionV2::BluetoothPeer {
                    action: BluetoothPeerActionV2::CreateGattPeer {
                        peer_id,
                        name: name.to_owned(),
                    },
                },
                "Bluetooth Peer 创建",
            )
        }
        DeviceUiCommand::BluetoothRemove => (
            InstanceActionV2::BluetoothPeer {
                action: BluetoothPeerActionV2::RemovePeer {
                    peer_id: parse_peer_id(&draft.bluetooth_peer_id)?,
                },
            },
            "Bluetooth Peer 移除",
        ),
        DeviceUiCommand::BluetoothAdvertise(enabled) => (
            InstanceActionV2::BluetoothPeer {
                action: BluetoothPeerActionV2::SetAdvertising {
                    peer_id: parse_peer_id(&draft.bluetooth_peer_id)?,
                    enabled,
                },
            },
            if enabled {
                "Bluetooth 广播启动"
            } else {
                "Bluetooth 广播停止"
            },
        ),
        DeviceUiCommand::NfcPresentType2 => (
            InstanceActionV2::NfcTag {
                action: NfcTagActionV2::PresentType2 {
                    ndef_hex: validate_ndef_hex(&draft.nfc_ndef_hex)?,
                },
            },
            "NFC Type 2 标签",
        ),
        DeviceUiCommand::NfcPresentType4 => (
            InstanceActionV2::NfcTag {
                action: NfcTagActionV2::PresentType4 {
                    ndef_hex: validate_ndef_hex(&draft.nfc_ndef_hex)?,
                },
            },
            "NFC Type 4 标签",
        ),
        DeviceUiCommand::NfcRemove => (
            InstanceActionV2::NfcTag {
                action: NfcTagActionV2::Remove,
            },
            "NFC 标签移除",
        ),
    };
    Ok(action)
}

fn parse_peer_id(value: &str) -> Result<Uuid, String> {
    Uuid::parse_str(value.trim()).map_err(|_| "Bluetooth Peer UUID 无效".to_owned())
}

fn validate_ndef_hex(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 16 * 1024
        || !value.len().is_multiple_of(2)
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("NDEF 必须是非空、偶数字节且不超过 8 KiB 的十六进制文本".to_owned());
    }
    Ok(value.to_ascii_lowercase())
}

fn non_empty_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    (!value.is_empty()).then(|| PathBuf::from(value))
}
