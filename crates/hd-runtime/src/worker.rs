use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Seek as _, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs2::FileExt as _;
use hd_core::{
    AdbModeV2, AndroidBugreportRecordV2, ApiErrorV2, BluetoothHciCaptureRecordV2,
    BluetoothPeerActionV2, BluetoothPeerKindV2, BluetoothPeerStateV2, COMPONENT_PROTOCOL_VERSION,
    DEVICE_GUEST_ENDPOINT_ROLES_V2, DeviceControlCommandV2, DeviceControlRequestV2,
    DeviceControlTokenV2, DeviceSerialEndpointV2, DisplayConfigV2, DisplayIdV2, DisplayViewportV2,
    FRAME_PROTOCOL_VERSION, FormalComponentConfigurationV2, FormalComponentLaunchV2,
    FormalComponentReadyV2, FrameMetricsV2, FrameReadyMarkerV2, GuestKindV2, InstanceActionV2,
    InstanceSpecV2, KeyActionV2, LaunchPlanV2, LeaseKindV2, LeaseV2, LocationRouteFinishReasonV2,
    LocationRoutePlaybackStateV2, LocationRouteRecordV2, LocationRouteStatusV2, LocationRouteV2,
    MAX_SECONDARY_DISPLAYS, MicrodroidCpuTopologyV2, MicrodroidDebugLevelV2, MicrodroidPayloadV2,
    NativeDisplayTargetV2, ObservedStateV2, PreparedNativeDisplayV2, RunManifestV2, RunResultV2,
    RuntimeDisplayV2, ScreenRecordingRecordV2, ScreenRecordingStatusV2, ScreenshotRecordV2,
    SecondaryDisplayConfigV2, StopModeV2, WORKER_PROTOCOL_VERSION, WorkerCommandV2,
    WorkerDescriptorV2, WorkerIdentityV2, WorkerPayloadV2, WorkerRequestV2, WorkerResponseV2,
    WorkerStatusV2, device_component_guest_roles_v2,
};
use hd_platform::{
    DataPaths, DiskProvisioner as _, ProcessSpec, ProcessSupervisor as _, VmBackend as _,
    VmLaunchContextV2, process_identity_is_alive,
};
use sha2::Digest as _;
use subtle::ConstantTimeEq as _;
use time::OffsetDateTime;
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

#[cfg(not(any(windows, target_os = "macos")))]
use crate::AdbScreenRecording;
#[cfg(any(windows, target_os = "macos"))]
use crate::host_recorder::{HostRecording, endpoint_for_run as host_recorder_endpoint_for_run};
use crate::{
    AdbClient, AdbError, AndroidDeviceRuntimeHealth, AndroidNetworkHealth, CapabilityDiscovery,
    CrosvmBackend, ManagedProcess, NativeDiskProvisioner, RunJournalV2, TokioProcessSupervisor,
    available_runtime_disk_bytes, enforce_run_retention, expected_frame_transport,
    microdroid_exit::{MicrodroidLauncherCompletion, inspect_microdroid_launcher_completion},
    remove_finished_run_ephemeral_artifacts, resolve_microdroid_runtime_paths,
    runtime_disk_requirement, send_device_control_request, spawn_run_log_maintenance,
    write_json_atomic,
};
#[cfg(unix)]
use crate::{
    MICRODROID_CONSOLE_CHALLENGE_TIMEOUT, MicrodroidConsoleChallengeChannel,
    MicrodroidConsoleChallengeError,
};

const FRAME_READY_TIMEOUT: Duration = Duration::from_secs(90);
const COMPONENT_READY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BLUETOOTH_HCI_CAPTURE_BYTES: u64 = 4 * 1024 * 1024;
const MICRODROID_IDSIG_TIMEOUT: Duration = Duration::from_mins(10);
const ADBD_LOG_READY_TIMEOUT: Duration = Duration::from_mins(3);
const DEV_BOOT_LOG_READY_TIMEOUT: Duration = Duration::from_secs(150);
const DEV_BOOT_LOG_SCAN_LIMIT: u64 = 32 * 1024 * 1024;
// Native gfxstream HWND creation follows SurfaceFlinger rather than crosvm process creation.
// Keep the VM/control plane usable while that window appears instead of holding Start (and the
// per-instance operation lock) for the entire Android cold boot.
const INITIAL_DISPLAY_ATTACH_RETRY_WINDOW: Duration = Duration::from_mins(3);
const INITIAL_DISPLAY_ATTACH_RETRY_MIN: Duration = Duration::from_millis(100);
const INITIAL_DISPLAY_ATTACH_RETRY_MAX: Duration = Duration::from_millis(500);
const DEFERRED_INTERACTIVE_POLICY_TIMEOUT: Duration = Duration::from_mins(5);
const DEFERRED_INTERACTIVE_POLICY_RETRY_DELAY: Duration = Duration::from_secs(2);
const CROSVM_DISPLAY_PARENT_HWND_ENV: &str = "CROSVM_DISPLAY_PARENT_HWND";
const CROSVM_DISPLAY_WIDTH_ENV: &str = "CROSVM_DISPLAY_WIDTH";
const CROSVM_DISPLAY_HEIGHT_ENV: &str = "CROSVM_DISPLAY_HEIGHT";
const CROSVM_COCOA_CONTEXT_ENDPOINT_ENV: &str = "CROSVM_COCOA_CONTEXT_ENDPOINT";
#[cfg(any(windows, target_os = "macos"))]
const HD_HOST_RECORDER_ENDPOINT_ENV: &str = "HD_HOST_RECORDER_ENDPOINT";

#[derive(Debug, Clone)]
struct DeferredAdbReadinessPolicy {
    display: DisplayConfigV2,
    bluetooth_enabled: bool,
    nfc_enabled: bool,
    modem_enabled: bool,
    network_enabled: bool,
}

impl DeferredAdbReadinessPolicy {
    async fn apply(&self, adb: &AdbClient, instance_id: Uuid, run_id: Uuid, serial: &str) {
        adb.apply_runtime_device_policy(
            serial,
            self.bluetooth_enabled,
            self.nfc_enabled,
            self.modem_enabled,
        )
        .await;
        refresh_network_validation(adb, self.network_enabled, instance_id, run_id, serial).await;
    }
}

fn log_network_validation(
    instance_id: Uuid,
    run_id: Uuid,
    serial: &str,
    result: Result<AndroidNetworkHealth, AdbError>,
) {
    match result {
        Ok(health) if health.is_healthy() => {
            tracing::info!(
                event = "worker.network.validation.succeeded",
                %instance_id,
                %run_id,
                %serial,
                detail = %health.detail(),
                "Android network validation succeeded"
            );
        }
        Ok(health) => {
            tracing::warn!(
                event = "worker.network.validation.degraded",
                error_code = "guest_network_unvalidated",
                %instance_id,
                %run_id,
                %serial,
                detail = %health.detail(),
                "Android network remains unvalidated after one bounded link refresh"
            );
        }
        Err(error) => {
            tracing::warn!(
                event = "worker.network.validation.failed",
                error_code = "guest_network_probe_failed",
                %instance_id,
                %run_id,
                %serial,
                %error,
                "Android network validation probe failed"
            );
        }
    }
}

async fn refresh_network_validation(
    adb: &AdbClient,
    enabled: bool,
    instance_id: Uuid,
    run_id: Uuid,
    serial: &str,
) {
    if enabled {
        log_network_validation(
            instance_id,
            run_id,
            serial,
            adb.refresh_network_validation(serial).await,
        );
    }
}

fn device_runtime_diagnostic(health: AndroidDeviceRuntimeHealth) -> hd_core::DiagnosticCheckV2 {
    let mut fields = BTreeMap::new();
    fields.insert("installed".to_owned(), health.installed.to_string());
    fields.insert("configured".to_owned(), health.configured.to_string());
    fields.insert("running".to_owned(), health.running.to_string());
    fields.insert("controllable".to_owned(), health.controllable.to_string());
    fields.insert("verified".to_owned(), health.verified.to_string());
    hd_core::DiagnosticCheckV2 {
        id: format!("device.{}", health.id),
        status: if health.verified {
            hd_core::DiagnosticStatusV2::Pass
        } else if health.installed || health.configured {
            hd_core::DiagnosticStatusV2::Warn
        } else {
            hd_core::DiagnosticStatusV2::Fail
        },
        detail: health.detail,
        fields,
    }
}

async fn guest_network_diagnostic(
    network: Option<&(AdbClient, String)>,
) -> hd_core::DiagnosticCheckV2 {
    let Some((adb, serial)) = network else {
        return hd_core::DiagnosticCheckV2 {
            id: "guest.network".to_owned(),
            status: hd_core::DiagnosticStatusV2::Blocked,
            detail: "ADB is not ready; Android connectivity cannot be verified".to_owned(),
            fields: BTreeMap::new(),
        };
    };
    match adb.network_health(serial).await {
        Ok(health) => {
            let mut fields = BTreeMap::new();
            fields.insert(
                "active_network".to_owned(),
                health
                    .active_network
                    .clone()
                    .unwrap_or_else(|| "none".to_owned()),
            );
            fields.insert(
                "interface".to_owned(),
                health
                    .interface
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned()),
            );
            fields.insert("validated".to_owned(), health.validated.to_string());
            fields.insert("dns".to_owned(), health.has_dns.to_string());
            fields.insert(
                "default_route".to_owned(),
                health.has_default_route.to_string(),
            );
            hd_core::DiagnosticCheckV2 {
                id: "guest.network".to_owned(),
                status: if health.is_healthy() {
                    hd_core::DiagnosticStatusV2::Pass
                } else {
                    hd_core::DiagnosticStatusV2::Fail
                },
                detail: health.detail(),
                fields,
            }
        }
        Err(error) => hd_core::DiagnosticCheckV2 {
            id: "guest.network".to_owned(),
            status: hd_core::DiagnosticStatusV2::Fail,
            detail: format!("Android connectivity probe failed: {error}"),
            fields: BTreeMap::new(),
        },
    }
}

#[cfg(target_os = "macos")]
const BOOTCONFIG_MAGIC: &[u8] = b"#BOOTCONFIG\n";

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_lines)]
fn patch_android_initrd_bootconfig(source: &Path, destination: &Path) -> std::io::Result<()> {
    let data = std::fs::read(source)?;
    let (prefix, existing) = if data.ends_with(BOOTCONFIG_MAGIC) {
        let trailer_start = data.len() - BOOTCONFIG_MAGIC.len();
        if trailer_start < 8 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Android initrd bootconfig trailer is too short",
            ));
        }
        let size_offset = trailer_start - 8;
        let size = u32::from_le_bytes(
            data[size_offset..size_offset + 4]
                .try_into()
                .expect("bootconfig size field has a fixed width"),
        ) as usize;
        let checksum = u32::from_le_bytes(
            data[size_offset + 4..trailer_start]
                .try_into()
                .expect("bootconfig checksum field has a fixed width"),
        );
        if size > size_offset {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid Android initrd bootconfig size {size}"),
            ));
        }
        let config_start = size_offset - size;
        let existing = &data[config_start..size_offset];
        let actual_checksum = existing
            .iter()
            .fold(0_u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
        if actual_checksum != checksum {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Android initrd bootconfig checksum mismatch",
            ));
        }
        (&data[..config_start], existing)
    } else {
        (&data[..], &[][..])
    };

    let text = std::str::from_utf8(existing).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Android initrd bootconfig is not UTF-8: {error}"),
        )
    })?;
    let mut entries = Vec::<(String, String)>::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let key = line.split_once('=').map_or(line, |(key, _)| key);
        if let Some((_, value)) = entries.iter_mut().find(|(entry, _)| entry == key) {
            line.clone_into(value);
        } else {
            entries.push((key.to_owned(), line.to_owned()));
        }
    }
    let key = "androidboot.boot_devices";
    let value = "androidboot.boot_devices=10000.pci";
    if let Some((_, entry)) = entries.iter_mut().find(|(entry, _)| entry == key) {
        value.clone_into(entry);
    } else {
        entries.push((key.to_owned(), value.to_owned()));
    }
    let mut bootconfig = String::new();
    for (_, entry) in entries {
        bootconfig.push_str(&entry);
        bootconfig.push('\n');
    }
    let bootconfig = bootconfig.into_bytes();
    let checksum = bootconfig
        .iter()
        .fold(0_u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
    let mut patched =
        Vec::with_capacity(prefix.len() + bootconfig.len() + 8 + BOOTCONFIG_MAGIC.len());
    let bootconfig_len = u32::try_from(bootconfig.len()).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Android initrd bootconfig length exceeds u32: {error}"),
        )
    })?;
    patched.extend_from_slice(prefix);
    patched.extend_from_slice(&bootconfig);
    patched.extend_from_slice(&bootconfig_len.to_le_bytes());
    patched.extend_from_slice(&checksum.to_le_bytes());
    patched.extend_from_slice(BOOTCONFIG_MAGIC);
    std::fs::write(destination, patched)?;
    Ok(())
}

fn inject_initial_display_environment(
    environment: &mut BTreeMap<String, String>,
    display: &PreparedNativeDisplayV2,
) {
    match &display.target {
        NativeDisplayTargetV2::WindowsHwnd { hwnd, .. } => {
            environment.insert(CROSVM_DISPLAY_PARENT_HWND_ENV.to_owned(), hwnd.to_string());
            environment.insert(
                CROSVM_DISPLAY_WIDTH_ENV.to_owned(),
                display.viewport.width_px.to_string(),
            );
            environment.insert(
                CROSVM_DISPLAY_HEIGHT_ENV.to_owned(),
                display.viewport.height_px.to_string(),
            );
        }
        NativeDisplayTargetV2::MacCaContext { endpoint, .. } => {
            environment.insert(
                CROSVM_COCOA_CONTEXT_ENDPOINT_ENV.to_owned(),
                endpoint.clone(),
            );
        }
    }
}

fn configured_runtime_displays(spec: &InstanceSpecV2) -> Vec<RuntimeDisplayV2> {
    if spec.guest_kind != GuestKindV2::Android {
        return Vec::new();
    }
    let mut displays = vec![RuntimeDisplayV2 {
        display_id: DisplayIdV2::Primary,
        scanout_id: 0,
        name: "主屏".to_owned(),
        width: spec.display.width,
        height: spec.display.height,
        dpi: spec.display.dpi,
        refresh_rate_hz: spec.display.refresh_rate_hz,
    }];
    displays.extend(
        spec.display
            .secondary_displays
            .iter()
            .enumerate()
            .map(|(index, display)| {
                runtime_display_for_secondary(
                    display,
                    u32::try_from(index + 1).expect("bounded secondary display index"),
                )
            }),
    );
    displays
}

fn runtime_display_for_secondary(
    display: &SecondaryDisplayConfigV2,
    scanout_id: u32,
) -> RuntimeDisplayV2 {
    RuntimeDisplayV2 {
        display_id: DisplayIdV2::Secondary { id: display.id },
        scanout_id,
        name: display.name.clone(),
        width: display.width,
        height: display.height,
        dpi: display.dpi,
        refresh_rate_hz: display.refresh_rate_hz,
    }
}

fn secondary_display_geometry_changed(
    current: &SecondaryDisplayConfigV2,
    next: &SecondaryDisplayConfigV2,
) -> bool {
    current.width != next.width
        || current.height != next.height
        || current.dpi != next.dpi
        || current.refresh_rate_hz != next.refresh_rate_hz
}

// Crosvm acknowledges the virtio-gpu control command before Android's HWC hotplug event has
// crossed SurfaceFlinger. On the pinned Android 15/macOS stack, issuing `wm density -d` during
// that window can leave the new logical display permanently at 0x0 / density -1. Keep every
// guest display query behind one bounded settling window after add/remove/replace.
const ANDROID_DISPLAY_HOTPLUG_SETTLE_DELAY: Duration = Duration::from_secs(4);

async fn rollback_secondary_display_transaction(
    backend: &CrosvmBackend,
    control_endpoint: &str,
    added: &[(u32, SecondaryDisplayConfigV2)],
    removed: &[(u32, SecondaryDisplayConfigV2)],
    changed: &[(u32, SecondaryDisplayConfigV2)],
) -> Result<(), String> {
    let mut errors = Vec::new();
    for (scanout_id, _) in added.iter().rev() {
        if let Err(error) = backend.remove_display(control_endpoint, *scanout_id).await {
            errors.push(format!("remove added scanout {scanout_id}: {error}"));
        }
    }
    let mut removed = removed.to_vec();
    removed.sort_by_key(|(scanout_id, _)| *scanout_id);
    for (expected_scanout, display) in removed {
        match backend
            .add_secondary_display(control_endpoint, expected_scanout, &display)
            .await
        {
            Ok(()) => {}
            Err(error) => errors.push(format!(
                "restore display {} on scanout {expected_scanout}: {error}",
                display.id
            )),
        }
    }
    for (scanout_id, display) in changed.iter().rev() {
        if let Err(error) = backend
            .replace_secondary_display(control_endpoint, *scanout_id, display)
            .await
        {
            errors.push(format!("restore scanout {scanout_id}: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[derive(Debug)]
struct ManagedComponent {
    id: String,
    process: ManagedProcess,
}

#[derive(Debug)]
struct ComponentHandshake {
    component: String,
    run_id: Uuid,
    pid: u32,
    ready_path: PathBuf,
    launch_sha256: String,
    started: Instant,
}

#[derive(Debug)]
struct ComponentStartSpec {
    executable: PathBuf,
    component: String,
    run_id: Uuid,
    run_dir: PathBuf,
    configuration: FormalComponentConfigurationV2,
}

#[derive(Debug)]
struct WorkerMutable {
    status: WorkerStatusV2,
    active_spec: Option<InstanceSpecV2>,
    process: Option<ManagedProcess>,
    components: Vec<ManagedComponent>,
    device_control_tokens: BTreeMap<String, DeviceControlTokenV2>,
    backend: Option<CrosvmBackend>,
    launch: Option<LaunchPlanV2>,
    adb: Option<AdbClient>,
    adb_ready: bool,
    journal: Option<Arc<RunJournalV2>>,
    started_at: Option<OffsetDateTime>,
    display_session: Option<ActiveDisplaySession>,
    #[cfg(any(windows, target_os = "macos"))]
    host_recorder_endpoint: Option<PathBuf>,
    active_screen_recording: Option<ActiveScreenRecording>,
    active_location_route: Option<ActiveLocationRoute>,
    #[cfg(unix)]
    device_output_files: Vec<std::fs::File>,
    #[cfg(unix)]
    device_input_fifos: Vec<std::fs::File>,
    #[cfg(unix)]
    microdroid_console_challenge: Option<MicrodroidConsoleChallengeChannel>,
}

#[derive(Debug)]
struct ActiveDisplaySession {
    id: Uuid,
    generation: u64,
    display_id: DisplayIdV2,
    target: NativeDisplayTargetV2,
    viewport: DisplayViewportV2,
}

#[derive(Debug)]
struct ActiveScreenRecording {
    status: ScreenRecordingStatusV2,
    output_path: PathBuf,
    backend: ActiveScreenRecordingBackend,
    started: Instant,
}

#[derive(Debug)]
enum ActiveScreenRecordingBackend {
    #[cfg(not(any(windows, target_os = "macos")))]
    Guest {
        adb: AdbClient,
        serial: String,
        process: AdbScreenRecording,
    },
    #[cfg(any(windows, target_os = "macos"))]
    Host(HostRecording),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocationRouteControl {
    Playing,
    Paused,
    Stop,
}

#[derive(Debug)]
struct ActiveLocationRoute {
    status: LocationRouteStatusV2,
    control: watch::Sender<LocationRouteControl>,
    paused_by_instance: bool,
}

pub struct WorkerService {
    paths: DataPaths,
    instance_id: Uuid,
    identity: WorkerIdentityV2,
    endpoint: String,
    secret: Vec<u8>,
    _instance_lock: std::fs::File,
    mutable: Mutex<WorkerMutable>,
    operation: Mutex<()>,
    display_operation: Mutex<()>,
    shutdown: watch::Sender<bool>,
}

impl std::fmt::Debug for WorkerService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerService")
            .field("paths", &self.paths)
            .field("instance_id", &self.instance_id)
            .field("identity", &self.identity)
            .field("endpoint", &self.endpoint)
            .field("secret", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl Drop for WorkerService {
    fn drop(&mut self) {
        self.secret.fill(0);
        let path = self.paths.worker_descriptor(self.instance_id);
        let owned = hd_platform::read_regular_nofollow_limited(&path, 64 * 1024)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<WorkerDescriptorV2>(&bytes).ok())
            .is_some_and(|descriptor| descriptor.identity == self.identity);
        if owned
            && let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                event = "worker.descriptor.cleanup.failed",
                instance_id = %self.instance_id,
                path = %path.display(),
                %error,
                "failed to remove owned worker descriptor"
            );
        }
    }
}

impl WorkerService {
    pub fn open(
        paths: DataPaths,
        instance_id: Uuid,
        nonce: Uuid,
        endpoint: String,
    ) -> Result<Arc<Self>, WorkerError> {
        paths.ensure()?;
        #[cfg(target_os = "macos")]
        hd_platform::ensure_open_file_limit(4096)?;
        hd_platform::ensure_owner_only_directory(&paths.worker_dir(instance_id))?;
        let instance_lock = acquire_worker_instance_lock(&paths, instance_id)?;
        let secret_path = paths.worker_secret(instance_id);
        let mut secret = hd_platform::read_regular_nofollow_limited(&secret_path, 256)?;
        while secret.last().is_some_and(u8::is_ascii_whitespace) {
            secret.pop();
        }
        if secret.len() != 64 || !secret.iter().all(u8::is_ascii_hexdigit) {
            return Err(WorkerError::SecretInvalid);
        }
        let identity = hd_platform::current_process_identity(nonce)?;
        let descriptor = WorkerDescriptorV2 {
            protocol_version: WORKER_PROTOCOL_VERSION,
            instance_id,
            identity: identity.clone(),
            endpoint: endpoint.clone(),
            secret_path,
            started_at: OffsetDateTime::now_utc(),
        };
        let descriptor_bytes = serde_json::to_vec_pretty(&descriptor)?;
        hd_platform::write_owner_only(&paths.worker_descriptor(instance_id), &descriptor_bytes)?;
        let (shutdown, _) = watch::channel(false);
        Ok(Arc::new(Self {
            paths,
            instance_id,
            identity: identity.clone(),
            endpoint,
            secret,
            _instance_lock: instance_lock,
            mutable: Mutex::new(WorkerMutable {
                status: WorkerStatusV2 {
                    identity,
                    observed: ObservedStateV2::Stopped,
                    run_id: None,
                    child_pid: None,
                    cleanup_pending: false,
                    adb_serial: None,
                    adb_ready: false,
                    frame_generation: 0,
                    frame_metrics: FrameMetricsV2::default(),
                    runtime_displays: Vec::new(),
                    screen_recording: None,
                    last_screen_recording: None,
                    location_route: None,
                    last_location_route: None,
                    uwb_ranging: None,
                    modem_state: None,
                    sensor_pose: None,
                    bluetooth_peers: Vec::new(),
                    last_bluetooth_hci_capture: None,
                    last_error: None,
                },
                active_spec: None,
                process: None,
                components: Vec::new(),
                device_control_tokens: BTreeMap::new(),
                backend: None,
                launch: None,
                adb: None,
                adb_ready: false,
                journal: None,
                started_at: None,
                display_session: None,
                #[cfg(any(windows, target_os = "macos"))]
                host_recorder_endpoint: None,
                active_screen_recording: None,
                active_location_route: None,
                #[cfg(unix)]
                device_output_files: Vec::new(),
                #[cfg(unix)]
                device_input_fifos: Vec::new(),
                #[cfg(unix)]
                microdroid_console_challenge: None,
            }),
            operation: Mutex::new(()),
            display_operation: Mutex::new(()),
            shutdown,
        }))
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub async fn status(&self) -> WorkerStatusV2 {
        let mutable = self.mutable.lock().await;
        let mut status = mutable.status.clone();
        status.adb_ready = mutable.adb_ready;
        status.screen_recording = mutable
            .active_screen_recording
            .as_ref()
            .map(|recording| recording.status.clone());
        status.location_route = mutable
            .active_location_route
            .as_ref()
            .map(|route| route.status.clone());
        drop(mutable);
        if let Some(run_id) = status.run_id {
            let metrics_path = self
                .paths
                .run_dir(self.instance_id, run_id)
                .join("frame-metrics-v2.json");
            if let Ok(bytes) = hd_platform::read_regular_nofollow_limited(&metrics_path, 64 * 1024)
                && let Ok(metrics) = serde_json::from_slice::<FrameMetricsV2>(&bytes)
                && metrics.generation == status.frame_generation
            {
                status.frame_metrics = metrics;
            }
        }
        status
    }

    pub async fn shutdown_gracefully(&self) -> Result<(), WorkerError> {
        let status = self.status().await;
        let components_pending = !self.mutable.lock().await.components.is_empty();
        if status.observed != ObservedStateV2::Stopped
            || status.cleanup_pending
            || status.child_pid.is_some()
            || components_pending
        {
            self.stop(StopModeV2::Graceful, Duration::from_secs(20))
                .await?;
        }
        let final_status = self.status().await;
        if final_status.observed != ObservedStateV2::Stopped
            || final_status.cleanup_pending
            || final_status.child_pid.is_some()
            || !self.mutable.lock().await.components.is_empty()
        {
            return Err(WorkerError::ComponentCleanup(
                "worker shutdown requires a clean Stopped state".to_owned(),
            ));
        }
        let _ = self.shutdown.send(true);
        Ok(())
    }

    pub async fn handle(self: Arc<Self>, request: WorkerRequestV2) -> WorkerResponseV2 {
        let request_id = request.request_id;
        if request.protocol_version != WORKER_PROTOCOL_VERSION {
            return WorkerResponseV2::failure(
                request_id,
                ApiErrorV2::new("worker_protocol_version", "unsupported worker protocol"),
            );
        }
        if request.instance_id != self.instance_id {
            return WorkerResponseV2::failure(
                request_id,
                ApiErrorV2::new("worker_instance_mismatch", "worker instance does not match"),
            );
        }
        let supplied = request.bearer_token.as_bytes();
        if supplied.len() != self.secret.len() || supplied.ct_eq(&self.secret).unwrap_u8() != 1 {
            tracing::warn!(
                event = "worker.auth.rejected",
                error_code = "worker_unauthorized",
                instance_id = %self.instance_id,
                "worker request authentication failed"
            );
            return WorkerResponseV2::failure(
                request_id,
                ApiErrorV2::new("worker_unauthorized", "worker authentication failed"),
            );
        }
        let result = Box::pin(self.dispatch_command(request.command)).await;
        match result {
            Ok(payload) => WorkerResponseV2::success(request_id, payload),
            Err(error) => {
                tracing::error!(
                    event = "worker.command.failed",
                    error_code = error.code(),
                    instance_id = %self.instance_id,
                    %error,
                    "worker command failed"
                );
                WorkerResponseV2::failure(request_id, error.api_error())
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn dispatch_command(
        self: &Arc<Self>,
        command: WorkerCommandV2,
    ) -> Result<WorkerPayloadV2, WorkerError> {
        match command {
            WorkerCommandV2::Ping => Ok(WorkerPayloadV2::Pong(self.status().await)),
            WorkerCommandV2::Status => Ok(WorkerPayloadV2::Status(self.status().await)),
            WorkerCommandV2::Start {
                spec,
                run_id,
                leases,
                capabilities_fingerprint,
                initial_display,
            } => self
                .start(
                    *spec,
                    run_id,
                    leases,
                    &capabilities_fingerprint,
                    initial_display,
                )
                .await
                .map(|()| WorkerPayloadV2::Empty),
            WorkerCommandV2::Stop {
                mode,
                graceful_timeout_ms,
            } => self
                .stop(mode, Duration::from_millis(u64::from(graceful_timeout_ms)))
                .await
                .map(|()| WorkerPayloadV2::Empty),
            WorkerCommandV2::Pause => self.pause().await.map(|()| WorkerPayloadV2::Empty),
            WorkerCommandV2::Resume => self.resume().await.map(|()| WorkerPayloadV2::Empty),
            WorkerCommandV2::Reconfigure { display, adb } => self
                .reconfigure(display, adb)
                .await
                .map(|()| WorkerPayloadV2::Empty),
            WorkerCommandV2::Action { action } => {
                self.action(action).await.map(|()| WorkerPayloadV2::Empty)
            }
            WorkerCommandV2::InstallApk {
                upload_path,
                sha256,
            } => self
                .install_apk(&upload_path, &sha256)
                .await
                .map(|()| WorkerPayloadV2::Empty),
            WorkerCommandV2::AttachDisplay {
                session_id,
                generation,
                display_id,
                target,
                viewport,
            } => self
                .attach_display(session_id, generation, display_id, target, viewport)
                .await
                .map(|()| WorkerPayloadV2::Empty),
            WorkerCommandV2::ResizeDisplay {
                session_id,
                generation,
                viewport,
            } => self
                .resize_display(session_id, generation, viewport)
                .await
                .map(|()| WorkerPayloadV2::Empty),
            WorkerCommandV2::DetachDisplay {
                session_id,
                generation,
            } => self
                .detach_display(session_id, generation)
                .await
                .map(|()| WorkerPayloadV2::Empty),
            WorkerCommandV2::CaptureScreenshot { output_path } => self
                .capture_screenshot(&output_path)
                .await
                .map(WorkerPayloadV2::Screenshot),
            WorkerCommandV2::CollectAndroidBugreport {
                bugreport_id,
                output_path,
            } => self
                .collect_android_bugreport(bugreport_id, &output_path)
                .await
                .map(WorkerPayloadV2::AndroidBugreport),
            WorkerCommandV2::StartScreenRecording {
                recording_id,
                display_id,
                output_path,
                max_duration_seconds,
            } => self
                .start_screen_recording(
                    recording_id,
                    display_id,
                    &output_path,
                    max_duration_seconds,
                )
                .await
                .map(WorkerPayloadV2::ScreenRecordingStatus),
            WorkerCommandV2::StopScreenRecording { recording_id } => self
                .stop_screen_recording(recording_id)
                .await
                .map(WorkerPayloadV2::ScreenRecording),
            WorkerCommandV2::CollectGuestLogs => self
                .collect_guest_logs()
                .await
                .map(WorkerPayloadV2::GuestLog),
            WorkerCommandV2::Diagnose => Ok(WorkerPayloadV2::Diagnostics(
                Box::pin(self.diagnostics()).await,
            )),
            WorkerCommandV2::Shutdown => {
                let status = self.status().await;
                if status.observed.is_active() {
                    Err(WorkerError::Busy(
                        "stop the instance before worker shutdown",
                    ))
                } else {
                    self.shutdown_gracefully()
                        .await
                        .map(|()| WorkerPayloadV2::Empty)
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn start(
        self: &Arc<Self>,
        spec: InstanceSpecV2,
        run_id: Uuid,
        leases: Vec<LeaseV2>,
        expected_capabilities: &str,
        initial_display: Option<PreparedNativeDisplayV2>,
    ) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        spec.validate()?;
        if spec.id != self.instance_id {
            return Err(WorkerError::InstanceMismatch);
        }
        let frame_generation =
            validate_start_leases(&leases, &self.identity, &spec, &self.paths, &self.endpoint)?;
        if let Some(display) = &initial_display {
            if !display.viewport.is_valid() {
                return Err(WorkerError::DisplaySession(
                    "initial display viewport is outside supported bounds".to_owned(),
                ));
            }
            if !process_identity_is_alive(display.target.owner()) {
                return Err(WorkerError::DisplaySession(
                    "initial display owner is not alive".to_owned(),
                ));
            }
        }
        {
            let mutable = self.mutable.lock().await;
            if mutable.process.is_some() || mutable.status.observed.is_active() {
                return Err(WorkerError::Busy("instance is already active"));
            }
            if frame_generation <= mutable.status.frame_generation {
                return Err(WorkerError::LeaseContract(format!(
                    "frame generation {frame_generation} is not newer than {}",
                    mutable.status.frame_generation
                )));
            }
        }
        let trace_id = Uuid::new_v4();
        let retention_paths = self.paths.clone();
        let retention_instance_id = spec.id;
        let retention = tokio::task::spawn_blocking(move || {
            enforce_run_retention(&retention_paths, retention_instance_id)
        })
        .await
        .map_err(|error| WorkerError::Task(format!("join run retention task: {error}")))??;
        for path in &retention.removed_ephemeral {
            tracing::info!(
                event = "runtime.run.ephemeral_artifact.removed",
                instance_id = %self.instance_id,
                path = %path.display(),
                maintenance = "pre_start_retention",
                "removed a reproducible launch artifact retained by a finalized run"
            );
        }
        for compacted in &retention.compacted {
            tracing::info!(
                event = "runtime.run.log_compacted",
                instance_id = %self.instance_id,
                path = %compacted.path.display(),
                previous_bytes = compacted.previous_bytes,
                retained_tail_bytes = compacted.retained_tail_bytes,
                "compacted an oversized log from a finalized run"
            );
        }
        for pruned in &retention.pruned {
            tracing::info!(
                event = "runtime.run.pruned",
                instance_id = %self.instance_id,
                run_id = %pruned.run_id,
                bytes = pruned.bytes,
                retained_count = retention.retained_count,
                retained_bytes = retention.retained_bytes,
                "pruned finalized run under the runtime retention policy"
            );
        }
        let available_disk_bytes = available_runtime_disk_bytes(&self.paths)?;
        let required_disk_bytes =
            runtime_disk_requirement(&self.paths, Some(&spec)).required_free_bytes;
        if available_disk_bytes < required_disk_bytes {
            tracing::error!(
                event = "runtime.disk.low_watermark",
                error_code = "disk_low_watermark",
                instance_id = %self.instance_id,
                available_disk_bytes,
                required_disk_bytes,
                "runtime data volume is below the start low-watermark"
            );
            return Err(WorkerError::DiskLowWatermark {
                available: available_disk_bytes,
                required: required_disk_bytes,
            });
        }
        let run_dir = self.paths.run_dir(self.instance_id, run_id);
        let journal = Arc::new(RunJournalV2::create(
            &run_dir,
            self.instance_id,
            run_id,
            trace_id,
        )?);
        spawn_run_log_maintenance(run_dir.clone());
        let started_at = OffsetDateTime::now_utc();
        let initial_manifest = RunManifestV2 {
            schema_version: 2,
            run_id,
            instance: spec.clone(),
            artifact_bundles: Vec::new(),
            capabilities_fingerprint: expected_capabilities.to_owned(),
            launch: None,
            toolchain: toolchain_fingerprint(None),
        };
        journal.write_manifest(&initial_manifest)?;
        {
            let mut mutable = self.mutable.lock().await;
            mutable.status.run_id = Some(run_id);
            mutable.status.cleanup_pending = false;
            mutable.status.screen_recording = None;
            mutable.status.last_screen_recording = None;
            mutable.active_screen_recording = None;
            mutable.status.location_route = None;
            mutable.status.last_location_route = None;
            mutable.active_location_route = None;
            mutable.status.uwb_ranging = (spec.guest_kind == GuestKindV2::Android
                && spec.devices.uwb)
                .then_some(hd_core::UwbRangingV2 { distance_cm: 250 });
            mutable.status.modem_state = (spec.guest_kind == GuestKindV2::Android
                && spec.devices.modem)
                .then(hd_core::ModemStateV2::default);
            mutable.status.bluetooth_peers.clear();
            mutable.status.last_bluetooth_hci_capture = None;
            mutable.status.runtime_displays = configured_runtime_displays(&spec);
            #[cfg(unix)]
            {
                mutable.microdroid_console_challenge = None;
            }
            mutable.active_spec = Some(spec.clone());
            mutable.journal = Some(Arc::clone(&journal));
            mutable.started_at = Some(started_at);
            #[cfg(any(windows, target_os = "macos"))]
            {
                mutable.host_recorder_endpoint = None;
            }
            mutable.display_session =
                initial_display
                    .as_ref()
                    .map(|display| ActiveDisplaySession {
                        id: display.session_id,
                        generation: frame_generation,
                        display_id: display.display_id,
                        target: display.target.clone(),
                        viewport: display.viewport.clone(),
                    });
        }
        self.transition(ObservedStateV2::Preparing, None).await?;
        let start_result: Result<(), WorkerError> = async {
            if spec.guest_kind == GuestKindV2::Microdroid {
                return self
                    .start_microdroid(
                        &spec,
                        run_id,
                        &run_dir,
                        &leases,
                        frame_generation,
                        expected_capabilities,
                        &journal,
                    )
                    .await;
            }
            journal.boundary_started("capability.discovery", BTreeMap::new())?;
            let discovery = CapabilityDiscovery::discover_defaults(self.paths.clone(), None)
                .discover(Some(&spec))
                .await;
            if discovery.capabilities.fingerprint != expected_capabilities {
                let error = WorkerError::CapabilityChanged {
                    expected: expected_capabilities.to_owned(),
                    actual: discovery.capabilities.fingerprint,
                };
                return Err(error);
            }
            if !discovery.capabilities.can_start() {
                let blocked = discovery
                    .capabilities
                    .probes
                    .iter()
                    .filter(|probe| {
                        probe.required
                            && !matches!(probe.status, hd_core::CapabilityStatusV2::Supported)
                    })
                    .map(|probe| probe.id.clone())
                    .collect::<Vec<_>>();
                let error = WorkerError::CapabilityBlocked(blocked);
                return Err(error);
            }
            let bundles = discovery.bundles.ok_or_else(|| {
                WorkerError::CapabilityBlocked(vec!["artifact.bundles".to_owned()])
            })?;
            crate::disk::validate_android_fstab(&bundles.artifacts.android_fstab)
                .map_err(WorkerError::Platform)?;
            let dev_display_copy_fallback = crate::dev::allow_display_copy_fallback_enabled();
            let native_display_direct = crate::dev::native_display_direct_enabled();
            let frame_tool = if dev_display_copy_fallback || native_display_direct {
                None
            } else {
                Some(discovery.frame_tool.ok_or_else(|| {
                    WorkerError::CapabilityBlocked(vec!["display.zero_copy".to_owned()])
                })?)
            };
            #[cfg(any(windows, target_os = "macos"))]
            let adb_bridge = discovery.adb_bridge;
            let host_tools = bundles.artifacts.host_tools.clone();
            let sensor_injector = bundles.artifacts.sensor_injector.clone();
            journal.boundary_succeeded("capability.discovery", 0, BTreeMap::new())?;

            self.transition(ObservedStateV2::StartingWorker, None)
                .await?;
            let overlay = self.paths.disk_overlay(self.instance_id);
            let disk_started = Instant::now();
            journal.boundary_started(
                "disk.provision",
                BTreeMap::from([("destination".to_owned(), overlay.display().to_string())]),
            )?;
            let provisioned = NativeDiskProvisioner
                .provision(&bundles.artifacts.rootfs, &overlay)
                .await
                .map_err(WorkerError::Platform)?;
            journal.boundary_succeeded(
                "disk.provision",
                elapsed_ms(disk_started),
                BTreeMap::from([
                    ("method".to_owned(), format!("{:?}", provisioned.method)),
                    ("bytes".to_owned(), provisioned.bytes.to_string()),
                ]),
            )?;

            let guest_cid = lease_number::<u32>(&leases, LeaseKindV2::GuestCid)?;
            let adb_port = if matches!(spec.adb.mode, AdbModeV2::Loopback) {
                Some(lease_number::<u16>(&leases, LeaseKindV2::AdbPort)?)
            } else {
                None
            };
            let endpoints = RuntimeEndpoints::create(&spec, run_id)?;
            #[cfg(unix)]
            {
                let mut mutable = self.mutable.lock().await;
                mutable.device_output_files = endpoints.output_files;
                mutable.device_input_fifos = endpoints.input_fifos;
            }
            let backend = CrosvmBackend::new(discovery.crosvm.clone());
            backend
                .prepare_keyboard_endpoint(&endpoints.keyboard)
                .await?;
            if let Some(endpoint) = endpoints.trackpad.as_deref() {
                backend.prepare_trackpad_endpoint(endpoint).await?;
            }
            let artifacts = bundles.artifacts.clone();
            #[cfg(target_os = "macos")]
            let artifacts = {
                let mut artifacts = artifacts;
                let patched_initrd = run_dir.join("initrd-android-hd.img");
                patch_android_initrd_bootconfig(&artifacts.initrd, &patched_initrd).map_err(
                    |source| WorkerError::Io {
                        operation: "patch Android initrd bootconfig",
                        path: patched_initrd.clone(),
                        source,
                    },
                )?;
                artifacts.initrd = patched_initrd;
                artifacts
            };
            let context = VmLaunchContextV2 {
                spec: spec.clone(),
                run_id,
                guest_cid,
                run_dir: run_dir.clone(),
                disk_overlay: overlay,
                artifacts,
                control_endpoint: endpoints.control,
                frame_endpoint: endpoints.frame,
                keyboard_endpoint: endpoints.keyboard,
                trackpad_endpoint: endpoints.trackpad,
                device_endpoints: endpoints.devices,
                device_control_endpoints: endpoints.device_controls,
                adb_host_port: adb_port,
            };
            let launch = backend.build_launch_plan(&context).await?;
            journal.write_manifest(&RunManifestV2 {
                schema_version: 2,
                run_id,
                instance: spec.clone(),
                artifact_bundles: vec![bundles.guest_manifest, bundles.host_manifest],
                capabilities_fingerprint: expected_capabilities.to_owned(),
                launch: Some(launch.clone()),
                toolchain: toolchain_fingerprint(Some(&launch.executable)),
            })?;
            {
                let mut mutable = self.mutable.lock().await;
                mutable.backend = Some(backend.clone());
                mutable.launch = Some(launch.clone());
            }
            if let Some(frame_tool) = frame_tool {
                self.start_component(
                    ComponentStartSpec {
                        executable: frame_tool,
                        component: "frame-producer".to_owned(),
                        run_id,
                        run_dir: run_dir.clone(),
                        configuration: FormalComponentConfigurationV2::FrameBroker {
                            broker_endpoint: launch.frame_endpoint.clone(),
                            frame_ready_marker: run_dir.join("frame-ready-v2.json"),
                            generation: frame_generation,
                            transport: expected_frame_transport(),
                            display: spec.display.clone(),
                        },
                    },
                    &journal,
                )
                .await?;
            }
            self.transition(ObservedStateV2::LaunchingGuest, None)
                .await?;
            let mut process_environment = launch.environment.clone();
            #[cfg(any(windows, target_os = "macos"))]
            {
                let host_recorder_endpoint = host_recorder_endpoint_for_run(run_id);
                process_environment.insert(
                    HD_HOST_RECORDER_ENDPOINT_ENV.to_owned(),
                    host_recorder_endpoint.to_string_lossy().into_owned(),
                );
            }
            #[cfg(any(windows, target_os = "macos"))]
            {
                process_environment.insert(
                    "HD_FRAME_METRICS_PATH".to_owned(),
                    run_dir
                        .join("frame-metrics-v2.json")
                        .to_string_lossy()
                        .into_owned(),
                );
                process_environment.insert(
                    "HD_FRAME_GENERATION".to_owned(),
                    frame_generation.to_string(),
                );
            }
            if let Some(display) = &initial_display {
                inject_initial_display_environment(&mut process_environment, display);
            }
            let process = TokioProcessSupervisor
                .spawn(&ProcessSpec {
                    executable: launch.executable.clone(),
                    arguments: launch.arguments.clone(),
                    environment: process_environment,
                    working_directory: launch.working_directory.clone(),
                    stdout_path: run_dir.join("crosvm.stdout.log"),
                    stderr_path: run_dir.join("crosvm.stderr.log"),
                    latency_sensitive: cfg!(windows),
                    kill_on_drop: true,
                })
                .await?;
            {
                let mut mutable = self.mutable.lock().await;
                mutable.status.child_pid = Some(process.id());
                mutable.status.adb_serial.clone_from(&launch.adb_serial);
                mutable.status.frame_generation = frame_generation;
                mutable.status.frame_metrics.generation = frame_generation;
                mutable.process = Some(process);
                #[cfg(any(windows, target_os = "macos"))]
                {
                    mutable.host_recorder_endpoint = Some(host_recorder_endpoint_for_run(run_id));
                }
            }

            self.start_device_components(
                &spec,
                run_id,
                &run_dir,
                guest_cid,
                &launch,
                &host_tools,
                &journal,
            )
            .await?;

            self.transition(ObservedStateV2::NegotiatingDisplay, None)
                .await?;
            // gfxstream creates its Vulkan render HWND only after SurfaceFlinger starts. Start all
            // Guest-facing device backends first; KeyMint, Gatekeeper and networking are part of
            // Android userspace boot and waiting before them creates a startup deadlock.
            if let Some(display) = initial_display.clone() {
                self.spawn_deferred_initial_display(
                    display,
                    run_id,
                    frame_generation,
                );
            }
            if dev_display_copy_fallback || native_display_direct {
                self.wait_native_display_ready().await?;
            } else {
                self.wait_frame_ready(&run_dir, run_id, frame_generation)
                    .await?;
            }
            #[cfg(windows)]
            if !spec.display.secondary_displays.is_empty() {
                let display_started = Instant::now();
                journal.boundary_started(
                    "display.secondary.cold_start",
                    BTreeMap::from([(
                        "count".to_owned(),
                        spec.display.secondary_displays.len().to_string(),
                    )]),
                )?;
                for (index, secondary) in spec.display.secondary_displays.iter().enumerate() {
                    let scanout_id = u32::try_from(index + 1)
                        .expect("configured secondary display count fits in u32");
                    backend
                        .add_secondary_display(&launch.control_endpoint, scanout_id, secondary)
                        .await?;
                }
                tokio::time::sleep(ANDROID_DISPLAY_HOTPLUG_SETTLE_DELAY).await;
                journal.boundary_succeeded(
                    "display.secondary.cold_start",
                    elapsed_ms(display_started),
                    BTreeMap::from([(
                        "scanouts".to_owned(),
                        format!("1..={}", spec.display.secondary_displays.len()),
                    )]),
                )?;
            }
            self.transition(ObservedStateV2::GuestBooting, None).await?;

            if let Some(serial) = &launch.adb_serial {
                self.transition(ObservedStateV2::AdbConnecting, None)
                    .await?;
                #[cfg(any(windows, target_os = "macos"))]
                {
                    let bridge = adb_bridge.ok_or_else(|| {
                        WorkerError::CapabilityBlocked(vec!["adb.bridge".to_owned()])
                    })?;
                    let port = adb_port.ok_or(WorkerError::ReadinessUnavailable)?;
                    self.start_component(
                        ComponentStartSpec {
                            executable: bridge,
                            component: "adb-bridge".to_owned(),
                            run_id,
                            run_dir: run_dir.clone(),
                            configuration: FormalComponentConfigurationV2::AdbBridge {
                                listen_address: "127.0.0.1".to_owned(),
                                listen_port: port,
                                guest_cid,
                                guest_port: 5555,
                                vm_control_endpoint: launch.control_endpoint.clone(),
                                crosvm_executable: launch.executable.clone(),
                            },
                        },
                        &journal,
                    )
                    .await?;
                }
                #[cfg(not(any(windows, target_os = "macos")))]
                tracing::warn!(
                    event = "worker.adb.bridge.skipped",
                    instance_id = %self.instance_id,
                    run_id = %run_id,
                    %serial,
                    "ADB bridge is not available on this host; readiness remains deferred"
                );
                let adb = AdbClient::new(discovery.adb, None)
                    .with_aapt2(host_tools.get("aapt2").cloned())
                    .with_sensor_injector(sensor_injector.is_file().then_some(sensor_injector));
                if native_display_direct {
                    // Native display attachment must not depend on adbd. Direct-linux images can
                    // create and present the gfxstream surface before their ADB transport is
                    // configured (and some development images intentionally leave adbd offline).
                    // Publish Ready now so HD can keep retrying the native HWND attach while the
                    // guest finishes booting. ADB-backed actions retain their normal per-command
                    // errors until the transport becomes available.
                    tracing::warn!(
                        event = "worker.adb.readiness.deferred_for_native_display",
                        instance_id = %self.instance_id,
                        run_id = %run_id,
                        %serial,
                        "native display startup is not blocked on ADB readiness"
                    );
                } else if crate::dev::allow_adb_offline_boot_ready_enabled() {
                    tracing::warn!(
                        event = "worker.adb.readiness.dev_fallback",
                        instance_id = %self.instance_id,
                        run_id = %run_id,
                        %serial,
                        "using dev boot-log readiness fallback instead of strict ADB connect/readiness"
                    );
                    self.wait_dev_boot_log_ready(&run_dir).await?;
                } else {
                    self.wait_adbd_log_ready(&run_dir).await?;
                    adb.connect(serial).await.map_err(WorkerError::Adb)?;
                    adb.wait_ready(serial).await.map_err(WorkerError::Adb)?;
                }
                if !native_display_direct {
                    adb.apply_runtime_device_policy(
                        serial,
                        spec.devices.bluetooth,
                        spec.devices.nfc,
                        spec.devices.modem,
                    )
                    .await;
                    refresh_network_validation(
                        &adb,
                        spec.devices.network,
                        self.instance_id,
                        run_id,
                        serial,
                    )
                    .await;
                    adb.keep_display_awake(serial).await.map_err(WorkerError::Adb)?;
                    adb.set_display_configuration(serial, &spec.display)
                        .await
                        .map_err(WorkerError::Adb)?;
                }
                self.ensure_components_alive().await?;
                let mut mutable = self.mutable.lock().await;
                mutable.adb = Some(adb);
                mutable.adb_ready = !native_display_direct;
            } else {
                return Err(WorkerError::ReadinessUnavailable);
            }
            self.transition(ObservedStateV2::Ready, None).await?;
            if native_display_direct {
                self.spawn_deferred_adb_readiness(
                    run_id,
                    run_dir.clone(),
                    launch
                        .adb_serial
                        .clone()
                        .ok_or(WorkerError::ReadinessUnavailable)?,
                    DeferredAdbReadinessPolicy {
                        display: spec.display.clone(),
                        bluetooth_enabled: spec.devices.bluetooth,
                        nfc_enabled: spec.devices.nfc,
                        modem_enabled: spec.devices.modem,
                        network_enabled: spec.devices.network,
                    },
                );
            }
            self.spawn_exit_monitor();
            Ok(())
        }
        .await;
        match start_result {
            Ok(()) => Ok(()),
            Err(error) => {
                let blocked = error.blocks_start();
                if let Err(cleanup_error) = self.fail_start(&error, blocked).await {
                    tracing::error!(
                        event = "worker.start.cleanup.failed",
                        error_code = cleanup_error.code(),
                        instance_id = %self.instance_id,
                        run_id = %run_id,
                        %cleanup_error,
                        "failed to complete start failure cleanup"
                    );
                }
                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn start_microdroid(
        self: &Arc<Self>,
        spec: &InstanceSpecV2,
        run_id: Uuid,
        run_dir: &Path,
        leases: &[LeaseV2],
        frame_generation: u64,
        expected_capabilities: &str,
        journal: &RunJournalV2,
    ) -> Result<(), WorkerError> {
        if !(cfg!(all(target_os = "macos", target_arch = "aarch64"))
            || cfg!(all(target_os = "windows", target_arch = "x86_64")))
        {
            return Err(WorkerError::CapabilityBlocked(vec![
                "microdroid.host_platform".to_owned(),
            ]));
        }
        journal.boundary_started("microdroid.capability.discovery", BTreeMap::new())?;
        let discovery = CapabilityDiscovery::discover_defaults(self.paths.clone(), None)
            .discover(Some(spec))
            .await;
        if discovery.capabilities.fingerprint != expected_capabilities {
            return Err(WorkerError::CapabilityChanged {
                expected: expected_capabilities.to_owned(),
                actual: discovery.capabilities.fingerprint,
            });
        }
        if !discovery.capabilities.can_start() {
            return Err(WorkerError::CapabilityBlocked(
                discovery
                    .capabilities
                    .probes
                    .iter()
                    .filter(|probe| {
                        probe.required
                            && !matches!(probe.status, hd_core::CapabilityStatusV2::Supported)
                    })
                    .map(|probe| probe.id.clone())
                    .collect(),
            ));
        }
        journal.boundary_succeeded("microdroid.capability.discovery", 0, BTreeMap::new())?;

        let config = spec
            .microdroid
            .as_ref()
            .ok_or_else(|| WorkerError::CapabilityBlocked(vec!["microdroid.config".to_owned()]))?;
        let guest_cid = lease_number::<u32>(leases, LeaseKindV2::GuestCid)?;
        let adb_port = if matches!(spec.adb.mode, AdbModeV2::Loopback) {
            Some(lease_number::<u16>(leases, LeaseKindV2::AdbPort)?)
        } else {
            None
        };
        let runtime = resolve_microdroid_runtime_paths();
        let vm = runtime.vm();
        let virtmgr = runtime.virtmgr();
        let crosvm = runtime.crosvm();
        let bin = runtime.bin_dir.clone();
        #[cfg(target_os = "macos")]
        let lib = runtime.lib_dir.clone();
        let apex_tree = runtime.product_root.join("apex_dir");
        let apex_root = apex_tree.join("apex");
        let instance_root = self.paths.instance_dir(spec.id).join("microdroid");
        let work_dir = instance_root.join("work");
        let temp_dir = run_dir.join("microdroid-temp");
        for path in [&instance_root, &work_dir, &temp_dir] {
            hd_platform::ensure_owner_only_directory(path)?;
        }
        let console = run_dir.join("microdroid-console.txt");
        #[cfg(unix)]
        let console_in = run_dir.join("microdroid-console-in.fifo");
        #[cfg(not(unix))]
        let console_in = run_dir.join("microdroid-console-in.txt");
        let guest_log = run_dir.join("microdroid-guest.log");
        let trace_file = run_dir.join("microdroid-virtmgr-trace.log");
        let client_trace = run_dir.join("microdroid-vmclient-trace.log");
        // AOSP `vm` opens this path as the console input FD. On Unix keep an owner-only FIFO
        // open on both ends so it does not become an EOF-only launch file. The product API only
        // writes one fixed, random nonce challenge and never accepts arbitrary console text.
        #[cfg(unix)]
        let console_challenge = MicrodroidConsoleChallengeChannel::create(
            &console_in,
            &console,
            run_dir.join("microdroid-console-challenge.json"),
        )?;
        #[cfg(not(unix))]
        hd_platform::write_owner_only(&console_in, &[])?;
        let mut environment = BTreeMap::from([
            (
                "VIRTMGR_PATH".to_owned(),
                virtmgr.to_string_lossy().into_owned(),
            ),
            (
                "VIRTMGR_CROSVM_PATH".to_owned(),
                crosvm.to_string_lossy().into_owned(),
            ),
            (
                "VIRTMGR_APEX_ROOT".to_owned(),
                apex_root.to_string_lossy().into_owned(),
            ),
            (
                "VIRTMGR_SYSTEM_ROOT".to_owned(),
                apex_tree.join("system").to_string_lossy().into_owned(),
            ),
            (
                "VIRTMGR_SYSTEM_EXT_ROOT".to_owned(),
                apex_tree.join("system_ext").to_string_lossy().into_owned(),
            ),
            (
                "ANDROID_PROP_RO_BUILD_VERSION_SDK".to_owned(),
                "35".to_owned(),
            ),
            (
                "VIRTMGR_TRACE_FILE".to_owned(),
                trace_file.to_string_lossy().into_owned(),
            ),
            (
                "VMCLIENT_TRACE_FILE".to_owned(),
                client_trace.to_string_lossy().into_owned(),
            ),
            ("VIRTMGR_GUEST_CID".to_owned(), guest_cid.to_string()),
            ("VIRTMGR_KEEP_TEMP".to_owned(), "1".to_owned()),
            ("TMPDIR".to_owned(), temp_dir.to_string_lossy().into_owned()),
        ]);
        #[cfg(target_os = "macos")]
        environment.insert(
            "DYLD_LIBRARY_PATH".to_owned(),
            lib.to_string_lossy().into_owned(),
        );
        if let Some(parent_path) = std::env::var_os("PATH") {
            #[cfg(windows)]
            let separator = ";";
            #[cfg(not(windows))]
            let separator = ":";
            environment.insert(
                "PATH".to_owned(),
                format!(
                    "{}{separator}{}",
                    bin.display(),
                    PathBuf::from(parent_path).display()
                ),
            );
        } else {
            environment.insert("PATH".to_owned(), bin.to_string_lossy().into_owned());
        }
        let mut arguments = match &config.payload {
            MicrodroidPayloadV2::Empty => vec![
                "run-microdroid".to_owned(),
                "--work-dir".to_owned(),
                work_dir.to_string_lossy().into_owned(),
            ],
            MicrodroidPayloadV2::Uploaded {
                upload_id,
                sha256,
                config_path,
            } => {
                let apk = self.paths.upload_path(*upload_id);
                verify_upload_digest(&apk, sha256)?;
                let inspection =
                    crate::inspect_microdroid_payload_apk(&apk)
                        .await
                        .map_err(|error| {
                            WorkerError::ComponentContract(format!(
                                "Microdroid Payload APK is invalid: {error}"
                            ))
                        })?;
                let declared = usize::from(inspection.declared_extra_apk_count);
                let selected = config.extra_apks.len();
                if declared != selected {
                    return Err(WorkerError::MicrodroidExtraApkCountMismatch {
                        declared,
                        selected,
                    });
                }
                let idsig = instance_root.join("payload.idsig");
                create_microdroid_idsig(spec.id, run_id, &vm, &apk, &idsig, &environment, run_dir)
                    .await?;
                vec![
                    "run-app".to_owned(),
                    apk.to_string_lossy().into_owned(),
                    idsig.to_string_lossy().into_owned(),
                    instance_root
                        .join("instance.img")
                        .to_string_lossy()
                        .into_owned(),
                    "--config-path".to_owned(),
                    config_path.clone(),
                ]
            }
        };
        for (index, extra) in config.extra_apks.iter().enumerate() {
            let apk = self.paths.upload_path(extra.upload_id);
            verify_upload_digest(&apk, &extra.sha256)?;
            crate::validate_microdroid_extra_apk(&apk)
                .await
                .map_err(|error| {
                    WorkerError::ComponentContract(format!(
                        "Microdroid extra APK #{index} is invalid: {error}"
                    ))
                })?;
            let idsig = run_dir.join(format!("microdroid-extra-{index}.idsig"));
            let idsig_file = hd_platform::open_owner_only_rw(&idsig)?;
            idsig_file.set_len(0).map_err(|source| WorkerError::Io {
                operation: "truncate Microdroid extra APK idsig",
                path: idsig.clone(),
                source,
            })?;
            arguments.extend([
                "--extra-apk-override".to_owned(),
                apk.to_string_lossy().into_owned(),
                "--extra-idsig".to_owned(),
                idsig.to_string_lossy().into_owned(),
            ]);
        }
        arguments.extend([
            "--name".to_owned(),
            format!("HD-{}-{}", spec.name, &spec.id.simple().to_string()[..8]),
            "--mem".to_owned(),
            spec.memory_mib.to_string(),
            "--cpu-topology".to_owned(),
            match config.cpu_topology {
                MicrodroidCpuTopologyV2::OneCpu => "one_cpu",
                MicrodroidCpuTopologyV2::MatchHost => "match_host",
            }
            .to_owned(),
            "--debug".to_owned(),
            match config.debug_level {
                MicrodroidDebugLevelV2::None => "none",
                MicrodroidDebugLevelV2::Full => "full",
            }
            .to_owned(),
            "--console".to_owned(),
            console.to_string_lossy().into_owned(),
            "--console-in".to_owned(),
            console_in.to_string_lossy().into_owned(),
            "--log".to_owned(),
            guest_log.to_string_lossy().into_owned(),
        ]);
        if let Some(port) = adb_port {
            arguments.extend(["--adb-tcp-port".to_owned(), port.to_string()]);
        }
        if let Some(size_mib) = config.encrypted_storage_mib {
            let storage = instance_root.join("storage.img");
            validate_existing_microdroid_storage(&storage, size_mib)?;
            arguments.extend([
                "--storage".to_owned(),
                storage.to_string_lossy().into_owned(),
                "--storage-size".to_owned(),
                (u64::from(size_mib) * 1024 * 1024).to_string(),
            ]);
        }
        let endpoints = RuntimeEndpoints::create(spec, run_id)?;
        let launch = LaunchPlanV2 {
            schema_version: 2,
            instance_id: spec.id,
            run_id,
            executable: vm.clone(),
            arguments,
            environment,
            working_directory: run_dir.to_owned(),
            control_endpoint: endpoints.control,
            frame_endpoint: endpoints.frame,
            keyboard_endpoint: endpoints.keyboard,
            trackpad_endpoint: endpoints.trackpad,
            device_endpoints: BTreeMap::new(),
            device_control_endpoints: BTreeMap::new(),
            adb_serial: adb_port.map(|port| format!("127.0.0.1:{port}")),
            guest_cid,
        };
        journal.write_manifest(&RunManifestV2 {
            schema_version: 2,
            run_id,
            instance: spec.clone(),
            artifact_bundles: Vec::new(),
            capabilities_fingerprint: expected_capabilities.to_owned(),
            launch: Some(launch.clone()),
            toolchain: toolchain_fingerprint(Some(&vm)),
        })?;
        {
            let mut mutable = self.mutable.lock().await;
            mutable.launch = Some(launch.clone());
            mutable.display_session = None;
            #[cfg(unix)]
            {
                mutable.microdroid_console_challenge = Some(console_challenge);
            }
        }
        self.transition(ObservedStateV2::StartingWorker, None)
            .await?;
        self.transition(ObservedStateV2::LaunchingGuest, None)
            .await?;
        tracing::info!(
            event = "microdroid.vm.created",
            instance_id = %spec.id,
            %run_id,
            guest_cid,
            adb_port,
            payload = ?config.payload,
            "starting isolated Microdroid workload"
        );
        let process = TokioProcessSupervisor
            .spawn(&ProcessSpec {
                executable: launch.executable.clone(),
                arguments: launch.arguments.clone(),
                environment: launch.environment.clone(),
                working_directory: launch.working_directory.clone(),
                stdout_path: run_dir.join("microdroid.stdout.log"),
                stderr_path: run_dir.join("microdroid.stderr.log"),
                latency_sensitive: false,
                kill_on_drop: true,
            })
            .await?;
        {
            let mut mutable = self.mutable.lock().await;
            mutable.status.child_pid = Some(process.id());
            mutable.status.adb_serial.clone_from(&launch.adb_serial);
            mutable.status.frame_generation = frame_generation;
            mutable.process = Some(process);
        }
        self.transition(ObservedStateV2::GuestBooting, None).await?;
        match self.wait_microdroid_payload_ready(run_dir).await? {
            MicrodroidPayloadReadiness::Ready => {}
            MicrodroidPayloadReadiness::Completed(payload_exit_code) => {
                tracing::info!(
                    event = "microdroid.payload.finished_during_start",
                    instance_id = %spec.id,
                    %run_id,
                    payload_exit_code,
                    "finite Microdroid payload completed before the first Ready sample"
                );
                // `start` already owns the instance operation lock. Reuse the same exact cleanup
                // implementation directly instead of trying to reacquire that lock through the
                // asynchronous exit monitor.
                self.stop_locked(StopModeV2::Force, Duration::ZERO, Some(payload_exit_code))
                    .await?;
                return Ok(());
            }
        }
        tracing::info!(
            event = "microdroid.payload.ready",
            instance_id = %spec.id,
            %run_id,
            guest_cid,
            "Microdroid payload reported ready"
        );
        if let Some(serial) = launch.adb_serial.clone() {
            let adb = AdbClient::new(discovery.adb, None);
            {
                let mut mutable = self.mutable.lock().await;
                mutable.adb = Some(adb.clone());
            }
            self.spawn_microdroid_deferred_adb_readiness(run_id, serial, adb);
        }
        self.transition(ObservedStateV2::Ready, None).await?;
        self.spawn_exit_monitor();
        Ok(())
    }

    async fn wait_microdroid_payload_ready(
        &self,
        run_dir: &Path,
    ) -> Result<MicrodroidPayloadReadiness, WorkerError> {
        let started = Instant::now();
        while started.elapsed() < Duration::from_mins(3) {
            for name in [
                "microdroid.stdout.log",
                "microdroid-guest.log",
                "microdroid-virtmgr-trace.log",
            ] {
                let path = run_dir.join(name);
                if !path.is_file() {
                    continue;
                }
                let text = read_log_tail_lossy(&path)?;
                if text.contains("notifyPayloadReady")
                    || text.contains("Notified host payload ready successfully")
                    || text.contains("payload is ready")
                {
                    return Ok(MicrodroidPayloadReadiness::Ready);
                }
            }
            // A finite workload can notify Ready and finish before the next 250 ms
            // polling interval. Consume the durable Ready marker first so the exit
            // monitor can classify its host launcher completion after Start returns.
            if let Some(exit) = self.poll_process().await? {
                return match classify_microdroid_exit_disposition_after_log_settle(
                    run_dir, exit.code,
                )
                .await
                {
                    MicrodroidExitDisposition::Completed(payload_exit_code) => {
                        Ok(MicrodroidPayloadReadiness::Completed(payload_exit_code))
                    }
                    MicrodroidExitDisposition::Failed(error) => Err(error),
                };
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(WorkerError::ComponentTimeout(
            "microdroid-payload".to_owned(),
        ))
    }

    async fn start_component(
        &self,
        spec: ComponentStartSpec,
        journal: &RunJournalV2,
    ) -> Result<(), WorkerError> {
        let started = Instant::now();
        let component = spec.component.clone();
        let result = self.spawn_formal_component(spec, journal).await;
        if let Err(error) = &result {
            journal.boundary_failed(
                &format!("component.{component}"),
                error.code(),
                elapsed_ms(started),
                BTreeMap::new(),
            )?;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_device_components(
        &self,
        spec: &InstanceSpecV2,
        run_id: Uuid,
        run_dir: &Path,
        guest_cid: u32,
        launch: &LaunchPlanV2,
        host_tools: &BTreeMap<String, PathBuf>,
        journal: &RunJournalV2,
    ) -> Result<(), WorkerError> {
        for component in enabled_device_components(spec) {
            let executable = host_tools.get(component).cloned().ok_or_else(|| {
                WorkerError::CapabilityBlocked(vec![format!("device.{component}")])
            })?;
            let control_endpoint = launch
                .device_control_endpoints
                .get(component)
                .cloned()
                .ok_or_else(|| {
                    WorkerError::ComponentContract(format!(
                        "device component {component} has no control endpoint"
                    ))
                })?;
            let guest_endpoints = device_component_guest_roles_v2(component)
                .iter()
                .filter(|role| device_role_enabled(spec, role))
                .map(|role| {
                    launch
                        .device_endpoints
                        .get(*role)
                        .cloned()
                        .map(|endpoint| ((*role).to_owned(), endpoint))
                        .ok_or_else(|| {
                            WorkerError::ComponentContract(format!(
                                "device component {component} is missing enabled Guest role {role}"
                            ))
                        })
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let control_token = random_device_control_token()?;
            self.start_component(
                ComponentStartSpec {
                    executable,
                    component: component.to_owned(),
                    run_id,
                    run_dir: run_dir.to_owned(),
                    configuration: FormalComponentConfigurationV2::DeviceAdapter {
                        control_endpoint,
                        control_token: control_token.clone(),
                        guest_cid,
                        vm_control_endpoint: launch.control_endpoint.clone(),
                        guest_endpoints,
                    },
                },
                journal,
            )
            .await?;
            self.mutable
                .lock()
                .await
                .device_control_tokens
                .insert(component.to_owned(), control_token);
            self.call_device_component(component, DeviceControlCommandV2::Ping)
                .await?;
        }
        Ok(())
    }

    async fn call_device_component(
        &self,
        component: &str,
        command: DeviceControlCommandV2,
    ) -> Result<(), WorkerError> {
        let (endpoint, run_id, bearer_token) = {
            let mutable = self.mutable.lock().await;
            let launch = mutable.launch.as_ref().ok_or(WorkerError::NotRunning)?;
            let endpoint = launch
                .device_control_endpoints
                .get(component)
                .cloned()
                .ok_or_else(|| WorkerError::DeviceEndpoint(component.to_owned()))?;
            let bearer_token = mutable
                .device_control_tokens
                .get(component)
                .cloned()
                .ok_or_else(|| WorkerError::DeviceEndpoint(component.to_owned()))?;
            (endpoint, launch.run_id, bearer_token)
        };
        let request = DeviceControlRequestV2 {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id: self.instance_id,
            run_id,
            bearer_token,
            command,
        };
        let response = send_device_control_request(&endpoint, &request).await?;
        if response.protocol_version != COMPONENT_PROTOCOL_VERSION
            || response.request_id != request.request_id
            || response.instance_id != self.instance_id
            || response.run_id != run_id
        {
            return Err(WorkerError::ComponentContract(format!(
                "device component {component} returned a mismatched control response"
            )));
        }
        if response.ok && response.error.is_none() {
            Ok(())
        } else {
            Err(WorkerError::DeviceRejected(response.error.map_or_else(
                || format!("device component {component} rejected the request"),
                |error| format!("{}: {}", error.code, error.message),
            )))
        }
    }

    async fn spawn_formal_component(
        &self,
        spec: ComponentStartSpec,
        journal: &RunJournalV2,
    ) -> Result<(), WorkerError> {
        let component = spec.component.as_str();
        if component.is_empty()
            || component.len() > 64
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'0'..=b'9' | b'-'))
        {
            return Err(WorkerError::ComponentContract(
                "component id is not safe ASCII".to_owned(),
            ));
        }
        let directory = spec.run_dir.join("components");
        hd_platform::ensure_owner_only_directory(&directory)?;
        let launch_path = directory.join(format!("{component}-launch-v2.json"));
        let ready_path = directory.join(format!("{component}-ready-v2.json"));
        if std::fs::symlink_metadata(&ready_path).is_ok() {
            return Err(WorkerError::ComponentContract(format!(
                "component ready marker already exists: {}",
                ready_path.display()
            )));
        }
        let launch = FormalComponentLaunchV2 {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            component: component.to_owned(),
            instance_id: self.instance_id,
            run_id: spec.run_id,
            component_ready_marker: ready_path.clone(),
            configuration: spec.configuration,
        };
        let launch_bytes = serde_json::to_vec_pretty(&launch)?;
        let launch_sha256 = hex::encode(sha2::Sha256::digest(&launch_bytes));
        hd_platform::write_owner_only(&launch_path, &launch_bytes)?;
        let started = Instant::now();
        journal.boundary_started(
            &format!("component.{component}"),
            BTreeMap::from([
                (
                    "executable".to_owned(),
                    spec.executable.display().to_string(),
                ),
                ("launch_sha256".to_owned(), launch_sha256.clone()),
            ]),
        )?;
        let process = TokioProcessSupervisor
            .spawn(&ProcessSpec {
                executable: spec.executable,
                arguments: vec![
                    "--serve-v2".to_owned(),
                    "--launch".to_owned(),
                    launch_path.to_string_lossy().into_owned(),
                ],
                environment: BTreeMap::new(),
                working_directory: spec.run_dir,
                stdout_path: directory.join(format!("{component}.stdout.log")),
                stderr_path: directory.join(format!("{component}.stderr.log")),
                latency_sensitive: false,
                kill_on_drop: true,
            })
            .await?;
        let pid = process.id();
        {
            let mut mutable = self.mutable.lock().await;
            if mutable
                .components
                .iter()
                .any(|managed| managed.id == component)
            {
                return Err(WorkerError::ComponentContract(format!(
                    "component {component} is already managed"
                )));
            }
            mutable.components.push(ManagedComponent {
                id: component.to_owned(),
                process,
            });
        }
        self.wait_formal_component(
            ComponentHandshake {
                component: component.to_owned(),
                run_id: spec.run_id,
                pid,
                ready_path,
                launch_sha256,
                started,
            },
            journal,
        )
        .await
    }

    async fn wait_formal_component(
        &self,
        handshake: ComponentHandshake,
        journal: &RunJournalV2,
    ) -> Result<(), WorkerError> {
        let component = handshake.component.as_str();
        loop {
            let exit = {
                let mut mutable = self.mutable.lock().await;
                let managed = mutable
                    .components
                    .iter_mut()
                    .find(|managed| managed.id == component)
                    .ok_or_else(|| {
                        WorkerError::ComponentContract(format!(
                            "component {component} lost process ownership"
                        ))
                    })?;
                managed.process.try_wait()?
            };
            if let Some(exit) = exit {
                return Err(WorkerError::ComponentExited {
                    component: component.to_owned(),
                    code: exit.code,
                });
            }
            if handshake.ready_path.is_file() {
                let bytes =
                    hd_platform::read_regular_nofollow_limited(&handshake.ready_path, 64 * 1024)?;
                let ready: FormalComponentReadyV2 = serde_json::from_slice(&bytes)?;
                let actual_start_marker = hd_platform::process_start_marker(handshake.pid)?;
                if ready.protocol_version != COMPONENT_PROTOCOL_VERSION
                    || ready.component != component
                    || ready.instance_id != self.instance_id
                    || ready.run_id != handshake.run_id
                    || ready.launch_sha256 != handshake.launch_sha256
                    || ready.pid != handshake.pid
                    || ready.process_start_marker != actual_start_marker
                {
                    return Err(WorkerError::ComponentContract(format!(
                        "component {component} ready marker does not match its exact launch"
                    )));
                }
                journal.boundary_succeeded(
                    &format!("component.{component}"),
                    elapsed_ms(handshake.started),
                    BTreeMap::from([("pid".to_owned(), handshake.pid.to_string())]),
                )?;
                return Ok(());
            }
            if handshake.started.elapsed() >= COMPONENT_READY_TIMEOUT {
                return Err(WorkerError::ComponentTimeout(component.to_owned()));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn terminate_components(&self) -> Result<(), WorkerError> {
        let components = {
            let mut mutable = self.mutable.lock().await;
            std::mem::take(&mut mutable.components)
        };
        let mut retained = Vec::new();
        let mut failures = Vec::new();
        for mut component in components {
            let needs_termination = match component.process.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                Err(error) => {
                    failures.push(format!("{} poll: {error}", component.id));
                    true
                }
            };
            if needs_termination
                && let Err(error) = TokioProcessSupervisor
                    .terminate(&mut component.process)
                    .await
            {
                failures.push(format!("{} terminate: {error}", component.id));
                retained.push(component);
            }
        }
        if retained.is_empty() && failures.is_empty() {
            Ok(())
        } else {
            self.mutable.lock().await.components = retained;
            Err(WorkerError::ComponentCleanup(failures.join("; ")))
        }
    }

    async fn wait_frame_ready(
        &self,
        run_dir: &Path,
        run_id: Uuid,
        generation: u64,
    ) -> Result<(), WorkerError> {
        let path = run_dir.join("frame-ready-v2.json");
        let started = Instant::now();
        loop {
            if let Some(exit) = self.poll_process().await? {
                return Err(WorkerError::GuestExited(exit.code));
            }
            self.ensure_components_alive().await?;
            if path.is_file() {
                let marker_bytes = hd_platform::read_regular_nofollow_limited(&path, 64 * 1024)?;
                let marker: FrameReadyMarkerV2 = serde_json::from_slice(&marker_bytes)?;
                let producer = self.component_identity("frame-producer").await?;
                if marker.protocol_version != FRAME_PROTOCOL_VERSION
                    || marker.instance_id != self.instance_id
                    || marker.run_id != run_id
                    || marker.producer_pid != producer.pid
                    || marker.producer_process_start_marker != producer.process_start_marker
                    || marker.generation != generation
                    || marker.transport != expected_frame_transport()
                    || !marker.strict_zero_copy()
                    || !process_identity_is_alive(&producer)
                {
                    return Err(WorkerError::FrameHandshake(
                        "frame marker did not prove strict same-adapter zero-copy".to_owned(),
                    ));
                }
                return Ok(());
            }
            if started.elapsed() >= FRAME_READY_TIMEOUT {
                return Err(WorkerError::FrameHandshake(
                    "strict frame negotiation timed out".to_owned(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn wait_native_display_ready(&self) -> Result<(), WorkerError> {
        let started = Instant::now();
        let minimum_alive = Duration::from_secs(3);
        loop {
            if let Some(exit) = self.poll_process().await? {
                return Err(WorkerError::GuestExited(exit.code));
            }
            self.ensure_components_alive().await?;
            if started.elapsed() >= minimum_alive {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn wait_dev_boot_log_ready(&self, run_dir: &Path) -> Result<(), WorkerError> {
        let started = Instant::now();
        loop {
            if let Some(exit) = self.poll_process().await? {
                return Err(WorkerError::GuestExited(exit.code));
            }
            self.ensure_components_alive().await?;
            if dev_boot_completed_from_logs(run_dir)? {
                return Ok(());
            }
            if started.elapsed() >= DEV_BOOT_LOG_READY_TIMEOUT {
                return Err(WorkerError::FrameHandshake(
                    "dev boot-log readiness fallback timed out".to_owned(),
                ));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn wait_adbd_log_ready(&self, run_dir: &Path) -> Result<(), WorkerError> {
        let started = Instant::now();
        loop {
            if let Some(exit) = self.poll_process().await? {
                return Err(WorkerError::GuestExited(exit.code));
            }
            self.ensure_components_alive().await?;
            if adbd_started_from_logs(run_dir)? {
                return Ok(());
            }
            if started.elapsed() >= ADBD_LOG_READY_TIMEOUT {
                return Err(WorkerError::FrameHandshake(
                    "adbd log readiness timed out before strict ADB connect".to_owned(),
                ));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn attach_display(
        &self,
        session_id: Uuid,
        generation: u64,
        display_id: DisplayIdV2,
        target: NativeDisplayTargetV2,
        viewport: DisplayViewportV2,
    ) -> Result<(), WorkerError> {
        // Native child reparenting is process-global for this VM. Keep explicit Player
        // replacement ordered with startup's deferred attach so an old target cannot reappear
        // after a newer session has already won.
        let _display_operation = self.display_operation.lock().await;
        if !viewport.is_valid() {
            return Err(WorkerError::DisplaySession(
                "viewport is outside supported bounds".to_owned(),
            ));
        }
        if !hd_platform::process_identity_is_alive(target.owner()) {
            return Err(WorkerError::DisplaySession(
                "Player process identity is not alive".to_owned(),
            ));
        }
        let status = self.status().await;
        if status.frame_generation != generation {
            return Err(WorkerError::DisplaySession(
                "display generation is stale".to_owned(),
            ));
        }
        let runtime_display = status
            .runtime_displays
            .iter()
            .find(|display| display.display_id == display_id)
            .ok_or_else(|| {
                WorkerError::DisplaySession(
                    "requested display is not configured for the active Android run".to_owned(),
                )
            })?;
        if target.scanout_id() != runtime_display.scanout_id {
            return Err(WorkerError::DisplaySession(
                "native display target scanout does not match the requested product display"
                    .to_owned(),
            ));
        }
        let child_pid = status.child_pid.ok_or(WorkerError::NotRunning)?;
        let debug_toplevel = hd_platform::native_display_toplevel_debug_enabled();
        tracing::info!(
            event = "worker.display.attach.started",
            instance_id = %self.instance_id,
            session_id = %session_id,
            generation,
            child_pid,
            debug_toplevel,
            viewport_width = viewport.width_px,
            viewport_height = viewport.height_px,
            "attaching crosvm native display"
        );
        hd_platform::attach_native_display(
            child_pid,
            &target,
            &viewport,
            runtime_display.width,
            runtime_display.height,
        )?;
        self.mutable.lock().await.display_session = Some(ActiveDisplaySession {
            id: session_id,
            generation,
            display_id,
            target,
            viewport,
        });
        tracing::info!(
            event = "worker.display.attach.succeeded",
            instance_id = %self.instance_id,
            session_id = %session_id,
            generation,
            child_pid,
            debug_toplevel,
            "crosvm native display attached"
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn spawn_deferred_initial_display(
        self: &Arc<Self>,
        prepared: PreparedNativeDisplayV2,
        run_id: Uuid,
        generation: u64,
    ) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            let started = Instant::now();
            let mut retry_delay = INITIAL_DISPLAY_ATTACH_RETRY_MIN;
            let mut next_progress_log = Duration::from_secs(15);
            loop {
                let display_operation = worker.display_operation.lock().await;
                let status = worker.status().await;
                let session_is_current = worker
                    .mutable
                    .lock()
                    .await
                    .display_session
                    .as_ref()
                    .is_some_and(|session| {
                        session.id == prepared.session_id && session.generation == generation
                    });
                if status.run_id != Some(run_id)
                    || status.frame_generation != generation
                    || status.child_pid.is_none()
                    || !status.observed.is_active()
                    || !session_is_current
                    || !process_identity_is_alive(prepared.target.owner())
                {
                    tracing::debug!(
                        event = "worker.display.initial_attach.cancelled",
                        instance_id = %worker.instance_id,
                        session_id = %prepared.session_id,
                        run_id = %run_id,
                        generation,
                        "deferred native display attach no longer belongs to the active runtime"
                    );
                    return;
                }
                let child_pid = status.child_pid.expect("checked above");
                let Some(runtime_display) = status
                    .runtime_displays
                    .iter()
                    .find(|display| display.display_id == prepared.display_id)
                else {
                    tracing::warn!(
                        event = "worker.display.initial_attach.cancelled",
                        instance_id = %worker.instance_id,
                        session_id = %prepared.session_id,
                        "configured display disappeared before native attach"
                    );
                    return;
                };
                match hd_platform::attach_native_display(
                    child_pid,
                    &prepared.target,
                    &prepared.viewport,
                    runtime_display.width,
                    runtime_display.height,
                ) {
                    Ok(()) => {
                        tracing::info!(
                            event = "worker.display.initial_attach.succeeded",
                            instance_id = %worker.instance_id,
                            session_id = %prepared.session_id,
                            child_pid,
                            elapsed_ms = elapsed_ms(started),
                            viewport_width = prepared.viewport.width_px,
                            viewport_height = prepared.viewport.height_px,
                            "gfxstream display was attached without blocking VM startup"
                        );
                        return;
                    }
                    Err(error) if started.elapsed() < INITIAL_DISPLAY_ATTACH_RETRY_WINDOW => {
                        if started.elapsed() >= next_progress_log {
                            tracing::info!(
                                event = "worker.display.initial_attach.pending",
                                instance_id = %worker.instance_id,
                                session_id = %prepared.session_id,
                                child_pid,
                                elapsed_ms = elapsed_ms(started),
                                %error,
                                "Android is still booting; waiting for the gfxstream render HWND"
                            );
                            next_progress_log =
                                next_progress_log.saturating_add(Duration::from_secs(15));
                        }
                        // Do not make a boot-time retry delay block a newer Player session. The
                        // next iteration re-enters the lock and revalidates session ownership.
                        drop(display_operation);
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = retry_delay
                            .saturating_mul(2)
                            .min(INITIAL_DISPLAY_ATTACH_RETRY_MAX);
                    }
                    Err(error) => {
                        // Display availability is recoverable independently from the VM. The UI
                        // can acquire a fresh session later, so never turn a healthy Android
                        // runtime into a terminal start failure here.
                        tracing::warn!(
                            event = "worker.display.initial_attach.deferred_timeout",
                            instance_id = %worker.instance_id,
                            session_id = %prepared.session_id,
                            child_pid,
                            elapsed_ms = elapsed_ms(started),
                            %error,
                            "native display is still unavailable; VM remains running for reattach"
                        );
                        return;
                    }
                }
            }
        });
    }

    async fn resize_display(
        &self,
        session_id: Uuid,
        generation: u64,
        viewport: DisplayViewportV2,
    ) -> Result<(), WorkerError> {
        let _display_operation = self.display_operation.lock().await;
        if !viewport.is_valid() {
            return Err(WorkerError::DisplaySession(
                "viewport is outside supported bounds".to_owned(),
            ));
        }
        let child_pid = self
            .status()
            .await
            .child_pid
            .ok_or(WorkerError::NotRunning)?;
        let mut mutable = self.mutable.lock().await;
        let session = mutable.display_session.as_mut().ok_or_else(|| {
            WorkerError::DisplaySession("no display session is attached".to_owned())
        })?;
        if session.id != session_id || session.generation != generation {
            return Err(WorkerError::DisplaySession(
                "display session identity or generation mismatch".to_owned(),
            ));
        }
        if viewport.revision < session.viewport.revision {
            return Ok(());
        }
        if viewport.revision == session.viewport.revision {
            hd_platform::ensure_native_display(child_pid, &session.target, &viewport)?;
            return Ok(());
        }
        let geometry_changed = viewport.width_px != session.viewport.width_px
            || viewport.height_px != session.viewport.height_px
            || viewport.dpi != session.viewport.dpi;
        if geometry_changed {
            hd_platform::resize_native_display(child_pid, &session.target, &viewport)?;
        } else if viewport.visible != session.viewport.visible {
            hd_platform::set_native_display_visibility(
                child_pid,
                &session.target,
                viewport.visible,
            )?;
        }
        tracing::debug!(
            event = "worker.display.resize.succeeded",
            instance_id = %self.instance_id,
            display_id = ?session.display_id,
            scanout_id = session.target.scanout_id(),
            viewport_revision = viewport.revision,
            "native display viewport updated"
        );
        session.viewport = viewport;
        Ok(())
    }

    async fn detach_display(&self, session_id: Uuid, generation: u64) -> Result<(), WorkerError> {
        let _display_operation = self.display_operation.lock().await;
        let child_pid = self
            .status()
            .await
            .child_pid
            .ok_or(WorkerError::NotRunning)?;
        let mut mutable = self.mutable.lock().await;
        if let Some(session) = &mutable.display_session
            && (session.id != session_id || session.generation != generation)
        {
            return Err(WorkerError::DisplaySession(
                "display session identity or generation mismatch".to_owned(),
            ));
        }
        if let Some(session) = &mutable.display_session {
            hd_platform::detach_native_display(child_pid, &session.target)?;
        }
        mutable.display_session = None;
        Ok(())
    }

    async fn capture_screenshot(
        &self,
        output_path: &Path,
    ) -> Result<ScreenshotRecordV2, WorkerError> {
        let screenshot_dir = self.paths.screenshot_directory();
        if output_path.parent() != Some(screenshot_dir.as_path()) {
            return Err(WorkerError::DisplaySession(
                "screenshot path is outside the managed screenshot directory".to_owned(),
            ));
        }
        hd_platform::ensure_owner_only_directory(&screenshot_dir)?;
        let (adb, serial) = {
            let mutable = self.mutable.lock().await;
            #[cfg(not(target_os = "macos"))]
            if !mutable.adb_ready {
                return Err(WorkerError::AdbNotReady);
            }
            (
                mutable.adb.clone().ok_or(WorkerError::NotReady)?,
                mutable
                    .status
                    .adb_serial
                    .clone()
                    .ok_or(WorkerError::ReadinessUnavailable)?,
            )
        };
        let png = adb.screenshot(&serial).await?;
        let sha256 = hex::encode(sha2::Sha256::digest(&png));
        hd_platform::write_owner_only(output_path, &png)?;
        Ok(ScreenshotRecordV2 {
            instance_id: self.instance_id,
            path: output_path.to_owned(),
            sha256,
            size_bytes: png.len() as u64,
            created_at: OffsetDateTime::now_utc(),
        })
    }

    async fn collect_android_bugreport(
        &self,
        bugreport_id: Uuid,
        output_path: &Path,
    ) -> Result<AndroidBugreportRecordV2, WorkerError> {
        let _operation = self.operation.lock().await;
        if self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::NotReady);
        }
        let directory = &self.paths.diagnostics;
        let expected_name = format!(
            "android-bugreport-{}-{}.zip",
            self.instance_id,
            bugreport_id.simple()
        );
        if output_path.parent() != Some(directory.as_path())
            || output_path.file_name().and_then(|name| name.to_str()) != Some(&expected_name)
        {
            return Err(WorkerError::Unsupported(
                "Android bugreport path is outside the managed diagnostics directory",
            ));
        }
        if output_path.exists() {
            return Err(WorkerError::Busy(
                "Android bugreport artifact already exists",
            ));
        }
        hd_platform::ensure_owner_only_directory(directory)?;
        let (adb, serial, run_id) = {
            let mutable = self.mutable.lock().await;
            if !mutable.adb_ready {
                return Err(WorkerError::AdbNotReady);
            }
            if mutable
                .active_spec
                .as_ref()
                .is_none_or(|spec| spec.guest_kind != GuestKindV2::Android)
            {
                return Err(WorkerError::Unsupported(
                    "bugreport is only available for Android instances",
                ));
            }
            (
                mutable.adb.clone().ok_or(WorkerError::NotRunning)?,
                mutable
                    .status
                    .adb_serial
                    .clone()
                    .ok_or(WorkerError::ReadinessUnavailable)?,
                mutable.status.run_id.ok_or(WorkerError::NotRunning)?,
            )
        };
        let result = adb.collect_android_bugreport(&serial, output_path).await;
        let size_bytes = match result {
            Ok(size_bytes) => size_bytes,
            Err(error) => {
                let _ = std::fs::remove_file(output_path);
                return Err(error.into());
            }
        };
        let sha256 = crate::sha256_file(output_path)
            .map_err(|error| WorkerError::Task(format!("hash Android bugreport: {error}")))?;
        Ok(AndroidBugreportRecordV2 {
            id: bugreport_id,
            instance_id: self.instance_id,
            run_id,
            path: output_path.to_owned(),
            sha256,
            size_bytes,
            created_at: OffsetDateTime::now_utc(),
            contains_sensitive_data: true,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn start_screen_recording(
        &self,
        recording_id: Uuid,
        display_id: DisplayIdV2,
        output_path: &Path,
        max_duration_seconds: u16,
    ) -> Result<ScreenRecordingStatusV2, WorkerError> {
        let _operation = self.operation.lock().await;
        if self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::NotReady);
        }
        let directory = self.paths.screen_recording_directory();
        if output_path.parent() != Some(directory.as_path()) {
            return Err(WorkerError::DisplaySession(
                "screen recording path is outside the managed Videos/HD directory".to_owned(),
            ));
        }
        hd_platform::ensure_owner_only_directory(&directory)?;
        let scanout_id = {
            let mutable = self.mutable.lock().await;
            if mutable.active_screen_recording.is_some() {
                return Err(WorkerError::Busy(
                    "one screen recording is already active for this instance",
                ));
            }
            if !mutable.adb_ready {
                return Err(WorkerError::AdbNotReady);
            }
            if mutable
                .active_spec
                .as_ref()
                .is_some_and(|spec| spec.guest_kind != GuestKindV2::Android)
            {
                return Err(WorkerError::Unsupported(
                    "screen recording is only available for Android instances",
                ));
            }
            mutable
                .status
                .runtime_displays
                .iter()
                .find(|display| display.display_id == display_id)
                .map(|display| display.scanout_id)
                .ok_or(WorkerError::Busy(
                    "requested screen-recording display is not active in this Android run",
                ))?
        };
        #[cfg(any(windows, target_os = "macos"))]
        let backend = {
            let endpoint = self
                .mutable
                .lock()
                .await
                .host_recorder_endpoint
                .clone()
                .ok_or_else(|| {
                    WorkerError::HostRecorder(
                        "gfxstream host-recorder endpoint is unavailable".to_owned(),
                    )
                })?;
            ActiveScreenRecordingBackend::Host(
                HostRecording::start(endpoint, scanout_id, output_path, max_duration_seconds)
                    .await
                    .map_err(WorkerError::HostRecorder)?,
            )
        };
        #[cfg(not(any(windows, target_os = "macos")))]
        let backend = {
            let (adb, serial) = {
                let mutable = self.mutable.lock().await;
                if !mutable.adb_ready {
                    return Err(WorkerError::AdbNotReady);
                }
                (
                    mutable.adb.clone().ok_or(WorkerError::NotRunning)?,
                    mutable
                        .status
                        .adb_serial
                        .clone()
                        .ok_or(WorkerError::ReadinessUnavailable)?,
                )
            };
            let process = adb
                .start_screen_recording(&serial, recording_id, scanout_id, max_duration_seconds)
                .await?;
            ActiveScreenRecordingBackend::Guest {
                adb,
                serial,
                process,
            }
        };
        let status = ScreenRecordingStatusV2 {
            id: recording_id,
            instance_id: self.instance_id,
            display_id,
            max_duration_seconds,
            started_at: OffsetDateTime::now_utc(),
        };
        let mut mutable = self.mutable.lock().await;
        mutable.status.screen_recording = Some(status.clone());
        mutable.active_screen_recording = Some(ActiveScreenRecording {
            status: status.clone(),
            output_path: output_path.to_owned(),
            backend,
            started: Instant::now(),
        });
        tracing::info!(
            event = "worker.screen_recording.started",
            instance_id = %self.instance_id,
            %recording_id,
            max_duration_seconds,
            "Android screen recording started"
        );
        Ok(status)
    }

    async fn stop_screen_recording(
        &self,
        recording_id: Uuid,
    ) -> Result<ScreenRecordingRecordV2, WorkerError> {
        let _operation = self.operation.lock().await;
        self.finish_active_screen_recording(Some(recording_id))
            .await
    }

    async fn finish_active_screen_recording(
        &self,
        expected_id: Option<Uuid>,
    ) -> Result<ScreenRecordingRecordV2, WorkerError> {
        let mut active = {
            let mut mutable = self.mutable.lock().await;
            let active = mutable
                .active_screen_recording
                .take()
                .ok_or(WorkerError::Busy("no screen recording is active"))?;
            if expected_id.is_some_and(|id| id != active.status.id) {
                mutable.active_screen_recording = Some(active);
                return Err(WorkerError::Busy(
                    "screen recording identity does not match the active recording",
                ));
            }
            mutable.status.screen_recording = None;
            active
        };
        match &mut active.backend {
            #[cfg(not(any(windows, target_os = "macos")))]
            ActiveScreenRecordingBackend::Guest {
                adb,
                serial,
                process,
            } => {
                if let Err(error) = adb
                    .finish_screen_recording(serial, process, &active.output_path)
                    .await
                {
                    let _ = std::fs::remove_file(&active.output_path);
                    return Err(WorkerError::Adb(error));
                }
            }
            #[cfg(any(windows, target_os = "macos"))]
            ActiveScreenRecordingBackend::Host(recording) => {
                let stats = match recording.clone().finish().await {
                    Ok(stats) => stats,
                    Err(error) => {
                        let _ = std::fs::remove_file(&active.output_path);
                        return Err(WorkerError::HostRecorder(error));
                    }
                };
                tracing::info!(
                    event = "worker.screen_recording.host_stats",
                    instance_id = %self.instance_id,
                    recording_id = %active.status.id,
                    encoded_frames = stats.encoded_frames,
                    dropped_frames = stats.dropped_frames,
                    initial_static_frame = stats.initial_static_frame,
                    initial_frame_y_direction = stats.initial_frame_y_direction,
                    near_black_frames = stats.near_black_frames,
                    max_consecutive_near_black_frames =
                        stats.max_consecutive_near_black_frames,
                    max_source_frame_gap_millis = stats.max_source_frame_gap_millis,
                    source_frame_gaps_over_100_millis =
                        stats.source_frame_gaps_over_100_millis,
                    "gfxstream host recording finalized"
                );
            }
        }
        let (size_bytes, sha256) = match validate_screen_recording_mp4(&active.output_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = std::fs::remove_file(&active.output_path);
                return Err(error);
            }
        };
        let record = ScreenRecordingRecordV2 {
            id: active.status.id,
            instance_id: self.instance_id,
            display_id: active.status.display_id,
            path: active.output_path,
            sha256,
            size_bytes,
            duration_millis: u64::try_from(active.started.elapsed().as_millis())
                .unwrap_or(u64::MAX),
            started_at: active.status.started_at,
            finished_at: OffsetDateTime::now_utc(),
        };
        self.mutable.lock().await.status.last_screen_recording = Some(record.clone());
        tracing::info!(
            event = "worker.screen_recording.finished",
            instance_id = %self.instance_id,
            recording_id = %record.id,
            size_bytes = record.size_bytes,
            duration_millis = record.duration_millis,
            "Android screen recording finished"
        );
        Ok(record)
    }

    #[allow(clippy::too_many_lines)]
    async fn stop(&self, mode: StopModeV2, graceful_timeout: Duration) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        self.stop_locked(mode, graceful_timeout, None).await
    }

    async fn stop_after_microdroid_completion(
        &self,
        payload_exit_code: i32,
    ) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        if !self.status().await.observed.is_active() {
            return Ok(());
        }
        self.stop_locked(StopModeV2::Force, Duration::ZERO, Some(payload_exit_code))
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn stop_locked(
        &self,
        mode: StopModeV2,
        graceful_timeout: Duration,
        completed_exit_code: Option<i32>,
    ) -> Result<(), WorkerError> {
        // Stop owns the display pipeline until the child is gone; otherwise a late heartbeat or
        // deferred startup attach could reparent the render HWND during shutdown.
        let _display_operation = self.display_operation.lock().await;
        let status = self.status().await;
        if matches!(status.observed, ObservedStateV2::Stopped) {
            return Ok(());
        }
        if matches!(status.observed, ObservedStateV2::Deleted) {
            return Err(WorkerError::Busy("deleted worker cannot be stopped"));
        }
        let graceful_shutdown_available = {
            let mutable = self.mutable.lock().await;
            let guest_kind = mutable
                .active_spec
                .as_ref()
                .map_or(GuestKindV2::Android, |spec| spec.guest_kind);
            microdroid_graceful_shutdown_available(
                guest_kind,
                mutable.adb_ready,
                mutable.adb.is_some(),
                mutable
                    .launch
                    .as_ref()
                    .is_some_and(|launch| launch.adb_serial.is_some()),
            )
        };
        if matches!(mode, StopModeV2::Graceful)
            && matches!(
                status.observed,
                ObservedStateV2::Ready | ObservedStateV2::Paused
            )
            && !graceful_shutdown_available
        {
            return Err(WorkerError::Unsupported(
                "Microdroid graceful shutdown requires a ready ADB service; use explicit force stop",
            ));
        }
        let was_failed = matches!(
            status.observed,
            ObservedStateV2::Blocked | ObservedStateV2::Failed
        );
        if status.observed.can_transition_to(ObservedStateV2::Stopping) {
            self.transition(ObservedStateV2::Stopping, None).await?;
        } else if was_failed {
            // A failed start can still own a child handle or runtime endpoints.  Keep the
            // terminal state until cleanup is proven, then transition to Stopped below.
        } else {
            return Err(WorkerError::StateTransition {
                previous: status.observed,
                next: ObservedStateV2::Stopping,
            });
        }
        let display_target = self
            .mutable
            .lock()
            .await
            .display_session
            .as_ref()
            .map(|session| session.target.clone());
        if let Some(child_pid) = status.child_pid
            && let Some(target) = display_target.as_ref()
        {
            let _ = hd_platform::detach_native_display(child_pid, target);
        }
        if self.mutable.lock().await.active_screen_recording.is_some()
            && let Err(error) = self.finish_active_screen_recording(None).await
        {
            tracing::warn!(
                event = "worker.screen_recording.stop_cleanup.failed",
                instance_id = %self.instance_id,
                %error,
                "instance stop continued after screen recording cleanup failed"
            );
        }
        if self.mutable.lock().await.active_location_route.is_some()
            && let Err(error) = self.stop_location_route(false).await
        {
            tracing::warn!(
                event = "worker.location_route.stop_cleanup.failed",
                instance_id = %self.instance_id,
                %error,
                "instance stop continued after location route cleanup failed"
            );
        }
        let (process, backend, launch, adb, guest_kind) = {
            let mut mutable = self.mutable.lock().await;
            (
                mutable.process.take(),
                mutable.backend.clone(),
                mutable.launch.clone(),
                mutable.adb.clone(),
                mutable.active_spec.as_ref().map(|spec| spec.guest_kind),
            )
        };
        let mut retained_process = None;
        let mut cleanup_error = None;
        if let Some(mut process) = process {
            let mut exited = false;
            if matches!(mode, StopModeV2::Graceful)
                && let Some(launch) = &launch
            {
                let adb_poweroff_requested = if let (Some(adb), Some(serial)) =
                    (&adb, launch.adb_serial.as_deref())
                {
                    let power_off = if matches!(guest_kind, Some(GuestKindV2::Microdroid)) {
                        adb.power_off_debuggable(serial).await
                    } else {
                        adb.power_off(serial).await
                    };
                    match power_off {
                        Ok(()) => true,
                        Err(error) => {
                            tracing::warn!(
                                event = "worker.stop.adb_power_off.failed",
                                instance_id = %self.instance_id,
                                %error,
                                "Guest ADB power-off request failed; falling back to crosvm power button"
                            );
                            false
                        }
                    }
                } else {
                    false
                };
                let power_requested = if adb_poweroff_requested {
                    Ok(())
                } else if let Some(backend) = &backend {
                    backend.power_button(&launch.control_endpoint).await
                } else {
                    Err(hd_platform::PlatformError::Process(
                        "guest has no graceful power control backend".to_owned(),
                    ))
                };
                match power_requested {
                    Ok(()) => {
                        let started = Instant::now();
                        while started.elapsed() < graceful_timeout {
                            match process.try_wait() {
                                Ok(Some(_)) => {
                                    exited = true;
                                    break;
                                }
                                Ok(None) => {
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        event = "worker.stop.poll.failed",
                                        instance_id = %self.instance_id,
                                        %error,
                                        "graceful stop polling failed; forcing exact termination"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            event = "worker.stop.power_button.failed",
                            instance_id = %self.instance_id,
                            %error,
                            "guest power request failed; forcing exact termination"
                        );
                    }
                }
            }
            if !exited {
                let needs_termination = match process.try_wait() {
                    Ok(Some(_)) => false,
                    Ok(None) => true,
                    Err(error) => {
                        tracing::warn!(
                            event = "worker.stop.final_poll.failed",
                            instance_id = %self.instance_id,
                            %error,
                            "final process poll failed; forcing exact termination"
                        );
                        true
                    }
                };
                if needs_termination
                    && let Err(error) = TokioProcessSupervisor.terminate(&mut process).await
                {
                    cleanup_error = Some(WorkerError::Platform(error));
                    retained_process = Some(process);
                }
            }
        }
        if let Err(error) = self.terminate_components().await {
            cleanup_error.get_or_insert(error);
        }
        let component_cleanup_pending = !self.mutable.lock().await.components.is_empty();
        if retained_process.is_none()
            && !component_cleanup_pending
            && let Some(launch) = &launch
            && let Err(error) = cleanup_runtime_endpoints(launch)
        {
            cleanup_error = Some(error);
        }
        if let (Some(adb), Some(serial)) = (
            &adb,
            launch
                .as_ref()
                .and_then(|launch| launch.adb_serial.as_deref()),
        ) && let Err(error) = adb.disconnect(serial).await
        {
            tracing::warn!(
                event = "worker.stop.adb_disconnect.failed",
                instance_id = %self.instance_id,
                %serial,
                %error,
                "instance cleanup succeeded but the ADB server retained its transport"
            );
        }

        if let Some(error) = cleanup_error {
            {
                let mut mutable = self.mutable.lock().await;
                mutable.status.cleanup_pending = true;
                mutable.process = retained_process;
                if mutable.process.is_none() {
                    mutable.status.child_pid = None;
                }
            }
            self.transition(ObservedStateV2::Failed, Some(&error))
                .await?;
            self.finish_run(ObservedStateV2::Failed, None, Some(&error))
                .await?;
            return Err(error);
        }

        if !was_failed
            && completed_exit_code.is_some()
            && let Err(error) = self
                .finish_run(ObservedStateV2::Stopped, completed_exit_code, None)
                .await
        {
            self.transition(ObservedStateV2::Failed, Some(&error))
                .await?;
            return Err(error);
        }
        self.transition(ObservedStateV2::Stopped, None).await?;
        if !was_failed {
            if completed_exit_code.is_none() {
                self.finish_run(ObservedStateV2::Stopped, None, None)
                    .await?;
            }
            self.remove_finished_run_ephemeral_artifacts().await?;
        }
        let mut mutable = self.mutable.lock().await;
        mutable.status.run_id = None;
        mutable.status.child_pid = None;
        mutable.status.cleanup_pending = false;
        mutable.status.adb_serial = None;
        mutable.status.last_error = None;
        mutable.status.runtime_displays.clear();
        mutable.active_spec = None;
        mutable.backend = None;
        mutable.launch = None;
        mutable.adb = None;
        mutable.adb_ready = false;
        mutable.display_session = None;
        #[cfg(any(windows, target_os = "macos"))]
        {
            mutable.host_recorder_endpoint = None;
        }
        mutable.active_screen_recording = None;
        mutable.active_location_route = None;
        mutable.status.uwb_ranging = None;
        mutable.status.modem_state = None;
        mutable.status.sensor_pose = None;
        mutable.status.bluetooth_peers.clear();
        mutable.components.clear();
        mutable.device_control_tokens.clear();
        mutable.journal = None;
        #[cfg(unix)]
        mutable.device_output_files.clear();
        #[cfg(unix)]
        mutable.device_input_fifos.clear();
        #[cfg(unix)]
        {
            mutable.microdroid_console_challenge = None;
        }
        Ok(())
    }

    async fn pause(&self) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        if self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::Busy("pause requires Ready"));
        }
        if self
            .mutable
            .lock()
            .await
            .active_spec
            .as_ref()
            .is_some_and(|spec| spec.guest_kind == GuestKindV2::Microdroid)
        {
            return Err(WorkerError::Unsupported(
                "pause is not available for Microdroid instances",
            ));
        }
        let route_was_playing = self.pause_location_route_for_instance().await?;
        self.transition(ObservedStateV2::Pausing, None).await?;
        let (backend, endpoint) = self.backend_control().await?;
        if let Err(error) = backend
            .pause(&endpoint)
            .await
            .map_err(WorkerError::Platform)
        {
            if route_was_playing {
                let _ = self.resume_location_route_for_instance().await;
            }
            self.transition(ObservedStateV2::Ready, Some(&error))
                .await?;
            return Err(error);
        }
        self.transition(ObservedStateV2::Paused, None).await
    }

    async fn resume(&self) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        if self.status().await.observed != ObservedStateV2::Paused {
            return Err(WorkerError::Busy("resume requires Paused"));
        }
        if self
            .mutable
            .lock()
            .await
            .active_spec
            .as_ref()
            .is_some_and(|spec| spec.guest_kind == GuestKindV2::Microdroid)
        {
            return Err(WorkerError::Unsupported(
                "resume is not available for Microdroid instances",
            ));
        }
        self.transition(ObservedStateV2::Resuming, None).await?;
        let (backend, endpoint) = self.backend_control().await?;
        if let Err(error) = backend
            .resume(&endpoint)
            .await
            .map_err(WorkerError::Platform)
        {
            self.transition(ObservedStateV2::Paused, Some(&error))
                .await?;
            return Err(error);
        }
        self.transition(ObservedStateV2::Ready, None).await?;
        self.resume_location_route_for_instance().await
    }

    #[allow(clippy::too_many_lines)]
    async fn reconfigure(
        &self,
        display: hd_core::DisplayConfigV2,
        adb_config: hd_core::AdbConfigV2,
    ) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        let (
            mut spec,
            adb,
            serial,
            backend,
            control_endpoint,
            runtime_displays,
            selected_display,
            recording_active,
        ) = {
            let mutable = self.mutable.lock().await;
            (
                mutable.active_spec.clone().ok_or(WorkerError::NotRunning)?,
                mutable.adb_ready.then(|| mutable.adb.clone()).flatten(),
                mutable.status.adb_serial.clone(),
                mutable.backend.clone().ok_or(WorkerError::NotRunning)?,
                mutable
                    .launch
                    .as_ref()
                    .map(|launch| launch.control_endpoint.clone())
                    .ok_or(WorkerError::NotRunning)?,
                mutable.status.runtime_displays.clone(),
                mutable
                    .display_session
                    .as_ref()
                    .map(|session| session.display_id),
                mutable.active_screen_recording.is_some(),
            )
        };
        if display == spec.display && adb_config == spec.adb {
            return Ok(());
        }
        let mut live_display = spec.display.clone();
        live_display.orientation = display.orientation;
        live_display.show_host_fps = display.show_host_fps;
        live_display
            .secondary_displays
            .clone_from(&display.secondary_displays);
        if display != live_display || adb_config != spec.adb {
            return Err(WorkerError::RestartRequired);
        }
        let mut next_spec = spec.clone();
        next_spec.display.clone_from(&display);
        next_spec.adb.clone_from(&adb_config);
        next_spec.validate()?;

        let previous = spec.display.clone();
        let secondary_changed = display.secondary_displays != previous.secondary_displays;
        if secondary_changed && self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::Busy(
                "runtime display hotplug requires a Ready Android instance",
            ));
        }
        let current_by_id = previous
            .secondary_displays
            .iter()
            .map(|secondary| (secondary.id, secondary.clone()))
            .collect::<BTreeMap<_, _>>();
        let next_by_id = display
            .secondary_displays
            .iter()
            .map(|secondary| (secondary.id, secondary.clone()))
            .collect::<BTreeMap<_, _>>();
        let scanout_by_id = runtime_displays
            .iter()
            .filter_map(|runtime| match runtime.display_id {
                DisplayIdV2::Primary => None,
                DisplayIdV2::Secondary { id } => Some((id, runtime.scanout_id)),
            })
            .collect::<BTreeMap<_, _>>();
        if current_by_id
            .keys()
            .any(|display_id| !scanout_by_id.contains_key(display_id))
        {
            return Err(WorkerError::DisplaySession(
                "active runtime display mapping does not match the saved instance specification"
                    .to_owned(),
            ));
        }

        let removed = current_by_id
            .iter()
            .filter(|(id, _)| !next_by_id.contains_key(id))
            .map(|(id, secondary)| {
                Ok((
                    *scanout_by_id.get(id).ok_or_else(|| {
                        WorkerError::DisplaySession(
                            "removed display has no active scanout mapping".to_owned(),
                        )
                    })?,
                    secondary.clone(),
                ))
            })
            .collect::<Result<Vec<_>, WorkerError>>()?;
        let changed = current_by_id
            .iter()
            .filter_map(|(id, current)| {
                next_by_id.get(id).and_then(|next| {
                    (current != next).then(|| {
                        scanout_by_id
                            .get(id)
                            .copied()
                            .map(|scanout_id| (scanout_id, current.clone(), next.clone()))
                    })
                })
            })
            .flatten()
            .collect::<Vec<_>>();
        let added = display
            .secondary_displays
            .iter()
            .filter(|secondary| !current_by_id.contains_key(&secondary.id))
            .cloned()
            .collect::<Vec<_>>();
        let guest_display_changed = !removed.is_empty()
            || !added.is_empty()
            || changed
                .iter()
                .any(|(_, current, next)| secondary_display_geometry_changed(current, next));
        if guest_display_changed && recording_active {
            return Err(WorkerError::Busy(
                "stop screen recording before changing runtime displays",
            ));
        }
        if guest_display_changed && (adb.is_none() || serial.is_none()) {
            return Err(WorkerError::AdbNotReady);
        }

        let selected_is_removed = selected_display.is_some_and(|selected| {
            removed
                .iter()
                .any(|(_, secondary)| selected == DisplayIdV2::Secondary { id: secondary.id })
        });
        let selected_geometry_changes = selected_display.is_some_and(|selected| {
            changed.iter().any(|(_, current, next)| {
                selected == DisplayIdV2::Secondary { id: current.id }
                    && secondary_display_geometry_changed(current, next)
            })
        });
        if selected_is_removed || selected_geometry_changes {
            return Err(WorkerError::Busy(
                "switch Player to another display before removing or resizing the selected display",
            ));
        }

        let mut removed_applied = Vec::new();
        let mut changed_applied = Vec::new();
        let mut added_applied = Vec::new();
        let mut used_scanouts = scanout_by_id.values().copied().collect::<BTreeSet<_>>();
        let transaction = async {
            for (scanout_id, secondary) in &removed {
                backend
                    .remove_display(&control_endpoint, *scanout_id)
                    .await?;
                used_scanouts.remove(scanout_id);
                removed_applied.push((*scanout_id, secondary.clone()));
            }
            for (scanout_id, current, next) in &changed {
                if secondary_display_geometry_changed(current, next) {
                    backend
                        .replace_secondary_display(&control_endpoint, *scanout_id, next)
                        .await?;
                    changed_applied.push((*scanout_id, current.clone()));
                }
            }
            for secondary in &added {
                let scanout_id = (1..=u32::try_from(MAX_SECONDARY_DISPLAYS)
                    .expect("bounded display capacity"))
                    .find(|scanout_id| !used_scanouts.contains(scanout_id))
                    .ok_or_else(|| {
                        WorkerError::DisplaySession(
                            "no free secondary display scanout is available".to_owned(),
                        )
                    })?;
                backend
                    .add_secondary_display(&control_endpoint, scanout_id, secondary)
                    .await?;
                used_scanouts.insert(scanout_id);
                added_applied.push((scanout_id, secondary.clone()));
            }

            let added_scanouts = added_applied
                .iter()
                .map(|(scanout_id, secondary)| (secondary.id, *scanout_id))
                .collect::<BTreeMap<_, _>>();
            let mut next_runtime = runtime_displays
                .iter()
                .filter(|runtime| runtime.display_id == DisplayIdV2::Primary)
                .cloned()
                .collect::<Vec<_>>();
            for secondary in &display.secondary_displays {
                let scanout_id = scanout_by_id
                    .get(&secondary.id)
                    .or_else(|| added_scanouts.get(&secondary.id))
                    .copied()
                    .ok_or_else(|| {
                        WorkerError::DisplaySession(
                            "hotplugged display has no stable scanout mapping".to_owned(),
                        )
                    })?;
                next_runtime.push(runtime_display_for_secondary(secondary, scanout_id));
            }

            if let (Some(adb), Some(serial)) = (&adb, serial.as_deref()) {
                if guest_display_changed {
                    tokio::time::sleep(ANDROID_DISPLAY_HOTPLUG_SETTLE_DELAY).await;
                    for runtime in next_runtime.iter().skip(1) {
                        adb.set_display_density(serial, runtime.scanout_id * 2, runtime.dpi)
                            .await?;
                    }
                }
                if display.orientation != previous.orientation {
                    adb.set_display_configuration_orientation(serial, &display)
                        .await?;
                }
            }
            Ok::<_, WorkerError>(next_runtime)
        }
        .await;

        let next_runtime = match transaction {
            Ok(runtime) => runtime,
            Err(error) => {
                let rollback = rollback_secondary_display_transaction(
                    &backend,
                    &control_endpoint,
                    &added_applied,
                    &removed_applied,
                    &changed_applied,
                )
                .await;
                if let (Some(adb), Some(serial)) = (&adb, serial.as_deref()) {
                    for runtime in runtime_displays.iter().skip(1) {
                        let _ = adb
                            .set_display_density(serial, runtime.scanout_id * 2, runtime.dpi)
                            .await;
                    }
                    if display.orientation != previous.orientation {
                        let _ = adb
                            .set_display_configuration_orientation(serial, &previous)
                            .await;
                    }
                }
                if let Err(rollback_error) = rollback {
                    return Err(WorkerError::Platform(hd_platform::PlatformError::Vm(
                        format!(
                            "runtime display reconfiguration failed ({error}); rollback also failed ({rollback_error})"
                        ),
                    )));
                }
                return Err(error);
            }
        };

        if display.orientation != previous.orientation && (adb.is_none() || serial.is_none()) {
            return Err(WorkerError::AdbNotReady);
        }
        spec.display = display;
        let mut mutable = self.mutable.lock().await;
        mutable.active_spec = Some(spec);
        mutable.status.runtime_displays = next_runtime;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn action(self: &Arc<Self>, action: InstanceActionV2) -> Result<(), WorkerError> {
        action.validate()?;
        if self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::NotReady);
        }
        match action {
            InstanceActionV2::Key { key } => {
                if key == KeyActionV2::Power {
                    // Prefer the same Android framework keyevent path as every other toolbar key.
                    // If ADB readiness is still converging, use the platform adapter's supported
                    // native path: Windows crosvm powerbtn or the Unix virtio keyboard endpoint.
                    let (adb_target, platform_target) = {
                        let mutable = self.mutable.lock().await;
                        if mutable.adb_ready {
                            (
                                Some((
                                    mutable
                                        .adb
                                        .clone()
                                        .ok_or(WorkerError::ReadinessUnavailable)?,
                                    mutable
                                        .status
                                        .adb_serial
                                        .clone()
                                        .ok_or(WorkerError::ReadinessUnavailable)?,
                                )),
                                None,
                            )
                        } else {
                            let launch = mutable.launch.as_ref().ok_or(WorkerError::NotRunning)?;
                            (
                                None,
                                Some((
                                    mutable.backend.clone().ok_or(WorkerError::NotRunning)?,
                                    launch.keyboard_endpoint.clone(),
                                    launch.control_endpoint.clone(),
                                )),
                            )
                        }
                    };
                    if let Some((adb, serial)) = adb_target {
                        tracing::info!(
                            event = "worker.key.power.started",
                            instance_id = %self.instance_id,
                            transport = "adb",
                            "delivering Android power key"
                        );
                        match adb.send_key(&serial, key).await {
                            Ok(()) => {
                                tracing::info!(
                                    event = "worker.key.power.succeeded",
                                    instance_id = %self.instance_id,
                                    transport = "adb",
                                    "Android power key delivered"
                                );
                            }
                            Err(error) if error.is_definitively_unavailable() => {
                                tracing::warn!(
                                    event = "worker.adb.stale",
                                    instance_id = %self.instance_id,
                                    %error,
                                    "ADB transport is definitively unavailable; clearing stale readiness"
                                );
                                let (backend, keyboard_endpoint, control_endpoint) = {
                                    let mut mutable = self.mutable.lock().await;
                                    mutable.adb_ready = false;
                                    mutable.status.adb_ready = false;
                                    let launch =
                                        mutable.launch.as_ref().ok_or(WorkerError::NotRunning)?;
                                    (
                                        mutable.backend.clone().ok_or(WorkerError::NotRunning)?,
                                        launch.keyboard_endpoint.clone(),
                                        launch.control_endpoint.clone(),
                                    )
                                };
                                backend
                                    .send_power_key(&keyboard_endpoint, &control_endpoint)
                                    .await
                                    .map_err(WorkerError::Platform)?;
                                tracing::info!(
                                    event = "worker.key.power.succeeded",
                                    instance_id = %self.instance_id,
                                    transport = "platform_recovery",
                                    adb_error = %error,
                                    "Android power key delivered after definitive ADB transport loss"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    event = "worker.key.power.failed",
                                    instance_id = %self.instance_id,
                                    transport = "adb",
                                    %error,
                                    "Android power key delivery failed without safe fallback"
                                );
                                return Err(WorkerError::Adb(error));
                            }
                        }
                    } else if let Some((backend, keyboard_endpoint, control_endpoint)) =
                        platform_target
                    {
                        tracing::info!(
                            event = "worker.key.power.started",
                            instance_id = %self.instance_id,
                            transport = "platform_fallback",
                            "delivering Android power key"
                        );
                        backend
                            .send_power_key(&keyboard_endpoint, &control_endpoint)
                            .await
                            .map_err(|error| {
                                tracing::warn!(
                                    event = "worker.key.power.failed",
                                    instance_id = %self.instance_id,
                                    transport = "platform_fallback",
                                    %error,
                                    "Android power key delivery failed"
                                );
                                WorkerError::Platform(error)
                            })?;
                        tracing::info!(
                            event = "worker.key.power.succeeded",
                            instance_id = %self.instance_id,
                            transport = "platform_fallback",
                            "Android power key delivered"
                        );
                    }
                } else {
                    let (adb, serial) = {
                        let mutable = self.mutable.lock().await;
                        if !mutable.adb_ready {
                            return Err(WorkerError::AdbNotReady);
                        }
                        (
                            mutable
                                .adb
                                .clone()
                                .ok_or(WorkerError::ReadinessUnavailable)?,
                            mutable
                                .status
                                .adb_serial
                                .clone()
                                .ok_or(WorkerError::ReadinessUnavailable)?,
                        )
                    };
                    adb.send_key(&serial, key).await?;
                }
            }
            InstanceActionV2::Trackpad { event } => {
                let (backend, endpoint) = {
                    let mutable = self.mutable.lock().await;
                    let spec = mutable
                        .active_spec
                        .as_ref()
                        .ok_or(WorkerError::NotRunning)?;
                    if spec.guest_kind != GuestKindV2::Android || !spec.devices.touchpad {
                        return Err(WorkerError::DeviceActionUnsupported("touchpad is disabled"));
                    }
                    let launch = mutable.launch.as_ref().ok_or(WorkerError::NotRunning)?;
                    (
                        mutable.backend.clone().ok_or(WorkerError::NotRunning)?,
                        launch
                            .trackpad_endpoint
                            .clone()
                            .ok_or(WorkerError::NotRunning)?,
                    )
                };
                backend.send_trackpad(&endpoint, event).await?;
            }
            InstanceActionV2::Rotate { orientation } => {
                let (adb, serial, mut display) = {
                    let mutable = self.mutable.lock().await;
                    if !mutable.adb_ready {
                        return Err(WorkerError::AdbNotReady);
                    }
                    (
                        mutable
                            .adb
                            .clone()
                            .ok_or(WorkerError::ReadinessUnavailable)?,
                        mutable
                            .status
                            .adb_serial
                            .clone()
                            .ok_or(WorkerError::ReadinessUnavailable)?,
                        mutable
                            .active_spec
                            .as_ref()
                            .ok_or(WorkerError::NotRunning)?
                            .display
                            .clone(),
                    )
                };
                display.orientation = orientation;
                adb.set_display_configuration_orientation(&serial, &display)
                    .await?;
                self.mutable
                    .lock()
                    .await
                    .active_spec
                    .as_mut()
                    .ok_or(WorkerError::NotRunning)?
                    .display
                    .orientation = orientation;
            }
            InstanceActionV2::SetLocation { location } => {
                self.stop_location_route(false).await?;
                self.call_device_component(
                    "hd-device-sim",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::SetLocation {
                            location: location.clone(),
                        },
                    },
                )
                .await?;
            }
            InstanceActionV2::StartLocationRoute { route } => {
                self.start_location_route(route).await?;
            }
            InstanceActionV2::PauseLocationRoute => {
                self.set_location_route_control(LocationRouteControl::Paused)
                    .await?;
            }
            InstanceActionV2::ResumeLocationRoute => {
                self.set_location_route_control(LocationRouteControl::Playing)
                    .await?;
            }
            InstanceActionV2::StopLocationRoute => {
                self.stop_location_route(true).await?;
            }
            InstanceActionV2::SetBattery { battery } => {
                self.call_device_component(
                    "hd-device-sim",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::SetBattery {
                            battery: battery.clone(),
                        },
                    },
                )
                .await?;
                let (adb, serial) = {
                    let mutable = self.mutable.lock().await;
                    if !mutable.adb_ready {
                        return Err(WorkerError::AdbNotReady);
                    }
                    (
                        mutable
                            .adb
                            .clone()
                            .ok_or(WorkerError::ReadinessUnavailable)?,
                        mutable
                            .status
                            .adb_serial
                            .clone()
                            .ok_or(WorkerError::ReadinessUnavailable)?,
                    )
                };
                adb.set_battery(&serial, &battery).await?;
            }
            InstanceActionV2::SetNetworkCondition { condition } => {
                self.call_device_component(
                    "hd-device-sim",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::SetNetworkCondition {
                            condition: condition.clone(),
                        },
                    },
                )
                .await?;
                let (adb, serial) = {
                    let mutable = self.mutable.lock().await;
                    if !mutable.adb_ready {
                        return Err(WorkerError::AdbNotReady);
                    }
                    (
                        mutable
                            .adb
                            .clone()
                            .ok_or(WorkerError::ReadinessUnavailable)?,
                        mutable
                            .status
                            .adb_serial
                            .clone()
                            .ok_or(WorkerError::ReadinessUnavailable)?,
                    )
                };
                adb.set_network_condition(&serial, &condition).await?;
            }
            InstanceActionV2::InjectSensor { injection } => {
                #[cfg(any(target_os = "macos", windows))]
                {
                    let _ = injection;
                    return Err(WorkerError::Unsupported(
                        "this Android 15 profile exposes AOSP three-axis motion injection, not independent or timed sensor overrides",
                    ));
                }
                #[cfg(not(any(target_os = "macos", windows)))]
                self.call_device_component(
                    "hd-device-sim",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::InjectSensor {
                            injection: injection.clone(),
                        },
                    },
                )
                .await?;
                #[cfg(not(any(target_os = "macos", windows)))]
                {
                    let (adb, serial) = {
                        let mutable = self.mutable.lock().await;
                        if !mutable.adb_ready {
                            return Err(WorkerError::AdbNotReady);
                        }
                        (
                            mutable
                                .adb
                                .clone()
                                .ok_or(WorkerError::ReadinessUnavailable)?,
                            mutable
                                .status
                                .adb_serial
                                .clone()
                                .ok_or(WorkerError::ReadinessUnavailable)?,
                        )
                    };
                    adb.inject_sensor(&serial, &injection).await?;
                }
            }
            InstanceActionV2::SetSensorPose { pose } => {
                {
                    let (adb, serial, previous) = {
                        let mutable = self.mutable.lock().await;
                        if !mutable.adb_ready {
                            return Err(WorkerError::AdbNotReady);
                        }
                        (
                            mutable
                                .adb
                                .clone()
                                .ok_or(WorkerError::ReadinessUnavailable)?,
                            mutable
                                .status
                                .adb_serial
                                .clone()
                                .ok_or(WorkerError::ReadinessUnavailable)?,
                            mutable.status.sensor_pose.unwrap_or_default(),
                        )
                    };
                    adb.inject_sensor_pose(&serial, hd_core::sensor_motion_frame(previous, pose))
                        .await?;
                }
                self.call_device_component(
                    "hd-device-sim",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::SetSensorPose { pose },
                    },
                )
                .await?;
                self.mutable.lock().await.status.sensor_pose = Some(pose);
            }
            InstanceActionV2::BluetoothPeer { action } => {
                self.call_device_component(
                    "rootcanal-adapter",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::BluetoothPeer {
                            action: action.clone(),
                        },
                    },
                )
                .await?;
                let capture_record = if let BluetoothPeerActionV2::CaptureHci {
                    capture_id,
                    duration_ms,
                } = &action
                {
                    let run_dir = self
                        .mutable
                        .lock()
                        .await
                        .journal
                        .as_ref()
                        .map(|journal| journal.run_dir().to_owned())
                        .ok_or(WorkerError::NotRunning)?;
                    Some(read_bluetooth_hci_capture_record(
                        &run_dir,
                        *capture_id,
                        *duration_ms,
                    )?)
                } else {
                    None
                };
                let mut mutable = self.mutable.lock().await;
                match action {
                    BluetoothPeerActionV2::CreateGattPeer { peer_id, name } => {
                        mutable.status.bluetooth_peers.push(BluetoothPeerStateV2 {
                            peer_id,
                            name,
                            kind: BluetoothPeerKindV2::Gatt,
                            advertising: true,
                            scripted_frame_count: None,
                            repeat: false,
                            keyboard_reports_sent: 0,
                        });
                    }
                    BluetoothPeerActionV2::CreateBeacon { peer_id, name, .. } => {
                        mutable.status.bluetooth_peers.push(BluetoothPeerStateV2 {
                            peer_id,
                            name,
                            kind: BluetoothPeerKindV2::Beacon,
                            advertising: true,
                            scripted_frame_count: None,
                            repeat: false,
                            keyboard_reports_sent: 0,
                        });
                    }
                    BluetoothPeerActionV2::CreateScriptedBeacon {
                        peer_id,
                        name,
                        frames,
                        repeat,
                    } => {
                        mutable.status.bluetooth_peers.push(BluetoothPeerStateV2 {
                            peer_id,
                            name,
                            kind: BluetoothPeerKindV2::ScriptedBeacon,
                            advertising: true,
                            scripted_frame_count: u16::try_from(frames.len()).ok(),
                            repeat,
                            keyboard_reports_sent: 0,
                        });
                    }
                    BluetoothPeerActionV2::CreateHidKeyboard { peer_id, name } => {
                        mutable.status.bluetooth_peers.push(BluetoothPeerStateV2 {
                            peer_id,
                            name,
                            kind: BluetoothPeerKindV2::HidKeyboard,
                            advertising: true,
                            scripted_frame_count: None,
                            repeat: false,
                            keyboard_reports_sent: 0,
                        });
                    }
                    BluetoothPeerActionV2::SendHidKeyboardReport { peer_id, .. } => {
                        if let Some(peer) = mutable
                            .status
                            .bluetooth_peers
                            .iter_mut()
                            .find(|peer| peer.peer_id == peer_id)
                        {
                            peer.keyboard_reports_sent =
                                peer.keyboard_reports_sent.saturating_add(1);
                        }
                    }
                    BluetoothPeerActionV2::RemovePeer { peer_id } => {
                        mutable
                            .status
                            .bluetooth_peers
                            .retain(|peer| peer.peer_id != peer_id);
                    }
                    BluetoothPeerActionV2::SetAdvertising { peer_id, enabled } => {
                        if let Some(peer) = mutable
                            .status
                            .bluetooth_peers
                            .iter_mut()
                            .find(|peer| peer.peer_id == peer_id)
                        {
                            peer.advertising = enabled;
                        }
                    }
                    BluetoothPeerActionV2::CaptureHci { .. } => {
                        mutable.status.last_bluetooth_hci_capture = capture_record;
                    }
                }
            }
            InstanceActionV2::NfcTag { action } => {
                self.call_device_component(
                    "casimir-adapter",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::NfcTag { action },
                    },
                )
                .await?;
            }
            InstanceActionV2::SetUwbRanging { ranging } => {
                self.call_device_component(
                    "uwb-adapter",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::SetUwbRanging { ranging },
                    },
                )
                .await?;
                self.mutable.lock().await.status.uwb_ranging = Some(ranging);
            }
            InstanceActionV2::SetModemState { modem } => {
                self.call_device_component(
                    "modem-adapter",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::SetModemState {
                            modem: modem.clone(),
                        },
                    },
                )
                .await?;
                self.mutable.lock().await.status.modem_state = Some(modem);
            }
            InstanceActionV2::MicrodroidConsoleChallenge {
                challenge_id,
                confirmed,
            } => {
                #[cfg(unix)]
                self.send_microdroid_console_challenge(challenge_id, confirmed)
                    .await?;
                #[cfg(not(unix))]
                {
                    let _ = (challenge_id, confirmed);
                    return Err(WorkerError::Unsupported(
                        "Microdroid console challenge requires a Unix FIFO",
                    ));
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    async fn send_microdroid_console_challenge(
        &self,
        challenge_id: Uuid,
        confirmed: bool,
    ) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        let (run_id, mut channel) = {
            let mut mutable = self.mutable.lock().await;
            let spec = mutable
                .active_spec
                .as_ref()
                .ok_or(WorkerError::NotRunning)?;
            let microdroid = spec.microdroid.as_ref().ok_or(WorkerError::Unsupported(
                "console challenge is available only for Microdroid",
            ))?;
            if spec.guest_kind != GuestKindV2::Microdroid {
                return Err(WorkerError::Unsupported(
                    "console challenge is available only for Microdroid",
                ));
            }
            if microdroid.debug_level != MicrodroidDebugLevelV2::Full {
                return Err(WorkerError::Unsupported(
                    "console challenge requires Full-debug Microdroid",
                ));
            }
            let run_id = mutable.status.run_id.ok_or(WorkerError::NotRunning)?;
            let channel =
                mutable
                    .microdroid_console_challenge
                    .take()
                    .ok_or(WorkerError::Unsupported(
                        "Microdroid console challenge channel is unavailable",
                    ))?;
            (run_id, channel)
        };
        let result = channel
            .send_and_verify(
                challenge_id,
                confirmed,
                MICRODROID_CONSOLE_CHALLENGE_TIMEOUT,
            )
            .await;
        {
            let mut mutable = self.mutable.lock().await;
            if mutable.status.run_id == Some(run_id) {
                mutable.microdroid_console_challenge = Some(channel);
            }
        }
        let receipt = result?;
        tracing::info!(
            event = "microdroid.console_challenge.verified",
            instance_id = %self.instance_id,
            %run_id,
            challenge_id = %receipt.challenge_id,
            nonce_sha256 = %receipt.nonce_sha256,
            request_size_bytes = receipt.request_size_bytes,
            "trusted Microdroid Payload consumed and answered the typed console challenge"
        );
        Ok(())
    }

    async fn start_location_route(
        self: &Arc<Self>,
        route: LocationRouteV2,
    ) -> Result<(), WorkerError> {
        self.stop_location_route(false).await?;
        let first = route.points.first().cloned().ok_or_else(|| {
            WorkerError::Task("validated location route has no first point".to_owned())
        })?;
        self.call_device_component(
            "hd-device-sim",
            DeviceControlCommandV2::Action {
                action: InstanceActionV2::SetLocation { location: first },
            },
        )
        .await?;

        let point_count = u32::try_from(route.points.len())
            .map_err(|_| WorkerError::Task("location route point count exceeds u32".to_owned()))?;
        let status = LocationRouteStatusV2 {
            id: route.id,
            name: route.name.clone(),
            point_count,
            current_point: 1,
            interval_ms: route.interval_ms,
            repeat: route.repeat,
            state: LocationRoutePlaybackStateV2::Playing,
            started_at: OffsetDateTime::now_utc(),
        };
        let (control, receiver) = watch::channel(LocationRouteControl::Playing);
        self.mutable.lock().await.active_location_route = Some(ActiveLocationRoute {
            status: status.clone(),
            control,
            paused_by_instance: false,
        });
        tracing::info!(
            event = "worker.location_route.started",
            instance_id = %self.instance_id,
            route_id = %route.id,
            point_count,
            interval_ms = route.interval_ms,
            repeat = route.repeat,
            "location route playback started after the first Guest point was applied"
        );
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.run_location_route(route, receiver).await;
        });
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn run_location_route(
        self: Arc<Self>,
        route: LocationRouteV2,
        mut control: watch::Receiver<LocationRouteControl>,
    ) {
        let mut index = 1_usize;
        let mut applied_points = 1_u32;
        let mut failure = None;
        loop {
            let current_control = *control.borrow();
            match current_control {
                LocationRouteControl::Stop => break,
                LocationRouteControl::Paused => {
                    if control.changed().await.is_err() {
                        break;
                    }
                    continue;
                }
                LocationRouteControl::Playing => {}
            }

            tokio::select! {
                biased;
                changed = control.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    continue;
                }
                () = tokio::time::sleep(Duration::from_millis(u64::from(route.interval_ms))) => {}
            }
            if *control.borrow() != LocationRouteControl::Playing {
                continue;
            }
            if index == route.points.len() {
                if route.repeat {
                    index = 0;
                } else {
                    break;
                }
            }
            let location = route.points[index].clone();
            if let Err(error) = self
                .call_device_component(
                    "hd-device-sim",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::SetLocation { location },
                    },
                )
                .await
            {
                failure = Some(error);
                break;
            }
            let current_point = u32::try_from(index + 1).unwrap_or(u32::MAX);
            let mut mutable = self.mutable.lock().await;
            if let Some(active) = mutable
                .active_location_route
                .as_mut()
                .filter(|active| active.status.id == route.id)
            {
                active.status.current_point = current_point;
            } else {
                break;
            }
            drop(mutable);
            index += 1;
            applied_points = applied_points.saturating_add(1);
            if index == route.points.len() && !route.repeat {
                break;
            }
        }

        let stopped = *control.borrow() == LocationRouteControl::Stop;
        let reason = if failure.is_some() {
            LocationRouteFinishReasonV2::Failed
        } else if stopped {
            LocationRouteFinishReasonV2::Stopped
        } else {
            LocationRouteFinishReasonV2::Completed
        };
        let record = LocationRouteRecordV2 {
            id: route.id,
            name: route.name.clone(),
            point_count: u32::try_from(route.points.len()).unwrap_or(u32::MAX),
            applied_points,
            repeat: route.repeat,
            reason,
            error_code: failure.as_ref().map(|error| error.code().to_owned()),
            started_at: self
                .mutable
                .lock()
                .await
                .active_location_route
                .as_ref()
                .filter(|active| active.status.id == route.id)
                .map_or_else(OffsetDateTime::now_utc, |active| active.status.started_at),
            finished_at: OffsetDateTime::now_utc(),
        };
        let mut mutable = self.mutable.lock().await;
        let still_current = mutable
            .active_location_route
            .as_ref()
            .is_some_and(|active| active.status.id == route.id);
        if still_current {
            mutable.active_location_route = None;
            mutable.status.last_location_route = Some(record);
        }
        drop(mutable);
        if let Some(error) = failure {
            tracing::error!(
                event = "worker.location_route.failed",
                error_code = %error.code(),
                instance_id = %self.instance_id,
                route_id = %route.id,
                %error,
                "location route playback stopped after a Guest device action failed"
            );
        } else {
            tracing::info!(
                event = "worker.location_route.finished",
                instance_id = %self.instance_id,
                route_id = %route.id,
                "location route playback finished"
            );
        }
    }

    async fn set_location_route_control(
        &self,
        control: LocationRouteControl,
    ) -> Result<(), WorkerError> {
        let mut mutable = self.mutable.lock().await;
        let active = mutable
            .active_location_route
            .as_mut()
            .ok_or(WorkerError::Busy("no location route is active"))?;
        active
            .control
            .send(control)
            .map_err(|_| WorkerError::Task("location route task is unavailable".to_owned()))?;
        active.status.state = match control {
            LocationRouteControl::Playing => LocationRoutePlaybackStateV2::Playing,
            LocationRouteControl::Paused => LocationRoutePlaybackStateV2::Paused,
            LocationRouteControl::Stop => {
                return Err(WorkerError::Task(
                    "stop must use the location route cleanup boundary".to_owned(),
                ));
            }
        };
        active.paused_by_instance = false;
        tracing::info!(
            event = "worker.location_route.controlled",
            instance_id = %self.instance_id,
            route_id = %active.status.id,
            state = ?active.status.state,
            "location route playback state changed"
        );
        Ok(())
    }

    async fn pause_location_route_for_instance(&self) -> Result<bool, WorkerError> {
        let mut mutable = self.mutable.lock().await;
        let Some(active) = mutable.active_location_route.as_mut() else {
            return Ok(false);
        };
        if active.status.state == LocationRoutePlaybackStateV2::Paused {
            return Ok(false);
        }
        active
            .control
            .send(LocationRouteControl::Paused)
            .map_err(|_| WorkerError::Task("location route task is unavailable".to_owned()))?;
        active.status.state = LocationRoutePlaybackStateV2::Paused;
        active.paused_by_instance = true;
        Ok(true)
    }

    async fn resume_location_route_for_instance(&self) -> Result<(), WorkerError> {
        let mut mutable = self.mutable.lock().await;
        let Some(active) = mutable.active_location_route.as_mut() else {
            return Ok(());
        };
        if !active.paused_by_instance {
            return Ok(());
        }
        active
            .control
            .send(LocationRouteControl::Playing)
            .map_err(|_| WorkerError::Task("location route task is unavailable".to_owned()))?;
        active.status.state = LocationRoutePlaybackStateV2::Playing;
        active.paused_by_instance = false;
        Ok(())
    }

    async fn stop_location_route(&self, require_active: bool) -> Result<(), WorkerError> {
        let active = {
            let mutable = self.mutable.lock().await;
            mutable.active_location_route.as_ref().map(|active| {
                let _ = active.control.send(LocationRouteControl::Stop);
                active.status.id
            })
        };
        let Some(route_id) = active else {
            return if require_active {
                Err(WorkerError::Busy("no location route is active"))
            } else {
                Ok(())
            };
        };
        let stopped = tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if self
                    .mutable
                    .lock()
                    .await
                    .active_location_route
                    .as_ref()
                    .is_none_or(|active| active.status.id != route_id)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if stopped.is_err() {
            return Err(WorkerError::Task(
                "location route task did not stop within 6 seconds".to_owned(),
            ));
        }
        tracing::info!(
            event = "worker.location_route.stopped",
            instance_id = %self.instance_id,
            %route_id,
            "location route playback stopped"
        );
        Ok(())
    }

    async fn install_apk(&self, path: &Path, expected_sha256: &str) -> Result<(), WorkerError> {
        if self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::NotReady);
        }
        let actual = crate::sha256_file(path)?;
        if actual != expected_sha256 {
            return Err(WorkerError::UploadDigestMismatch {
                expected: expected_sha256.to_owned(),
                actual,
            });
        }
        let (adb, serial) = {
            let mutable = self.mutable.lock().await;
            if !mutable.adb_ready {
                return Err(WorkerError::AdbNotReady);
            }
            (
                mutable.adb.clone().ok_or(WorkerError::NotRunning)?,
                mutable
                    .status
                    .adb_serial
                    .clone()
                    .ok_or(WorkerError::NotRunning)?,
            )
        };
        adb.install_and_verify(&serial, path).await?;
        Ok(())
    }

    async fn collect_guest_logs(&self) -> Result<hd_core::DiagnosticFileV2, WorkerError> {
        if self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::NotReady);
        }
        let microdroid_log = {
            let mutable = self.mutable.lock().await;
            mutable
                .active_spec
                .as_ref()
                .filter(|spec| spec.guest_kind == GuestKindV2::Microdroid)
                .zip(mutable.journal.clone())
                .map(|(_, journal)| journal.run_dir().join("microdroid-guest.log"))
        };
        if let Some(path) = microdroid_log {
            let metadata = std::fs::metadata(&path).map_err(|source| WorkerError::Io {
                operation: "inspect Microdroid guest log",
                path: path.clone(),
                source,
            })?;
            return Ok(hd_core::DiagnosticFileV2 {
                relative_path: path.clone(),
                sha256: crate::sha256_file(&path).map_err(|error| {
                    WorkerError::Task(format!("hash Microdroid guest log: {error}"))
                })?,
                size_bytes: metadata.len(),
                truncated: false,
            });
        }
        let (adb, serial, journal) = {
            let mutable = self.mutable.lock().await;
            if !mutable.adb_ready {
                return Err(WorkerError::AdbNotReady);
            }
            (
                mutable.adb.clone().ok_or(WorkerError::NotRunning)?,
                mutable
                    .status
                    .adb_serial
                    .clone()
                    .ok_or(WorkerError::NotRunning)?,
                mutable.journal.clone().ok_or(WorkerError::NotRunning)?,
            )
        };
        let bytes = adb.guest_logcat(&serial).await?;
        let path = journal.run_dir().join("guest-logcat.txt");
        hd_platform::write_owner_only(&path, &bytes)?;
        Ok(hd_core::DiagnosticFileV2 {
            relative_path: path,
            sha256: hex::encode(sha2::Sha256::digest(&bytes)),
            size_bytes: bytes.len() as u64,
            truncated: false,
        })
    }

    async fn diagnostics(&self) -> Vec<hd_core::DiagnosticCheckV2> {
        let status = self.status().await;
        let (network, microdroid, payload, guest_cid) = {
            let mutable = self.mutable.lock().await;
            (
                mutable
                    .adb_ready
                    .then(|| Some((mutable.adb.clone()?, mutable.status.adb_serial.clone()?)))
                    .flatten(),
                mutable
                    .active_spec
                    .as_ref()
                    .is_some_and(|spec| spec.guest_kind == GuestKindV2::Microdroid),
                mutable
                    .active_spec
                    .as_ref()
                    .and_then(|spec| spec.microdroid.as_ref())
                    .map(|config| format!("{:?}", config.payload)),
                mutable.launch.as_ref().map(|launch| launch.guest_cid),
            )
        };
        let mut checks = vec![
            hd_core::DiagnosticCheckV2 {
                id: "worker.identity".to_owned(),
                status: if process_identity_is_alive(&status.identity) {
                    hd_core::DiagnosticStatusV2::Pass
                } else {
                    hd_core::DiagnosticStatusV2::Fail
                },
                detail: format!("pid={}", status.identity.pid),
                fields: BTreeMap::new(),
            },
            hd_core::DiagnosticCheckV2 {
                id: "worker.state".to_owned(),
                status: if matches!(
                    status.observed,
                    ObservedStateV2::Failed | ObservedStateV2::Blocked
                ) {
                    hd_core::DiagnosticStatusV2::Fail
                } else {
                    hd_core::DiagnosticStatusV2::Pass
                },
                detail: format!("{:?}", status.observed),
                fields: BTreeMap::new(),
            },
        ];
        if microdroid {
            checks.push(hd_core::DiagnosticCheckV2 {
                id: "microdroid.payload".to_owned(),
                status: if status.observed == ObservedStateV2::Ready {
                    hd_core::DiagnosticStatusV2::Pass
                } else {
                    hd_core::DiagnosticStatusV2::Blocked
                },
                detail: payload.unwrap_or_else(|| "Microdroid payload configuration".to_owned()),
                fields: guest_cid
                    .map(|cid| BTreeMap::from([("guest_cid".to_owned(), cid.to_string())]))
                    .unwrap_or_default(),
            });
            checks.push(hd_core::DiagnosticCheckV2 {
                id: "microdroid.adb".to_owned(),
                status: if status.adb_ready {
                    hd_core::DiagnosticStatusV2::Pass
                } else {
                    hd_core::DiagnosticStatusV2::Blocked
                },
                detail: if status.adb_ready {
                    "Microdroid ADB is ready".to_owned()
                } else {
                    "the active Payload does not expose an authenticated adbd service".to_owned()
                },
                fields: status
                    .adb_serial
                    .clone()
                    .map(|serial| BTreeMap::from([("serial".to_owned(), serial)]))
                    .unwrap_or_default(),
            });
            return checks;
        }
        checks.push(guest_network_diagnostic(network.as_ref()).await);
        match network {
            Some((adb, serial)) => match Box::pin(adb.device_runtime_health(&serial)).await {
                Ok(devices) => checks.extend(devices.into_iter().map(device_runtime_diagnostic)),
                Err(error) => checks.push(hd_core::DiagnosticCheckV2 {
                    id: "device.runtime_probe".to_owned(),
                    status: hd_core::DiagnosticStatusV2::Fail,
                    detail: format!("Android device runtime probe failed: {error}"),
                    fields: BTreeMap::new(),
                }),
            },
            None => checks.push(hd_core::DiagnosticCheckV2 {
                id: "device.runtime_probe".to_owned(),
                status: hd_core::DiagnosticStatusV2::Blocked,
                detail: "ADB is not ready; device runtime state cannot be verified".to_owned(),
                fields: BTreeMap::new(),
            }),
        }
        checks
    }

    fn spawn_exit_monitor(self: &Arc<Self>) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let status = worker.status().await;
                if !status.observed.is_active() {
                    break;
                }
                match worker.poll_process().await {
                    Ok(Some(exit)) => {
                        let microdroid_run_dir = {
                            let mutable = worker.mutable.lock().await;
                            mutable
                                .active_spec
                                .as_ref()
                                .is_some_and(|spec| spec.guest_kind == GuestKindV2::Microdroid)
                                .then(|| {
                                    mutable
                                        .journal
                                        .as_ref()
                                        .map(|journal| journal.run_dir().to_owned())
                                })
                                .flatten()
                        };
                        let disposition = match microdroid_run_dir {
                            Some(run_dir) => {
                                classify_microdroid_exit_disposition_after_log_settle(
                                    &run_dir, exit.code,
                                )
                                .await
                            }
                            None => MicrodroidExitDisposition::Failed(WorkerError::GuestExited(
                                exit.code,
                            )),
                        };
                        match disposition {
                            MicrodroidExitDisposition::Completed(payload_exit_code) => {
                                tracing::info!(
                                    event = "microdroid.payload.finished",
                                    instance_id = %worker.instance_id,
                                    payload_exit_code,
                                    process_exit_code = ?exit.code,
                                    "Microdroid payload completed and the AOSP vm launcher shut down cleanly"
                                );
                                if let Err(error) = worker
                                    .stop_after_microdroid_completion(payload_exit_code)
                                    .await
                                {
                                    tracing::error!(
                                        event = "microdroid.payload.finish_cleanup.failed",
                                        instance_id = %worker.instance_id,
                                        error_code = error.code(),
                                        %error,
                                        "Microdroid payload completed but exact runtime cleanup failed"
                                    );
                                }
                            }
                            MicrodroidExitDisposition::Failed(error) => {
                                let _operation = worker.operation.lock().await;
                                if !worker.status().await.observed.is_active() {
                                    break;
                                }
                                tracing::warn!(
                                    event = "worker.guest.exit.classified",
                                    instance_id = %worker.instance_id,
                                    error_code = error.code(),
                                    exit_code = ?exit.code,
                                    "unexpected Guest exit was classified from the active run evidence"
                                );
                                let _ = worker.fail_start(&error, false).await;
                            }
                        }
                        break;
                    }
                    Ok(None) => match worker.ensure_components_alive().await {
                        Ok(()) => {}
                        Err(error) => {
                            let _operation = worker.operation.lock().await;
                            if !worker.status().await.observed.is_active() {
                                break;
                            }
                            let _ = worker.fail_start(&error, false).await;
                            break;
                        }
                    },
                    Err(error) => {
                        let _operation = worker.operation.lock().await;
                        if !worker.status().await.observed.is_active() {
                            break;
                        }
                        let _ = worker.fail_start(&error, false).await;
                        break;
                    }
                }
            }
        });
    }

    fn spawn_deferred_adb_readiness(
        self: &Arc<Self>,
        run_id: Uuid,
        run_dir: PathBuf,
        serial: String,
        policy: DeferredAdbReadinessPolicy,
    ) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            let adb = {
                let mutable = worker.mutable.lock().await;
                if mutable.status.run_id != Some(run_id) {
                    return;
                }
                let Some(adb) = mutable.adb.clone() else {
                    return;
                };
                adb
            };

            worker.record_deferred_adb_readiness(
                &run_dir,
                run_id,
                &serial,
                "connect",
                "waiting for the exact loopback ADB transport",
            );
            if !worker.connect_deferred_adb(&adb, run_id, &serial).await {
                worker.record_deferred_adb_readiness(
                    &run_dir,
                    run_id,
                    &serial,
                    "failed",
                    "loopback ADB transport did not connect",
                );
                return;
            }

            worker.record_deferred_adb_readiness(
                &run_dir,
                run_id,
                &serial,
                "android_readiness",
                "waiting for stable boot, animation, user and interactive services",
            );
            if !worker
                .wait_deferred_adb_readiness_stage(
                    &adb,
                    run_id,
                    &serial,
                    "worker.adb.deferred_readiness.failed",
                    "initial Android readiness did not stabilize",
                )
                .await
            {
                worker.record_deferred_adb_readiness(
                    &run_dir,
                    run_id,
                    &serial,
                    "failed",
                    "initial Android readiness did not stabilize",
                );
                return;
            }
            worker.record_deferred_adb_readiness(
                &run_dir,
                run_id,
                &serial,
                "device_and_network_policy",
                "applying configured device policy and validating Android networking",
            );
            let policy_apply = policy.apply(&adb, worker.instance_id, run_id, &serial);
            policy_apply.await;

            worker.record_deferred_adb_readiness(
                &run_dir,
                run_id,
                &serial,
                "interactive_policy",
                "converging keep-awake and Android display configuration",
            );
            if !worker
                .converge_deferred_interactive_policy(&adb, run_id, &serial, &policy.display)
                .await
            {
                worker.record_deferred_adb_readiness(
                    &run_dir,
                    run_id,
                    &serial,
                    "failed",
                    "interactive Android display policy did not converge",
                );
                return;
            }

            worker
                .publish_deferred_adb_ready(&run_dir, run_id, &serial)
                .await;
        });
    }

    async fn publish_deferred_adb_ready(&self, run_dir: &Path, run_id: Uuid, serial: &str) {
        let mut mutable = self.mutable.lock().await;
        if mutable.status.run_id == Some(run_id)
            && matches!(
                mutable.status.observed,
                ObservedStateV2::Ready | ObservedStateV2::Paused
            )
        {
            mutable.adb_ready = true;
            drop(mutable);
            self.record_deferred_adb_readiness(
                run_dir,
                run_id,
                serial,
                "complete",
                "ADB-backed HD actions are ready",
            );
            tracing::info!(
                event = "worker.adb.deferred_readiness.succeeded",
                instance_id = %self.instance_id,
                %run_id,
                %serial,
                "ADB-backed HD actions are ready"
            );
        } else {
            drop(mutable);
            self.record_deferred_adb_readiness(
                run_dir,
                run_id,
                serial,
                "cancelled",
                "the active run changed before deferred ADB readiness completed",
            );
        }
    }

    fn record_deferred_adb_readiness(
        &self,
        run_dir: &Path,
        run_id: Uuid,
        serial: &str,
        stage: &str,
        detail: &str,
    ) {
        let marker = serde_json::json!({
            "schema_version": 1,
            "instance_id": self.instance_id,
            "run_id": run_id,
            "serial": serial,
            "stage": stage,
            "detail": detail,
            "updated_at_unix_nanos": OffsetDateTime::now_utc().unix_timestamp_nanos(),
        });
        let path = run_dir.join("deferred-adb-readiness-v1.json");
        if let Err(error) = write_json_atomic(&path, &marker) {
            tracing::warn!(
                event = "worker.adb.deferred_readiness.marker_failed",
                instance_id = %self.instance_id,
                %run_id,
                %serial,
                %stage,
                %error,
                path = %path.display(),
                "failed to persist deferred ADB readiness diagnostics"
            );
        }
    }

    fn spawn_microdroid_deferred_adb_readiness(
        self: &Arc<Self>,
        run_id: Uuid,
        serial: String,
        adb: AdbClient,
    ) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            // Microdroid has no Android framework, PackageManager or boot animation. Reusing the
            // Android `wait_ready` conjunction would keep a real adbd transport false forever.
            // `connect` already requires this exact serial to report the stable ADB `device`
            // state, which is the complete Microdroid ADB readiness contract.
            let readiness =
                tokio::time::timeout(Duration::from_secs(30), adb.connect(&serial)).await;
            let ready = match readiness {
                Ok(Ok(())) => true,
                Ok(Err(error)) => {
                    tracing::warn!(
                        event = "microdroid.adb.deferred",
                        instance_id = %worker.instance_id,
                        %run_id,
                        %error,
                        "Microdroid Payload is Ready but did not expose adbd"
                    );
                    false
                }
                Err(_) => false,
            };
            if !ready {
                return;
            }
            let mut mutable = worker.mutable.lock().await;
            if mutable.status.run_id == Some(run_id)
                && mutable.status.observed == ObservedStateV2::Ready
            {
                mutable.adb_ready = true;
                tracing::info!(
                    event = "microdroid.adb.ready",
                    instance_id = %worker.instance_id,
                    %run_id,
                    %serial,
                    "Microdroid ADB became ready after Payload readiness"
                );
            }
        });
    }

    async fn connect_deferred_adb(&self, adb: &AdbClient, run_id: Uuid, serial: &str) -> bool {
        let connect = adb.connect(serial);
        tokio::pin!(connect);
        let result = loop {
            tokio::select! {
                result = &mut connect => break result,
                () = tokio::time::sleep(Duration::from_millis(500)) => {
                    if self.status().await.run_id != Some(run_id) {
                        return false;
                    }
                }
            }
        };
        match result {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    event = "worker.adb.deferred_connect.failed",
                    instance_id = %self.instance_id,
                    %run_id,
                    %serial,
                    %error,
                    "deferred ADB connection failed; native display remains available"
                );
                false
            }
        }
    }

    async fn converge_deferred_interactive_policy(
        &self,
        adb: &AdbClient,
        run_id: Uuid,
        serial: &str,
        display: &DisplayConfigV2,
    ) -> bool {
        let started = Instant::now();
        loop {
            // Device and network policy can overlap the tail of Android boot. Require stable
            // services before every idempotent attempt instead of publishing a stale sample.
            if !self
                .wait_deferred_adb_readiness_stage(
                    adb,
                    run_id,
                    serial,
                    "worker.adb.deferred_policy_readiness.failed",
                    "Android was not interactive after startup policy reconciliation",
                )
                .await
            {
                return false;
            }

            let (operation, result) = match adb.keep_display_awake(serial).await {
                Ok(()) => (
                    "set_display_configuration",
                    adb.set_display_configuration(serial, display).await,
                ),
                Err(error) => ("keep_display_awake", Err(error)),
            };
            match result {
                Ok(()) => {
                    // Close the interval between the service probe and policy commands. Ready is
                    // published only when the configured Guest still satisfies the full contract.
                    return self
                        .wait_deferred_adb_readiness_stage(
                            adb,
                            run_id,
                            serial,
                            "worker.adb.deferred_final_readiness.failed",
                            "Android lost interactivity while finalizing startup policy",
                        )
                        .await;
                }
                Err(error) if started.elapsed() < DEFERRED_INTERACTIVE_POLICY_TIMEOUT => {
                    tracing::warn!(
                        event = "worker.adb.deferred_interactive_policy.retrying",
                        instance_id = %self.instance_id,
                        %run_id,
                        %serial,
                        %operation,
                        %error,
                        elapsed_ms = started.elapsed().as_millis(),
                        "interactive Android startup policy encountered a transient service withdrawal"
                    );
                    tokio::time::sleep(DEFERRED_INTERACTIVE_POLICY_RETRY_DELAY).await;
                    if self.status().await.run_id != Some(run_id) {
                        return false;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        event = "worker.adb.deferred_interactive_policy.failed",
                        instance_id = %self.instance_id,
                        %run_id,
                        %serial,
                        %operation,
                        %error,
                        elapsed_ms = started.elapsed().as_millis(),
                        "interactive Android startup policy did not converge"
                    );
                    return false;
                }
            }
        }
    }

    async fn wait_deferred_adb_readiness_stage(
        &self,
        adb: &AdbClient,
        run_id: Uuid,
        serial: &str,
        failure_event: &'static str,
        failure_detail: &'static str,
    ) -> bool {
        let readiness = adb.wait_ready(serial);
        tokio::pin!(readiness);
        let result = loop {
            tokio::select! {
                result = &mut readiness => break result,
                () = tokio::time::sleep(Duration::from_millis(500)) => {
                    if self.status().await.run_id != Some(run_id) {
                        return false;
                    }
                }
            }
        };
        match result {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    event = failure_event,
                    instance_id = %self.instance_id,
                    %run_id,
                    %serial,
                    %error,
                    %failure_detail,
                    "deferred Android readiness stage failed; native display remains available"
                );
                false
            }
        }
    }

    async fn poll_process(&self) -> Result<Option<hd_platform::ProcessExit>, WorkerError> {
        let mut mutable = self.mutable.lock().await;
        mutable
            .process
            .as_mut()
            .map(ManagedProcess::try_wait)
            .transpose()
            .map(Option::flatten)
            .map_err(WorkerError::Platform)
    }

    async fn ensure_components_alive(&self) -> Result<(), WorkerError> {
        let mut mutable = self.mutable.lock().await;
        for component in &mut mutable.components {
            if let Some(exit) = component.process.try_wait()? {
                return Err(WorkerError::ComponentExited {
                    component: component.id.clone(),
                    code: exit.code,
                });
            }
        }
        Ok(())
    }

    async fn component_identity(&self, component: &str) -> Result<WorkerIdentityV2, WorkerError> {
        let pid = self
            .mutable
            .lock()
            .await
            .components
            .iter()
            .find(|managed| managed.id == component)
            .map(|managed| managed.process.id())
            .ok_or_else(|| {
                WorkerError::ComponentContract(format!(
                    "component {component} has no managed process"
                ))
            })?;
        Ok(WorkerIdentityV2 {
            pid,
            process_start_marker: hd_platform::process_start_marker(pid)?,
            nonce: Uuid::nil(),
        })
    }

    async fn backend_control(&self) -> Result<(CrosvmBackend, String), WorkerError> {
        let mutable = self.mutable.lock().await;
        Ok((
            mutable.backend.clone().ok_or(WorkerError::NotRunning)?,
            mutable
                .launch
                .as_ref()
                .map(|plan| plan.control_endpoint.clone())
                .ok_or(WorkerError::NotRunning)?,
        ))
    }

    async fn transition(
        &self,
        next: ObservedStateV2,
        error: Option<&WorkerError>,
    ) -> Result<(), WorkerError> {
        let mut mutable = self.mutable.lock().await;
        let previous = mutable.status.observed;
        if !previous.can_transition_to(next) {
            return Err(WorkerError::StateTransition { previous, next });
        }
        mutable.status.observed = next;
        mutable.status.last_error = error.map(WorkerError::api_error);
        if let Some(journal) = &mutable.journal {
            journal.event(
                "state",
                "worker.state.transition",
                Some(next),
                error.map(|value| value.code().to_owned()),
                BTreeMap::from([("previous".to_owned(), format!("{previous:?}"))]),
            )?;
        }
        tracing::info!(
            event = "worker.state.transition",
            instance_id = %self.instance_id,
            ?previous,
            ?next,
            "worker state changed"
        );
        Ok(())
    }

    async fn fail_start(&self, error: &WorkerError, blocked: bool) -> Result<(), WorkerError> {
        let target = if blocked {
            ObservedStateV2::Blocked
        } else {
            ObservedStateV2::Failed
        };
        if self.mutable.lock().await.active_location_route.is_some()
            && let Err(route_error) = self.stop_location_route(false).await
        {
            tracing::warn!(
                event = "worker.location_route.failure_cleanup.failed",
                instance_id = %self.instance_id,
                %route_error,
                "runtime failure cleanup continued after location route cleanup failed"
            );
        }
        let (process, launch) = {
            let mut mutable = self.mutable.lock().await;
            (mutable.process.take(), mutable.launch.clone())
        };
        let mut retained_process = None;
        let mut cleanup_error = None;
        if let Some(mut process) = process {
            let needs_termination = match process.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                Err(error) => {
                    tracing::warn!(
                        event = "worker.failure.poll.failed",
                        instance_id = %self.instance_id,
                        %error,
                        "failed to poll guest during failure cleanup; forcing termination"
                    );
                    true
                }
            };
            if needs_termination
                && let Err(error) = TokioProcessSupervisor.terminate(&mut process).await
            {
                cleanup_error = Some(WorkerError::Platform(error));
                retained_process = Some(process);
            }
        }
        if let Err(error) = self.terminate_components().await {
            cleanup_error.get_or_insert(error);
        }
        let component_cleanup_pending = !self.mutable.lock().await.components.is_empty();
        if retained_process.is_none()
            && !component_cleanup_pending
            && let Some(launch) = &launch
            && let Err(error) = cleanup_runtime_endpoints(launch)
        {
            cleanup_error.get_or_insert(error);
        }
        let transition_result = self.transition(target, Some(error)).await;
        let run_exit_code = match error {
            WorkerError::GuestExited(exit_code) => *exit_code,
            WorkerError::MicrodroidPayloadFailed(exit_code) => Some(*exit_code),
            _ => None,
        };
        let finish_result = self.finish_run(target, run_exit_code, Some(error)).await;
        {
            let mut mutable = self.mutable.lock().await;
            mutable.status.cleanup_pending = cleanup_error.is_some();
            mutable.process = retained_process;
            if mutable.process.is_none() {
                mutable.status.child_pid = None;
            }
            mutable.status.adb_serial = None;
            mutable.adb = None;
            mutable.adb_ready = false;
            if cleanup_error.is_none() {
                mutable.backend = None;
                mutable.launch = None;
                #[cfg(any(windows, target_os = "macos"))]
                {
                    mutable.host_recorder_endpoint = None;
                }
                mutable.device_control_tokens.clear();
                #[cfg(unix)]
                mutable.device_output_files.clear();
                #[cfg(unix)]
                mutable.device_input_fifos.clear();
                #[cfg(unix)]
                {
                    mutable.microdroid_console_challenge = None;
                }
            }
        }
        transition_result?;
        finish_result?;
        if let Some(error) = cleanup_error {
            return Err(error);
        }
        Ok(())
    }

    async fn finish_run(
        &self,
        final_state: ObservedStateV2,
        exit_code: Option<i32>,
        error: Option<&WorkerError>,
    ) -> Result<(), WorkerError> {
        let mutable = self.mutable.lock().await;
        if let (Some(journal), Some(run_id), Some(started_at)) = (
            mutable.journal.as_ref(),
            mutable.status.run_id,
            mutable.started_at,
        ) {
            journal.finish(&RunResultV2 {
                schema_version: 2,
                run_id,
                instance_id: self.instance_id,
                started_at,
                finished_at: Some(OffsetDateTime::now_utc()),
                final_state,
                exit_code,
                error_code: error.map(|value| value.code().to_owned()),
                reason: error.map(ToString::to_string),
            })?;
        }
        Ok(())
    }

    async fn remove_finished_run_ephemeral_artifacts(&self) -> Result<(), WorkerError> {
        let run_dir = {
            let mutable = self.mutable.lock().await;
            mutable
                .journal
                .as_ref()
                .map(|journal| journal.run_dir().to_owned())
        };
        let Some(run_dir) = run_dir else {
            return Ok(());
        };
        for path in remove_finished_run_ephemeral_artifacts(&run_dir)? {
            tracing::info!(
                event = "runtime.run.ephemeral_artifact.removed",
                instance_id = %self.instance_id,
                path = %path.display(),
                "removed reproducible launch artifact from finalized run"
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
struct RuntimeEndpoints {
    control: String,
    frame: String,
    keyboard: String,
    trackpad: Option<String>,
    devices: BTreeMap<String, DeviceSerialEndpointV2>,
    device_controls: BTreeMap<String, String>,
    #[cfg(unix)]
    output_files: Vec<std::fs::File>,
    #[cfg(unix)]
    input_fifos: Vec<std::fs::File>,
}

fn read_bluetooth_hci_capture_record(
    run_dir: &Path,
    capture_id: Uuid,
    requested_duration_ms: u32,
) -> Result<BluetoothHciCaptureRecordV2, WorkerError> {
    let component_directory = run_dir.join("components");
    let file_name = format!("rootcanal-hci-{capture_id}.btsnoop");
    let metadata_name = format!("rootcanal-hci-{capture_id}.json");
    let capture_path = component_directory.join(&file_name);
    let metadata_path = component_directory.join(metadata_name);
    let capture_metadata = std::fs::symlink_metadata(&capture_path).map_err(|error| {
        WorkerError::ComponentContract(format!(
            "inspect Bluetooth HCI capture {}: {error}",
            capture_path.display()
        ))
    })?;
    let record_metadata = std::fs::symlink_metadata(&metadata_path).map_err(|error| {
        WorkerError::ComponentContract(format!(
            "inspect Bluetooth HCI capture metadata {}: {error}",
            metadata_path.display()
        ))
    })?;
    if !capture_metadata.is_file()
        || capture_metadata.file_type().is_symlink()
        || !record_metadata.is_file()
        || record_metadata.file_type().is_symlink()
        || record_metadata.len() > 64 * 1024
    {
        return Err(WorkerError::ComponentContract(
            "Bluetooth HCI capture outputs are not safe regular files".to_owned(),
        ));
    }
    let record: BluetoothHciCaptureRecordV2 =
        serde_json::from_slice(&std::fs::read(&metadata_path).map_err(|error| {
            WorkerError::ComponentContract(format!("read Bluetooth HCI capture metadata: {error}"))
        })?)
        .map_err(|error| {
            WorkerError::ComponentContract(format!(
                "decode Bluetooth HCI capture metadata: {error}"
            ))
        })?;
    if record.capture_id != capture_id
        || record.file_name != file_name
        || record.requested_duration_ms != requested_duration_ms
        || record.output_size_bytes != capture_metadata.len()
        || record.output_size_bytes < 16
        || record.output_size_bytes > MAX_BLUETOOTH_HCI_CAPTURE_BYTES
    {
        return Err(WorkerError::ComponentContract(
            "Bluetooth HCI capture metadata does not match the bounded output".to_owned(),
        ));
    }
    let mut file = std::fs::File::open(&capture_path).map_err(|error| {
        WorkerError::ComponentContract(format!("open Bluetooth HCI capture: {error}"))
    })?;
    let mut header = [0_u8; 16];
    file.read_exact(&mut header).map_err(|error| {
        WorkerError::ComponentContract(format!("read Bluetooth HCI capture header: {error}"))
    })?;
    if &header[..8] != b"btsnoop\0"
        || u32::from_be_bytes(header[8..12].try_into().expect("fixed header")) != 1
        || u32::from_be_bytes(header[12..16].try_into().expect("fixed header")) != 1_002
    {
        return Err(WorkerError::ComponentContract(
            "Bluetooth HCI capture is not a btsnoop HCI UART artifact".to_owned(),
        ));
    }
    Ok(record)
}

fn enabled_device_components(spec: &InstanceSpecV2) -> Vec<&'static str> {
    let mut components = Vec::new();
    if spec.devices.gnss || spec.devices.sensors || spec.devices.power || spec.devices.network {
        components.push("hd-device-sim");
    }
    #[cfg(target_os = "macos")]
    for (enabled, component) in [
        (spec.devices.bluetooth, "rootcanal-adapter"),
        (spec.devices.nfc, "casimir-adapter"),
        (spec.devices.uwb, "uwb-adapter"),
        (spec.devices.modem, "modem-adapter"),
    ] {
        if enabled {
            components.push(component);
        }
    }
    #[cfg(not(target_os = "macos"))]
    for (enabled, component) in [
        (spec.devices.bluetooth, "rootcanal-adapter"),
        (spec.devices.nfc, "casimir-adapter"),
        (spec.devices.uwb, "uwb-adapter"),
        (spec.devices.modem, "modem-adapter"),
    ] {
        if enabled {
            components.push(component);
        }
    }
    components
}

fn device_role_enabled(spec: &InstanceSpecV2, role: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        match role {
            "bluetooth" => spec.devices.bluetooth,
            "gnss" | "location" => spec.devices.gnss,
            "uwb" => spec.devices.uwb,
            "nfc" => spec.devices.nfc,
            "sensors" => spec.devices.sensors,
            "mcu-control" | "mcu-uart" => spec.devices.power,
            _ => false,
        }
    }
    #[cfg(not(target_os = "macos"))]
    match role {
        "bluetooth" => spec.devices.bluetooth,
        "gnss" | "location" => spec.devices.gnss,
        "uwb" => spec.devices.uwb,
        "nfc" => spec.devices.nfc,
        "sensors" => spec.devices.sensors,
        "mcu-control" | "mcu-uart" => spec.devices.power,
        "modem" => spec.devices.modem,
        _ => false,
    }
}

impl RuntimeEndpoints {
    fn create(spec: &InstanceSpecV2, run_id: Uuid) -> Result<Self, WorkerError> {
        let control = runtime_endpoint(spec.id, run_id, "vm-control", "sock")?;
        let frame = runtime_endpoint(spec.id, run_id, "frame", "sock")?;
        let keyboard = runtime_endpoint(spec.id, run_id, "keyboard", "sock")?;
        let trackpad = spec
            .devices
            .touchpad
            .then(|| runtime_endpoint(spec.id, run_id, "trackpad", "sock"))
            .transpose()?;
        let device_controls = enabled_device_components(spec)
            .into_iter()
            .map(|component| {
                runtime_endpoint(spec.id, run_id, &format!("{component}-control"), "sock")
                    .map(|endpoint| (component.to_owned(), endpoint))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let mut devices = BTreeMap::new();
        #[cfg(unix)]
        let mut output_files = Vec::new();
        #[cfg(unix)]
        let mut input_fifos = Vec::new();
        for role in DEVICE_GUEST_ENDPOINT_ROLES_V2 {
            if !device_role_enabled(spec, role) {
                continue;
            }
            let output = runtime_endpoint(spec.id, run_id, &format!("{role}-out"), "bin")?;
            let input = runtime_endpoint(spec.id, run_id, &format!("{role}-in"), "fifo")?;
            #[cfg(unix)]
            {
                let output_file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&output)
                    .map_err(|source| WorkerError::Io {
                        operation: "create device output file",
                        path: PathBuf::from(&output),
                        source,
                    })?;
                hd_platform::create_owner_only_fifo(Path::new(&input))?;
                let fifo = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&input)
                    .map_err(|source| WorkerError::Io {
                        operation: "hold device input FIFO open",
                        path: PathBuf::from(&input),
                        source,
                    })?;
                output_files.push(output_file);
                input_fifos.push(fifo);
            }
            devices.insert(
                role.to_owned(),
                DeviceSerialEndpointV2 {
                    guest_output: output,
                    guest_input: input,
                },
            );
        }
        Ok(Self {
            control,
            frame,
            keyboard,
            trackpad,
            devices,
            device_controls,
            #[cfg(unix)]
            output_files,
            #[cfg(unix)]
            input_fifos,
        })
    }
}

fn runtime_endpoint(
    instance_id: Uuid,
    run_id: Uuid,
    role: &str,
    suffix: &str,
) -> Result<String, WorkerError> {
    #[cfg(windows)]
    {
        let _ = suffix;
        let scope = hd_platform::current_user_scope()?;
        Ok(format!(
            r"\\.\pipe\bscp-hd-{scope}-{instance_id}-{run_id}-{role}"
        ))
    }
    #[cfg(unix)]
    {
        let root = runtime_root();
        let scope = hd_platform::current_user_scope()?;
        let instance = &instance_id.simple().to_string()[..12];
        let run = &run_id.simple().to_string()[..12];
        let path = root.join(format!("hd-{scope}-{instance}-{run}-{role}.{suffix}"));
        if path.as_os_str().len() >= 100 {
            return Err(WorkerError::EndpointTooLong(path));
        }
        Ok(path.to_string_lossy().into_owned())
    }
}

#[cfg(all(unix, target_os = "macos"))]
fn runtime_root() -> PathBuf {
    PathBuf::from("/tmp")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn runtime_root() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
}

#[allow(clippy::unnecessary_wraps)]
fn cleanup_runtime_endpoints(launch: &LaunchPlanV2) -> Result<(), WorkerError> {
    #[cfg(windows)]
    {
        let _ = launch;
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::collections::BTreeSet;
        use std::os::unix::fs::FileTypeExt as _;

        let root = runtime_root();
        let prefix = format!(
            "hd-{}-{}-{}-",
            hd_platform::current_user_scope()?,
            &launch.instance_id.simple().to_string()[..12],
            &launch.run_id.simple().to_string()[..12]
        );
        let mut endpoints = BTreeSet::from([
            launch.control_endpoint.clone(),
            launch.frame_endpoint.clone(),
            launch.keyboard_endpoint.clone(),
        ]);
        endpoints.extend(launch.trackpad_endpoint.iter().cloned());
        for endpoint in launch.device_endpoints.values() {
            endpoints.insert(endpoint.guest_output.clone());
            endpoints.insert(endpoint.guest_input.clone());
        }
        endpoints.extend(launch.device_control_endpoints.values().cloned());
        for endpoint in endpoints {
            let path = PathBuf::from(&endpoint);
            let valid_parent = path.parent().is_some_and(|parent| parent == root);
            let valid_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix));
            if !path.is_absolute() || !valid_parent || !valid_name {
                return Err(WorkerError::UnsafeEndpoint(path));
            }
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(WorkerError::Io {
                        operation: "inspect runtime endpoint",
                        path,
                        source,
                    });
                }
            };
            if !(metadata.file_type().is_socket()
                || metadata.file_type().is_fifo()
                || metadata.file_type().is_file())
            {
                return Err(WorkerError::UnsafeEndpoint(path));
            }
            std::fs::remove_file(&path).map_err(|source| WorkerError::Io {
                operation: "remove runtime endpoint",
                path,
                source,
            })?;
        }
        Ok(())
    }
}

fn dev_boot_completed_from_logs(run_dir: &Path) -> Result<bool, WorkerError> {
    for name in ["console-hvc0.txt", "logcat-hvc2.txt"] {
        let path = run_dir.join(name);
        if !path.is_file() {
            continue;
        }
        let text = read_log_tail_lossy(&path)?;
        if text.contains("sys.boot_completed=1")
            || text.contains("Posting BOOT_COMPLETED")
            || text.contains("Finished processing BOOT_COMPLETED")
            || text.contains("BOOT_COMPLETED_BROADCAST_COMPLETION_LATENCY_REPORTED")
        {
            tracing::warn!(
                event = "worker.dev.boot_log_ready",
                path = %path.display(),
                "dev boot-log readiness fallback observed boot completion"
            );
            return Ok(true);
        }
    }
    Ok(false)
}

fn adbd_started_from_logs(run_dir: &Path) -> Result<bool, WorkerError> {
    for name in ["console-hvc0.txt", "logcat-hvc2.txt"] {
        let path = run_dir.join(name);
        if !path.is_file() {
            continue;
        }
        let text = read_log_tail_lossy(&path)?;
        if text.contains("adbd started")
            || text.contains("adbd listening on tcp:5555")
            || text.contains("adbd listening on vsock:5555")
            || text.contains("authentication not required")
        {
            tracing::info!(
                event = "worker.adbd.log_ready",
                path = %path.display(),
                "observed adbd startup before strict ADB connect"
            );
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_log_tail_lossy(path: &Path) -> Result<String, WorkerError> {
    let mut file = std::fs::File::open(path).map_err(|source| WorkerError::Io {
        operation: "open dev readiness log",
        path: path.to_owned(),
        source,
    })?;
    let len = file
        .metadata()
        .map_err(|source| WorkerError::Io {
            operation: "inspect dev readiness log",
            path: path.to_owned(),
            source,
        })?
        .len();
    if len > DEV_BOOT_LOG_SCAN_LIMIT {
        file.seek(SeekFrom::Start(len - DEV_BOOT_LOG_SCAN_LIMIT))
            .map_err(|source| WorkerError::Io {
                operation: "seek dev readiness log",
                path: path.to_owned(),
                source,
            })?;
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(len.min(DEV_BOOT_LOG_SCAN_LIMIT)).unwrap_or_default());
    file.take(DEV_BOOT_LOG_SCAN_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(|source| WorkerError::Io {
            operation: "read dev readiness log",
            path: path.to_owned(),
            source,
        })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

enum MicrodroidExitDisposition {
    Completed(i32),
    Failed(WorkerError),
}

enum MicrodroidPayloadReadiness {
    Ready,
    Completed(i32),
}

/// The AOSP `vm` launcher can exit while descendants that inherited its redirected stdout/stderr
/// handles are still closing. On Windows, observing the process exit is therefore not sufficient
/// to prove that the final callback and `VM ended` lines are already visible to a second reader.
/// Keep the evidence rules strict, but give the authoritative launcher logs a short bounded window
/// to settle before falling back to the generic `guest_exited` classification.
async fn classify_microdroid_exit_disposition_after_log_settle(
    run_dir: &Path,
    exit_code: Option<i32>,
) -> MicrodroidExitDisposition {
    const LOG_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
    const LOG_SETTLE_INTERVAL: Duration = Duration::from_millis(25);

    let started = tokio::time::Instant::now();
    loop {
        let disposition = classify_microdroid_exit_disposition(run_dir, exit_code);
        let evidence_incomplete = matches!(
            &disposition,
            MicrodroidExitDisposition::Failed(WorkerError::GuestExited(_))
        );
        if !evidence_incomplete || started.elapsed() >= LOG_SETTLE_TIMEOUT {
            return disposition;
        }
        tokio::time::sleep(LOG_SETTLE_INTERVAL).await;
    }
}

fn classify_microdroid_exit_disposition(
    run_dir: &Path,
    exit_code: Option<i32>,
) -> MicrodroidExitDisposition {
    let mut logs = String::new();
    for name in [
        "microdroid.stdout.log",
        "microdroid.stderr.log",
        "microdroid-guest.log",
        "microdroid-virtmgr-trace.log",
        "microdroid-vmclient-trace.log",
    ] {
        let path = run_dir.join(name);
        if path.is_file()
            && let Ok(text) = read_log_tail_lossy(&path)
        {
            logs.push_str(&text);
            logs.push('\n');
        }
    }
    if let Some(error) = classify_microdroid_failure_logs(&logs) {
        return MicrodroidExitDisposition::Failed(error);
    }
    match inspect_microdroid_launcher_completion(run_dir, exit_code) {
        Ok(Some(MicrodroidLauncherCompletion::Completed { payload_exit_code })) => {
            MicrodroidExitDisposition::Completed(payload_exit_code)
        }
        Ok(Some(MicrodroidLauncherCompletion::PayloadFailed { payload_exit_code })) => {
            MicrodroidExitDisposition::Failed(WorkerError::MicrodroidPayloadFailed(
                payload_exit_code,
            ))
        }
        Ok(None) | Err(_) => MicrodroidExitDisposition::Failed(WorkerError::GuestExited(exit_code)),
    }
}

#[cfg(test)]
fn classify_microdroid_exit_logs(logs: &str, exit_code: Option<i32>) -> WorkerError {
    classify_microdroid_failure_logs(logs).unwrap_or(WorkerError::GuestExited(exit_code))
}

fn classify_microdroid_failure_logs(logs: &str) -> Option<WorkerError> {
    // microdroid_manager reports a changed payload underneath the generic
    // "Payload verification has failed" wrapper. Match the specific cause first so the UI
    // does not incorrectly tell the user to replace a valid APK signature/idsig.
    if [
        "MicrodroidPayloadHasChanged",
        "PayloadChanged",
        "MICRODROID_PAYLOAD_HAS_CHANGED",
        "PAYLOAD_CHANGED",
        "Payload has changed",
        "APEXes have changed",
    ]
    .iter()
    .any(|marker| logs.contains(marker))
    {
        Some(WorkerError::MicrodroidPayloadChanged)
    } else if [
        "MicrodroidPayloadVerificationFailed",
        "PayloadVerificationFailed",
        "MICRODROID_PAYLOAD_VERIFICATION_FAILED",
        "PAYLOAD_VERIFICATION_FAILED",
        "Payload verification failed",
        "Payload verification has failed",
    ]
    .iter()
    .any(|marker| logs.contains(marker))
    {
        Some(WorkerError::MicrodroidPayloadVerificationFailed)
    } else if [
        "MicrodroidInvalidPayloadConfig",
        "PayloadInvalidConfig",
        "MICRODROID_INVALID_PAYLOAD_CONFIG",
        "PAYLOAD_INVALID_CONFIG",
    ]
    .iter()
    .any(|marker| logs.contains(marker))
    {
        Some(WorkerError::MicrodroidInvalidPayloadConfig)
    } else if [
        "MicrodroidFailedToConnectToVirtualizationService",
        "MICRODROID_FAILED_TO_CONNECT_TO_VIRTUALIZATION_SERVICE",
    ]
    .iter()
    .any(|marker| logs.contains(marker))
    {
        Some(WorkerError::MicrodroidServiceConnectionFailed)
    } else if [
        "MicrodroidUnknownRuntimeError",
        "MICRODROID_UNKNOWN_RUNTIME_ERROR",
        "VirtualizationServiceDied",
        "InfrastructureError",
        "StartFailed",
        "WatchdogReboot",
        "Unrecognised",
        "VM ended: Crash",
        "VM ended: Hangup",
        "VM ended: Killed",
        "reason=Crash",
        "reason=Hangup",
        "reason=Killed",
    ]
    .iter()
    .any(|marker| logs.contains(marker))
    {
        Some(WorkerError::MicrodroidRuntimeFailed)
    } else {
        None
    }
}

fn validate_start_leases(
    leases: &[LeaseV2],
    identity: &WorkerIdentityV2,
    spec: &InstanceSpecV2,
    paths: &DataPaths,
    worker_endpoint: &str,
) -> Result<u64, WorkerError> {
    let instance_id = spec.id;
    validate_lease_set_shape(leases, identity, spec)?;
    if lease_resource(leases, LeaseKindV2::CpuCapacity)?
        != format!("{instance_id}:{}", spec.cpu_count)
        || lease_resource(leases, LeaseKindV2::MemoryBytes)?
            != format!(
                "{instance_id}:{}",
                u64::from(spec.memory_mib).saturating_mul(1024 * 1024)
            )
    {
        return Err(WorkerError::LeaseContract(
            "CPU or memory lease does not exactly match the instance specification".to_owned(),
        ));
    }
    if spec.guest_kind == GuestKindV2::Android {
        let disk = lease_resource(leases, LeaseKindV2::DiskOverlay)?;
        if disk != paths.disk_overlay(instance_id).to_string_lossy() {
            return Err(WorkerError::LeaseContract(
                "disk overlay lease does not match the instance path".to_owned(),
            ));
        }
        let _gpu_slot = lease_number::<u32>(leases, LeaseKindV2::GpuSlot)?;
    }
    if lease_resource(leases, LeaseKindV2::WorkerEndpoint)? != worker_endpoint {
        return Err(WorkerError::LeaseContract(
            "worker endpoint lease does not match this worker".to_owned(),
        ));
    }
    let guest_cid = lease_number::<u32>(leases, LeaseKindV2::GuestCid)?;
    if !(3..=i32::MAX as u32).contains(&guest_cid) {
        return Err(WorkerError::LeaseInvalid(LeaseKindV2::GuestCid));
    }
    if let Some(port) = spec.adb.host_port
        && lease_number::<u16>(leases, LeaseKindV2::AdbPort)? != port
    {
        return Err(WorkerError::LeaseContract(
            "ADB port lease does not match the explicit configured port".to_owned(),
        ));
    }
    let generation_lease = leases
        .iter()
        .find(|lease| lease.kind == LeaseKindV2::FrameGeneration)
        .ok_or(WorkerError::LeaseMissing(LeaseKindV2::FrameGeneration))?;
    let frame_generation = lease_number::<u64>(leases, LeaseKindV2::FrameGeneration)?;
    if frame_generation == 0 || generation_lease.generation != frame_generation {
        return Err(WorkerError::LeaseInvalid(LeaseKindV2::FrameGeneration));
    }
    Ok(frame_generation)
}

fn validate_lease_set_shape(
    leases: &[LeaseV2],
    identity: &WorkerIdentityV2,
    spec: &InstanceSpecV2,
) -> Result<(), WorkerError> {
    let instance_id = spec.id;
    let expected_devices = crate::leases::enabled_device_names(spec)
        .into_iter()
        .map(|name| format!("{instance_id}:{name}"))
        .collect::<BTreeSet<_>>();
    let required_kinds = if spec.guest_kind == GuestKindV2::Android {
        &[
            LeaseKindV2::CpuCapacity,
            LeaseKindV2::MemoryBytes,
            LeaseKindV2::GuestCid,
            LeaseKindV2::DiskOverlay,
            LeaseKindV2::GpuSlot,
            LeaseKindV2::WorkerEndpoint,
            LeaseKindV2::FrameGeneration,
        ][..]
    } else {
        &[
            LeaseKindV2::CpuCapacity,
            LeaseKindV2::MemoryBytes,
            LeaseKindV2::GuestCid,
            LeaseKindV2::WorkerEndpoint,
            LeaseKindV2::FrameGeneration,
        ][..]
    };
    let expected_count = required_kinds.len()
        + usize::from(matches!(spec.adb.mode, AdbModeV2::Loopback))
        + expected_devices.len();
    if leases.len() != expected_count {
        return Err(WorkerError::LeaseContract(format!(
            "expected {expected_count} leases, received {}",
            leases.len()
        )));
    }
    let mut ids = BTreeSet::new();
    let mut counts = BTreeMap::<LeaseKindV2, usize>::new();
    for &kind in required_kinds {
        let count = leases.iter().filter(|lease| lease.kind == kind).count();
        if count == 0 {
            return Err(WorkerError::LeaseMissing(kind));
        }
        if count != 1 {
            return Err(WorkerError::LeaseContract(format!(
                "lease kind {kind:?} occurs {count} times"
            )));
        }
    }
    for lease in leases {
        if !ids.insert(lease.id) {
            return Err(WorkerError::LeaseContract(format!(
                "duplicate lease id {}",
                lease.id
            )));
        }
        *counts.entry(lease.kind).or_default() += 1;
        if lease.owner.instance_id != instance_id
            || lease.owner.worker_nonce != Some(identity.nonce)
            || lease.owner.pid != Some(identity.pid)
            || lease.owner.process_start_marker.as_deref()
                != Some(identity.process_start_marker.as_str())
        {
            return Err(WorkerError::LeaseOwnerMismatch(lease.id));
        }
        if lease.last_verified_at < lease.acquired_at {
            return Err(WorkerError::LeaseContract(format!(
                "lease {} verification time predates acquisition",
                lease.id
            )));
        }
        if lease.kind != LeaseKindV2::FrameGeneration && lease.generation != 1 {
            return Err(WorkerError::LeaseContract(format!(
                "lease {} has an unsupported generation",
                lease.id
            )));
        }
    }
    let adb_count = counts.get(&LeaseKindV2::AdbPort).copied().unwrap_or(0);
    let expected_adb_count = usize::from(matches!(spec.adb.mode, AdbModeV2::Loopback));
    if adb_count != expected_adb_count {
        return Err(WorkerError::LeaseContract(
            "ADB lease does not match the configured mode".to_owned(),
        ));
    }
    let actual_devices = leases
        .iter()
        .filter(|lease| lease.kind == LeaseKindV2::DeviceEndpoint)
        .map(|lease| lease.resource.clone())
        .collect::<BTreeSet<_>>();
    if actual_devices != expected_devices {
        return Err(WorkerError::LeaseContract(
            "device endpoint leases do not exactly match enabled devices".to_owned(),
        ));
    }
    Ok(())
}

fn lease_resource(leases: &[LeaseV2], kind: LeaseKindV2) -> Result<&str, WorkerError> {
    leases
        .iter()
        .find(|lease| lease.kind == kind)
        .map(|lease| lease.resource.as_str())
        .ok_or(WorkerError::LeaseMissing(kind))
}

fn lease_number<T>(leases: &[LeaseV2], kind: LeaseKindV2) -> Result<T, WorkerError>
where
    T: std::str::FromStr,
{
    leases
        .iter()
        .find(|lease| lease.kind == kind)
        .ok_or(WorkerError::LeaseMissing(kind))?
        .resource
        .parse()
        .map_err(|_| WorkerError::LeaseInvalid(kind))
}

fn verify_upload_digest(path: &Path, expected: &str) -> Result<(), WorkerError> {
    let actual = crate::sha256_file(path)?;
    if actual != expected {
        return Err(WorkerError::UploadDigestMismatch {
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(())
}

fn validate_existing_microdroid_storage(path: &Path, size_mib: u32) -> Result<(), WorkerError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(WorkerError::Io {
                operation: "inspect Microdroid encrypted storage",
                path: path.to_owned(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(WorkerError::ComponentContract(
            "Microdroid encrypted storage must be a regular non-symlink file".to_owned(),
        ));
    }
    let expected = u64::from(size_mib).saturating_mul(1024 * 1024);
    if metadata.len() != expected {
        return Err(WorkerError::ComponentContract(format!(
            "existing Microdroid encrypted storage is {} bytes but the instance requires {expected}; automatic resize is unsupported",
            metadata.len()
        )));
    }
    Ok(())
}

fn validate_screen_recording_mp4(path: &Path) -> Result<(u64, String), WorkerError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| WorkerError::Io {
        operation: "inspect Android screen recording",
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() || metadata.len() < 12 {
        return Err(WorkerError::Task(
            "Android screen recording did not produce a regular MP4 artifact".to_owned(),
        ));
    }
    let mut file = std::fs::File::open(path).map_err(|source| WorkerError::Io {
        operation: "open Android screen recording",
        path: path.to_owned(),
        source,
    })?;
    let mut header = [0_u8; 12];
    file.read_exact(&mut header)
        .map_err(|source| WorkerError::Io {
            operation: "read Android screen recording header",
            path: path.to_owned(),
            source,
        })?;
    if &header[4..8] != b"ftyp" {
        return Err(WorkerError::Task(
            "Android screen recording artifact is not an ISO base media MP4".to_owned(),
        ));
    }
    let mut offset = 0_u64;
    let mut has_media_data = false;
    let mut has_movie_metadata = false;
    while offset < metadata.len() {
        if metadata.len().saturating_sub(offset) < 8 {
            return Err(WorkerError::Task(
                "Android screen recording MP4 has a truncated top-level box".to_owned(),
            ));
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|source| WorkerError::Io {
                operation: "seek Android screen recording MP4",
                path: path.to_owned(),
                source,
            })?;
        let mut box_header = [0_u8; 8];
        file.read_exact(&mut box_header)
            .map_err(|source| WorkerError::Io {
                operation: "read Android screen recording MP4 box",
                path: path.to_owned(),
                source,
            })?;
        let compact_size =
            u32::from_be_bytes([box_header[0], box_header[1], box_header[2], box_header[3]]);
        let (box_size, header_size) = match compact_size {
            0 => (metadata.len() - offset, 8_u64),
            1 => {
                let mut extended_size = [0_u8; 8];
                file.read_exact(&mut extended_size)
                    .map_err(|source| WorkerError::Io {
                        operation: "read Android screen recording MP4 extended box",
                        path: path.to_owned(),
                        source,
                    })?;
                (u64::from_be_bytes(extended_size), 16_u64)
            }
            value => (u64::from(value), 8_u64),
        };
        if box_size < header_size || box_size > metadata.len() - offset {
            return Err(WorkerError::Task(
                "Android screen recording MP4 has an invalid top-level box size".to_owned(),
            ));
        }
        match &box_header[4..8] {
            b"mdat" if box_size > header_size => has_media_data = true,
            b"moov" if box_size > header_size => has_movie_metadata = true,
            _ => {}
        }
        offset += box_size;
    }
    if !has_media_data || !has_movie_metadata {
        return Err(WorkerError::Task(
            "Android screen recording MP4 is incomplete (missing media data or movie metadata)"
                .to_owned(),
        ));
    }
    let sha256 = crate::sha256_file(path)?;
    Ok((metadata.len(), sha256))
}

const fn microdroid_graceful_shutdown_available(
    guest_kind: GuestKindV2,
    adb_ready: bool,
    has_adb_client: bool,
    has_adb_serial: bool,
) -> bool {
    !matches!(guest_kind, GuestKindV2::Microdroid)
        || (adb_ready && has_adb_client && has_adb_serial)
}

async fn create_microdroid_idsig(
    instance_id: Uuid,
    run_id: Uuid,
    vm: &Path,
    apk: &Path,
    idsig: &Path,
    environment: &BTreeMap<String, String>,
    run_dir: &Path,
) -> Result<(), WorkerError> {
    let started = Instant::now();
    tracing::info!(
        event = "microdroid.idsig.creation.started",
        %instance_id,
        %run_id,
        "creating Microdroid payload idsig"
    );
    if let Some(parent) = idsig.parent() {
        hd_platform::ensure_owner_only_directory(parent)?;
    }
    let stdout_path = run_dir.join("microdroid-idsig.stdout.log");
    let stderr_path = run_dir.join("microdroid-idsig.stderr.log");
    let stdout = hd_platform::open_owner_only_rw(&stdout_path)?;
    stdout.set_len(0).map_err(|source| WorkerError::Io {
        operation: "truncate Microdroid idsig stdout",
        path: stdout_path.clone(),
        source,
    })?;
    let stderr = hd_platform::open_owner_only_rw(&stderr_path)?;
    stderr.set_len(0).map_err(|source| WorkerError::Io {
        operation: "truncate Microdroid idsig stderr",
        path: stderr_path.clone(),
        source,
    })?;
    let mut command = tokio::process::Command::new(vm);
    command
        .args(["create-idsig"])
        .arg(apk)
        .arg(idsig)
        .envs(environment)
        .current_dir(run_dir)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|source| WorkerError::Io {
        operation: "start Microdroid payload idsig creation",
        path: vm.to_owned(),
        source,
    })?;
    let status =
        if let Ok(result) = tokio::time::timeout(MICRODROID_IDSIG_TIMEOUT, child.wait()).await {
            result.map_err(|source| WorkerError::Io {
                operation: "wait for Microdroid payload idsig creation",
                path: vm.to_owned(),
                source,
            })?
        } else {
            let _ = child.kill().await;
            tracing::error!(
                event = "microdroid.idsig.creation.failed",
                error_code = "microdroid_idsig_timeout",
                %instance_id,
                %run_id,
                duration_ms = elapsed_ms(started),
                "Microdroid payload idsig creation timed out"
            );
            return Err(WorkerError::ComponentContract(
                "Microdroid idsig creation timed out".to_owned(),
            ));
        };
    if !status.success() {
        let stderr = hd_platform::read_regular_nofollow_limited(&stderr_path, 1024 * 1024)
            .unwrap_or_else(|error| format!("read idsig stderr failed: {error}").into_bytes());
        tracing::error!(
            event = "microdroid.idsig.creation.failed",
            error_code = "microdroid_idsig_failed",
            %instance_id,
            %run_id,
            duration_ms = elapsed_ms(started),
            exit_code = status.code(),
            "Microdroid payload idsig creation failed"
        );
        return Err(WorkerError::ComponentContract(format!(
            "Microdroid idsig creation failed with {:?}: {}",
            status.code(),
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    tracing::info!(
        event = "microdroid.idsig.creation.succeeded",
        %instance_id,
        %run_id,
        duration_ms = elapsed_ms(started),
        "Microdroid payload idsig created"
    );
    Ok(())
}

fn toolchain_fingerprint(crosvm: Option<&Path>) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "hd_version".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        ),
        (
            "target".to_owned(),
            format!(
                "{}-{}-{}",
                std::env::consts::ARCH,
                std::env::consts::OS,
                if cfg!(target_env = "gnu") {
                    "gnu"
                } else {
                    "native"
                }
            ),
        ),
        (
            "crosvm".to_owned(),
            crosvm.map_or_else(
                || "unresolved".to_owned(),
                |path| path.display().to_string(),
            ),
        ),
    ])
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn random_device_control_token() -> Result<DeviceControlTokenV2, WorkerError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| WorkerError::Random(error.to_string()))?;
    DeviceControlTokenV2::from_hex(hex::encode(bytes))
        .ok_or_else(|| WorkerError::Random("generated an invalid device control token".to_owned()))
}

fn acquire_worker_instance_lock(
    paths: &DataPaths,
    instance_id: Uuid,
) -> Result<std::fs::File, WorkerError> {
    let path = paths.worker_lock(instance_id);
    let file = hd_platform::open_owner_only_rw(&path)?;
    file.try_lock_exclusive()
        .map_err(|source| WorkerError::Io {
            operation: "lock worker instance",
            path,
            source,
        })?;
    Ok(file)
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("worker secret must be exactly 64 hexadecimal bytes")]
    SecretInvalid,
    #[error("secure random generation failed: {0}")]
    Random(String),
    #[error(transparent)]
    Action(#[from] hd_core::ActionValidationError),
    #[error("request instance does not match this worker")]
    InstanceMismatch,
    #[error("worker is busy: {0}")]
    Busy(&'static str),
    #[error("operation is unsupported for this guest type: {0}")]
    Unsupported(&'static str),
    #[error("instance is not running")]
    NotRunning,
    #[error("instance is not Ready")]
    NotReady,
    #[error("configuration change requires restart")]
    RestartRequired,
    #[error("ADB-disabled instances have no authenticated readiness probe")]
    ReadinessUnavailable,
    #[error("ADB transport has not reached authenticated Android readiness yet")]
    AdbNotReady,
    #[error(
        "host capabilities changed between reservation and worker launch: {expected} != {actual}"
    )]
    CapabilityChanged { expected: String, actual: String },
    #[error("required host capabilities are blocked: {0:?}")]
    CapabilityBlocked(Vec<String>),
    #[error("required lease is missing: {0:?}")]
    LeaseMissing(LeaseKindV2),
    #[error("lease has an invalid numeric value: {0:?}")]
    LeaseInvalid(LeaseKindV2),
    #[error("lease {0} is not bound to this worker identity")]
    LeaseOwnerMismatch(Uuid),
    #[error("lease set violates the exact start contract: {0}")]
    LeaseContract(String),
    #[error("strict frame handshake failed: {0}")]
    FrameHandshake(String),
    #[error("formal component contract failed: {0}")]
    ComponentContract(String),
    #[error("formal component {0} did not publish its exact ready marker before timeout")]
    ComponentTimeout(String),
    #[error("formal component {component} exited unexpectedly with code {code:?}")]
    ComponentExited {
        component: String,
        code: Option<i32>,
    },
    #[error("formal component cleanup could not be proven: {0}")]
    ComponentCleanup(String),
    #[error("guest process exited unexpectedly with code {0:?}")]
    GuestExited(Option<i32>),
    #[error("Microdroid Payload exited with nonzero code {0}")]
    MicrodroidPayloadFailed(i32),
    #[error(
        "Microdroid rejected the Payload APK signature or idsig; use an Android 15 APK Signature Scheme v3-signed Payload and upload it again"
    )]
    MicrodroidPayloadVerificationFailed,
    #[error(
        "Microdroid detected that the Payload changed for this instance; restore the original Payload or recreate the instance"
    )]
    MicrodroidPayloadChanged,
    #[error(
        "Microdroid rejected assets/vm_config.json; verify the task type, command, APEX list and extra APK declarations"
    )]
    MicrodroidInvalidPayloadConfig,
    #[error(
        "Microdroid Payload declares {declared} extra APKs but this instance selected {selected}; add or remove extra APKs in the declared order"
    )]
    MicrodroidExtraApkCountMismatch { declared: usize, selected: usize },
    #[error(
        "Microdroid could not connect to the host VirtualizationService; verify that the packaged host tools and guest image are from the certified bundle"
    )]
    MicrodroidServiceConnectionFailed,
    #[error("Microdroid manager reported a runtime failure; collect diagnostics for the Guest log")]
    MicrodroidRuntimeFailed,
    #[error("device endpoint is unavailable for role {0}")]
    DeviceEndpoint(String),
    #[error("device action is unsupported by this host: {0}")]
    DeviceActionUnsupported(&'static str),
    #[error("device request was rejected: {0}")]
    DeviceRejected(String),
    #[error(
        "runtime disk is below the start low-watermark: {available} bytes available, {required} required"
    )]
    DiskLowWatermark { available: u64, required: u64 },
    #[error("display session failed: {0}")]
    DisplaySession(String),
    #[error("host screen recorder failed: {0}")]
    HostRecorder(String),
    #[error("APK digest mismatch: expected {expected}, actual {actual}")]
    UploadDigestMismatch { expected: String, actual: String },
    #[error("runtime endpoint path is too long: {0}")]
    EndpointTooLong(PathBuf),
    #[error("refusing to access an unexpected runtime endpoint: {0}")]
    UnsafeEndpoint(PathBuf),
    #[error("background task failed: {0}")]
    Task(String),
    #[error("invalid state transition {previous:?} -> {next:?}")]
    StateTransition {
        previous: ObservedStateV2,
        next: ObservedStateV2,
    },
    #[error(transparent)]
    Config(#[from] hd_core::ConfigError),
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
    #[error(transparent)]
    Journal(#[from] crate::JournalError),
    #[error(transparent)]
    Artifacts(#[from] crate::ArtifactError),
    #[error(transparent)]
    Adb(#[from] crate::AdbError),
    #[error(transparent)]
    DeviceIpc(#[from] crate::DeviceIpcError),
    #[error(transparent)]
    Storage(#[from] crate::RuntimeStorageError),
    #[cfg(unix)]
    #[error(transparent)]
    MicrodroidConsoleChallenge(#[from] MicrodroidConsoleChallengeError),
    #[error("JSON operation failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl WorkerError {
    const fn blocks_start(&self) -> bool {
        matches!(
            self,
            Self::ReadinessUnavailable
                | Self::CapabilityChanged { .. }
                | Self::CapabilityBlocked(_)
                | Self::FrameHandshake(_)
                | Self::ComponentContract(_)
                | Self::MicrodroidPayloadVerificationFailed
                | Self::MicrodroidPayloadChanged
                | Self::MicrodroidInvalidPayloadConfig
                | Self::MicrodroidExtraApkCountMismatch { .. }
                | Self::MicrodroidServiceConnectionFailed
                | Self::Artifacts(_)
        )
    }

    pub const fn code(&self) -> &'static str {
        match self {
            Self::SecretInvalid => "worker_secret_invalid",
            Self::Random(_) => "secure_random",
            Self::Action(_) => "action_invalid",
            Self::InstanceMismatch => "worker_instance_mismatch",
            Self::Busy(_) => "worker_busy",
            Self::Unsupported(_) => "unsupported",
            Self::NotRunning => "worker_not_running",
            Self::NotReady => "worker_not_ready",
            Self::RestartRequired => "restart_required",
            Self::ReadinessUnavailable => "readiness_unavailable",
            Self::AdbNotReady => "adb_not_ready",
            Self::CapabilityChanged { .. } => "capability_changed",
            Self::CapabilityBlocked(_) => "capability_blocked",
            Self::LeaseMissing(_) => "lease_missing",
            Self::LeaseInvalid(_) => "lease_invalid",
            Self::LeaseOwnerMismatch(_) => "lease_owner_mismatch",
            Self::LeaseContract(_) => "lease_contract",
            Self::FrameHandshake(_) => "frame_handshake",
            Self::ComponentContract(_) => "component_contract",
            Self::ComponentTimeout(_) => "component_timeout",
            Self::ComponentExited { .. } => "component_exited",
            Self::ComponentCleanup(_) => "component_cleanup",
            Self::GuestExited(_) => "guest_exited",
            Self::MicrodroidPayloadFailed(_) => "microdroid_payload_failed",
            Self::MicrodroidPayloadVerificationFailed => "microdroid_payload_verification_failed",
            Self::MicrodroidPayloadChanged => "microdroid_payload_changed",
            Self::MicrodroidInvalidPayloadConfig => "microdroid_invalid_payload_config",
            Self::MicrodroidExtraApkCountMismatch { .. } => "microdroid_extra_apk_count_mismatch",
            Self::MicrodroidServiceConnectionFailed => "microdroid_service_connection_failed",
            Self::MicrodroidRuntimeFailed => "microdroid_runtime_failed",
            Self::DeviceEndpoint(_) => "device_endpoint",
            Self::DeviceActionUnsupported(_) => "device_action_unsupported",
            Self::DeviceRejected(_) => "device_rejected",
            Self::DiskLowWatermark { .. } => "disk_low_watermark",
            Self::DisplaySession(_) => "display_session",
            Self::HostRecorder(_) => "host_screen_recorder",
            Self::UploadDigestMismatch { .. } => "upload_digest_mismatch",
            Self::EndpointTooLong(_) => "endpoint_too_long",
            Self::UnsafeEndpoint(_) => "unsafe_endpoint",
            Self::Task(_) => "task_failed",
            Self::StateTransition { .. } => "state_transition",
            Self::Config(_) => "config",
            Self::Platform(_) => "platform",
            Self::Journal(_) => "journal",
            Self::Artifacts(_) => "artifacts",
            Self::Adb(_) => "adb",
            Self::DeviceIpc(_) => "device_ipc",
            Self::Storage(_) => "runtime_storage",
            #[cfg(unix)]
            Self::MicrodroidConsoleChallenge(error) => error.code(),
            Self::Json(_) => "json",
            Self::Io { .. } => "io",
        }
    }

    pub fn api_error(&self) -> ApiErrorV2 {
        ApiErrorV2::new(self.code(), self.to_string()).retryable(matches!(
            self,
            Self::Busy(_)
                | Self::AdbNotReady
                | Self::CapabilityChanged { .. }
                | Self::DeviceIpc(_)
                | Self::HostRecorder(_)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_only_configuration_starts_the_device_simulator() {
        let mut spec = InstanceSpecV2::default();
        spec.devices.bluetooth = false;
        spec.devices.nfc = false;
        spec.devices.uwb = false;
        spec.devices.modem = false;
        spec.devices.gnss = false;
        spec.devices.sensors = false;
        spec.devices.audio = false;
        spec.devices.camera = false;
        spec.devices.power = false;
        spec.devices.network = true;
        assert_eq!(enabled_device_components(&spec), vec!["hd-device-sim"]);
    }

    #[test]
    fn unsupported_device_action_has_a_stable_api_code() {
        assert_eq!(
            WorkerError::DeviceActionUnsupported("contract").code(),
            "device_action_unsupported"
        );
    }

    #[test]
    fn microdroid_death_reasons_have_actionable_stable_codes() {
        let cases = [
            (
                "VM ended: MicrodroidPayloadVerificationFailed",
                "microdroid_payload_verification_failed",
            ),
            (
                "MICRODROID_PAYLOAD_HAS_CHANGED",
                "microdroid_payload_changed",
            ),
            (
                "VM ended unexpectedly: MicrodroidInvalidPayloadConfig",
                "microdroid_invalid_payload_config",
            ),
            (
                "MICRODROID_FAILED_TO_CONNECT_TO_VIRTUALIZATION_SERVICE",
                "microdroid_service_connection_failed",
            ),
            (
                "MICRODROID_UNKNOWN_RUNTIME_ERROR",
                "microdroid_runtime_failed",
            ),
            ("VirtualizationServiceDied", "microdroid_runtime_failed"),
        ];
        for (logs, expected) in cases {
            assert_eq!(
                classify_microdroid_exit_logs(logs, Some(0)).code(),
                expected
            );
        }
        assert!(matches!(
            classify_microdroid_exit_logs("unclassified failure", Some(17)),
            WorkerError::GuestExited(Some(17))
        ));
    }

    #[test]
    fn microdroid_graceful_shutdown_requires_a_ready_adb_channel() {
        assert!(!microdroid_graceful_shutdown_available(
            GuestKindV2::Microdroid,
            false,
            false,
            false
        ));
        assert!(microdroid_graceful_shutdown_available(
            GuestKindV2::Microdroid,
            true,
            true,
            true
        ));
        assert!(microdroid_graceful_shutdown_available(
            GuestKindV2::Android,
            false,
            false,
            false
        ));
    }

    #[test]
    fn a_foreign_lease_owner_is_rejected() {
        let temporary = tempfile::tempdir().expect("temporary data root");
        let temporary_root = temporary
            .path()
            .canonicalize()
            .expect("canonical temp root");
        let paths = DataPaths::from_root(temporary_root.join("data"));
        paths.ensure().expect("data paths");
        let store = crate::PersistentStore::open(&paths.database()).expect("store");
        let manager = crate::LeaseManager::new(store, paths.clone()).expect("lease manager");
        let spec = InstanceSpecV2::default();
        let identity = WorkerIdentityV2 {
            pid: 1,
            process_start_marker: "one".to_owned(),
            nonce: Uuid::nil(),
        };
        manager
            .reserve_start(&spec, None, 1)
            .expect("reserve leases");
        let mut leases = manager
            .bind_worker_identity(spec.id, &identity)
            .expect("bind identity");
        leases[0].owner.instance_id = Uuid::new_v4();
        let endpoint = crate::worker_endpoint(spec.id).expect("worker endpoint");
        assert!(validate_start_leases(&leases, &identity, &spec, &paths, &endpoint).is_err());
    }
}
