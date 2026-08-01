use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Seek as _, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs2::FileExt as _;
use hd_core::{
    AdbModeV2, ApiErrorV2, COMPONENT_PROTOCOL_VERSION, DEVICE_GUEST_ENDPOINT_ROLES_V2,
    DeviceControlCommandV2, DeviceControlRequestV2, DeviceControlTokenV2, DeviceSerialEndpointV2,
    DisplayConfigV2, DisplayViewportV2, FRAME_PROTOCOL_VERSION, FormalComponentConfigurationV2,
    FormalComponentLaunchV2, FormalComponentReadyV2, FrameMetricsV2, FrameReadyMarkerV2,
    InstanceActionV2, InstanceSpecV2, KeyActionV2, LaunchPlanV2, LeaseKindV2, LeaseV2,
    NativeDisplayTargetV2, ObservedStateV2, PreparedNativeDisplayV2, RunManifestV2, RunResultV2,
    ScreenshotRecordV2, StopModeV2, WORKER_PROTOCOL_VERSION, WorkerCommandV2, WorkerDescriptorV2,
    WorkerIdentityV2, WorkerPayloadV2, WorkerRequestV2, WorkerResponseV2, WorkerStatusV2,
    device_component_guest_roles_v2,
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

use crate::{
    AdbClient, CapabilityDiscovery, CrosvmBackend, ManagedProcess, NativeDiskProvisioner,
    RunJournalV2, TokioProcessSupervisor, expected_frame_transport, send_device_control_request,
};

const FRAME_READY_TIMEOUT: Duration = Duration::from_secs(90);
const COMPONENT_READY_TIMEOUT: Duration = Duration::from_secs(30);
const ADBD_LOG_READY_TIMEOUT: Duration = Duration::from_mins(3);
const DEV_BOOT_LOG_READY_TIMEOUT: Duration = Duration::from_secs(150);
const DEV_BOOT_LOG_SCAN_LIMIT: u64 = 32 * 1024 * 1024;
// Native gfxstream HWND creation follows SurfaceFlinger rather than crosvm process creation.
// Keep the VM/control plane usable while that window appears instead of holding Start (and the
// per-instance operation lock) for the entire Android cold boot.
const INITIAL_DISPLAY_ATTACH_RETRY_WINDOW: Duration = Duration::from_mins(3);
const INITIAL_DISPLAY_ATTACH_RETRY_MIN: Duration = Duration::from_millis(100);
const INITIAL_DISPLAY_ATTACH_RETRY_MAX: Duration = Duration::from_millis(500);
const CROSVM_DISPLAY_PARENT_HWND_ENV: &str = "CROSVM_DISPLAY_PARENT_HWND";
const CROSVM_DISPLAY_WIDTH_ENV: &str = "CROSVM_DISPLAY_WIDTH";
const CROSVM_DISPLAY_HEIGHT_ENV: &str = "CROSVM_DISPLAY_HEIGHT";
const CROSVM_COCOA_CONTEXT_ENDPOINT_ENV: &str = "CROSVM_COCOA_CONTEXT_ENDPOINT";

#[cfg(target_os = "macos")]
const BOOTCONFIG_MAGIC: &[u8] = b"#BOOTCONFIG\n";

#[cfg(target_os = "macos")]
fn append_newc_entry(
    archive: &mut Vec<u8>,
    name: &str,
    data: &[u8],
    mode: u32,
    inode: u32,
) -> std::io::Result<()> {
    let name_size = u32::try_from(name.len() + 1).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Android initramfs member name is too long: {error}"),
        )
    })?;
    let file_size = u32::try_from(data.len()).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Android initramfs member is too large: {error}"),
        )
    })?;
    let fields = [inode, mode, 0, 0, 1, 0, file_size, 0, 0, 0, 0, name_size, 0];
    archive.extend_from_slice(b"070701");
    for value in fields {
        archive.extend_from_slice(format!("{value:08x}").as_bytes());
    }
    archive.extend_from_slice(name.as_bytes());
    archive.push(0);
    archive.resize(archive.len().next_multiple_of(4), 0);
    archive.extend_from_slice(data);
    archive.resize(archive.len().next_multiple_of(4), 0);
    Ok(())
}

#[cfg(target_os = "macos")]
fn normalize_android_fstab_for_nonsecure_keymint(source: &Path) -> std::io::Result<Vec<u8>> {
    let text = std::fs::read_to_string(source)?;
    let mut output = String::new();
    let mut saw_data = false;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            output.push_str(raw_line);
            output.push('\n');
            continue;
        }
        let mut columns = line
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if columns.len() != 5 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported Android fstab line: {raw_line}"),
            ));
        }
        if columns[1] == "/data" {
            saw_data = true;
            let mut flags = columns[4]
                .split(',')
                .filter(|flag| {
                    !flag.starts_with("keydirectory=")
                        && !flag.starts_with("fileencryption=")
                        && *flag != "latemount"
                })
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if !flags.iter().any(|flag| flag == "first_stage_mount") {
                flags.push("first_stage_mount".to_owned());
            }
            columns[4] = flags.join(",");
        }
        output.push_str(&columns.join("\t"));
        output.push('\n');
    }
    if !saw_data {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Android fstab has no /data entry",
        ));
    }
    Ok(output.into_bytes())
}

#[cfg(target_os = "macos")]
fn write_android_fstab_for_nonsecure_keymint(
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    std::fs::write(
        destination,
        normalize_android_fstab_for_nonsecure_keymint(source)?,
    )
}

#[cfg(target_os = "macos")]
fn make_android_fstab_override_cpio(source: &Path) -> std::io::Result<Vec<u8>> {
    let normalized = normalize_android_fstab_for_nonsecure_keymint(source)?;
    let normalized = std::str::from_utf8(&normalized).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("normalized Android fstab is not UTF-8: {error}"),
        )
    })?;
    let mut fstab = String::new();
    for raw_line in normalized.lines() {
        if raw_line.split_whitespace().nth(1) != Some("/data") {
            fstab.push_str(raw_line);
            fstab.push('\n');
        }
    }
    let mut archive = Vec::new();
    let mut inode = 1_u32;
    for name in [
        ".",
        "first_stage_ramdisk",
        "first_stage_ramdisk/system",
        "first_stage_ramdisk/system/etc",
        "system",
        "system/etc",
    ] {
        append_newc_entry(&mut archive, name, &[], 0o040_755, inode)?;
        inode += 1;
    }
    for prefix in [
        "",
        "first_stage_ramdisk/",
        "first_stage_ramdisk/system/etc/",
        "system/etc/",
    ] {
        append_newc_entry(
            &mut archive,
            &format!("{prefix}fstab.hd"),
            fstab.as_bytes(),
            0o100_644,
            inode,
        )?;
        inode += 1;
    }
    append_newc_entry(&mut archive, "TRAILER!!!", &[], 0, inode)?;
    archive.resize(archive.len().next_multiple_of(512), 0);
    Ok(archive)
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_lines)]
fn patch_android_initrd_bootconfig(
    source: &Path,
    android_fstab: &Path,
    destination: &Path,
) -> std::io::Result<bool> {
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
    let nonsecure_keymint = entries.iter().any(|(key, entry)| {
        key == "androidboot.vendor.apex.com.android.hardware.keymint"
            && entry == "androidboot.vendor.apex.com.android.hardware.keymint=com.android.hardware.keymint.rust_nonsecure"
    });
    let fstab_override = if nonsecure_keymint {
        let key = "androidboot.fstab_suffix";
        let value = "androidboot.fstab_suffix=hd";
        if let Some((_, entry)) = entries.iter_mut().find(|(entry, _)| entry == key) {
            value.clone_into(entry);
        } else {
            entries.push((key.to_owned(), value.to_owned()));
        }
        // The nonsecure KeyMint profile has no host-backed persistent root secret. Cuttlefish's
        // keydirectory and file-encryption keys therefore succeed only on blank userdata and
        // cannot be reopened after a VM power cycle. Project a dedicated unencrypted development
        // fstab; the initramfs view deliberately omits /data so Android's native late-fs phase
        // mounts it from the FDT view and emits `nonencrypted`. Persistent encryption requires a
        // future host-backed secure KeyMint profile.
        Some(make_android_fstab_override_cpio(android_fstab)?)
    } else {
        None
    };

    let mut bootconfig = String::new();
    for (_, entry) in entries {
        bootconfig.push_str(&entry);
        bootconfig.push('\n');
    }
    let bootconfig = bootconfig.into_bytes();
    let checksum = bootconfig
        .iter()
        .fold(0_u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
    let mut patched = Vec::with_capacity(
        prefix.len()
            + fstab_override.as_ref().map_or(0, Vec::len)
            + bootconfig.len()
            + 8
            + BOOTCONFIG_MAGIC.len(),
    );
    let bootconfig_len = u32::try_from(bootconfig.len()).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Android initrd bootconfig length exceeds u32: {error}"),
        )
    })?;
    patched.extend_from_slice(prefix);
    if let Some(fstab_override) = fstab_override {
        patched.extend_from_slice(&fstab_override);
    }
    patched.extend_from_slice(&bootconfig);
    patched.extend_from_slice(&bootconfig_len.to_le_bytes());
    patched.extend_from_slice(&checksum.to_le_bytes());
    patched.extend_from_slice(BOOTCONFIG_MAGIC);
    std::fs::write(destination, patched)?;
    Ok(nonsecure_keymint)
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
    #[cfg(unix)]
    device_output_files: Vec<std::fs::File>,
    #[cfg(unix)]
    device_input_fifos: Vec<std::fs::File>,
}

#[derive(Debug)]
struct ActiveDisplaySession {
    id: Uuid,
    generation: u64,
    target: NativeDisplayTargetV2,
    viewport: DisplayViewportV2,
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
                #[cfg(unix)]
                device_output_files: Vec::new(),
                #[cfg(unix)]
                device_input_fifos: Vec::new(),
            }),
            operation: Mutex::new(()),
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
        let result = self.dispatch_command(request.command).await;
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
                target,
                viewport,
            } => self
                .attach_display(session_id, generation, target, viewport)
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
            WorkerCommandV2::CollectGuestLogs => self
                .collect_guest_logs()
                .await
                .map(WorkerPayloadV2::GuestLog),
            WorkerCommandV2::Diagnose => Ok(WorkerPayloadV2::Diagnostics(self.diagnostics().await)),
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
        let run_dir = self.paths.run_dir(self.instance_id, run_id);
        let journal = Arc::new(RunJournalV2::create(
            &run_dir,
            self.instance_id,
            run_id,
            trace_id,
        )?);
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
            mutable.active_spec = Some(spec.clone());
            mutable.journal = Some(Arc::clone(&journal));
            mutable.started_at = Some(started_at);
            mutable.display_session =
                initial_display
                    .as_ref()
                    .map(|display| ActiveDisplaySession {
                        id: display.session_id,
                        generation: frame_generation,
                        target: display.target.clone(),
                        viewport: display.viewport.clone(),
                    });
        }
        self.transition(ObservedStateV2::Preparing, None).await?;
        let start_result: Result<(), WorkerError> = async {
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
            let mut artifacts = bundles.artifacts.clone();
            #[cfg(target_os = "macos")]
            {
                let patched_initrd = run_dir.join("initrd-android-hd.img");
                let nonsecure_keymint = patch_android_initrd_bootconfig(
                    &artifacts.initrd,
                    &artifacts.android_fstab,
                    &patched_initrd,
                )
                .map_err(
                    |source| WorkerError::Io {
                        operation: "patch Android initrd bootconfig",
                        path: patched_initrd.clone(),
                        source,
                    },
                )?;
                artifacts.initrd = patched_initrd;
                if nonsecure_keymint {
                    // The same fstab must also populate the FDT passed to crosvm. Android's
                    // first-stage init reads the initramfs copy, while `mount_all --late`
                    // consults the device tree after /vendor has replaced the ramdisk paths.
                    // Mixing the projected unencrypted fstab with the source encrypted FDT
                    // makes late-fs attempt to remount /data and suppresses `nonencrypted`.
                    let patched_fstab = run_dir.join("android_fstab-hd.dt");
                    write_android_fstab_for_nonsecure_keymint(
                        &artifacts.android_fstab,
                        &patched_fstab,
                    )
                    .map_err(|source| WorkerError::Io {
                        operation: "project Android fstab for nonsecure KeyMint",
                        path: patched_fstab.clone(),
                        source,
                    })?;
                    artifacts.android_fstab = patched_fstab;
                }
            }
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
                    launch
                        .adb_serial
                        .clone()
                        .ok_or(WorkerError::ReadinessUnavailable)?,
                    spec.display.clone(),
                    spec.devices.bluetooth,
                    spec.devices.nfc,
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
        target: NativeDisplayTargetV2,
        viewport: DisplayViewportV2,
    ) -> Result<(), WorkerError> {
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
        hd_platform::attach_native_display(child_pid, &target, &viewport)?;
        self.mutable.lock().await.display_session = Some(ActiveDisplaySession {
            id: session_id,
            generation,
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
                match hd_platform::attach_native_display(
                    child_pid,
                    &prepared.target,
                    &prepared.viewport,
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
        if viewport.revision <= session.viewport.revision {
            return Ok(());
        }
        let geometry_changed = viewport.width_px != session.viewport.width_px
            || viewport.height_px != session.viewport.height_px
            || viewport.dpi != session.viewport.dpi;
        if geometry_changed {
            hd_platform::resize_native_display(child_pid, &session.target, &viewport)?;
        } else if viewport.visible != session.viewport.visible {
            hd_platform::set_native_display_visibility(child_pid, viewport.visible)?;
        }
        session.viewport = viewport;
        Ok(())
    }

    async fn detach_display(&self, session_id: Uuid, generation: u64) -> Result<(), WorkerError> {
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
        hd_platform::detach_native_display(child_pid)?;
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

    #[allow(clippy::too_many_lines)]
    async fn stop(&self, mode: StopModeV2, graceful_timeout: Duration) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        let status = self.status().await;
        if matches!(status.observed, ObservedStateV2::Stopped) {
            return Ok(());
        }
        if matches!(status.observed, ObservedStateV2::Deleted) {
            return Err(WorkerError::Busy("deleted worker cannot be stopped"));
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
        if let Some(child_pid) = status.child_pid
            && self.mutable.lock().await.display_session.is_some()
        {
            let _ = hd_platform::detach_native_display(child_pid);
        }
        let (process, backend, launch, adb) = {
            let mut mutable = self.mutable.lock().await;
            (
                mutable.process.take(),
                mutable.backend.clone(),
                mutable.launch.clone(),
                mutable.adb.clone(),
            )
        };
        let mut retained_process = None;
        let mut cleanup_error = None;
        if let Some(mut process) = process {
            let mut exited = false;
            if matches!(mode, StopModeV2::Graceful)
                && let (Some(backend), Some(launch)) = (&backend, &launch)
            {
                let adb_poweroff_requested = if let (Some(adb), Some(serial)) =
                    (&adb, launch.adb_serial.as_deref())
                {
                    match adb.power_off(serial).await {
                        Ok(()) => true,
                        Err(error) => {
                            tracing::warn!(
                                event = "worker.stop.adb_power_off.failed",
                                instance_id = %self.instance_id,
                                %error,
                                "Android power-off request failed; falling back to crosvm power button"
                            );
                            false
                        }
                    }
                } else {
                    false
                };
                let power_requested = if adb_poweroff_requested {
                    Ok(())
                } else {
                    backend.power_button(&launch.control_endpoint).await
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

        self.transition(ObservedStateV2::Stopped, None).await?;
        if !was_failed {
            self.finish_run(ObservedStateV2::Stopped, None, None)
                .await?;
        }
        let mut mutable = self.mutable.lock().await;
        mutable.status.run_id = None;
        mutable.status.child_pid = None;
        mutable.status.cleanup_pending = false;
        mutable.status.adb_serial = None;
        mutable.status.last_error = None;
        mutable.active_spec = None;
        mutable.backend = None;
        mutable.launch = None;
        mutable.adb = None;
        mutable.adb_ready = false;
        mutable.display_session = None;
        mutable.components.clear();
        mutable.device_control_tokens.clear();
        mutable.journal = None;
        #[cfg(unix)]
        mutable.device_output_files.clear();
        #[cfg(unix)]
        mutable.device_input_fifos.clear();
        Ok(())
    }

    async fn pause(&self) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        if self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::Busy("pause requires Ready"));
        }
        self.transition(ObservedStateV2::Pausing, None).await?;
        let (backend, endpoint) = self.backend_control().await?;
        if let Err(error) = backend
            .pause(&endpoint)
            .await
            .map_err(WorkerError::Platform)
        {
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
        self.transition(ObservedStateV2::Ready, None).await
    }

    async fn reconfigure(
        &self,
        display: hd_core::DisplayConfigV2,
        adb_config: hd_core::AdbConfigV2,
    ) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        let (mut spec, adb, serial) = {
            let mutable = self.mutable.lock().await;
            (
                mutable.active_spec.clone().ok_or(WorkerError::NotRunning)?,
                mutable.adb_ready.then(|| mutable.adb.clone()).flatten(),
                mutable.status.adb_serial.clone(),
            )
        };
        if display == spec.display && adb_config == spec.adb {
            return Ok(());
        }
        let mut restart_display = spec.display.clone();
        restart_display.orientation = display.orientation;
        if display != restart_display || adb_config != spec.adb {
            return Err(WorkerError::RestartRequired);
        }
        let previous = spec.display.clone();
        if display.orientation != previous.orientation
            && let (Some(adb), Some(serial)) = (&adb, serial.as_deref())
            && let Err(error) = adb.set_display_configuration(serial, &display).await
        {
            return Err(WorkerError::Adb(error));
        }
        spec.display = display;
        self.mutable.lock().await.active_spec = Some(spec);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn action(&self, action: InstanceActionV2) -> Result<(), WorkerError> {
        action.validate()?;
        if self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::NotReady);
        }
        match action {
            InstanceActionV2::Key { key } => {
                if key == KeyActionV2::Power {
                    // Match Android's ADB KEYCODE_POWER semantics through the always-connected
                    // virtio keyboard. The crosvm powerbtn control is unsupported by HVF, while
                    // KEY_POWER must remain available when ADB is connecting or reconnecting.
                    let (backend, keyboard_endpoint) = {
                        let mutable = self.mutable.lock().await;
                        (
                            mutable.backend.clone().ok_or(WorkerError::NotRunning)?,
                            mutable
                                .launch
                                .as_ref()
                                .map(|plan| plan.keyboard_endpoint.clone())
                                .ok_or(WorkerError::NotRunning)?,
                        )
                    };
                    backend.send_key(&keyboard_endpoint, key).await?;
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
                adb.set_display_configuration(&serial, &display).await?;
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
                self.call_device_component(
                    "hd-device-sim",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::SetLocation {
                            location: location.clone(),
                        },
                    },
                )
                .await?;
                #[cfg(target_os = "macos")]
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
                    adb.set_location(&serial, &location).await?;
                }
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
                self.call_device_component(
                    "hd-device-sim",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::InjectSensor {
                            injection: injection.clone(),
                        },
                    },
                )
                .await?;
                #[cfg(not(target_os = "macos"))]
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
            InstanceActionV2::BluetoothPeer { action } => {
                self.call_device_component(
                    "rootcanal-adapter",
                    DeviceControlCommandV2::Action {
                        action: InstanceActionV2::BluetoothPeer { action },
                    },
                )
                .await?;
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
        }
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
        vec![
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
        ]
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
                        let _operation = worker.operation.lock().await;
                        if !worker.status().await.observed.is_active() {
                            break;
                        }
                        let error = WorkerError::GuestExited(exit.code);
                        let _ = worker.fail_start(&error, false).await;
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
        serial: String,
        display: DisplayConfigV2,
        bluetooth_enabled: bool,
        nfc_enabled: bool,
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

            let connect = adb.connect(&serial);
            tokio::pin!(connect);
            let connect_result = loop {
                tokio::select! {
                    result = &mut connect => break result,
                    () = tokio::time::sleep(Duration::from_millis(500)) => {
                        if worker.status().await.run_id != Some(run_id) {
                            return;
                        }
                    }
                }
            };
            if let Err(error) = connect_result {
                tracing::warn!(
                    event = "worker.adb.deferred_connect.failed",
                    instance_id = %worker.instance_id,
                    %run_id,
                    %serial,
                    %error,
                    "deferred ADB connection failed; native display remains available"
                );
                return;
            }

            let readiness = adb.wait_ready(&serial);
            tokio::pin!(readiness);
            let readiness_result = loop {
                tokio::select! {
                    result = &mut readiness => break result,
                    () = tokio::time::sleep(Duration::from_millis(500)) => {
                        if worker.status().await.run_id != Some(run_id) {
                            return;
                        }
                    }
                }
            };
            if let Err(error) = readiness_result {
                tracing::warn!(
                    event = "worker.adb.deferred_readiness.failed",
                    instance_id = %worker.instance_id,
                    %run_id,
                    %serial,
                    %error,
                    "deferred Android readiness failed; native display remains available"
                );
                return;
            }
            adb.apply_runtime_device_policy(&serial, bluetooth_enabled, nfc_enabled)
                .await;
            if let Err(error) = adb.keep_display_awake(&serial).await {
                tracing::warn!(
                    event = "worker.adb.deferred_keep_awake.failed",
                    instance_id = %worker.instance_id,
                    %run_id,
                    %serial,
                    %error,
                    "Android display keep-awake policy could not be applied"
                );
            }
            if let Err(error) = adb.set_display_configuration(&serial, &display).await {
                tracing::warn!(
                    event = "worker.adb.deferred_orientation.failed",
                    instance_id = %worker.instance_id,
                    %run_id,
                    %serial,
                    %error,
                    "initial Android orientation could not be applied"
                );
            }

            let mut mutable = worker.mutable.lock().await;
            if mutable.status.run_id == Some(run_id)
                && matches!(
                    mutable.status.observed,
                    ObservedStateV2::Ready | ObservedStateV2::Paused
                )
            {
                mutable.adb_ready = true;
                tracing::info!(
                    event = "worker.adb.deferred_readiness.succeeded",
                    instance_id = %worker.instance_id,
                    %run_id,
                    %serial,
                    "ADB-backed HD actions are ready"
                );
            }
        });
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
        let finish_result = self.finish_run(target, None, Some(error)).await;
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
                mutable.device_control_tokens.clear();
                #[cfg(unix)]
                mutable.device_output_files.clear();
                #[cfg(unix)]
                mutable.device_input_fifos.clear();
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
}

#[derive(Debug)]
struct RuntimeEndpoints {
    control: String,
    frame: String,
    keyboard: String,
    devices: BTreeMap<String, DeviceSerialEndpointV2>,
    device_controls: BTreeMap<String, String>,
    #[cfg(unix)]
    output_files: Vec<std::fs::File>,
    #[cfg(unix)]
    input_fifos: Vec<std::fs::File>,
}

fn enabled_device_components(spec: &InstanceSpecV2) -> Vec<&'static str> {
    let mut components = Vec::new();
    if spec.devices.gnss
        || spec.devices.sensors
        || spec.devices.power
        || (spec.devices.network && !cfg!(target_os = "macos"))
    {
        components.push("hd-device-sim");
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
            "gnss" | "location" => spec.devices.gnss,
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
    let disk = lease_resource(leases, LeaseKindV2::DiskOverlay)?;
    if disk != paths.disk_overlay(instance_id).to_string_lossy() {
        return Err(WorkerError::LeaseContract(
            "disk overlay lease does not match the instance path".to_owned(),
        ));
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
    let _gpu_slot = lease_number::<u32>(leases, LeaseKindV2::GpuSlot)?;
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
    let expected_count =
        7 + usize::from(matches!(spec.adb.mode, AdbModeV2::Loopback)) + expected_devices.len();
    if leases.len() != expected_count {
        return Err(WorkerError::LeaseContract(format!(
            "expected {expected_count} leases, received {}",
            leases.len()
        )));
    }
    let mut ids = BTreeSet::new();
    let mut counts = BTreeMap::<LeaseKindV2, usize>::new();
    for kind in [
        LeaseKindV2::CpuCapacity,
        LeaseKindV2::MemoryBytes,
        LeaseKindV2::GuestCid,
        LeaseKindV2::DiskOverlay,
        LeaseKindV2::GpuSlot,
        LeaseKindV2::WorkerEndpoint,
        LeaseKindV2::FrameGeneration,
    ] {
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
    #[error("device endpoint is unavailable for role {0}")]
    DeviceEndpoint(String),
    #[error("device request was rejected: {0}")]
    DeviceRejected(String),
    #[error("display session failed: {0}")]
    DisplaySession(String),
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
            Self::DeviceEndpoint(_) => "device_endpoint",
            Self::DeviceRejected(_) => "device_rejected",
            Self::DisplaySession(_) => "display_session",
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
            Self::Json(_) => "json",
            Self::Io { .. } => "io",
        }
    }

    pub fn api_error(&self) -> ApiErrorV2 {
        ApiErrorV2::new(self.code(), self.to_string()).retryable(matches!(
            self,
            Self::Busy(_) | Self::AdbNotReady | Self::CapabilityChanged { .. } | Self::DeviceIpc(_)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_foreign_lease_owner_is_rejected() {
        let temporary = tempfile::tempdir().expect("temporary data root");
        let paths = DataPaths::from_root(temporary.path().join("data"));
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
