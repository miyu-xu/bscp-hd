use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Parser, Subcommand, ValueEnum};
use ed25519_dalek::{Signer as _, SigningKey};
#[cfg(windows)]
use hd_core::FormalComponentProbeV2;
use hd_core::{
    ARTIFACT_INDEX_VERSION, ApiErrorV2, ArtifactBundleKindV2, ArtifactBundleV2, ArtifactFileV2,
    ArtifactReadyMarkerV2, COMPONENT_PROTOCOL_VERSION, CONTROL_PROTOCOL_VERSION,
    CreateInstanceRequestV2, DEVICE_GUEST_ENDPOINT_ROLES_V2, DeviceControlCommandV2,
    DeviceControlRequestV2, DeviceControlResponseV2, DeviceControlTokenV2, DeviceSerialEndpointV2,
    DiagnosticRequestV2, FRAME_PROTOCOL_VERSION, FormalComponentConfigurationV2,
    FormalComponentLaunchV2, FormalComponentReadyV2, GuestKindV2, HOST_CERTIFICATION_VERSION,
    HostCapabilitiesV2, HostCertificationV2, InstanceActionV2, InstanceSpecV2, KeyActionV2,
    LeaseKindV2, LeaseV2, ModemStateV2, NfcTagActionV2, ObservedStateV2, OperationKindV2,
    PackagedArtifactChannelV2, ResolvedGuestArtifactsV2, StopModeV2, UwbRangingV2,
    WORKER_PROTOCOL_VERSION, WorkerCommandV2, WorkerDescriptorV2, WorkerIdentityV2,
    WorkerPayloadV2, WorkerRequestV2, device_component_guest_roles_v2,
};
#[cfg(any(windows, target_os = "macos"))]
use hd_core::{BluetoothAdvertisementFrameV2, BluetoothHciCaptureRecordV2, BluetoothPeerActionV2};
#[cfg(windows)]
use hd_platform::FrameInteropProbeV2;
use hd_platform::{DataPaths, ProcessSpec, ProcessSupervisor, VmBackend as _, VmLaunchContextV2};
use hd_runtime::{
    ArtifactResolver, ArtifactTrustStore, ClientError, CrosvmBackend, HostClientV2, HostService,
    LeaseManager, PersistentStore, TokioProcessSupervisor, canonical_payload, run_host_http,
    send_device_control_request, send_worker_request, verify_packaged_android_artifact_store,
    worker_endpoint,
};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

mod cuttlefish;
mod process;

const ANDROID_CERTIFICATION_EVIDENCE: [&str; 8] = [
    "hd_quality",
    "host_worker_smoke",
    "http_security_smoke",
    "lease_recovery_smoke",
    "diagnostic_smoke",
    "real_guest",
    "zero_copy",
    "device_profile_conformance",
];
const MICRODROID_CERTIFICATION_EVIDENCE: [&str; 8] = [
    "hd_quality",
    "host_worker_smoke",
    "http_security_smoke",
    "lease_recovery_smoke",
    "diagnostic_smoke",
    "microdroid_real_guest",
    "microdroid_multi_instance",
    "microdroid_payload_conformance",
];

const HD_FORMAT_PACKAGES: [&str; 15] = [
    "hd-adb-bridge",
    "hd-casimir-adapter",
    "hd-rootcanal-adapter",
    "hd-core",
    "hd-device-sim",
    "hd-frame",
    "hd-frame-producer",
    "hd-peripheral-adapters",
    "hd-host",
    "hd-platform",
    "hd-runtime",
    "hd-ui",
    "hd-worker",
    "hdctl",
    "xtask",
];
const UI_SHELL_SOURCE: &str = include_str!("../../crates/hd-ui/src/web_shell.rs");
const UI_CONTRACT_SOURCE: &str = include_str!("../../crates/hd-ui/src/ui_contract.rs");
#[cfg(windows)]
const PACKAGED_WINDOWS_HELP_PROBES: &[&str] = &[
    "hd.exe",
    "hdctl.exe",
    "hd-host.exe",
    "hd-worker.exe",
    "crosvm.exe",
    "vm.exe",
    "virtmgr.exe",
];

#[derive(Debug, Parser)]
#[command(about = "HD developer, evidence and quality tasks")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    ProcessCheck,
    Quality {
        #[arg(long)]
        evidence_output: Option<PathBuf>,
    },
    CheckPortable,
    Smoke {
        #[arg(long)]
        evidence_output: Option<PathBuf>,
    },
    PeAudit {
        #[arg(long)]
        bin_dir: PathBuf,
        #[arg(long, default_value = "objdump")]
        objdump: PathBuf,
    },
    Package {
        #[arg(long)]
        target_dir: PathBuf,
        #[arg(long)]
        runtime_dir: PathBuf,
        #[arg(long)]
        adb: PathBuf,
        #[arg(long)]
        aapt2: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    AiCycle {
        #[arg(long)]
        task: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Readback {
        #[arg(long)]
        task: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long = "gate-report")]
        gate_reports: Vec<PathBuf>,
    },
    /// Create a new owner-only Ed25519 signing key and matching HD trust store.
    InitTrustRoot {
        #[arg(long)]
        data_root: PathBuf,
        #[arg(long)]
        signer_key_id: String,
        #[arg(long)]
        signing_key: PathBuf,
    },
    /// Build, sign, verify and atomically publish a content-addressed artifact bundle.
    PublishBundle {
        #[arg(long)]
        kind: BundleKindArgument,
        #[arg(long)]
        input_root: PathBuf,
        #[arg(long)]
        store_root: PathBuf,
        #[arg(long)]
        platform: String,
        #[arg(long)]
        architecture: String,
        #[arg(long)]
        source_manifest_digest: String,
        #[arg(long)]
        signer_key_id: String,
        #[arg(long)]
        signing_key: PathBuf,
        #[arg(long)]
        trust_store: PathBuf,
        #[arg(long = "capability", required = true)]
        capabilities: Vec<String>,
        #[arg(long = "file", value_parser = parse_bundle_file, required = true)]
        files: Vec<(String, PathBuf)>,
        #[arg(long = "executable-role")]
        executable_roles: Vec<String>,
    },
    /// Verify a relocatable packaged Android signed artifact store and its exact closure.
    VerifyAndroidArtifactStore {
        #[arg(long)]
        store_root: PathBuf,
        #[arg(long)]
        trust_store: PathBuf,
        #[arg(long, value_enum)]
        channel: ArtifactChannelArgument,
    },
    /// Convert a supported `x86_64` Cuttlefish release into unsigned HD Guest staging.
    ImportCuttlefish(CuttlefishImportArguments),
    #[command(hide = true)]
    ProcessProbe {
        #[arg(long)]
        marker: Option<PathBuf>,
        #[arg(long)]
        leaf: bool,
    },
    #[command(name = "connect_vsock", hide = true)]
    AdbBridgeConnectProbe {
        guest_port: u32,
        vm_control_endpoint: String,
    },
    #[command(name = "adb-bridge-pipe-probe", hide = true)]
    AdbBridgePipeProbe {
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        marker: PathBuf,
    },
    /// Sign an exact platform/bundle certification after all eight evidence gates pass.
    Certify {
        #[arg(long)]
        data_root: PathBuf,
        #[arg(long, value_enum, default_value_t = CertificationGuestKindArgument::Android)]
        guest_kind: CertificationGuestKindArgument,
        #[arg(long)]
        guest_digest: String,
        #[arg(long)]
        host_digest: String,
        #[arg(long)]
        capability_fingerprint: String,
        #[arg(long)]
        signer_key_id: String,
        #[arg(long)]
        signing_key: PathBuf,
        #[arg(long, default_value_t = 14_u64, value_parser = parse_validity_days)]
        validity_days: u64,
        #[arg(long = "evidence", value_parser = parse_evidence, num_args = 8)]
        evidence: Vec<(String, PathBuf)>,
    },
}

#[derive(Debug, clap::Args)]
struct CuttlefishImportArguments {
    #[arg(long, default_value = "python")]
    python: PathBuf,
    #[arg(
        long,
        required_unless_present = "self_check",
        conflicts_with = "self_check"
    )]
    image_zip: Option<PathBuf>,
    #[arg(long, conflicts_with = "self_check")]
    target_files_zip: Option<PathBuf>,
    #[arg(long, conflicts_with = "self_check")]
    ota_metadata: Option<PathBuf>,
    #[arg(long, conflicts_with = "self_check")]
    sensor_injector: Option<PathBuf>,
    #[arg(
        long,
        required_unless_present = "self_check",
        conflicts_with = "self_check"
    )]
    output: Option<PathBuf>,
    #[arg(long)]
    self_check: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BundleKindArgument {
    Guest,
    HostTools,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ArtifactChannelArgument {
    Development,
    Release,
}

impl From<ArtifactChannelArgument> for PackagedArtifactChannelV2 {
    fn from(value: ArtifactChannelArgument) -> Self {
        match value {
            ArtifactChannelArgument::Development => Self::Development,
            ArtifactChannelArgument::Release => Self::Release,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CertificationGuestKindArgument {
    Android,
    Microdroid,
}

impl From<CertificationGuestKindArgument> for GuestKindV2 {
    fn from(value: CertificationGuestKindArgument) -> Self {
        match value {
            CertificationGuestKindArgument::Android => Self::Android,
            CertificationGuestKindArgument::Microdroid => Self::Microdroid,
        }
    }
}

impl From<BundleKindArgument> for ArtifactBundleKindV2 {
    fn from(value: BundleKindArgument) -> Self {
        match value {
            BundleKindArgument::Guest => Self::Guest,
            BundleKindArgument::HostTools => Self::HostTools,
        }
    }
}

fn main() {
    if let Err(error) = run_main() {
        eprintln!("xtask failed: {error:#}");
        std::process::exit(1);
    }
}

fn run_main() -> Result<()> {
    let root = workspace_root()?;
    run_task(&root, Cli::parse().command)
}

fn run_task(root: &Path, task: Task) -> Result<()> {
    match task {
        Task::ProcessCheck => process::process_check(root),
        Task::Quality { evidence_output } => quality(root, evidence_output.as_deref()),
        Task::CheckPortable => check_portable(root),
        Task::Smoke { evidence_output } => {
            require_windows_gnu()?;
            smoke(root, evidence_output.as_deref())
        }
        Task::PeAudit { bin_dir, objdump } => pe_audit(&bin_dir, &objdump),
        Task::Package {
            target_dir,
            runtime_dir,
            adb,
            aapt2,
            output,
        } => {
            require_windows_gnu()?;
            package(root, &target_dir, &runtime_dir, &adb, &aapt2, &output)
        }
        Task::AiCycle { task, output } => {
            require_windows_gnu()?;
            process::ai_cycle(root, &task, &output)
        }
        Task::Readback {
            task,
            output,
            gate_reports,
        } => process::readback(root, &task, &output, &gate_reports),
        Task::InitTrustRoot {
            data_root,
            signer_key_id,
            signing_key,
        } => init_trust_root(&data_root, &signer_key_id, &signing_key),
        Task::PublishBundle {
            kind,
            input_root,
            store_root,
            platform,
            architecture,
            source_manifest_digest,
            signer_key_id,
            signing_key,
            trust_store,
            capabilities,
            files,
            executable_roles,
        } => publish_bundle(PublishBundleRequest {
            kind: kind.into(),
            input_root,
            store_root,
            platform,
            architecture,
            source_manifest_digest,
            signer_key_id,
            signing_key,
            trust_store,
            capabilities,
            files,
            executable_roles,
            print_result: true,
        }),
        Task::VerifyAndroidArtifactStore {
            store_root,
            trust_store,
            channel,
        } => verify_android_artifact_store(&store_root, &trust_store, channel.into()),
        Task::ImportCuttlefish(arguments) => import_cuttlefish(root, arguments),
        Task::ProcessProbe { marker, leaf } => process_probe(marker.as_deref(), leaf),
        Task::AdbBridgeConnectProbe {
            guest_port,
            vm_control_endpoint,
        } => adb_bridge_connect_probe(guest_port, &vm_control_endpoint),
        Task::AdbBridgePipeProbe { endpoint, marker } => adb_bridge_pipe_probe(&endpoint, &marker),
        Task::Certify {
            data_root,
            guest_kind,
            guest_digest,
            host_digest,
            capability_fingerprint,
            signer_key_id,
            signing_key,
            validity_days,
            evidence,
        } => certify(
            &data_root,
            guest_kind.into(),
            &guest_digest,
            &host_digest,
            &capability_fingerprint,
            &signer_key_id,
            &signing_key,
            validity_days,
            evidence,
        ),
    }
}

fn check_portable(root: &Path) -> Result<()> {
    require_windows_gnu()?;
    let target = quality_target_args();
    let mut arguments = vec!["check", "--workspace", "--all-targets"];
    arguments.extend(target.iter().copied());
    run(root, "cargo", &arguments)
}

fn import_cuttlefish(root: &Path, arguments: CuttlefishImportArguments) -> Result<()> {
    cuttlefish::import(
        root,
        cuttlefish::ImportOptions {
            python: arguments.python,
            image_zip: arguments.image_zip,
            target_files_zip: arguments.target_files_zip,
            ota_metadata: arguments.ota_metadata,
            sensor_injector: arguments.sensor_injector,
            output: arguments.output,
            self_check: arguments.self_check,
        },
    )
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ProcessProbeMarker {
    parent_pid: u32,
    parent_start_marker: String,
    child_pid: u32,
    child_start_marker: String,
}

fn process_probe(marker: Option<&Path>, leaf: bool) -> Result<()> {
    if leaf {
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let marker = marker.context("managed process probe requires --marker")?;
    let executable = std::env::current_exe()?;
    let child = Command::new(executable)
        .args(["process-probe", "--leaf"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn managed process probe leaf")?;
    let record = ProcessProbeMarker {
        parent_pid: std::process::id(),
        parent_start_marker: hd_platform::process_start_marker(std::process::id())?,
        child_pid: child.id(),
        child_start_marker: hd_platform::process_start_marker(child.id())?,
    };
    hd_platform::write_owner_only(marker, &serde_json::to_vec_pretty(&record)?)?;
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn adb_bridge_connect_probe(guest_port: u32, _vm_control_endpoint: &str) -> Result<()> {
    let endpoint = std::env::var("HD_ADB_BRIDGE_SMOKE_PIPE")
        .context("HD_ADB_BRIDGE_SMOKE_PIPE is required by the probe")?;
    let marker = std::env::var_os("HD_ADB_BRIDGE_SMOKE_MARKER")
        .map(PathBuf::from)
        .context("HD_ADB_BRIDGE_SMOKE_MARKER is required by the probe")?;
    ensure!(
        endpoint.ends_with(&format!("_{guest_port}")),
        "ADB bridge probe guest port does not match its pipe endpoint"
    );
    Command::new(std::env::current_exe()?)
        .arg("adb-bridge-pipe-probe")
        .arg("--endpoint")
        .arg(&endpoint)
        .arg("--marker")
        .arg(&marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn ADB bridge pipe probe")?;
    // Match real crosvm connect_vsock semantics: return after dispatching the listener. The
    // bridge retries opening the pipe, and the parent smoke independently verifies the marker
    // and exact probe process identity after forwarding completes.
    Ok(())
}

fn adb_bridge_pipe_probe(endpoint: &str, marker: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::windows::named_pipe::ServerOptions;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let options = ServerOptions::new();
            let mut pipe = hd_platform::create_owner_only_named_pipe(&options, endpoint)?;
            let identity = WorkerIdentityV2 {
                pid: std::process::id(),
                process_start_marker: hd_platform::process_start_marker(std::process::id())?,
                nonce: Uuid::nil(),
            };
            hd_platform::write_owner_only(marker, &serde_json::to_vec_pretty(&identity)?)?;
            pipe.connect().await?;
            let mut buffer = [0_u8; 4096];
            let count = pipe.read(&mut buffer).await?;
            ensure!(count != 0, "ADB bridge pipe probe received an empty stream");
            pipe.write_all(&buffer[..count]).await?;
            pipe.flush().await?;
            Ok(())
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (endpoint, marker);
        bail!("ADB bridge pipe probe requires Windows")
    }
}

fn quality(root: &Path, evidence_output: Option<&Path>) -> Result<()> {
    require_windows_gnu()?;
    process::process_check(root)?;
    run(root, "git", &["diff", "--check"])?;
    run(root, "git", &["diff", "--cached", "--check"])?;
    let mut format_arguments = vec!["fmt"];
    for package in HD_FORMAT_PACKAGES {
        format_arguments.extend(["--package", package]);
    }
    format_arguments.extend(["--", "--check"]);
    run(root, "cargo", &format_arguments)?;
    let target = quality_target_args();
    let mut check = vec!["check", "--workspace", "--all-targets"];
    check.extend(target.iter().copied());
    run(root, "cargo", &check)?;
    let mut clippy = vec!["clippy", "--workspace", "--all-targets"];
    clippy.extend(target.iter().copied());
    clippy.extend(["--", "-D", "warnings"]);
    run(root, "cargo", &clippy)?;
    // The quality driver is already running from this target directory. Building
    // it again would try to replace the in-use executable on Windows.
    let mut build = vec!["build", "--workspace", "--bins", "--exclude", "xtask"];
    build.extend(target.iter().copied());
    run(root, "cargo", &build)?;
    smoke(root, evidence_output)
}

fn require_windows_gnu() -> Result<()> {
    if cfg!(all(windows, not(target_env = "gnu"))) {
        bail!(
            "Windows developer tasks must run as x86_64-pc-windows-gnu; use cargo run --target \
             x86_64-pc-windows-gnu -p xtask -- <task>"
        );
    }
    Ok(())
}

fn quality_target_args() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["--target", "x86_64-pc-windows-gnu"]
    } else {
        Vec::new()
    }
}

fn secure_tempdir() -> Result<tempfile::TempDir> {
    let root = std::env::temp_dir()
        .canonicalize()
        .context("canonicalize system temporary directory")?;
    tempfile::Builder::new()
        .prefix("hd-")
        .tempdir_in(root)
        .context("create HD temporary directory")
}

struct SmokeEvidence {
    output: PathBuf,
    gates: BTreeMap<String, process::GateRecord>,
}

impl SmokeEvidence {
    fn new(root: &Path, output: &Path) -> Result<Self> {
        let output = if output.is_absolute() {
            output.to_owned()
        } else {
            root.join(output)
        };
        std::fs::create_dir_all(output.join("artifacts"))?;
        std::fs::create_dir_all(output.join("logs"))?;
        Ok(Self {
            output,
            gates: BTreeMap::new(),
        })
    }

    fn write_artifact(&self, name: &str, bytes: &[u8]) -> Result<()> {
        std::fs::write(self.output.join("artifacts").join(name), bytes)
            .with_context(|| format!("write smoke evidence artifact {name}"))
    }

    fn write_json_artifact(&self, name: &str, value: &impl serde::Serialize) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        self.write_artifact(name, &bytes)
    }

    fn pass(&mut self, name: &str, started: Instant, summary: &str) -> Result<()> {
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let log_relative = format!("logs/{name}.log");
        std::fs::write(
            self.output.join(&log_relative),
            format!("command: xtask smoke\nstatus: pass\nduration_ms: {duration_ms}\nsummary: {summary}\n"),
        )
        .with_context(|| format!("write smoke gate log {name}"))?;
        let previous = self.gates.insert(
            name.to_owned(),
            process::GateRecord {
                name: name.to_owned(),
                command: "xtask smoke".to_owned(),
                status: process::GateStatus::Pass,
                duration_ms: Some(duration_ms),
                log_path: Some(log_relative),
                summary: summary.to_owned(),
            },
        );
        ensure!(previous.is_none(), "smoke gate {name} was recorded twice");
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        let mut gates = Vec::new();
        for name in [
            "host-worker-smoke",
            "capability-smoke",
            "http-security-smoke",
            "ui-snapshot-performance-smoke",
            "ui-background-work-performance-smoke",
            "lease-smoke",
            "diagnostic-smoke",
            "android-artifact-selection-smoke",
            "microdroid-exit-contract-smoke",
            "runtime-storage-smoke",
            "location-route-smoke",
            "trackpad-smoke",
            "trackpad-queue-smoke",
        ] {
            gates.push(
                self.gates
                    .remove(name)
                    .with_context(|| format!("smoke gate {name} was not recorded"))?,
            );
        }
        ensure!(self.gates.is_empty(), "unexpected smoke gate was recorded");
        process::write_smoke_gate_report(&self.output, gates)
    }
}

#[allow(clippy::too_many_lines)]
fn smoke(root: &Path, evidence_output: Option<&Path>) -> Result<()> {
    let android_artifact_selection_smoke_executable =
        ensure_android_artifact_selection_smoke_binary(root)?;
    let microdroid_exit_smoke_executable = ensure_microdroid_exit_smoke_binary(root)?;
    let runtime_storage_smoke_executable = ensure_runtime_storage_smoke_binary(root)?;
    let location_route_smoke_executable = ensure_location_route_smoke_binary(root)?;
    let trackpad_smoke_executable = ensure_trackpad_smoke_binary(root)?;
    let trackpad_queue_smoke_executable = ensure_trackpad_queue_smoke_binary(root)?;
    let worker_executable = ensure_worker_binary(root)?;
    let device_sim_executable = ensure_device_sim_binary(root)?;
    #[cfg(windows)]
    let adb_bridge_executable = ensure_adb_bridge_binary(root)?;
    let casimir_adapter_executable = ensure_casimir_adapter_binary(root)?;
    #[cfg(any(windows, target_os = "macos"))]
    let rootcanal_adapter_executable = ensure_rootcanal_adapter_binary(root)?;
    #[cfg(windows)]
    let frame_producer_executable = ensure_frame_producer_binary(root)?;
    #[cfg(any(windows, target_os = "macos"))]
    let peripheral_adapter_executables = ensure_peripheral_adapter_binaries(root)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hd-smoke")
        .build()
        .context("create smoke runtime")?;
    runtime.block_on(async {
        let mut evidence = evidence_output
            .map(|output| SmokeEvidence::new(root, output))
            .transpose()?;
        let temporary = secure_tempdir()?;
        let temporary_root = temporary
            .path()
            .canonicalize()
            .context("canonicalize smoke data directory")?;
        let android_artifact_selection_started = Instant::now();
        let android_artifact_selection_evidence = run_android_artifact_selection_smoke(
            &android_artifact_selection_smoke_executable,
        )?;
        if let Some(evidence) = evidence.as_mut() {
            evidence.write_json_artifact(
                "android-artifact-selection-smoke.json",
                &android_artifact_selection_evidence,
            )?;
            evidence.pass(
                "android-artifact-selection-smoke",
                android_artifact_selection_started,
                "Host-architecture-specific Android product selection, AOSP fstab alternatives, and no cross-architecture fallback verified",
            )?;
        }
        let microdroid_exit_started = Instant::now();
        let microdroid_exit_evidence =
            run_microdroid_exit_smoke(&microdroid_exit_smoke_executable)?;
        if let Some(evidence) = evidence.as_mut() {
            evidence.write_json_artifact(
                "microdroid-exit-contract-smoke.json",
                &microdroid_exit_evidence,
            )?;
            evidence.pass(
                "microdroid-exit-contract-smoke",
                microdroid_exit_started,
                "strict host-only AOSP vm completion, payload exit retention and guest-log spoof isolation verified",
            )?;
        }
        let storage_started = Instant::now();
        let storage_output = temporary_root.join("runtime-storage-smoke.json");
        let storage_evidence =
            run_runtime_storage_smoke(&runtime_storage_smoke_executable, &storage_output)?;
        if let Some(evidence) = evidence.as_mut() {
            evidence.write_json_artifact("runtime-storage-smoke.json", &storage_evidence)?;
            evidence.pass(
                "runtime-storage-smoke",
                storage_started,
                "finalized-run log compaction, bounded run retention, active-run protection and restart disk headroom verified",
            )?;
        }
        let location_route_started = Instant::now();
        let location_route_evidence = run_location_route_smoke(&location_route_smoke_executable)?;
        if let Some(evidence) = evidence.as_mut() {
            evidence.write_json_artifact("location-route-smoke.json", &location_route_evidence)?;
            evidence.pass(
                "location-route-smoke",
                location_route_started,
                "bounded GPX/KML parsing, coordinate conversion, XML hardening and maximum IPC payload verified",
            )?;
        }
        let trackpad_started = Instant::now();
        let trackpad_evidence = run_trackpad_smoke(&trackpad_smoke_executable)?;
        if let Some(evidence) = evidence.as_mut() {
            evidence.write_json_artifact("trackpad-smoke.json", &trackpad_evidence)?;
            evidence.pass(
                "trackpad-smoke",
                trackpad_started,
                "owner-only endpoint and ordered little-endian virtio-input trackpad report verified",
            )?;
        }
        let trackpad_queue_started = Instant::now();
        let trackpad_queue_evidence =
            run_trackpad_queue_smoke(&trackpad_queue_smoke_executable)?;
        if let Some(evidence) = evidence.as_mut() {
            evidence.write_json_artifact(
                "trackpad-queue-smoke.json",
                &trackpad_queue_evidence,
            )?;
            evidence.pass(
                "trackpad-queue-smoke",
                trackpad_queue_started,
                "bounded queue, latest-MOVE coalescing, release reservation and DOWN-instance pinning verified",
            )?;
        }
        let lease_started = Instant::now();
        let lease_audit = lease_multi_instance_smoke(&temporary_root)?;
        if let Some(evidence) = evidence.as_mut() {
            evidence.write_artifact("lease-audit.jsonl", &lease_audit)?;
            evidence.pass(
                "lease-smoke",
                lease_started,
                "multi-instance leases, monotonic frame generation, audit and release verified",
            )?;
        }
        let host_worker_started = Instant::now();
        let paths = DataPaths::from_root(temporary_root.join("data"));
        paths.ensure()?;
        let legacy_id = Uuid::new_v4();
        let legacy_dir = paths.instance_dir(legacy_id);
        std::fs::create_dir_all(&legacy_dir)?;
        hd_platform::write_owner_only(
            &paths.legacy_instance_config(legacy_id),
            legacy_fixture(legacy_id).as_bytes(),
        )?;

        let host = HostService::open(paths.clone(), None).await?;
        ensure!(
            HostService::open(paths.clone(), None).await.is_err(),
            "a second host unexpectedly acquired the same data root"
        );
        let server_host = Arc::clone(&host);
        let server = tokio::spawn(async move { run_host_http(server_host, None).await });
        wait_for_file(&paths.host_runtime_descriptor(), Duration::from_secs(5)).await?;
        validate_openapi_contract(&paths.root.join("openapi-v2.json"))?;
        if let Some(evidence) = evidence.as_ref() {
            evidence.write_artifact(
                "openapi-v2.json",
                &std::fs::read(paths.root.join("openapi-v2.json"))?,
            )?;
        }
        let client = HostClientV2::connect(paths.clone()).await?;
        let descriptor = client.descriptor().clone();

        let health = client.health().await?;
        ensure!(health.pid == std::process::id(), "health PID mismatch");
        let capability_started = Instant::now();
        let first_capabilities = client.capabilities(None).await?;
        let second_capabilities = client.capabilities(None).await?;
        validate_capability_contract(&first_capabilities)?;
        ensure!(
            first_capabilities.fingerprint == second_capabilities.fingerprint,
            "capability fingerprint changed across consecutive dynamic resource samples"
        );
        let migrated = client.get_instance(legacy_id).await?;
        ensure!(
            migrated.spec.schema_version == 2,
            "legacy config was not migrated"
        );
        let resource_started = Instant::now();
        let resource_admission = client.resource_admission(legacy_id).await?;
        ensure!(
            resource_started.elapsed() <= Duration::from_secs(2),
            "resource-only admission exceeded the two second control-plane budget"
        );
        ensure!(
            resource_admission.id == "host.resources"
                && resource_admission
                    .properties
                    .contains_key("required_disk_bytes")
                && resource_admission
                    .properties
                    .contains_key("disk_requirement_mode"),
            "resource-only admission did not return the isolated resource probe"
        );
        ensure!(
            paths.migration_backup(legacy_id).is_file(),
            "migration backup is missing"
        );
        if let Some(evidence) = evidence.as_ref() {
            evidence.write_json_artifact(
                "migration-smoke.json",
                &serde_json::json!({
                    "schema_version": 2,
                    "legacy_instance_id": legacy_id,
                    "migrated_schema_version": migrated.spec.schema_version,
                    "backup_present": true
                }),
            )?;
        }

        let spec = InstanceSpecV2 {
            name: "HD contract smoke".to_owned(),
            ..InstanceSpecV2::default()
        };
        let id = spec.id;
        client
            .create_instance(&CreateInstanceRequestV2 { spec })
            .await?;
        ensure!(
            client.list_instances().await?.len() == 2,
            "instance list mismatch"
        );
        let ui_snapshot_started = Instant::now();
        let ui_snapshot = client.ui_snapshot(Some(id)).await?;
        let ui_snapshot_duration = ui_snapshot_started.elapsed();
        ensure!(
            ui_snapshot_duration <= Duration::from_secs(2),
            "atomic UI snapshot exceeded the two second control-plane budget"
        );
        let selected = ui_snapshot
            .selected
            .as_ref()
            .context("atomic UI snapshot omitted the requested instance")?;
        ensure!(
            selected.spec.id == id
                && ui_snapshot.summaries.len() == 2
                && ui_snapshot.summaries.iter().any(|summary| {
                    summary.id == selected.spec.id
                        && summary.status == selected.status
                        && summary.frame_generation == selected.frame_generation
                }),
            "atomic UI snapshot returned a torn list/detail revision"
        );
        let fallback_snapshot = client.ui_snapshot(Some(Uuid::new_v4())).await?;
        ensure!(
            fallback_snapshot.selected.as_ref().map(|record| record.spec.id)
                == fallback_snapshot.summaries.first().map(|summary| summary.id),
            "missing UI selection did not deterministically fall back to the first instance"
        );
        if let Some(evidence) = evidence.as_mut() {
            evidence.write_json_artifact(
                "ui-snapshot-performance.json",
                &serde_json::json!({
                    "schema_version": 1,
                    "endpoint": "GET /v2/ui-snapshot",
                    "duration_ms": u64::try_from(ui_snapshot_duration.as_millis()).unwrap_or(u64::MAX),
                    "budget_ms": 2000,
                    "host_requests_per_refresh": 1,
                    "summary_count": ui_snapshot.summaries.len(),
                    "selected_id": selected.spec.id,
                    "atomic_revision": true,
                    "fallback_selection": true
                }),
            )?;
            evidence.pass(
                "ui-snapshot-performance-smoke",
                ui_snapshot_started,
                "one bounded Host request returned summaries and selected detail from one store enumeration with deterministic fallback",
            )?;
        }
        let ui_background_started = Instant::now();
        let startup_source = UI_SHELL_SOURCE
            .split("event_loop.run")
            .next()
            .context("desktop shell has no native event loop")?;
        ensure!(
            !startup_source.contains("request_network_setup_status"),
            "desktop shell performs an unconditional network status probe at startup"
        );
        ensure!(
            UI_CONTRACT_SOURCE.contains("pub enum NetworkStatusPollMode")
                && UI_CONTRACT_SOURCE.contains("ForegroundDevices")
                && UI_CONTRACT_SOURCE.contains("Self::Suspended => None")
                && UI_SHELL_SOURCE.contains("request_network_setup_status(false)")
                && UI_SHELL_SOURCE.contains("request_network_setup_status(true)")
                && UI_SHELL_SOURCE.contains("ui.network_status.hidden_refresh_rejected")
                && UI_SHELL_SOURCE.contains("ui.network_status.hidden_install_rejected")
                && UI_SHELL_SOURCE.contains("ui.network_status.probe.started"),
            "network status background work is not guarded by the on-demand Devices-page policy"
        );
        if let Some(evidence) = evidence.as_mut() {
            evidence.write_json_artifact(
                "ui-background-work-performance.json",
                &serde_json::json!({
                    "schema_version": 1,
                    "operation": "macOS network status external process",
                    "startup_probe": false,
                    "legacy_player_probes_per_hour": 120,
                    "current_player_probes_per_hour": 0,
                    "foreground_devices_interval_ms": 30000,
                    "foreground_devices_max_probes_per_hour": 120,
                    "background_suspended": true,
                    "minimized_suspended": true,
                    "manual_refresh_preserved": true,
                    "install_completion_refresh_preserved": true,
                    "stale_page_result_rejected": true
                }),
            )?;
            evidence.pass(
                "ui-background-work-performance-smoke",
                ui_background_started,
                "startup, Player, auxiliary, background and minimized network probes are suspended; visible Devices refresh remains bounded to 30 seconds",
            )?;
        }
        let first_instance_capabilities = client.capabilities(Some(id)).await?;
        let second_instance_capabilities = client.capabilities(Some(id)).await?;
        ensure!(
            first_instance_capabilities.fingerprint == second_instance_capabilities.fingerprint,
            "instance capability fingerprint changed across consecutive live probes"
        );
        ensure!(
            client.capabilities(None).await?.fingerprint == first_capabilities.fingerprint,
            "instance capability discovery contaminated the global host snapshot"
        );
        if let Some(evidence) = evidence.as_ref() {
            evidence.write_json_artifact("host-capabilities.json", &first_capabilities)?;
        }

        let http_started = Instant::now();
        http_security_smoke(&descriptor).await?;

        let first = client
            .create_operation(id, OperationKindV2::Start, "smoke-start-stable")
            .await?;
        let repeated = client
            .create_operation(id, OperationKindV2::Start, "smoke-start-stable")
            .await?;
        ensure!(
            first.id == repeated.id,
            "idempotency did not return the same operation"
        );
        ensure!(
            client
                .wait_operation(first.id, Duration::from_secs(30))
                .await
                .is_err(),
            "uncertified start unexpectedly succeeded"
        );
        let blocked = client.get_instance(id).await?;
        ensure!(
            blocked.status.observed == ObservedStateV2::Blocked,
            "uncertified start did not enter Blocked"
        );
        if let Some(evidence) = evidence.as_mut() {
            evidence.pass(
                "capability-smoke",
                capability_started,
                "stable capability fingerprints and uncertified-start hard block verified",
            )?;
        }
        let typed_action = client
            .action(
                id,
                InstanceActionV2::Key {
                    key: KeyActionV2::Home,
                },
            )
            .await;
        ensure!(
            matches!(
                typed_action,
                Err(ClientError::Api { status, .. })
                    if status == reqwest::StatusCode::CONFLICT
            ),
            "typed action did not preserve its stopped-instance conflict boundary"
        );
        let invalid_action = client
            .action(
                id,
                InstanceActionV2::SetLocation {
                    location: hd_core::LocationV2 {
                        latitude_e7: i32::MAX,
                        longitude_e7: 0,
                        altitude_mm: 0,
                        accuracy_mm: 5_000,
                    },
                },
            )
            .await;
        ensure!(
            matches!(
                invalid_action,
                Err(ClientError::Api { status, error })
                    if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
                        && error.code == "action_invalid"
            ),
            "invalid typed action did not preserve its validation boundary"
        );

        let stop = client
            .create_operation(
                id,
                OperationKindV2::Stop {
                    mode: StopModeV2::Graceful,
                    graceful_timeout_ms: 1_000,
                },
                "smoke-stop-blocked",
            )
            .await?;
        client
            .wait_operation(stop.id, Duration::from_secs(30))
            .await?;
        ensure!(
            host.store().list_leases()?.is_empty(),
            "leases remained after stop"
        );

        let apk = temporary_root.join("Google Play 商店 contract.apk");
        std::fs::write(&apk, minimal_apk()?)?;
        let upload = client.upload_apk(&apk).await?;
        ensure!(upload.path.is_file(), "streamed APK upload is missing");
        if let Some(evidence) = evidence.as_mut() {
            evidence.pass(
                "http-security-smoke",
                http_started,
                "bearer, CORS/Origin, Host, size, idempotency, typed action and upload boundaries verified",
            )?;
        }

        let diagnostic_started = Instant::now();
        let diagnostic = client
            .collect_diagnostics(&DiagnosticRequestV2 {
                instance_id: Some(id),
                include_guest_logs: false,
            })
            .await?;
        ensure!(diagnostic.path.is_file(), "diagnostic archive is missing");
        ensure!(
            hd_runtime::sha256_file(&diagnostic.path)? == diagnostic.archive_sha256,
            "diagnostic archive hash did not round-trip"
        );
        let diagnostic_manifest = extract_diagnostic_manifest(&diagnostic.path)?;
        ensure!(
            hex::encode(Sha256::digest(&diagnostic_manifest)) == diagnostic.manifest_sha256,
            "diagnostic manifest hash did not round-trip"
        );
        if let Some(evidence) = evidence.as_mut() {
            evidence.write_artifact("diagnostic-manifest.json", &diagnostic_manifest)?;
            evidence.pass(
                "diagnostic-smoke",
                diagnostic_started,
                "archive, manifest and content hashes verified",
            )?;
        }

        for instance_id in [id, legacy_id] {
            let delete = client
                .create_operation(
                    instance_id,
                    OperationKindV2::Delete,
                    &format!("smoke-delete-{instance_id}"),
                )
                .await?;
            client
                .wait_operation(delete.id, Duration::from_secs(30))
                .await?;
        }
        ensure!(
            client.list_instances().await?.is_empty(),
            "instances remained after delete"
        );
        client.shutdown(false).await?;
        server.await.context("join smoke HTTP server")??;
        ensure!(
            !paths.host_runtime_descriptor().exists(),
            "runtime descriptor remained after shutdown"
        );
        let mut lifecycle = vec![managed_process_tree_smoke(root).await?];
        #[cfg(windows)]
        lifecycle.push(formal_frame_probe_smoke(&frame_producer_executable)?);
        lifecycle.push(device_launch_contract_smoke(root).await?);
        lifecycle.push(bundle_publish_smoke()?);
        lifecycle.push(
            formal_device_component_smoke(
                root,
                &device_sim_executable,
                "hd-device-sim",
                BTreeMap::new(),
                &[],
                None,
            )
            .await?,
        );
        #[cfg(unix)]
        lifecycle.push(
            formal_device_sim_location_smoke_unix(root, &device_sim_executable).await?,
        );
        #[cfg(windows)]
        {
            let peer_id = Uuid::new_v4();
            let beacon_id = Uuid::new_v4();
            let scripted_beacon_id = Uuid::new_v4();
            let hid_keyboard_id = Uuid::new_v4();
            let hci_capture_id = Uuid::new_v4();
            lifecycle.push(
                formal_device_component_smoke(
                    root,
                    &rootcanal_adapter_executable,
                    "rootcanal-adapter",
                    BTreeMap::from([(
                        "bluetooth".to_owned(),
                        DeviceSerialEndpointV2 {
                            guest_output: format!(
                                r"\\.\pipe\bscp-hd-rootcanal-smoke-{}-out",
                                Uuid::new_v4()
                            ),
                            guest_input: format!(
                                r"\\.\pipe\bscp-hd-rootcanal-smoke-{}-in",
                                Uuid::new_v4()
                            ),
                        },
                    )]),
                    &[
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::CreateGattPeer {
                                peer_id,
                                name: "HD RootCanal GATT Peer".to_owned(),
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::CreateBeacon {
                                peer_id: beacon_id,
                                name: "HD RootCanal Beacon".to_owned(),
                                advertising_data_hex: "02010605ff4c000215".to_owned(),
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::CreateScriptedBeacon {
                                peer_id: scripted_beacon_id,
                                name: "HD RootCanal Scripted Beacon".to_owned(),
                                frames: scripted_beacon_frames(),
                                repeat: true,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::CreateHidKeyboard {
                                peer_id: hid_keyboard_id,
                                name: "HD RootCanal HID Keyboard".to_owned(),
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::CaptureHci {
                                capture_id: hci_capture_id,
                                duration_ms: 1_000,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::SetAdvertising {
                                peer_id: scripted_beacon_id,
                                enabled: false,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::SetAdvertising {
                                peer_id: scripted_beacon_id,
                                enabled: true,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::RemovePeer {
                                peer_id: scripted_beacon_id,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::SetAdvertising {
                                peer_id: beacon_id,
                                enabled: false,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::SetAdvertising {
                                peer_id: beacon_id,
                                enabled: true,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::RemovePeer { peer_id: beacon_id },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::RemovePeer { peer_id },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::RemovePeer {
                                peer_id: hid_keyboard_id,
                            },
                        },
                    ],
                    None,
                )
                .await?,
            );
        }
        #[cfg(windows)]
        lifecycle.push(
            formal_device_component_smoke(
                root,
                &casimir_adapter_executable,
                "casimir-adapter",
                BTreeMap::from([(
                    "nfc".to_owned(),
                    DeviceSerialEndpointV2 {
                        guest_output: format!(
                            r"\\.\pipe\bscp-hd-casimir-smoke-{}-out",
                            Uuid::new_v4()
                        ),
                        guest_input: format!(
                            r"\\.\pipe\bscp-hd-casimir-smoke-{}-in",
                            Uuid::new_v4()
                        ),
                    },
                )]),
                &[
                    InstanceActionV2::NfcTag {
                        action: NfcTagActionV2::PresentType2 {
                            ndef_hex: "d1010a5402656e484420543254".to_owned(),
                        },
                    },
                    InstanceActionV2::NfcTag {
                        action: NfcTagActionV2::PresentType4 {
                            ndef_hex: "d1010a5402656e484420543454".to_owned(),
                        },
                    },
                    InstanceActionV2::NfcTag {
                        action: NfcTagActionV2::Remove,
                    },
                ],
                None,
            )
            .await?,
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            lifecycle.push(
                formal_uwb_component_smoke_unix(
                    root,
                    &peripheral_adapter_executables["uwb-adapter"],
                )
                .await?,
            );
            lifecycle.push(
                formal_modem_component_smoke_unix(
                    root,
                    &peripheral_adapter_executables["modem-adapter"],
                )
                .await?,
            );

            let rootcanal_suffix = Uuid::new_v4();
            let bluetooth_output =
                temporary_root.join(format!("rootcanal-{rootcanal_suffix}-out.bin"));
            let bluetooth_input =
                temporary_root.join(format!("rootcanal-{rootcanal_suffix}-in.fifo"));
            let _bluetooth_output_hold = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&bluetooth_output)
                .context("create RootCanal smoke Guest output")?;
            hd_platform::create_owner_only_fifo(&bluetooth_input)?;
            let _bluetooth_input_hold = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&bluetooth_input)
                .context("hold RootCanal smoke Guest input FIFO")?;
            let peer_id = Uuid::new_v4();
            let beacon_id = Uuid::new_v4();
            let scripted_beacon_id = Uuid::new_v4();
            let hid_keyboard_id = Uuid::new_v4();
            let hci_capture_id = Uuid::new_v4();
            lifecycle.push(
                formal_device_component_smoke(
                    root,
                    &rootcanal_adapter_executable,
                    "rootcanal-adapter",
                    BTreeMap::from([(
                        "bluetooth".to_owned(),
                        DeviceSerialEndpointV2 {
                            guest_output: bluetooth_output.to_string_lossy().into_owned(),
                            guest_input: bluetooth_input.to_string_lossy().into_owned(),
                        },
                    )]),
                    &[
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::CreateGattPeer {
                                peer_id,
                                name: "HD RootCanal macOS GATT Peer".to_owned(),
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::CreateBeacon {
                                peer_id: beacon_id,
                                name: "HD RootCanal macOS Beacon".to_owned(),
                                advertising_data_hex: "02010605ff4c000215".to_owned(),
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::CreateScriptedBeacon {
                                peer_id: scripted_beacon_id,
                                name: "HD RootCanal macOS Scripted Beacon".to_owned(),
                                frames: scripted_beacon_frames(),
                                repeat: true,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::CreateHidKeyboard {
                                peer_id: hid_keyboard_id,
                                name: "HD RootCanal macOS HID Keyboard".to_owned(),
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::CaptureHci {
                                capture_id: hci_capture_id,
                                duration_ms: 1_000,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::SetAdvertising {
                                peer_id: scripted_beacon_id,
                                enabled: false,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::SetAdvertising {
                                peer_id: scripted_beacon_id,
                                enabled: true,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::RemovePeer {
                                peer_id: scripted_beacon_id,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::SetAdvertising {
                                peer_id: beacon_id,
                                enabled: false,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::SetAdvertising {
                                peer_id: beacon_id,
                                enabled: true,
                            },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::RemovePeer { peer_id: beacon_id },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::RemovePeer { peer_id },
                        },
                        InstanceActionV2::BluetoothPeer {
                            action: BluetoothPeerActionV2::RemovePeer {
                                peer_id: hid_keyboard_id,
                            },
                        },
                    ],
                    None,
                )
                .await?,
            );

            let suffix = Uuid::new_v4();
            let guest_output = temporary_root.join(format!("casimir-{suffix}-out.bin"));
            let guest_input = temporary_root.join(format!("casimir-{suffix}-in.fifo"));
            let _output_hold = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&guest_output)
                .context("create Casimir smoke Guest output")?;
            hd_platform::create_owner_only_fifo(&guest_input)?;
            let _input_hold = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&guest_input)
                .context("hold Casimir smoke Guest input FIFO")?;
            lifecycle.push(
                formal_device_component_smoke(
                    root,
                    &casimir_adapter_executable,
                    "casimir-adapter",
                    BTreeMap::from([(
                        "nfc".to_owned(),
                        DeviceSerialEndpointV2 {
                            guest_output: guest_output.to_string_lossy().into_owned(),
                            guest_input: guest_input.to_string_lossy().into_owned(),
                        },
                    )]),
                    &[
                        InstanceActionV2::NfcTag {
                            action: NfcTagActionV2::PresentType2 {
                                ndef_hex: "d1010a5402656e484420543254".to_owned(),
                            },
                        },
                        InstanceActionV2::NfcTag {
                            action: NfcTagActionV2::PresentType4 {
                                ndef_hex: "d1010a5402656e484420543454".to_owned(),
                            },
                        },
                        InstanceActionV2::NfcTag {
                            action: NfcTagActionV2::Remove,
                        },
                    ],
                    None,
                )
                .await?,
            );
        }
        #[cfg(windows)]
        lifecycle.push(formal_adb_bridge_smoke(root, &adb_bridge_executable).await?);
        #[cfg(windows)]
        for (component, role) in [
            ("uwb-adapter", "uwb"),
            ("modem-adapter", "modem"),
            ("network-adapter", "network-control"),
            ("audio-adapter", "audio-control"),
            ("camera-adapter", "camera-control"),
        ] {
            let actions = if component == "uwb-adapter" {
                vec![InstanceActionV2::SetUwbRanging {
                    ranging: UwbRangingV2 { distance_cm: 321 },
                }]
            } else if component == "modem-adapter" {
                vec![InstanceActionV2::SetModemState {
                    modem: modem_runtime_smoke_state(),
                }]
            } else {
                Vec::new()
            };
            let event = if matches!(component, "uwb-adapter" | "modem-adapter") {
                formal_peripheral_component_smoke(
                    root,
                    &peripheral_adapter_executables[component],
                    component,
                    role,
                    &actions,
                )
                .await?
            } else {
                nonformal_peripheral_component_smoke(
                    &peripheral_adapter_executables[component],
                    component,
                )?
            };
            lifecycle.push(event);
        }
        lifecycle.extend(worker_process_smoke(&worker_executable).await?);
        if let Some(evidence) = evidence.as_mut() {
            let mut journal = Vec::new();
            for event in lifecycle {
                serde_json::to_writer(&mut journal, &event)?;
                journal.push(b'\n');
            }
            evidence.write_artifact("lifecycle-journal.jsonl", &journal)?;
            evidence.pass(
                "host-worker-smoke",
                host_worker_started,
                "host exclusivity/shutdown, detached worker recovery, signed bundle publication, fixed Guest device endpoints, authenticated device lifecycle/Guest channel exchange and ADB TCP/vsock forwarding verified",
            )?;
        }
        if let Some(evidence) = evidence {
            evidence.finish()?;
        }
        println!("HD V2 contract, process separation and blocked-release smoke passed");
        Ok(())
    })
}

fn lease_multi_instance_smoke(root: &Path) -> Result<Vec<u8>> {
    let paths = DataPaths::from_root(root.join("lease-data"));
    paths.ensure()?;
    let store = PersistentStore::open(&paths.database())?;
    let leases = LeaseManager::new(store.clone(), paths.clone())?;
    let first = InstanceSpecV2 {
        name: "HD lease smoke A".to_owned(),
        cpu_count: 1,
        memory_mib: 2048,
        ..InstanceSpecV2::default()
    };
    let second = InstanceSpecV2 {
        name: "HD lease smoke B".to_owned(),
        cpu_count: 1,
        memory_mib: 2048,
        ..InstanceSpecV2::default()
    };
    let first_run = leases.reserve_start(&first, None, 1)?;
    let second_run = leases.reserve_start(&second, None, 1)?;
    ensure!(
        LeaseManager::frame_generation(&first_run)? == 1
            && LeaseManager::frame_generation(&second_run)? == 1,
        "per-instance frame generation was not independently allocated"
    );
    for kind in [
        LeaseKindV2::GuestCid,
        LeaseKindV2::AdbPort,
        LeaseKindV2::GpuSlot,
    ] {
        ensure!(
            lease_resource(&first_run, kind) != lease_resource(&second_run, kind),
            "global {kind:?} lease collided across instances"
        );
    }
    drop(leases);
    drop(store);
    let store = PersistentStore::open(&paths.database())?;
    let leases = LeaseManager::new(store.clone(), paths.clone())?;
    ensure!(
        !store.list_leases()?.is_empty(),
        "lease state did not survive manager restart"
    );
    leases.release_instance(first.id)?;
    leases.release_instance(second.id)?;
    let restarted = leases.reserve_start(&first, None, 2)?;
    ensure!(
        LeaseManager::frame_generation(&restarted)? == 2,
        "frame generation did not advance across instance restart"
    );
    leases.release_instance(first.id)?;
    ensure!(
        store.list_leases()?.is_empty(),
        "lease smoke leaked resources"
    );
    let audit = hd_platform::read_regular_nofollow_limited(
        &paths.logs.join("lease-audit-v2.jsonl"),
        1024 * 1024,
    )?;
    ensure!(
        audit
            .windows(b"lease.acquired".len())
            .any(|window| window == b"lease.acquired")
            && audit
                .windows(b"lease.released".len())
                .any(|window| window == b"lease.released"),
        "lease audit is missing acquisition or release events"
    );
    Ok(audit)
}

fn validate_capability_contract(capabilities: &HostCapabilitiesV2) -> Result<()> {
    let probes = capabilities
        .probes
        .iter()
        .map(|probe| probe.id.as_str())
        .collect::<BTreeSet<_>>();
    for required in [
        "platform.baseline",
        "hypervisor",
        "host.resources",
        "artifact.trust",
        "artifact.bundles",
        "display.zero_copy",
        "tool.crosvm",
        "tool.adb",
        "adb.bridge",
        "guest.readiness",
        "device.profile",
        "release.certification",
    ] {
        ensure!(
            probes.contains(required),
            "capability discovery omitted {required}"
        );
    }
    ensure!(
        capabilities.devices.profile == "hd-phone-android15-v2",
        "capability discovery returned an unexpected device profile"
    );
    let devices = capabilities
        .devices
        .devices
        .iter()
        .map(|device| device.id.as_str())
        .collect::<BTreeSet<_>>();
    let expected_devices = [
        "bluetooth",
        "nfc",
        "uwb",
        "modem",
        "gnss",
        "sensors",
        "network",
        "audio",
        "camera",
        "power",
        "touchpad",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    ensure!(
        devices == expected_devices,
        "capability discovery device profile is incomplete"
    );
    Ok(())
}

fn lease_resource(leases: &[LeaseV2], kind: LeaseKindV2) -> &str {
    leases
        .iter()
        .find(|lease| lease.kind == kind)
        .map_or("", |lease| lease.resource.as_str())
}

fn validate_openapi_contract(path: &Path) -> Result<()> {
    let bytes = hd_platform::read_regular_nofollow_limited(path, 1024 * 1024)
        .context("read generated OpenAPI contract")?;
    let document: serde_json::Value =
        serde_json::from_slice(&bytes).context("decode generated OpenAPI contract")?;
    ensure!(
        document.get("openapi").and_then(serde_json::Value::as_str) == Some("3.1.0"),
        "generated OpenAPI version is not 3.1.0"
    );
    let paths = document
        .get("paths")
        .and_then(serde_json::Value::as_object)
        .context("generated OpenAPI paths are missing")?;
    let required_paths = [
        "/v2/health",
        "/v2/capabilities",
        "/v2/ui-snapshot",
        "/v2/instances",
        "/v2/instances/{id}",
        "/v2/instances/{id}/resource-admission",
        "/v2/instances/{id}/operations",
        "/v2/instances/{id}/actions",
        "/v2/instances/{id}/display-session",
        "/v2/instances/{id}/screenshots",
        "/v2/instances/{id}/android-bugreports",
        "/v2/instances/{id}/screen-recordings",
        "/v2/operations",
        "/v2/operations/{id}",
        "/v2/uploads/apk",
        "/v2/diagnostics",
        "/v2/events",
        "/v2/openapi.json",
        "/v2/shutdown",
    ];
    ensure!(
        paths.len() == required_paths.len()
            && required_paths
                .iter()
                .all(|route| paths.contains_key(*route)),
        "generated OpenAPI route set does not match the HTTP V2 router"
    );
    let mut operation_ids = BTreeSet::new();
    for (route, item) in paths {
        validate_openapi_path_item(route, item, &mut operation_ids)?;
    }
    let instance_spec = document
        .pointer("/components/schemas/InstanceSpecV2")
        .context("InstanceSpecV2 OpenAPI schema is missing")?;
    let required = instance_spec
        .get("required")
        .and_then(serde_json::Value::as_array)
        .context("InstanceSpecV2 required fields are missing")?;
    let properties = instance_spec
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .context("InstanceSpecV2 properties are missing")?;
    ensure!(
        required.iter().all(|name| name
            .as_str()
            .is_some_and(|name| properties.contains_key(name))),
        "InstanceSpecV2 requires a field without defining its schema"
    );
    validate_openapi_references(&document, &document)?;
    Ok(())
}

fn validate_openapi_path_item(
    route: &str,
    item: &serde_json::Value,
    operation_ids: &mut BTreeSet<String>,
) -> Result<()> {
    let item = item
        .as_object()
        .with_context(|| format!("OpenAPI path item {route} is not an object"))?;
    if route.contains("{id}") {
        let has_id = item
            .get("parameters")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|parameters| {
                parameters.iter().any(|parameter| {
                    parameter.get("name").and_then(serde_json::Value::as_str) == Some("id")
                        && parameter.get("in").and_then(serde_json::Value::as_str) == Some("path")
                        && parameter
                            .get("required")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true)
                })
            });
        ensure!(
            has_id,
            "OpenAPI path {route} lacks its required id parameter"
        );
    }
    for (method, operation) in item {
        if !matches!(method.as_str(), "get" | "post" | "patch" | "delete") {
            continue;
        }
        let operation_id = operation
            .get("operationId")
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("OpenAPI {method} {route} lacks operationId"))?;
        ensure!(
            operation_ids.insert(operation_id.to_owned()),
            "duplicate OpenAPI operationId {operation_id}"
        );
        ensure!(
            operation
                .get("responses")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|responses| !responses.is_empty()),
            "OpenAPI {method} {route} lacks responses"
        );
    }
    Ok(())
}

fn validate_openapi_references(
    document: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<()> {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                validate_openapi_references(document, value)?;
            }
        }
        serde_json::Value::Object(values) => {
            if let Some(reference) = values.get("$ref").and_then(serde_json::Value::as_str) {
                let pointer = reference
                    .strip_prefix('#')
                    .context("OpenAPI contains a non-local schema reference")?;
                ensure!(
                    document.pointer(pointer).is_some(),
                    "OpenAPI reference {reference} does not resolve"
                );
            }
            for value in values.values() {
                validate_openapi_references(document, value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn minimal_apk() -> Result<Vec<u8>> {
    let name = b"AndroidManifest.xml";
    let name_length = u16::try_from(name.len()).context("manifest name exceeds ZIP limits")?;
    let mut bytes = Vec::new();
    push_u32(&mut bytes, 0x0403_4b50);
    push_u16(&mut bytes, 20);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, name_length);
    push_u16(&mut bytes, 0);
    bytes.extend_from_slice(name);
    let directory_offset =
        u32::try_from(bytes.len()).context("test APK local directory exceeds ZIP32 limits")?;
    push_u32(&mut bytes, 0x0201_4b50);
    push_u16(&mut bytes, 20);
    push_u16(&mut bytes, 20);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, name_length);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    bytes.extend_from_slice(name);
    let directory_end =
        u32::try_from(bytes.len()).context("test APK central directory exceeds ZIP32 limits")?;
    let directory_size = directory_end
        .checked_sub(directory_offset)
        .context("test APK central directory offset is invalid")?;
    push_u32(&mut bytes, 0x0605_4b50);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 1);
    push_u32(&mut bytes, directory_size);
    push_u32(&mut bytes, directory_offset);
    push_u16(&mut bytes, 0);
    Ok(bytes)
}

fn extract_diagnostic_manifest(path: &Path) -> Result<Vec<u8>> {
    let source = std::fs::File::open(path)
        .with_context(|| format!("open diagnostic archive {}", path.display()))?;
    let decoder = zstd::Decoder::new(source).context("decode diagnostic zstd stream")?;
    let mut archive = tar::Archive::new(decoder);
    for entry in archive
        .entries()
        .context("read diagnostic archive entries")?
    {
        let mut entry = entry.context("read diagnostic archive entry")?;
        if entry.path()?.as_ref() == Path::new("diagnostic-manifest-v2.json") {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .context("read diagnostic manifest")?;
            return Ok(bytes);
        }
    }
    bail!("diagnostic archive is missing diagnostic-manifest-v2.json")
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn ensure_worker_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name("hd-worker"));
    let mut arguments = vec!["build", "-p", "hd-worker"];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-worker binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

fn ensure_runtime_storage_smoke_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name("hd-runtime-storage-smoke"));
    let mut arguments = vec![
        "build",
        "-p",
        "hd-runtime",
        "--bin",
        "hd-runtime-storage-smoke",
    ];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-runtime-storage-smoke binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

fn ensure_microdroid_exit_smoke_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name(
            "hd-microdroid-exit-contract-smoke",
        ));
    let mut arguments = vec![
        "build",
        "-p",
        "hd-runtime",
        "--bin",
        "hd-microdroid-exit-contract-smoke",
    ];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-microdroid-exit-contract-smoke binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

fn ensure_android_artifact_selection_smoke_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name(
            "hd-android-artifact-selection-smoke",
        ));
    let mut arguments = vec![
        "build",
        "-p",
        "hd-ui",
        "--bin",
        "hd-android-artifact-selection-smoke",
    ];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-android-artifact-selection-smoke binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

fn ensure_location_route_smoke_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name("hd-location-route-smoke"));
    let mut arguments = vec![
        "build",
        "-p",
        "hd-runtime",
        "--bin",
        "hd-location-route-smoke",
    ];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-location-route-smoke binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

fn ensure_trackpad_smoke_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name("hd-trackpad-product-smoke"));
    let mut arguments = vec![
        "build",
        "-p",
        "hd-runtime",
        "--bin",
        "hd-trackpad-product-smoke",
    ];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-trackpad-product-smoke binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

fn ensure_trackpad_queue_smoke_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name("hd-trackpad-queue-smoke"));
    let mut arguments = vec!["build", "-p", "hd-ui", "--bin", "hd-trackpad-queue-smoke"];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-trackpad-queue-smoke binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

fn run_trackpad_smoke(executable: &Path) -> Result<serde_json::Value> {
    let process = Command::new(executable)
        .stdin(Stdio::null())
        .output()
        .context("run hd-trackpad-product-smoke")?;
    ensure!(
        process.status.success(),
        "hd-trackpad-product-smoke failed: {}",
        String::from_utf8_lossy(&process.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&process.stdout)
        .context("parse hd-trackpad-product-smoke evidence")?;
    ensure!(
        evidence.get("gate").and_then(serde_json::Value::as_str) == Some("trackpad-smoke")
            && evidence.get("status").and_then(serde_json::Value::as_str) == Some("pass"),
        "trackpad smoke did not report a passing contract"
    );
    Ok(evidence)
}

fn run_microdroid_exit_smoke(executable: &Path) -> Result<serde_json::Value> {
    let process = Command::new(executable)
        .stdin(Stdio::null())
        .output()
        .context("run hd-microdroid-exit-contract-smoke")?;
    ensure!(
        process.status.success(),
        "hd-microdroid-exit-contract-smoke failed: {}",
        String::from_utf8_lossy(&process.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&process.stdout)
        .context("parse hd-microdroid-exit-contract-smoke evidence")?;
    ensure!(
        evidence.get("gate").and_then(serde_json::Value::as_str)
            == Some("microdroid-exit-contract-smoke")
            && evidence.get("status").and_then(serde_json::Value::as_str) == Some("pass"),
        "Microdroid exit smoke did not report a passing contract"
    );
    Ok(evidence)
}

fn run_android_artifact_selection_smoke(executable: &Path) -> Result<serde_json::Value> {
    let process = Command::new(executable)
        .stdin(Stdio::null())
        .output()
        .context("run hd-android-artifact-selection-smoke")?;
    ensure!(
        process.status.success(),
        "hd-android-artifact-selection-smoke failed: {}",
        String::from_utf8_lossy(&process.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&process.stdout)
        .context("parse hd-android-artifact-selection-smoke evidence")?;
    ensure!(
        evidence.get("gate").and_then(serde_json::Value::as_str)
            == Some("android-artifact-selection-smoke")
            && evidence.get("status").and_then(serde_json::Value::as_str) == Some("pass"),
        "Android artifact selection smoke did not report a passing contract"
    );
    Ok(evidence)
}

fn run_trackpad_queue_smoke(executable: &Path) -> Result<serde_json::Value> {
    let process = Command::new(executable)
        .stdin(Stdio::null())
        .output()
        .context("run hd-trackpad-queue-smoke")?;
    ensure!(
        process.status.success(),
        "hd-trackpad-queue-smoke failed: {}",
        String::from_utf8_lossy(&process.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&process.stdout)
        .context("parse hd-trackpad-queue-smoke evidence")?;
    ensure!(
        evidence.get("gate").and_then(serde_json::Value::as_str) == Some("trackpad-queue-smoke")
            && evidence.get("status").and_then(serde_json::Value::as_str) == Some("pass"),
        "trackpad queue smoke did not report a passing contract"
    );
    Ok(evidence)
}

fn run_location_route_smoke(executable: &Path) -> Result<serde_json::Value> {
    let process = Command::new(executable)
        .stdin(Stdio::null())
        .output()
        .context("run hd-location-route-smoke")?;
    ensure!(
        process.status.success(),
        "hd-location-route-smoke failed: {}",
        String::from_utf8_lossy(&process.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&process.stdout)
        .context("parse hd-location-route-smoke evidence")?;
    ensure!(
        evidence.get("gate").and_then(serde_json::Value::as_str) == Some("location-route-smoke")
            && evidence.get("status").and_then(serde_json::Value::as_str) == Some("pass"),
        "location route smoke did not report a passing contract"
    );
    Ok(evidence)
}

fn run_runtime_storage_smoke(executable: &Path, output: &Path) -> Result<serde_json::Value> {
    let process = Command::new(executable)
        .arg("--output")
        .arg(output)
        .stdin(Stdio::null())
        .output()
        .context("run hd-runtime-storage-smoke")?;
    ensure!(
        process.status.success(),
        "hd-runtime-storage-smoke failed: {}",
        String::from_utf8_lossy(&process.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(
        &std::fs::read(output)
            .with_context(|| format!("read storage smoke evidence {}", output.display()))?,
    )?;
    ensure!(
        evidence.get("gate").and_then(serde_json::Value::as_str) == Some("runtime-storage-smoke")
            && evidence.get("status").and_then(serde_json::Value::as_str) == Some("pass"),
        "runtime storage smoke did not report a passing contract"
    );
    Ok(evidence)
}

fn ensure_device_sim_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name("hd-device-sim"));
    let mut arguments = vec!["build", "-p", "hd-device-sim"];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-device-sim binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

fn ensure_casimir_adapter_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name("hd-casimir-adapter"));
    let mut arguments = vec!["build", "-p", "hd-casimir-adapter"];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-casimir-adapter binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

#[cfg(any(windows, target_os = "macos"))]
fn ensure_rootcanal_adapter_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name("hd-rootcanal-adapter"));
    let mut arguments = vec!["build", "-p", "hd-rootcanal-adapter"];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-rootcanal-adapter binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

#[cfg(windows)]
fn ensure_adb_bridge_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name("hd-adb-bridge"));
    let mut arguments = vec!["build", "-p", "hd-adb-bridge"];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-adb-bridge binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

#[cfg(windows)]
fn ensure_frame_producer_binary(root: &Path) -> Result<PathBuf> {
    let sibling = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .join(hd_platform::executable_name("hd-frame-producer"));
    let mut arguments = vec!["build", "-p", "hd-frame-producer"];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    ensure!(
        sibling.is_file(),
        "hd-frame-producer binary was not produced at {}",
        sibling.display()
    );
    Ok(sibling)
}

#[cfg(windows)]
fn formal_frame_probe_smoke(executable: &Path) -> Result<serde_json::Value> {
    let output = Command::new(executable)
        .args(["--probe-v2", "--json"])
        .output()
        .context("run hd-frame-producer probe")?;
    ensure!(output.status.success(), "hd-frame-producer probe failed");
    let probe: FrameInteropProbeV2 = serde_json::from_slice(&output.stdout)?;
    ensure!(
        probe.supported() && probe.transport == hd_core::FrameTransportKindV2::VulkanWin32,
        "hd-frame-producer did not prove strict Vulkan Win32 interop: {}",
        probe.detail
    );
    Ok(serde_json::json!({
        "schema_version": 2,
        "event": "frame_producer.vulkan_win32_probe",
        "transport": probe.transport,
        "memory_export": probe.memory_export,
        "explicit_sync": probe.explicit_sync,
        "same_adapter": probe.same_adapter,
        "validation_clean": probe.validation_clean,
        "properties": probe.properties
    }))
}

#[cfg(any(windows, target_os = "macos"))]
fn ensure_peripheral_adapter_binaries(root: &Path) -> Result<BTreeMap<&'static str, PathBuf>> {
    let parent = std::env::current_exe()?
        .parent()
        .context("xtask executable has no parent")?
        .to_owned();
    let mut arguments = vec!["build", "-p", "hd-peripheral-adapters"];
    arguments.extend(quality_target_args());
    run(root, "cargo", &arguments)?;
    #[cfg(windows)]
    let components = [
        "uwb-adapter",
        "modem-adapter",
        "network-adapter",
        "audio-adapter",
        "camera-adapter",
    ];
    #[cfg(target_os = "macos")]
    let components = ["uwb-adapter", "modem-adapter"];
    components
        .into_iter()
        .map(|component| {
            let executable = parent.join(hd_platform::executable_name(&format!("hd-{component}")));
            ensure!(
                executable.is_file(),
                "hd-{component} binary was not produced at {}",
                executable.display()
            );
            Ok((component, executable))
        })
        .collect()
}

#[cfg(windows)]
fn nonformal_peripheral_component_smoke(
    executable: &Path,
    component: &str,
) -> Result<serde_json::Value> {
    let probe_output = Command::new(executable)
        .args(["--probe-v2", "--json"])
        .output()
        .with_context(|| format!("run {component} capability probe"))?;
    ensure!(
        probe_output.status.success(),
        "{component} capability probe failed: {}",
        String::from_utf8_lossy(&probe_output.stderr)
    );
    let probe: FormalComponentProbeV2 = serde_json::from_slice(&probe_output.stdout)
        .with_context(|| format!("decode {component} capability probe"))?;
    ensure!(
        probe.protocol_version == COMPONENT_PROTOCOL_VERSION
            && probe.component == component
            && !probe.formal,
        "{component} must remain an explicit non-formal reserved profile"
    );

    let rejected = Command::new(executable)
        .args([
            "--serve-v2",
            "--launch",
            "nonformal-component-must-reject-before-reading-launch.json",
        ])
        .output()
        .with_context(|| format!("run {component} non-formal launch rejection"))?;
    ensure!(
        !rejected.status.success(),
        "{component} launched despite having no formal Guest data plane"
    );
    let rejection = String::from_utf8_lossy(&rejected.stderr);
    ensure!(
        rejection.contains("has no formal Guest data plane"),
        "{component} launch rejection did not expose the stable unsupported reason: {rejection}"
    );
    Ok(serde_json::json!({
        "schema_version": 2,
        "event": "component.nonformal_launch.rejected",
        "component": component,
        "formal": false,
        "probe_features": probe.features,
        "serve_v2_rejected": true,
        "reason": "no formal Guest data plane"
    }))
}

#[allow(clippy::too_many_lines)]
fn scripted_beacon_frames() -> Vec<BluetoothAdvertisementFrameV2> {
    vec![
        BluetoothAdvertisementFrameV2 {
            advertising_data_hex: "02010605ff4c000215".to_owned(),
            duration_ms: 20,
        },
        BluetoothAdvertisementFrameV2 {
            advertising_data_hex: "02010605ff4c000216".to_owned(),
            duration_ms: 20,
        },
    ]
}

#[cfg(windows)]
fn device_contract_host_tools(root: &Path) -> BTreeMap<String, PathBuf> {
    BTreeMap::from([
        (
            "gfxstream-backend".to_owned(),
            root.join("out/device-contract/host/bin/libgfxstream_backend.dll"),
        ),
        (
            "angle-egl".to_owned(),
            root.join("out/device-contract/host/bin/libEGL.dll"),
        ),
        (
            "angle-glesv2".to_owned(),
            root.join("out/device-contract/host/bin/libGLESv2.dll"),
        ),
        (
            "angle-vulkan-loader".to_owned(),
            root.join("out/device-contract/host/bin/vulkan-1.dll"),
        ),
    ])
}

#[cfg(target_os = "macos")]
fn device_contract_host_tools(root: &Path) -> BTreeMap<String, PathBuf> {
    BTreeMap::from([
        (
            "gfxstream-backend".to_owned(),
            root.join("out/device-contract/host/bin/libgfxstream_backend.dylib"),
        ),
        (
            "angle-egl".to_owned(),
            root.join("out/device-contract/host/bin/libEGL.dylib"),
        ),
        (
            "angle-glesv2".to_owned(),
            root.join("out/device-contract/host/bin/libGLESv2.dylib"),
        ),
        (
            "angle-vulkan-loader".to_owned(),
            root.join("out/device-contract/host/bin/libvulkan.dylib"),
        ),
    ])
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn device_contract_host_tools(_root: &Path) -> BTreeMap<String, PathBuf> {
    BTreeMap::new()
}

fn validate_device_launch_contract(
    launch: &hd_core::LaunchPlanV2,
    device_endpoints: &BTreeMap<String, DeviceSerialEndpointV2>,
) -> Result<()> {
    ensure!(
        launch.device_endpoints == *device_endpoints,
        "crosvm launch did not preserve the exact device endpoint map"
    );
    for (role, endpoint) in device_endpoints {
        if role == "modem" {
            ensure!(
                !launch.arguments.iter().any(|argument| {
                    argument.contains(&endpoint.guest_output)
                        || argument.contains(&endpoint.guest_input)
                }),
                "modem must use its CID-scoped host-vsock 9697 endpoint, not a serial channel"
            );
            continue;
        }
        ensure!(
            launch.arguments.iter().any(|argument| {
                argument.contains(&endpoint.guest_output)
                    && argument.contains(&endpoint.guest_input)
            }),
            "crosvm launch did not bind both directions of device role {role}"
        );
    }
    ensure!(
        launch
            .arguments
            .iter()
            .filter(|argument| argument.as_str() == "--serial")
            .count()
            == 21,
        "fixed Guest profile did not preserve its exact serial/virtio-console count"
    );
    ensure!(
        launch
            .arguments
            .iter()
            .filter(|argument| argument.as_str() == "--net")
            .count()
            == 3,
        "fixed Guest profile did not preserve its three stable network device roles"
    );
    Ok(())
}

async fn device_launch_contract_smoke(root: &Path) -> Result<serde_json::Value> {
    let instance_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let endpoint = |role: &str, direction: &str| {
        if cfg!(windows) {
            format!(r"\\.\pipe\bscp-hd-device-contract-{role}-{direction}")
        } else {
            format!("/tmp/bscp-hd-device-contract-{role}-{direction}")
        }
    };
    let device_endpoints = DEVICE_GUEST_ENDPOINT_ROLES_V2
        .into_iter()
        .map(|role| {
            (
                role.to_owned(),
                DeviceSerialEndpointV2 {
                    guest_output: endpoint(role, "out"),
                    guest_input: endpoint(role, "in"),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let context = VmLaunchContextV2 {
        spec: InstanceSpecV2 {
            id: instance_id,
            ..InstanceSpecV2::default()
        },
        run_id,
        guest_cid: 3,
        run_dir: root.join("out/device-contract"),
        disk_overlay: root.join("out/device-contract/rootfs.img"),
        artifacts: ResolvedGuestArtifactsV2 {
            guest_bundle_digest: "00".repeat(32),
            host_bundle_digest: "11".repeat(32),
            guest_bundle_root: root.join("out/device-contract/guest"),
            host_bundle_root: root.join("out/device-contract/host"),
            kernel: root.join("out/device-contract/guest/kernel"),
            initrd: root.join("out/device-contract/guest/initrd"),
            rootfs: root.join("out/device-contract/guest/rootfs.img"),
            android_fstab: root.join("out/device-contract/guest/fstab"),
            sensor_injector: root.join("out/device-contract/guest/hd-sensor-injector"),
            system_image: None,
            vendor_image: None,
            host_tools: device_contract_host_tools(root),
        },
        control_endpoint: endpoint("vm", "control"),
        frame_endpoint: endpoint("frame", "control"),
        keyboard_endpoint: endpoint("keyboard", "control"),
        trackpad_endpoint: None,
        device_endpoints: device_endpoints.clone(),
        device_control_endpoints: BTreeMap::new(),
        adb_host_port: Some(55_555),
    };
    let launch = CrosvmBackend::new(root.join("out/device-contract/crosvm.exe"))
        .build_launch_plan(&context)
        .await?;
    validate_device_launch_contract(&launch, &device_endpoints)?;
    let components = [
        "hd-device-sim",
        "rootcanal-adapter",
        "casimir-adapter",
        "uwb-adapter",
        "modem-adapter",
        "network-adapter",
        "audio-adapter",
        "camera-adapter",
    ];
    let granted_roles = components
        .into_iter()
        .flat_map(device_component_guest_roles_v2)
        .copied()
        .collect::<BTreeSet<_>>();
    let serial_endpoint_roles = DEVICE_GUEST_ENDPOINT_ROLES_V2
        .into_iter()
        .filter(|role| *role != "modem")
        .collect::<BTreeSet<_>>();
    ensure!(
        granted_roles == serial_endpoint_roles,
        "formal device component grants did not cover the exact fixed Guest endpoint profile"
    );
    Ok(serde_json::json!({
        "schema_version": 2,
        "event": "device_profile.launch_contract_verified",
        "instance_id": instance_id,
        "run_id": run_id,
        "guest_endpoint_roles": DEVICE_GUEST_ENDPOINT_ROLES_V2,
        "serial_count": 21,
        "modem_transport": "host-vsock-9697",
        "network_device_count": 3,
        "network_roles": ["cuttlefish_wifi", "cuttlefish_mobile", "ethernet_uplink"]
    }))
}

#[allow(clippy::too_many_lines)]
async fn formal_device_component_smoke(
    root: &Path,
    executable: &Path,
    component: &str,
    guest_endpoints: BTreeMap<String, DeviceSerialEndpointV2>,
    actions: &[InstanceActionV2],
    additional_ready_marker: Option<&Path>,
) -> Result<serde_json::Value> {
    formal_device_component_smoke_with_guest_cid(
        root,
        executable,
        component,
        guest_endpoints,
        actions,
        additional_ready_marker,
        3,
    )
    .await
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn formal_device_component_smoke_with_guest_cid(
    root: &Path,
    executable: &Path,
    component: &str,
    guest_endpoints: BTreeMap<String, DeviceSerialEndpointV2>,
    actions: &[InstanceActionV2],
    additional_ready_marker: Option<&Path>,
    guest_cid: u32,
) -> Result<serde_json::Value> {
    let temporary = secure_tempdir()?;
    let instance_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let launch_path = temporary.path().join(format!("{component}-launch-v2.json"));
    let ready_path = temporary.path().join(format!("{component}-ready-v2.json"));
    let control_token = DeviceControlTokenV2::from_hex("ab".repeat(32))
        .context("construct formal component control token")?;
    #[cfg(windows)]
    let control_endpoint = format!(
        r"\\.\pipe\bscp-hd-{}-{instance_id}-{run_id}-device-component-smoke",
        hd_platform::current_user_scope()?
    );
    #[cfg(unix)]
    let control_endpoint = temporary
        .path()
        .join("device-component.sock")
        .to_string_lossy()
        .into_owned();
    let launch = FormalComponentLaunchV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        component: component.to_owned(),
        instance_id,
        run_id,
        component_ready_marker: ready_path.clone(),
        configuration: FormalComponentConfigurationV2::DeviceAdapter {
            control_endpoint: control_endpoint.clone(),
            control_token: control_token.clone(),
            guest_cid,
            vm_control_endpoint: "unused-by-ping".to_owned(),
            guest_endpoints,
        },
    };
    let launch_bytes = serde_json::to_vec_pretty(&launch)?;
    hd_platform::write_owner_only(&launch_path, &launch_bytes)?;
    let process_spec = ProcessSpec {
        executable: executable.to_owned(),
        arguments: vec![
            "--serve-v2".to_owned(),
            "--launch".to_owned(),
            launch_path.to_string_lossy().into_owned(),
        ],
        environment: BTreeMap::new(),
        working_directory: root.to_owned(),
        stdout_path: temporary.path().join("device-component.stdout.log"),
        stderr_path: temporary.path().join("device-component.stderr.log"),
        latency_sensitive: false,
        kill_on_drop: true,
    };
    #[cfg(unix)]
    let active_control_collision_verified = if matches!(
        component,
        "casimir-adapter" | "rootcanal-adapter" | "uwb-adapter" | "modem-adapter"
    ) {
        let active_listener = std::os::unix::net::UnixListener::bind(&control_endpoint)
            .with_context(|| format!("bind active {component} collision socket"))?;
        let mut collision_process = TokioProcessSupervisor.spawn(&process_spec).await?;
        let collision_identity = WorkerIdentityV2 {
            pid: collision_process.id(),
            process_start_marker: hd_platform::process_start_marker(collision_process.id())?,
            nonce: Uuid::nil(),
        };
        let mut collision_guard = ExactProcessGuard::new(collision_identity);
        let started = Instant::now();
        let collision_exit = loop {
            if let Some(exit) = collision_process.try_wait()? {
                break exit;
            }
            if started.elapsed() >= Duration::from_secs(10) {
                TokioProcessSupervisor
                    .terminate(&mut collision_process)
                    .await?;
                bail!("{component} replaced an active Unix control socket instead of rejecting it");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        collision_guard.disarm();
        ensure!(
            !collision_exit.success,
            "{component} accepted an already-active Unix control socket"
        );
        let collision_stderr = std::fs::read_to_string(&process_spec.stderr_path)?;
        ensure!(
            collision_stderr.contains("already has an active listener"),
            "{component} active-socket rejection did not report the stable cause"
        );
        drop(active_listener);
        ensure!(
            Path::new(&control_endpoint).exists(),
            "active Unix listener did not leave a stale socket for recovery validation"
        );
        true
    } else {
        false
    };
    #[cfg(not(unix))]
    let active_control_collision_verified = false;
    let mut process = TokioProcessSupervisor.spawn(&process_spec).await?;
    let identity = WorkerIdentityV2 {
        pid: process.id(),
        process_start_marker: hd_platform::process_start_marker(process.id())?,
        nonce: Uuid::nil(),
    };
    let mut process_guard = ExactProcessGuard::new(identity.clone());
    wait_for_file(&ready_path, Duration::from_secs(10)).await?;
    let ready: FormalComponentReadyV2 = serde_json::from_slice(
        &hd_platform::read_regular_nofollow_limited(&ready_path, 64 * 1024)?,
    )?;
    ensure!(
        ready.protocol_version == COMPONENT_PROTOCOL_VERSION
            && ready.component == component
            && ready.instance_id == instance_id
            && ready.run_id == run_id
            && ready.launch_sha256 == hex::encode(Sha256::digest(&launch_bytes))
            && ready.pid == identity.pid
            && ready.process_start_marker == identity.process_start_marker,
        "formal device component ready marker did not bind the exact launch and process"
    );
    let rejected_request = DeviceControlRequestV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        request_id: Uuid::new_v4(),
        instance_id,
        run_id,
        bearer_token: DeviceControlTokenV2::from_hex("cd".repeat(32))
            .context("construct rejected formal component control token")?,
        command: DeviceControlCommandV2::Ping,
    };
    let rejected_response: DeviceControlResponseV2 =
        send_device_control_request(&control_endpoint, &rejected_request)
            .await
            .with_context(|| format!("send rejected-bearer request to {component}"))?;
    ensure!(
        !rejected_response.ok
            && rejected_response.request_id == rejected_request.request_id
            && rejected_response
                .error
                .as_ref()
                .is_some_and(|error| error.code == "device_control_auth"),
        "formal device component accepted a request with the wrong bearer token"
    );
    let request = DeviceControlRequestV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        request_id: Uuid::new_v4(),
        instance_id,
        run_id,
        bearer_token: control_token.clone(),
        command: DeviceControlCommandV2::Ping,
    };
    let response: DeviceControlResponseV2 =
        send_device_control_request(&control_endpoint, &request)
            .await
            .with_context(|| format!("send Ping request to {component}"))?;
    ensure!(
        response.protocol_version == COMPONENT_PROTOCOL_VERSION
            && response.request_id == request.request_id
            && response.instance_id == instance_id
            && response.run_id == run_id
            && response.ok
            && response.error.is_none(),
        "formal device component Ping response did not preserve the exact request identity"
    );
    let invalid_action_rejected = if component == "rootcanal-adapter" {
        let invalid = DeviceControlRequestV2 {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id,
            run_id,
            bearer_token: control_token.clone(),
            command: DeviceControlCommandV2::Action {
                action: InstanceActionV2::BluetoothPeer {
                    action: BluetoothPeerActionV2::CreateBeacon {
                        peer_id: Uuid::new_v4(),
                        name: "invalid beacon".to_owned(),
                        advertising_data_hex: "00".to_owned(),
                    },
                },
            },
        };
        let invalid_response: DeviceControlResponseV2 =
            send_device_control_request(&control_endpoint, &invalid).await?;
        ensure!(
            !invalid_response.ok
                && invalid_response
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == "bluetooth_action_invalid"),
            "RootCanal adapter accepted malformed BLE advertising data"
        );
        let invalid_scripted = DeviceControlRequestV2 {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id,
            run_id,
            bearer_token: control_token.clone(),
            command: DeviceControlCommandV2::Action {
                action: InstanceActionV2::BluetoothPeer {
                    action: BluetoothPeerActionV2::CreateScriptedBeacon {
                        peer_id: Uuid::new_v4(),
                        name: "invalid scripted beacon".to_owned(),
                        frames: vec![BluetoothAdvertisementFrameV2 {
                            advertising_data_hex: "020106".to_owned(),
                            duration_ms: 19,
                        }],
                        repeat: false,
                    },
                },
            },
        };
        let invalid_scripted_response: DeviceControlResponseV2 =
            send_device_control_request(&control_endpoint, &invalid_scripted).await?;
        ensure!(
            !invalid_scripted_response.ok
                && invalid_scripted_response
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == "bluetooth_action_invalid"),
            "RootCanal adapter accepted an out-of-bounds scripted Beacon timeline"
        );
        let invalid_capture = DeviceControlRequestV2 {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id,
            run_id,
            bearer_token: control_token.clone(),
            command: DeviceControlCommandV2::Action {
                action: InstanceActionV2::BluetoothPeer {
                    action: BluetoothPeerActionV2::CaptureHci {
                        capture_id: Uuid::new_v4(),
                        duration_ms: 999,
                    },
                },
            },
        };
        let invalid_capture_response: DeviceControlResponseV2 =
            send_device_control_request(&control_endpoint, &invalid_capture).await?;
        ensure!(
            !invalid_capture_response.ok
                && invalid_capture_response
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == "bluetooth_action_invalid"),
            "RootCanal adapter accepted an out-of-bounds HCI capture duration"
        );
        let keyboard_id = Uuid::new_v4();
        let create_keyboard = DeviceControlRequestV2 {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id,
            run_id,
            bearer_token: control_token.clone(),
            command: DeviceControlCommandV2::Action {
                action: InstanceActionV2::BluetoothPeer {
                    action: BluetoothPeerActionV2::CreateHidKeyboard {
                        peer_id: keyboard_id,
                        name: "HD RootCanal unpaired keyboard".to_owned(),
                    },
                },
            },
        };
        let create_keyboard_response: DeviceControlResponseV2 =
            send_device_control_request(&control_endpoint, &create_keyboard).await?;
        ensure!(
            create_keyboard_response.ok && create_keyboard_response.error.is_none(),
            "RootCanal adapter rejected a bounded HID keyboard"
        );
        let unpaired_report = DeviceControlRequestV2 {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id,
            run_id,
            bearer_token: control_token.clone(),
            command: DeviceControlCommandV2::Action {
                action: InstanceActionV2::BluetoothPeer {
                    action: BluetoothPeerActionV2::SendHidKeyboardReport {
                        peer_id: keyboard_id,
                        modifiers: 0,
                        keys: vec![4],
                    },
                },
            },
        };
        let unpaired_report_response: DeviceControlResponseV2 =
            send_device_control_request(&control_endpoint, &unpaired_report).await?;
        ensure!(
            !unpaired_report_response.ok
                && unpaired_report_response
                    .error
                    .as_ref()
                    .is_some_and(|error| {
                        error.code == "bluetooth_action_failed"
                            && error.message.contains("not connected")
                    }),
            "RootCanal adapter accepted a HID report before a Guest connection"
        );
        let malformed_report = DeviceControlRequestV2 {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id,
            run_id,
            bearer_token: control_token.clone(),
            command: DeviceControlCommandV2::Action {
                action: InstanceActionV2::BluetoothPeer {
                    action: BluetoothPeerActionV2::SendHidKeyboardReport {
                        peer_id: keyboard_id,
                        modifiers: 0,
                        keys: vec![4, 5, 6, 7, 8, 9, 10],
                    },
                },
            },
        };
        let malformed_report_response: DeviceControlResponseV2 =
            send_device_control_request(&control_endpoint, &malformed_report).await?;
        ensure!(
            !malformed_report_response.ok
                && malformed_report_response
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == "bluetooth_action_invalid"),
            "RootCanal adapter accepted a HID keyboard report with more than six keys"
        );
        let remove_keyboard = DeviceControlRequestV2 {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id,
            run_id,
            bearer_token: control_token.clone(),
            command: DeviceControlCommandV2::Action {
                action: InstanceActionV2::BluetoothPeer {
                    action: BluetoothPeerActionV2::RemovePeer {
                        peer_id: keyboard_id,
                    },
                },
            },
        };
        let remove_keyboard_response: DeviceControlResponseV2 =
            send_device_control_request(&control_endpoint, &remove_keyboard).await?;
        ensure!(
            remove_keyboard_response.ok && remove_keyboard_response.error.is_none(),
            "RootCanal adapter could not remove the unpaired HID keyboard fixture"
        );
        true
    } else {
        false
    };
    for action in actions {
        let action_request = DeviceControlRequestV2 {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id,
            run_id,
            bearer_token: control_token.clone(),
            command: DeviceControlCommandV2::Action {
                action: action.clone(),
            },
        };
        let action_response: DeviceControlResponseV2 =
            send_device_control_request(&control_endpoint, &action_request)
                .await
                .with_context(|| format!("send conformance action to {component}"))?;
        ensure!(
            action_response.protocol_version == COMPONENT_PROTOCOL_VERSION
                && action_response.request_id == action_request.request_id
                && action_response.instance_id == instance_id
                && action_response.run_id == run_id
                && action_response.ok
                && action_response.error.is_none(),
            "formal device component {component} rejected a conformance action: {:?}",
            action_response.error
        );
        if matches!(
            action,
            InstanceActionV2::BluetoothPeer {
                action: BluetoothPeerActionV2::CreateScriptedBeacon { .. }
            }
        ) {
            tokio::time::sleep(Duration::from_millis(70)).await;
            ensure!(
                hd_platform::process_identity_is_alive(&identity),
                "RootCanal adapter exited while advancing a scripted Beacon timeline"
            );
        }
        if let InstanceActionV2::BluetoothPeer {
            action:
                BluetoothPeerActionV2::CaptureHci {
                    capture_id,
                    duration_ms,
                },
        } = action
        {
            let file_name = format!("rootcanal-hci-{capture_id}.btsnoop");
            let capture_path = temporary.path().join(&file_name);
            let metadata_path = temporary
                .path()
                .join(format!("rootcanal-hci-{capture_id}.json"));
            let capture_metadata = std::fs::symlink_metadata(&capture_path)?;
            let record_metadata = std::fs::symlink_metadata(&metadata_path)?;
            ensure!(
                capture_metadata.is_file()
                    && !capture_metadata.file_type().is_symlink()
                    && record_metadata.is_file()
                    && !record_metadata.file_type().is_symlink()
                    && capture_metadata.len() >= 16
                    && capture_metadata.len() <= 4 * 1024 * 1024,
                "RootCanal HCI capture did not produce bounded regular artifacts"
            );
            let mut header = [0_u8; 16];
            std::fs::File::open(&capture_path)?.read_exact(&mut header)?;
            ensure!(
                &header[..8] == b"btsnoop\0"
                    && u32::from_be_bytes(header[8..12].try_into()?) == 1
                    && u32::from_be_bytes(header[12..16].try_into()?) == 1_002,
                "RootCanal HCI capture did not use the btsnoop HCI UART format"
            );
            let record: BluetoothHciCaptureRecordV2 = serde_json::from_slice(
                &hd_platform::read_regular_nofollow_limited(&metadata_path, 64 * 1024)?,
            )?;
            ensure!(
                record.capture_id == *capture_id
                    && record.file_name == file_name
                    && record.requested_duration_ms == *duration_ms
                    && record.output_size_bytes == capture_metadata.len(),
                "RootCanal HCI capture metadata did not bind the exact request and artifact"
            );
        }
    }
    if let Some(marker) = additional_ready_marker {
        // The Windows modem conformance exchange is deliberately made of several
        // independently bounded phases: endpoint discovery, unsolicited delivery,
        // and three request/response checks.  Keep each phase's strict ten-second
        // timeout, while allowing the outer marker wait to cover their serial
        // budgets under a loaded build host.  A single ten-second outer timeout
        // could otherwise reject a healthy adapter before its later checks ran.
        let exchange_timeout = if component == "modem-adapter" {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(10)
        };
        wait_for_file(marker, exchange_timeout)
            .await
            .with_context(|| format!("wait for {component} Guest exchange marker"))?;
    }
    TokioProcessSupervisor.terminate(&mut process).await?;
    let started = Instant::now();
    while hd_platform::process_identity_is_alive(&identity)
        && started.elapsed() < Duration::from_secs(10)
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    ensure!(
        !hd_platform::process_identity_is_alive(&identity),
        "formal device component remained alive after managed termination"
    );
    process_guard.disarm();
    Ok(serde_json::json!({
        "schema_version": 2,
        "event": "formal_device_component.authenticated_ping_and_termination",
        "component": component,
        "instance_id": instance_id,
        "run_id": run_id,
        "pid": identity.pid,
        "invalid_bearer_rejected": true,
        "invalid_action_rejected": invalid_action_rejected,
        "actions_verified": actions.len(),
        "active_control_collision_verified": active_control_collision_verified,
        "control_transport": if cfg!(windows) { "named_pipe" } else { "unix_socket" }
    }))
}

#[cfg(unix)]
async fn formal_device_sim_location_smoke_unix(
    root: &Path,
    executable: &Path,
) -> Result<serde_json::Value> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let temporary = secure_tempdir()?;
    let suffix = Uuid::new_v4();
    let guest_output = temporary
        .path()
        .join(format!("fixed-location-{suffix}-out.bin"));
    let guest_input = temporary
        .path()
        .join(format!("fixed-location-{suffix}-in.fifo"));
    let _output_hold = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&guest_output)
        .context("create fixed-location smoke Guest output")?;
    hd_platform::create_owner_only_fifo(&guest_input)?;
    let _input_hold = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&guest_input)
        .context("hold fixed-location smoke Guest input FIFO")?;
    let exchange_marker = temporary
        .path()
        .join("fixed-location-guest-exchange-complete");
    let exchange_output = guest_output.clone();
    let exchange_input = guest_input.clone();
    let marker = exchange_marker.clone();
    let exchange = tokio::spawn(async move {
        let mut output = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&exchange_output)
            .await
            .context("open fixed-location smoke Guest output writer")?;
        let mut input = tokio::fs::OpenOptions::new()
            .read(true)
            .open(&exchange_input)
            .await
            .context("open fixed-location smoke Guest input reader")?;
        let expected = "Fix,gps,37.4219999,-122.0840575,123.456,0,7.890";
        let started = Instant::now();
        let mut received = Vec::new();
        loop {
            ensure!(
                started.elapsed() < Duration::from_secs(10),
                "fixed-location Guest channel did not return all configured fields; response={}",
                String::from_utf8_lossy(&received)
            );
            output.write_all(b"CMD_GET_LOCATION").await?;
            output.flush().await?;
            let mut chunk = [0_u8; 512];
            if let Ok(read_result) =
                tokio::time::timeout(Duration::from_millis(250), input.read(&mut chunk)).await
            {
                let count = read_result?;
                received.extend_from_slice(&chunk[..count]);
                if String::from_utf8_lossy(&received).contains(expected) {
                    break;
                }
                if received.len() > 16 * 1024 {
                    received.drain(..received.len() - 8 * 1024);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        hd_platform::write_owner_only(&marker, b"ok")?;
        std::future::pending::<Result<()>>().await
    });
    let event = formal_device_component_smoke(
        root,
        executable,
        "hd-device-sim",
        BTreeMap::from([(
            "location".to_owned(),
            DeviceSerialEndpointV2 {
                guest_output: guest_output.to_string_lossy().into_owned(),
                guest_input: guest_input.to_string_lossy().into_owned(),
            },
        )]),
        &[InstanceActionV2::SetLocation {
            location: hd_core::LocationV2 {
                latitude_e7: 374_219_999,
                longitude_e7: -1_220_840_575,
                altitude_mm: 123_456,
                accuracy_mm: 7_890,
            },
        }],
        Some(&exchange_marker),
    )
    .await;
    exchange.abort();
    let _ = exchange.await;
    let mut event = event?;
    event["fixed_location_guest_exchange_verified"] = serde_json::Value::Bool(true);
    event["fixed_location_altitude_verified"] = serde_json::Value::Bool(true);
    event["fixed_location_accuracy_verified"] = serde_json::Value::Bool(true);
    Ok(event)
}

#[cfg(target_os = "macos")]
fn uwb_ranging_distance(bytes: &[u8]) -> Option<u16> {
    let start = bytes
        .windows(4)
        .position(|header| header == [0x62, 0x00, 0x00, 56])?;
    (bytes.len() >= start + 60).then(|| u16::from_le_bytes([bytes[start + 33], bytes[start + 34]]))
}

#[cfg(any(windows, target_os = "macos"))]
fn modem_runtime_smoke_state() -> ModemStateV2 {
    ModemStateV2 {
        operator_numeric: "310260".to_owned(),
        operator_long_name: "HD Test Mobile".to_owned(),
        operator_short_name: "HDT".to_owned(),
        signal_strength: 17,
        registered: true,
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn verify_modem_unsolicited_sequence(bytes: &[u8]) -> Result<()> {
    let response = std::str::from_utf8(bytes).context("decode modem unsolicited result")?;
    let deregistration = response
        .find("+CREG: 0\r\n+CGREG: 0\r\n+CEREG: 0")
        .context("modem did not publish the bounded deregistration transition")?;
    let registration = response
        .find(
            "+CREG: 1,\"0001\",\"00000001\",7\r\n+CGREG: 1,\"0001\",\"00000001\",7\r\n+CEREG: 1,\"0001\",\"00000001\",7",
        )
        .context("modem did not publish the registered LTE transition")?;
    let signal = response
        .find("+CSQ: 17,2,80,100,70,100,6,70,90,15,100,12,100")
        .context("modem did not publish the updated signal strength")?;
    ensure!(
        deregistration < registration && registration < signal,
        "modem unsolicited results were not ordered deregister, register, signal"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
async fn read_raw_modem_until(
    stream: &mut tokio::net::UnixStream,
    expected: &[&str],
    operation: &'static str,
) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;

    tokio::time::timeout(Duration::from_secs(10), async {
        let mut received = Vec::new();
        loop {
            let mut chunk = [0_u8; 512];
            let count = stream.read(&mut chunk).await?;
            ensure!(count != 0, "modem Guest channel closed during {operation}");
            received.extend_from_slice(&chunk[..count]);
            ensure!(
                received.len() <= 16 * 1024,
                "modem Guest response exceeded the smoke bound during {operation}"
            );
            let text = std::str::from_utf8(&received)?;
            if expected.iter().all(|needle| text.contains(needle)) {
                return Ok::<_, anyhow::Error>(received);
            }
        }
    })
    .await
    .with_context(|| format!("{operation} timed out"))?
}

#[cfg(target_os = "macos")]
async fn query_raw_modem(
    stream: &mut tokio::net::UnixStream,
    request: &[u8],
    expected: &[&str],
    operation: &'static str,
) -> Result<Vec<u8>> {
    use tokio::io::AsyncWriteExt as _;

    stream.write_all(request).await?;
    stream.flush().await?;
    read_raw_modem_until(stream, expected, operation).await
}

#[cfg(target_os = "macos")]
async fn connect_raw_modem(endpoint: &Path) -> Result<tokio::net::UnixStream> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match tokio::net::UnixStream::connect(endpoint).await {
                Ok(stream) => return Ok::<_, anyhow::Error>(stream),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error).context("connect modem host-vsock UDS"),
            }
        }
    })
    .await
    .context("modem host-vsock UDS timed out")?
}

#[cfg(windows)]
async fn read_modem_vsock_pipe_frame<T>(pipe: &mut T) -> Result<Vec<u8>>
where
    T: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;

    let mut header = [0_u8; 8];
    pipe.read_exact(&mut header).await?;
    let payload_size = usize::try_from(u32::from_le_bytes(header[..4].try_into()?))?;
    ensure!(
        payload_size <= 16 * 1024,
        "modem host-vsock response exceeded the smoke bound"
    );
    ensure!(
        u32::from_le_bytes(header[4..].try_into()?) == 0,
        "modem host-vsock response unexpectedly contained handles"
    );
    let mut response = vec![0_u8; payload_size];
    pipe.read_exact(&mut response).await?;
    Ok(response)
}

#[cfg(windows)]
async fn write_modem_vsock_pipe_frame<T>(pipe: &mut T, payload: &[u8]) -> Result<()>
where
    T: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;

    pipe.write_all(&u32::try_from(payload.len())?.to_le_bytes())
        .await?;
    pipe.write_all(&0_u32.to_le_bytes()).await?;
    pipe.write_all(payload).await?;
    pipe.flush().await?;
    Ok(())
}

#[cfg(target_os = "macos")]
async fn formal_uwb_component_smoke_unix(
    root: &Path,
    executable: &Path,
) -> Result<serde_json::Value> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let temporary = secure_tempdir()?;
    let suffix = Uuid::new_v4();
    let guest_output = temporary.path().join(format!("uwb-{suffix}-out.bin"));
    let guest_input = temporary.path().join(format!("uwb-{suffix}-in.fifo"));
    let _output_hold = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&guest_output)
        .context("create UWB smoke Guest output")?;
    hd_platform::create_owner_only_fifo(&guest_input)?;
    let _input_hold = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&guest_input)
        .context("hold UWB smoke Guest input FIFO")?;
    let exchange_marker = temporary.path().join("uwb-guest-exchange-complete");
    let exchange_output = guest_output.clone();
    let exchange_input = guest_input.clone();
    let marker = exchange_marker.clone();
    let exchange = tokio::spawn(async move {
        let mut output = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&exchange_output)
            .await
            .context("open UWB smoke Guest output writer")?;
        let mut input = tokio::fs::OpenOptions::new()
            .read(true)
            .open(&exchange_input)
            .await
            .context("open UWB smoke Guest input reader")?;
        output.write_all(&[0x20, 0x02, 0x00, 0x00]).await?;
        output.flush().await?;
        let mut response = [0_u8; 64];
        let count = tokio::time::timeout(Duration::from_secs(10), input.read(&mut response))
            .await
            .context("UWB Guest response timed out")??;
        ensure!(
            count >= 5 && response[0] == 0x40 && response[1] == 0x02 && response[4] == 0x00,
            "UWB Guest channel did not return CORE_GET_DEVICE_INFO_RSP"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        output
            .write_all(&[0x22, 0x00, 0x00, 0x04, 0x07, 0x00, 0x00, 0x00])
            .await?;
        output.flush().await?;
        let mut ranging = Vec::new();
        let distance_cm = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut chunk = [0_u8; 256];
                let count = input.read(&mut chunk).await?;
                ensure!(count != 0, "UWB Guest ranging channel closed");
                ranging.extend_from_slice(&chunk[..count]);
                if let Some(distance_cm) = uwb_ranging_distance(&ranging) {
                    return Ok::<_, anyhow::Error>(distance_cm);
                }
            }
        })
        .await
        .context("UWB Guest ranging notification timed out")??;
        ensure!(
            distance_cm == 321,
            "UWB runtime control did not reach the Guest ranging notification"
        );
        hd_platform::write_owner_only(&marker, b"ok")?;
        std::future::pending::<Result<()>>().await
    });
    let event = formal_device_component_smoke(
        root,
        executable,
        "uwb-adapter",
        BTreeMap::from([(
            "uwb".to_owned(),
            DeviceSerialEndpointV2 {
                guest_output: guest_output.to_string_lossy().into_owned(),
                guest_input: guest_input.to_string_lossy().into_owned(),
            },
        )]),
        &[InstanceActionV2::SetUwbRanging {
            ranging: UwbRangingV2 { distance_cm: 321 },
        }],
        Some(&exchange_marker),
    )
    .await;
    exchange.abort();
    let _ = exchange.await;
    let mut event = event?;
    event["guest_channel_exchange_verified"] = serde_json::Value::Bool(true);
    event["runtime_distance_cm_verified"] = serde_json::Value::from(321_u16);
    Ok(event)
}

#[cfg(target_os = "macos")]
async fn formal_modem_component_smoke_unix(
    root: &Path,
    executable: &Path,
) -> Result<serde_json::Value> {
    let temporary = secure_tempdir()?;
    let random = Uuid::new_v4();
    let guest_cid =
        1_000_000_000 + u32::from_le_bytes(random.as_bytes()[..4].try_into()?) % 1_000_000_000;
    let endpoint = PathBuf::from(format!("/tmp/binder_rpc_vsock_{guest_cid}_9697.sock"));
    ensure!(
        !endpoint.exists(),
        "randomized modem smoke endpoint unexpectedly exists: {}",
        endpoint.display()
    );
    let exchange_marker = temporary.path().join("modem-guest-exchange-complete");
    let exchange_endpoint = endpoint.clone();
    let marker = exchange_marker.clone();
    let exchange = tokio::spawn(async move {
        let mut stream = connect_raw_modem(&exchange_endpoint).await?;
        let unsolicited = read_raw_modem_until(
            &mut stream,
            &[
                "+CREG: 0\r\n+CGREG: 0\r\n+CEREG: 0",
                "+CREG: 1,\"0001\",\"00000001\",7",
                "+CGREG: 1,\"0001\",\"00000001\",7",
                "+CEREG: 1,\"0001\",\"00000001\",7",
                "+CSQ: 17,2,80,100,70,100,6,70,90,15,100,12,100",
            ],
            "runtime modem unsolicited delivery",
        )
        .await?;
        verify_modem_unsolicited_sequence(&unsolicited)?;

        let response = query_raw_modem(
            &mut stream,
            b"AT+CSQ\r",
            &["+CSQ: 17,2,80,100,70,100,6,70,90,15,100,12,100", "OK"],
            "modem Guest signal query",
        )
        .await?;
        ensure!(
            std::str::from_utf8(&response)?.contains("+CSQ: 17,2"),
            "runtime modem signal did not reach the Guest AT channel"
        );
        let response = query_raw_modem(
            &mut stream,
            b"AT+COPS?\r",
            &["+COPS: 0,2,\"310260\",7", "OK"],
            "modem Guest operator query",
        )
        .await?;
        ensure!(
            std::str::from_utf8(&response)?.contains("+COPS: 0,2,\"310260\",7"),
            "runtime modem operator did not reach the Guest AT channel"
        );
        let response = query_raw_modem(
            &mut stream,
            b"AT+CREG?\r",
            &["+CREG: 2,1,\"0001\",\"00000001\",7", "OK"],
            "modem Guest registration query",
        )
        .await?;
        ensure!(
            std::str::from_utf8(&response)?.contains("+CREG: 2,1,"),
            "runtime modem registration did not reach the Guest AT channel"
        );
        hd_platform::write_owner_only(&marker, b"ok")?;
        std::future::pending::<Result<()>>().await
    });
    let event = formal_device_component_smoke_with_guest_cid(
        root,
        executable,
        "modem-adapter",
        BTreeMap::new(),
        &[InstanceActionV2::SetModemState {
            modem: modem_runtime_smoke_state(),
        }],
        Some(&exchange_marker),
        guest_cid,
    )
    .await;
    exchange.abort();
    let _ = exchange.await;
    let mut event = event?;
    ensure!(
        !endpoint.exists(),
        "modem host-vsock UDS remained after managed termination"
    );
    event["guest_vsock_exchange_verified"] = serde_json::Value::Bool(true);
    event["runtime_signal_strength_verified"] = serde_json::Value::from(17_u8);
    event["runtime_operator_numeric_verified"] = serde_json::Value::from("310260");
    event["runtime_registration_verified"] = serde_json::Value::Bool(true);
    event["runtime_unsolicited_verified"] = serde_json::Value::Bool(true);
    event["guest_cid"] = serde_json::Value::from(guest_cid);
    Ok(event)
}

#[cfg(windows)]
#[allow(clippy::too_many_lines)]
async fn formal_peripheral_component_smoke(
    root: &Path,
    executable: &Path,
    component: &str,
    role: &str,
    actions: &[InstanceActionV2],
) -> Result<serde_json::Value> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    let temporary = secure_tempdir()?;
    let exchange_marker = temporary.path().join("guest-exchange-complete");
    let suffix = Uuid::new_v4();
    let guest_output = format!(r"\\.\pipe\bscp-hd-{component}-smoke-{suffix}-out");
    let guest_input = format!(r"\\.\pipe\bscp-hd-{component}-smoke-{suffix}-in");
    let mut output_options = ServerOptions::new();
    output_options
        .first_pipe_instance(true)
        .reject_remote_clients(true);
    let mut output = hd_platform::create_owner_only_named_pipe(&output_options, &guest_output)?;
    let mut input_options = ServerOptions::new();
    input_options
        .first_pipe_instance(true)
        .reject_remote_clients(true);
    let mut input = hd_platform::create_owner_only_named_pipe(&input_options, &guest_input)?;
    let request = match component {
        "modem-adapter" => b"AT+CSQ\r".to_vec(),
        "uwb-adapter" => vec![0x20, 0x02, 0x00, 0x00],
        _ => br#"{"command":"status","request_id":"smoke"}"#.to_vec(),
    };
    let request_digest = hex::encode(Sha256::digest(&request));
    let expected_modem_signal = actions
        .iter()
        .find_map(|action| match action {
            InstanceActionV2::SetModemState { modem } => Some(modem.signal_strength),
            _ => None,
        })
        .unwrap_or(20);
    let exchange_component = component.to_owned();
    let marker = exchange_marker.clone();
    let exchange = tokio::spawn(async move {
        if exchange_component == "modem-adapter" {
            let endpoint = r"\\.\pipe\binder_rpc_vsock_3_9697";
            let mut pipe = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match ClientOptions::new().open(endpoint) {
                        Ok(pipe) => return Ok::<_, anyhow::Error>(pipe),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::WouldBlock
                            ) || error.raw_os_error() == Some(231) =>
                        {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                        Err(error) => return Err(error).context("open modem host-vsock pipe"),
                    }
                }
            })
            .await
            .context("modem host-vsock pipe timed out")??;
            let unsolicited = tokio::time::timeout(Duration::from_secs(10), async {
                let mut unsolicited = Vec::new();
                for _ in 0..3 {
                    unsolicited.extend(read_modem_vsock_pipe_frame(&mut pipe).await?);
                }
                Ok::<_, anyhow::Error>(unsolicited)
            })
            .await
            .context("runtime modem unsolicited delivery timed out")??;
            verify_modem_unsolicited_sequence(&unsolicited)?;

            write_modem_vsock_pipe_frame(&mut pipe, &request).await?;
            let response = tokio::time::timeout(
                Duration::from_secs(10),
                read_modem_vsock_pipe_frame(&mut pipe),
            )
            .await
            .context("controlled modem signal query timed out")??;
            let response = String::from_utf8(response)?;
            ensure!(
                response.contains(&format!("+CSQ: {expected_modem_signal},2"))
                    && response.contains("OK"),
                "controlled modem signal did not reach the host-vsock Guest channel"
            );

            write_modem_vsock_pipe_frame(&mut pipe, b"AT+COPS?\r").await?;
            let response = tokio::time::timeout(
                Duration::from_secs(10),
                read_modem_vsock_pipe_frame(&mut pipe),
            )
            .await
            .context("controlled modem operator query timed out")??;
            ensure!(
                std::str::from_utf8(&response)?.contains("+COPS: 0,2,\"310260\",7"),
                "controlled modem operator did not reach the host-vsock Guest channel"
            );

            write_modem_vsock_pipe_frame(&mut pipe, b"AT+CREG?\r").await?;
            let response = tokio::time::timeout(
                Duration::from_secs(10),
                read_modem_vsock_pipe_frame(&mut pipe),
            )
            .await
            .context("controlled modem registration query timed out")??;
            ensure!(
                std::str::from_utf8(&response)?.contains("+CREG: 2,1,\"0001\",\"00000001\",7"),
                "controlled modem registration did not reach the host-vsock Guest channel"
            );
        } else {
            output.connect().await?;
            input.connect().await?;
            output.write_all(&request).await?;
            output.flush().await?;
            let mut response = vec![0_u8; 4096];
            let count = tokio::time::timeout(Duration::from_secs(10), input.read(&mut response))
                .await
                .context("peripheral Guest response timed out")??;
            ensure!(count != 0, "peripheral Guest response was empty");
            response.truncate(count);
            if exchange_component == "uwb-adapter" {
                ensure!(
                    response.len() >= 5
                        && response[0] == 0x40
                        && response[1] == 0x02
                        && response[4] == 0x00,
                    "UWB Guest channel did not return CORE_GET_DEVICE_INFO_RSP"
                );
            } else {
                let response: serde_json::Value = serde_json::from_slice(&response)?;
                ensure!(
                    response["protocol_version"] == 2
                        && response["component"] == exchange_component
                        && response["request_sha256"] == request_digest
                        && response["status"] == "ready",
                    "{exchange_component} Guest channel response did not bind the request"
                );
            }
        }
        hd_platform::write_owner_only(&marker, b"ok")?;
        std::future::pending::<Result<()>>().await
    });
    let guest_endpoints = if component == "modem-adapter" {
        BTreeMap::new()
    } else {
        BTreeMap::from([(
            role.to_owned(),
            DeviceSerialEndpointV2 {
                guest_output,
                guest_input,
            },
        )])
    };
    let event = formal_device_component_smoke(
        root,
        executable,
        component,
        guest_endpoints,
        actions,
        Some(&exchange_marker),
    )
    .await;
    exchange.abort();
    let _ = exchange.await;
    let mut event = event?;
    event["guest_channel_exchange_verified"] = serde_json::Value::Bool(true);
    if component == "uwb-adapter" {
        event["runtime_ranging_action_verified"] = serde_json::Value::Bool(true);
    } else if component == "modem-adapter" {
        event["runtime_modem_action_verified"] = serde_json::Value::Bool(true);
        event["runtime_modem_unsolicited_verified"] = serde_json::Value::Bool(true);
        event["runtime_modem_registration_verified"] = serde_json::Value::Bool(true);
    }
    Ok(event)
}

#[cfg(windows)]
#[allow(clippy::too_many_lines)]
async fn formal_adb_bridge_smoke(root: &Path, executable: &Path) -> Result<serde_json::Value> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let probe_output = Command::new(executable)
        .args(["--probe-v2", "--json"])
        .output()
        .context("run hd-adb-bridge probe")?;
    ensure!(probe_output.status.success(), "hd-adb-bridge probe failed");
    let probe: FormalComponentProbeV2 = serde_json::from_slice(&probe_output.stdout)?;
    for feature in [
        "loopback-tcp-v2",
        "vsock-guest-v2",
        "ready-marker-v2",
        "lifecycle-v2",
        "windows-owner-pipe-v2",
    ] {
        ensure!(
            probe.features.iter().any(|actual| actual == feature),
            "hd-adb-bridge probe omitted {feature}"
        );
    }
    ensure!(
        probe.protocol_version == COMPONENT_PROTOCOL_VERSION
            && probe.component == "adb-bridge"
            && probe.formal,
        "hd-adb-bridge probe identity is invalid"
    );

    let temporary = secure_tempdir()?;
    let instance_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let guest_cid = 34_567_u32;
    let guest_port = 5555_u32;
    let launch_path = temporary.path().join("adb-bridge-launch-v2.json");
    let ready_path = temporary.path().join("adb-bridge-ready-v2.json");
    let pipe_probe_marker = temporary.path().join("adb-vsock-pipe-probe.json");
    let pipe_endpoint = format!(r"\\.\pipe\binder_rpc_vsock_{guest_cid}_{guest_port}");
    let vm_control_endpoint = format!(
        r"\\.\pipe\bscp-hd-{}-{instance_id}-{run_id}-adb-vm-control",
        hd_platform::current_user_scope()?
    );
    let tcp_probe = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let listen_port = tcp_probe.local_addr()?.port();
    drop(tcp_probe);
    let launch = FormalComponentLaunchV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        component: "adb-bridge".to_owned(),
        instance_id,
        run_id,
        component_ready_marker: ready_path.clone(),
        configuration: FormalComponentConfigurationV2::AdbBridge {
            listen_address: "127.0.0.1".to_owned(),
            listen_port,
            guest_cid,
            guest_port,
            vm_control_endpoint,
            crosvm_executable: std::env::current_exe()?,
        },
    };
    let launch_bytes = serde_json::to_vec_pretty(&launch)?;
    hd_platform::write_owner_only(&launch_path, &launch_bytes)?;
    let mut process = TokioProcessSupervisor
        .spawn(&ProcessSpec {
            executable: executable.to_owned(),
            arguments: vec![
                "--serve-v2".to_owned(),
                "--launch".to_owned(),
                launch_path.to_string_lossy().into_owned(),
            ],
            environment: BTreeMap::from([
                ("HD_ADB_BRIDGE_SMOKE_PIPE".to_owned(), pipe_endpoint),
                (
                    "HD_ADB_BRIDGE_SMOKE_MARKER".to_owned(),
                    pipe_probe_marker.to_string_lossy().into_owned(),
                ),
            ]),
            working_directory: root.to_owned(),
            stdout_path: temporary.path().join("adb-bridge.stdout.log"),
            stderr_path: temporary.path().join("adb-bridge.stderr.log"),
            latency_sensitive: false,
            kill_on_drop: true,
        })
        .await?;
    let identity = WorkerIdentityV2 {
        pid: process.id(),
        process_start_marker: hd_platform::process_start_marker(process.id())?,
        nonce: Uuid::nil(),
    };
    let mut process_guard = ExactProcessGuard::new(identity.clone());
    wait_for_file(&ready_path, Duration::from_secs(10)).await?;
    let ready: FormalComponentReadyV2 = serde_json::from_slice(
        &hd_platform::read_regular_nofollow_limited(&ready_path, 64 * 1024)?,
    )?;
    ensure!(
        ready.protocol_version == COMPONENT_PROTOCOL_VERSION
            && ready.component == "adb-bridge"
            && ready.instance_id == instance_id
            && ready.run_id == run_id
            && ready.launch_sha256 == hex::encode(Sha256::digest(&launch_bytes))
            && ready.pid == identity.pid
            && ready.process_start_marker == identity.process_start_marker,
        "formal ADB bridge ready marker did not bind the exact launch and process"
    );

    let mut tcp = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(("127.0.0.1", listen_port)),
    )
    .await
    .context("connect ADB bridge TCP listener timed out")??;
    let payload = b"CNXN hd-adb-bridge-smoke";
    tcp.write_all(payload).await?;
    tcp.flush().await?;
    let mut echo = vec![0_u8; payload.len()];
    let Ok(forwarding) =
        tokio::time::timeout(Duration::from_secs(25), tcp.read_exact(&mut echo)).await
    else {
        let stdout = std::fs::read_to_string(temporary.path().join("adb-bridge.stdout.log"))
            .unwrap_or_default();
        let stderr = std::fs::read_to_string(temporary.path().join("adb-bridge.stderr.log"))
            .unwrap_or_default();
        bail!("ADB bridge byte forwarding timed out; stdout={stdout}; stderr={stderr}");
    };
    if let Err(error) = forwarding {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let stdout = std::fs::read_to_string(temporary.path().join("adb-bridge.stdout.log"))
            .unwrap_or_default();
        let stderr = std::fs::read_to_string(temporary.path().join("adb-bridge.stderr.log"))
            .unwrap_or_default();
        bail!("read forwarded ADB bridge bytes: {error}; stdout={stdout}; stderr={stderr}");
    }
    ensure!(echo == payload, "ADB bridge changed forwarded bytes");
    drop(tcp);

    wait_for_file(&pipe_probe_marker, Duration::from_secs(5)).await?;
    let pipe_identity: WorkerIdentityV2 = serde_json::from_slice(
        &hd_platform::read_regular_nofollow_limited(&pipe_probe_marker, 64 * 1024)?,
    )?;
    let mut pipe_guard = ExactProcessGuard::new(pipe_identity.clone());
    let pipe_started = Instant::now();
    while hd_platform::process_identity_is_alive(&pipe_identity)
        && pipe_started.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    ensure!(
        !hd_platform::process_identity_is_alive(&pipe_identity),
        "ADB bridge pipe probe remained alive after TCP disconnect"
    );
    pipe_guard.disarm();
    TokioProcessSupervisor.terminate(&mut process).await?;
    process_guard.disarm();
    Ok(serde_json::json!({
        "schema_version": 2,
        "event": "formal_adb_bridge.loopback_vsock_forwarding_verified",
        "component": "adb-bridge",
        "instance_id": instance_id,
        "run_id": run_id,
        "listen_address": "127.0.0.1",
        "guest_cid": guest_cid,
        "guest_port": guest_port,
        "forwarded_bytes": payload.len(),
        "transport": "windows_named_pipe_vsock"
    }))
}

async fn managed_process_tree_smoke(root: &Path) -> Result<serde_json::Value> {
    let temporary = secure_tempdir()?;
    let marker = temporary.path().join("process-tree.json");
    let executable = std::env::current_exe()?;
    let spec = ProcessSpec {
        executable,
        arguments: vec![
            "process-probe".to_owned(),
            "--marker".to_owned(),
            marker.to_string_lossy().into_owned(),
        ],
        environment: BTreeMap::new(),
        working_directory: root.to_owned(),
        stdout_path: temporary.path().join("parent.stdout.log"),
        stderr_path: temporary.path().join("parent.stderr.log"),
        latency_sensitive: false,
        kill_on_drop: false,
    };
    let handle = TokioProcessSupervisor.spawn(&spec).await?;
    wait_for_file(&marker, Duration::from_secs(10)).await?;
    let record: ProcessProbeMarker = serde_json::from_slice(
        &hd_platform::read_regular_nofollow_limited(&marker, 64 * 1024)?,
    )?;
    ensure!(
        handle.id() == record.parent_pid,
        "managed process probe parent PID mismatch"
    );
    let parent = WorkerIdentityV2 {
        pid: record.parent_pid,
        process_start_marker: record.parent_start_marker,
        nonce: Uuid::nil(),
    };
    let child = WorkerIdentityV2 {
        pid: record.child_pid,
        process_start_marker: record.child_start_marker,
        nonce: Uuid::nil(),
    };
    let mut parent_guard = ExactProcessGuard::new(parent.clone());
    let mut child_guard = ExactProcessGuard::new(child.clone());
    ensure!(
        hd_platform::process_identity_is_alive(&parent)
            && hd_platform::process_identity_is_alive(&child),
        "managed process probe tree was not alive before containment close"
    );
    drop(handle);
    let started = Instant::now();
    while (hd_platform::process_identity_is_alive(&parent)
        || hd_platform::process_identity_is_alive(&child))
        && started.elapsed() < Duration::from_secs(10)
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    ensure!(
        !hd_platform::process_identity_is_alive(&parent)
            && !hd_platform::process_identity_is_alive(&child),
        "closing managed containment did not terminate the exact process tree"
    );
    parent_guard.disarm();
    child_guard.disarm();
    Ok(serde_json::json!({
        "schema_version": 2,
        "event": "managed_process.job_tree_terminated",
        "parent_pid": parent.pid,
        "child_pid": child.pid
    }))
}

#[allow(clippy::too_many_lines)]
async fn worker_process_smoke(worker_executable: &Path) -> Result<Vec<serde_json::Value>> {
    let temporary = secure_tempdir()?;
    let paths = DataPaths::from_root(temporary.path().join("worker-data"));
    paths.ensure()?;
    let instance_id = Uuid::new_v4();
    let nonce = Uuid::new_v4();
    let endpoint = worker_endpoint(instance_id)?;
    let secret = "ab".repeat(32);
    hd_platform::write_owner_only(&paths.worker_secret(instance_id), secret.as_bytes())?;
    let arguments = vec![
        "--data-root".to_owned(),
        paths.root.to_string_lossy().into_owned(),
        "--instance-id".to_owned(),
        instance_id.to_string(),
        "--nonce".to_owned(),
        nonce.to_string(),
        "--endpoint".to_owned(),
        endpoint.clone(),
    ];
    let pid =
        hd_platform::spawn_detached(worker_executable, &arguments, &BTreeMap::new(), &paths.root)?;
    let mut lifecycle = vec![serde_json::json!({
        "schema_version": 2,
        "event": "worker.spawned",
        "instance_id": instance_id,
        "pid": pid
    })];
    wait_for_file(
        &paths.worker_descriptor(instance_id),
        Duration::from_secs(10),
    )
    .await?;
    let descriptor: WorkerDescriptorV2 =
        serde_json::from_slice(&std::fs::read(paths.worker_descriptor(instance_id))?)?;
    let mut worker_guard = ExactProcessGuard::new(descriptor.identity.clone());
    ensure!(
        descriptor.identity.pid == pid
            && descriptor.identity.nonce == nonce
            && hd_platform::process_identity_is_alive(&descriptor.identity),
        "worker descriptor identity mismatch"
    );
    let ping_id = Uuid::new_v4();
    let ping_started = Instant::now();
    let ping = loop {
        match send_worker_request(
            &endpoint,
            &WorkerRequestV2 {
                protocol_version: WORKER_PROTOCOL_VERSION,
                request_id: ping_id,
                instance_id,
                bearer_token: secret.clone(),
                command: WorkerCommandV2::Ping,
            },
        )
        .await
        {
            Ok(response) => break response,
            Err(_) if ping_started.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let Some(WorkerPayloadV2::Pong(status)) = ping.payload else {
        bail!("detached worker ping returned an unexpected payload");
    };
    ensure!(
        ping.ok
            && ping.request_id == ping_id
            && status.observed == ObservedStateV2::Stopped
            && status.child_pid.is_none()
            && !status.cleanup_pending,
        "detached worker did not report a clean stopped state"
    );
    lifecycle.push(serde_json::json!({
        "schema_version": 2,
        "event": "worker.authenticated_ping",
        "instance_id": instance_id,
        "pid": pid,
        "observed": status.observed
    }));
    let duplicate_nonce = Uuid::new_v4();
    let mut duplicate_arguments = arguments.clone();
    let nonce_index = duplicate_arguments
        .iter()
        .position(|argument| argument == "--nonce")
        .context("worker arguments are missing --nonce")?
        + 1;
    duplicate_arguments[nonce_index] = duplicate_nonce.to_string();
    let duplicate_pid = hd_platform::spawn_detached(
        worker_executable,
        &duplicate_arguments,
        &BTreeMap::new(),
        &paths.root,
    )?;
    let duplicate_started = Instant::now();
    while hd_platform::process_start_marker(duplicate_pid).is_ok()
        && duplicate_started.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let duplicate_marker = hd_platform::process_start_marker(duplicate_pid).ok();
    let duplicate_survived = duplicate_marker.is_some();
    if let Some(process_start_marker) = duplicate_marker {
        hd_platform::terminate_process_identity(&hd_core::WorkerIdentityV2 {
            pid: duplicate_pid,
            process_start_marker,
            nonce: duplicate_nonce,
        })?;
    }
    ensure!(
        !duplicate_survived,
        "a second worker acquired the same per-instance lock"
    );
    lifecycle.push(serde_json::json!({
        "schema_version": 2,
        "event": "worker.duplicate_rejected",
        "instance_id": instance_id,
        "duplicate_pid": duplicate_pid
    }));
    let descriptor_after_duplicate: WorkerDescriptorV2 =
        serde_json::from_slice(&std::fs::read(paths.worker_descriptor(instance_id))?)?;
    ensure!(
        descriptor_after_duplicate.identity == descriptor.identity,
        "duplicate worker attempt replaced the live worker descriptor"
    );
    let unauthorized = send_worker_request(
        &endpoint,
        &WorkerRequestV2 {
            protocol_version: WORKER_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id,
            bearer_token: "00".repeat(32),
            command: WorkerCommandV2::Status,
        },
    )
    .await?;
    ensure!(
        !unauthorized.ok
            && unauthorized
                .error
                .as_ref()
                .is_some_and(|error| error.code == "worker_unauthorized"),
        "worker authentication rejection failed"
    );
    lifecycle.push(serde_json::json!({
        "schema_version": 2,
        "event": "worker.unauthorized_rejected",
        "instance_id": instance_id,
        "pid": pid
    }));
    let host = HostService::open(paths.clone(), Some(worker_executable.to_owned())).await?;
    let recovery_spec = InstanceSpecV2 {
        id: instance_id,
        name: "HD crash-window recovery".to_owned(),
        ..InstanceSpecV2::default()
    };
    host.create_instance(CreateInstanceRequestV2 {
        spec: recovery_spec,
    })?;
    std::fs::remove_file(paths.worker_descriptor(instance_id))?;
    let reports = host.reconcile().await?;
    ensure!(
        reports.iter().any(|report| report.worker_reconnected)
            && host
                .get_instance(instance_id)?
                .worker
                .is_some_and(|identity| identity == descriptor.identity)
            && paths.worker_descriptor(instance_id).is_file(),
        "host did not recover an authenticated worker missing its persisted descriptor"
    );
    lifecycle.push(serde_json::json!({
        "schema_version": 2,
        "event": "host.worker_reconnected",
        "instance_id": instance_id,
        "pid": pid
    }));
    drop(host);
    ensure!(
        hd_platform::process_identity_is_alive(&descriptor.identity),
        "worker exited when the host was dropped"
    );
    let restarted_host =
        HostService::open(paths.clone(), Some(worker_executable.to_owned())).await?;
    let restarted_reports = restarted_host.reconcile().await?;
    ensure!(
        restarted_reports
            .iter()
            .any(|report| report.worker_reconnected)
            && hd_platform::process_identity_is_alive(&descriptor.identity),
        "restarted host did not reconnect the detached worker"
    );
    lifecycle.push(serde_json::json!({
        "schema_version": 2,
        "event": "host.restarted_worker_reconnected",
        "instance_id": instance_id,
        "pid": pid
    }));
    drop(restarted_host);
    let shutdown = send_worker_request(
        &endpoint,
        &WorkerRequestV2 {
            protocol_version: WORKER_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            instance_id,
            bearer_token: secret,
            command: WorkerCommandV2::Shutdown,
        },
    )
    .await?;
    ensure!(shutdown.ok, "worker shutdown was rejected");
    let started = Instant::now();
    while hd_platform::process_identity_is_alive(&descriptor.identity)
        && started.elapsed() < Duration::from_secs(10)
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if hd_platform::process_identity_is_alive(&descriptor.identity) {
        hd_platform::terminate_process_identity(&descriptor.identity)?;
        bail!("detached worker did not exit after authenticated shutdown");
    }
    ensure!(
        !paths.worker_descriptor(instance_id).exists(),
        "worker descriptor remained after process exit"
    );
    lifecycle.push(serde_json::json!({
        "schema_version": 2,
        "event": "worker.authenticated_shutdown",
        "instance_id": instance_id,
        "pid": pid,
        "descriptor_removed": true
    }));
    worker_guard.disarm();
    Ok(lifecycle)
}

struct ExactProcessGuard(Option<hd_core::WorkerIdentityV2>);

impl ExactProcessGuard {
    fn new(identity: hd_core::WorkerIdentityV2) -> Self {
        Self(Some(identity))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ExactProcessGuard {
    fn drop(&mut self) {
        if let Some(identity) = self.0.take()
            && hd_platform::process_identity_is_alive(&identity)
        {
            let _ = hd_platform::terminate_process_identity(&identity);
        }
    }
}

async fn http_security_smoke(descriptor: &hd_core::HostRuntimeDescriptorV2) -> Result<()> {
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let health = format!("{}/v2/health", descriptor.origin);
    let unauthorized = http.get(&health).send().await?;
    ensure!(
        unauthorized.status() == reqwest::StatusCode::UNAUTHORIZED,
        "missing bearer was not rejected"
    );
    let unauthorized_error: ApiErrorV2 = unauthorized.json().await?;
    ensure!(
        unauthorized_error.code == "unauthorized",
        "unstable auth error code"
    );

    let host = descriptor
        .origin
        .strip_prefix("http://")
        .context("descriptor origin is not HTTP")?;
    let wrong_origin = http
        .get(&health)
        .header("host", host)
        .header(
            "authorization",
            format!("Bearer {}", descriptor.bearer_token),
        )
        .header("origin", "https://attacker.invalid")
        .send()
        .await?;
    ensure!(
        wrong_origin.status() == reqwest::StatusCode::FORBIDDEN,
        "foreign Origin was not rejected"
    );
    let wrong_host = http
        .get(&health)
        .header("host", "127.0.0.1:1")
        .header(
            "authorization",
            format!("Bearer {}", descriptor.bearer_token),
        )
        .send()
        .await?;
    ensure!(
        wrong_host.status() == reqwest::StatusCode::BAD_REQUEST,
        "foreign Host was not rejected"
    );
    http_cors_and_limits_smoke(&http, descriptor, host, &health).await
}

async fn http_cors_and_limits_smoke(
    http: &reqwest::Client,
    descriptor: &hd_core::HostRuntimeDescriptorV2,
    host: &str,
    health: &str,
) -> Result<()> {
    let accepted_origin = http
        .get(health)
        .header("host", host)
        .header(
            "authorization",
            format!("Bearer {}", descriptor.bearer_token),
        )
        .header("origin", &descriptor.origin)
        .send()
        .await?;
    ensure!(
        accepted_origin.status() == reqwest::StatusCode::OK
            && accepted_origin
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok())
                == Some(descriptor.origin.as_str())
            && accepted_origin
                .headers()
                .get("x-content-type-options")
                .and_then(|value| value.to_str().ok())
                == Some("nosniff")
            && accepted_origin.headers().contains_key("x-request-id"),
        "same-origin response did not carry the exact security headers"
    );
    let preflight = http
        .request(reqwest::Method::OPTIONS, health)
        .header("host", host)
        .header("origin", &descriptor.origin)
        .send()
        .await?;
    ensure!(
        preflight.status() == reqwest::StatusCode::NO_CONTENT
            && preflight
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok())
                == Some(descriptor.origin.as_str()),
        "same-origin CORS preflight was not handled exactly"
    );
    let oversized = http
        .post(format!("{}/v2/instances", descriptor.origin))
        .header("host", host)
        .header(
            "authorization",
            format!("Bearer {}", descriptor.bearer_token),
        )
        .header("content-type", "application/json")
        .header("content-length", 4_u64 * 1024 * 1024 * 1024 + 1)
        .body("")
        .send()
        .await?;
    ensure!(
        oversized.status() == reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "oversized request declaration was not rejected"
    );
    let oversized_error: ApiErrorV2 = oversized.json().await?;
    ensure!(
        oversized_error.code == "request_too_large",
        "oversized request returned an unstable error code"
    );
    Ok(())
}

async fn wait_for_file(path: &Path, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while !path.is_file() {
        if started.elapsed() >= timeout {
            bail!("timed out waiting for {}", path.display());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

fn legacy_fixture(id: Uuid) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "id": "{id}",
  "name": "Migrated Android",
  "cpu_count": 4,
  "memory_mib": 4096,
  "display": {{"width": 1080, "height": 1920, "dpi": 420, "refresh_rate_hz": 60, "orientation": "portrait", "vsync": "on", "show_host_fps": false}},
  "adb": {{"enabled": true, "auto_port": true, "host_port": null, "adb_path": null, "auto_root": false}},
  "artifacts": {{"kernel": "", "initrd": "", "rootfs": "", "android_fstab": "", "system_image": null, "vendor_image": null, "expected_sha256": {{}}}},
  "extra_kernel_args": []
}}"#
    )
}

#[allow(clippy::too_many_arguments)]
fn certify(
    data_root: &Path,
    guest_kind: GuestKindV2,
    guest_digest: &str,
    host_digest: &str,
    capability_fingerprint: &str,
    signer_key_id: &str,
    signing_key_path: &Path,
    validity_days: u64,
    evidence: Vec<(String, PathBuf)>,
) -> Result<()> {
    for (label, value) in [
        ("guest digest", guest_digest),
        ("host digest", host_digest),
        ("capability fingerprint", capability_fingerprint),
    ] {
        ensure!(
            valid_digest(value),
            "{label} is not a lowercase SHA-256 digest"
        );
    }
    ensure!(
        !signer_key_id.is_empty() && signer_key_id.len() <= 128,
        "signer key id is invalid"
    );
    let evidence_names = evidence
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<BTreeSet<_>>();
    let required = match guest_kind {
        GuestKindV2::Android => ANDROID_CERTIFICATION_EVIDENCE.as_slice(),
        GuestKindV2::Microdroid => MICRODROID_CERTIFICATION_EVIDENCE.as_slice(),
    }
    .iter()
    .copied()
    .collect::<BTreeSet<_>>();
    ensure!(
        evidence_names == required,
        "evidence names must exactly match the eight release gates"
    );
    let mut evidence_sha256 = BTreeMap::new();
    for (name, path) in evidence {
        ensure!(
            path.is_file(),
            "evidence file is missing: {}",
            path.display()
        );
        evidence_sha256.insert(name, hash_file(&path)?);
    }

    let paths = DataPaths::resolve(data_root.to_owned())?;
    paths.ensure()?;
    let trust = ArtifactTrustStore::load(&paths.root.join("trusted-keys-v2.json"))?;
    let mut key_bytes = read_signing_key(signing_key_path)?;
    let mut key_array: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key must contain exactly 32 bytes"))?;
    let signing_key = SigningKey::from_bytes(&key_array);
    key_bytes.fill(0);
    key_array.fill(0);
    let issued_at = OffsetDateTime::now_utc();
    let expires_at = issued_at
        + time::Duration::days(i64::try_from(validity_days).context("validity days overflow")?);
    let mut certification = HostCertificationV2 {
        schema_version: HOST_CERTIFICATION_VERSION,
        certification_id: Uuid::new_v4(),
        platform: hd_platform::platform_name().to_owned(),
        architecture: hd_platform::architecture_name().to_owned(),
        guest_kind,
        capability_fingerprint: capability_fingerprint.to_owned(),
        guest_bundle_digest: guest_digest.to_owned(),
        host_bundle_digest: host_digest.to_owned(),
        device_profile: match guest_kind {
            GuestKindV2::Android => "hd-phone-android15-v2",
            GuestKindV2::Microdroid => "hd-microdroid-macos-arm64-v2",
        }
        .to_owned(),
        control_protocol_version: CONTROL_PROTOCOL_VERSION,
        frame_protocol_version: FRAME_PROTOCOL_VERSION,
        issued_at,
        expires_at,
        evidence_sha256,
        signer_key_id: signer_key_id.to_owned(),
        signature_ed25519: String::new(),
    };
    let payload = serde_json::to_vec(&certification)?;
    certification.signature_ed25519 = BASE64.encode(signing_key.sign(&payload).to_bytes());
    trust.verify_detached(
        &certification.signer_key_id,
        &certification.signature_ed25519,
        &payload,
    )?;
    let output = paths.host_certification(
        &certification.platform,
        &certification.architecture,
        guest_digest,
        host_digest,
    );
    hd_platform::write_owner_only(&output, &serde_json::to_vec_pretty(&certification)?)?;
    println!("{}", output.display());
    Ok(())
}

fn parse_evidence(value: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = value
        .split_once('=')
        .ok_or_else(|| "evidence must be NAME=PATH".to_owned())?;
    if name.is_empty() || path.is_empty() {
        return Err("evidence must be NAME=PATH".to_owned());
    }
    Ok((name.to_owned(), PathBuf::from(path)))
}

fn parse_validity_days(value: &str) -> Result<u64, String> {
    let days = value
        .parse::<u64>()
        .map_err(|error| format!("validity days must be an integer: {error}"))?;
    if !(1..=31).contains(&days) {
        return Err("validity days must be in 1..=31".to_owned());
    }
    Ok(days)
}

fn read_signing_key(path: &Path) -> Result<Vec<u8>> {
    let bytes = hd_platform::read_regular_nofollow_limited(path, 4096)
        .context("read signing key without following links")?;
    let text = String::from_utf8(bytes).context("signing key is not UTF-8 text")?;
    let text = text.trim();
    if text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return hex::decode(text).context("decode hexadecimal signing key");
    }
    BASE64.decode(text).context("decode base64 signing key")
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = hd_platform::open_regular_read_nofollow(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 128 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn workspace_root() -> Result<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_owned)
        .context("xtask manifest has no parent")
}

fn run(root: &Path, program: &str, arguments: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("start {program} {}", arguments.join(" ")))?;
    if !status.success() {
        bail!("{program} {} failed with {status}", arguments.join(" "));
    }
    Ok(())
}

fn pe_audit(bin_dir: &Path, objdump: &Path) -> Result<()> {
    let mut audited = 0_u32;
    for entry in std::fs::read_dir(bin_dir)
        .with_context(|| format!("read PE directory {}", bin_dir.display()))?
    {
        let path = entry?.path();
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if !extension.eq_ignore_ascii_case("exe") && !extension.eq_ignore_ascii_case("dll") {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_uppercase();
        for forbidden_file in [
            "UCRTBASED.DLL",
            "VCRUNTIME140D.DLL",
            "MSVCP140D.DLL",
            "CONCRT140D.DLL",
        ] {
            ensure!(
                file_name != forbidden_file,
                "{} is a forbidden debug MSVC runtime",
                path.display()
            );
        }
        let output = Command::new(objdump)
            .args(["-p", &path.to_string_lossy()])
            .output()
            .with_context(|| format!("run {} for {}", objdump.display(), path.display()))?;
        ensure!(
            output.status.success(),
            "objdump failed for {}",
            path.display()
        );
        let imports = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().strip_prefix("DLL Name:"))
            .map(str::trim)
            .map(str::to_ascii_uppercase)
            .collect::<Vec<_>>();
        for forbidden in ["UCRTBASED", "VCRUNTIME", "MSVCP", "CONCRT", "MFC"] {
            ensure!(
                !imports.iter().any(|import| import.contains(forbidden)),
                "{} imports forbidden MSVC runtime {forbidden}",
                path.display()
            );
        }
        audited = audited.saturating_add(1);
    }
    ensure!(audited > 0, "no PE files found under {}", bin_dir.display());
    println!("PE audit passed for {audited} executables and libraries");
    Ok(())
}

fn init_trust_root(data_root: &Path, signer_key_id: &str, signing_key_path: &Path) -> Result<()> {
    ensure!(
        !signer_key_id.is_empty() && signer_key_id.len() <= 128,
        "signer key id is invalid"
    );
    ensure!(
        signing_key_path.is_absolute(),
        "signing key path must be absolute"
    );
    let paths = DataPaths::resolve(data_root.to_owned())?;
    paths.ensure()?;
    let trust_store = paths.root.join("trusted-keys-v2.json");
    ensure!(
        !trust_store.exists(),
        "refusing to replace existing trust store {}",
        trust_store.display()
    );
    ensure!(
        !signing_key_path.exists(),
        "refusing to replace existing signing key {}",
        signing_key_path.display()
    );
    if let Some(parent) = signing_key_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create signing key directory {}", parent.display()))?;
    }

    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret)
        .map_err(|error| anyhow::anyhow!("generate Ed25519 signing key: {error}"))?;
    let signing_key = SigningKey::from_bytes(&secret);
    let trust_bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": ARTIFACT_INDEX_VERSION,
        "keys": {
            signer_key_id: BASE64.encode(signing_key.verifying_key().to_bytes())
        }
    }))?;
    hd_platform::write_owner_only(signing_key_path, hex::encode(secret).as_bytes())?;
    secret.fill(0);
    hd_platform::write_owner_only(&trust_store, &trust_bytes)?;
    ArtifactTrustStore::load(&trust_store).context("verify generated trust store")?;
    println!(
        "Initialized HD trust root {} with signing key {}",
        trust_store.display(),
        signing_key_path.display()
    );
    Ok(())
}

#[derive(Debug)]
struct PublishBundleRequest {
    kind: ArtifactBundleKindV2,
    input_root: PathBuf,
    store_root: PathBuf,
    platform: String,
    architecture: String,
    source_manifest_digest: String,
    signer_key_id: String,
    signing_key: PathBuf,
    trust_store: PathBuf,
    capabilities: Vec<String>,
    files: Vec<(String, PathBuf)>,
    executable_roles: Vec<String>,
    print_result: bool,
}

fn parse_bundle_file(value: &str) -> std::result::Result<(String, PathBuf), String> {
    let (role, relative_path) = value
        .split_once('=')
        .ok_or_else(|| "bundle file must use role=relative/path syntax".to_owned())?;
    if role.is_empty() || relative_path.is_empty() {
        return Err("bundle file role and relative path must be non-empty".to_owned());
    }
    Ok((role.to_owned(), PathBuf::from(relative_path)))
}

fn verify_android_artifact_store(
    store_root: &Path,
    trust_store: &Path,
    channel: PackagedArtifactChannelV2,
) -> Result<()> {
    let verified = verify_packaged_android_artifact_store(store_root, trust_store, channel)
        .with_context(|| {
            format!(
                "verify packaged Android artifact store {}",
                store_root.display()
            )
        })?;
    let rootfs = verified
        .bundles
        .guest_manifest
        .files
        .iter()
        .find(|file| file.role == "rootfs")
        .context("verified Android Guest bundle omitted rootfs")?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "schema_version": verified.index.schema_version,
            "channel": verified.index.channel,
            "android_version": verified.index.android_version,
            "data_profile": verified.index.data_profile,
            "guest_bundle_digest": verified.index.guest_bundle_digest,
            "host_bundle_digest": verified.index.host_bundle_digest,
            "guest_file_count": verified.bundles.guest_manifest.files.len(),
            "host_file_count": verified.bundles.host_manifest.files.len(),
            "rootfs_relative_path": rootfs.relative_path,
            "rootfs_sha256": rootfs.sha256,
            "rootfs_size_bytes": rootfs.size_bytes,
            "exact_closure": true,
            "signature_verified": true
        }))?
    );
    Ok(())
}

fn publish_bundle(request: PublishBundleRequest) -> Result<()> {
    ensure!(
        is_sha256(&request.source_manifest_digest),
        "source manifest digest must be 64 lowercase hexadecimal characters"
    );
    ensure!(
        !request.signer_key_id.is_empty() && request.signer_key_id.len() <= 128,
        "signer key id is invalid"
    );
    let bundles_root = request.store_root.join("bundles");
    std::fs::create_dir_all(&bundles_root)
        .with_context(|| format!("create bundle store {}", bundles_root.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&bundles_root)
        .context("create bundle staging directory")?;
    let files = stage_bundle_files(&request, staging.path())?;
    let mut capabilities = request.capabilities;
    capabilities.sort();
    ensure!(
        capabilities.windows(2).all(|pair| pair[0] != pair[1]),
        "bundle capabilities contain duplicates"
    );
    let mut manifest = ArtifactBundleV2 {
        schema_version: ARTIFACT_INDEX_VERSION,
        digest: String::new(),
        kind: request.kind,
        platform: request.platform,
        architecture: request.architecture,
        source_manifest_digest: request.source_manifest_digest,
        files,
        capabilities,
        signer_key_id: request.signer_key_id,
        signature_ed25519: String::new(),
    };
    sign_bundle_manifest(&mut manifest, &request.signing_key)?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    hd_platform::write_owner_only(&staging.path().join("manifest-v2.json"), &manifest_bytes)?;
    let ready = ArtifactReadyMarkerV2 {
        schema_version: ARTIFACT_INDEX_VERSION,
        bundle_digest: manifest.digest.clone(),
        manifest_sha256: hex::encode(Sha256::digest(&manifest_bytes)),
        published_at: OffsetDateTime::now_utc(),
    };
    hd_platform::write_owner_only(
        &staging.path().join("READY-v2.json"),
        &serde_json::to_vec_pretty(&ready)?,
    )?;
    let trust = ArtifactTrustStore::load(&request.trust_store)?;
    ArtifactResolver::new(trust).verify_bundle(staging.path(), &manifest.digest, request.kind)?;
    let destination = bundles_root.join(&manifest.digest);
    ensure!(
        !destination.exists(),
        "bundle {} is already published",
        manifest.digest
    );
    let staging_path = staging.keep();
    std::fs::rename(&staging_path, &destination).with_context(|| {
        format!(
            "publish verified bundle {} to {}",
            manifest.digest,
            destination.display()
        )
    })?;
    if request.print_result {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "schema_version": ARTIFACT_INDEX_VERSION,
                "bundle_digest": manifest.digest,
                "kind": manifest.kind,
                "root": destination,
                "file_count": manifest.files.len()
            }))?
        );
    }
    Ok(())
}

fn bundle_publish_smoke() -> Result<serde_json::Value> {
    let temporary = secure_tempdir()?;
    let input = temporary.path().join("input");
    let store = temporary.path().join("store");
    std::fs::create_dir_all(&input)?;
    hd_platform::write_owner_only(&input.join("probe.bin"), b"HD signed bundle smoke v2")?;
    let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
    let signing_key_path = temporary.path().join("signing.key");
    hd_platform::write_owner_only(
        &signing_key_path,
        hex::encode(signing_key.to_bytes()).as_bytes(),
    )?;
    let trust_store = temporary.path().join("trusted-keys-v2.json");
    hd_platform::write_owner_only(
        &trust_store,
        &serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": ARTIFACT_INDEX_VERSION,
            "keys": {
                "smoke": BASE64.encode(signing_key.verifying_key().to_bytes())
            }
        }))?,
    )?;
    publish_bundle(PublishBundleRequest {
        kind: ArtifactBundleKindV2::HostTools,
        input_root: input,
        store_root: store.clone(),
        platform: hd_platform::platform_name().to_owned(),
        architecture: hd_platform::architecture_name().to_owned(),
        source_manifest_digest: "01".repeat(32),
        signer_key_id: "smoke".to_owned(),
        signing_key: signing_key_path,
        trust_store: trust_store.clone(),
        capabilities: vec!["bundle-publish-smoke-v2".to_owned()],
        files: vec![("probe".to_owned(), PathBuf::from("probe.bin"))],
        executable_roles: vec!["probe".to_owned()],
        print_result: false,
    })?;
    let bundle_roots = std::fs::read_dir(store.join("bundles"))?.collect::<Result<Vec<_>, _>>()?;
    ensure!(
        bundle_roots.len() == 1,
        "bundle smoke did not publish exactly one digest"
    );
    let digest = bundle_roots[0].file_name().to_string_lossy().into_owned();
    let trust = ArtifactTrustStore::load(&trust_store)?;
    let manifest = ArtifactResolver::new(trust).verify_bundle(
        &bundle_roots[0].path(),
        &digest,
        ArtifactBundleKindV2::HostTools,
    )?;
    Ok(serde_json::json!({
        "schema_version": ARTIFACT_INDEX_VERSION,
        "event": "artifact.bundle_published_and_verified",
        "bundle_digest": digest,
        "file_count": manifest.files.len(),
        "ready": true
    }))
}

fn stage_bundle_files(
    request: &PublishBundleRequest,
    staging: &Path,
) -> Result<Vec<ArtifactFileV2>> {
    let root_metadata = std::fs::symlink_metadata(&request.input_root)
        .with_context(|| format!("inspect input root {}", request.input_root.display()))?;
    ensure!(
        root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
        "bundle input root must be a regular directory, not a symlink"
    );
    let executable_roles = request
        .executable_roles
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    ensure!(
        executable_roles.len() == request.executable_roles.len(),
        "executable roles contain duplicates"
    );
    let mut roles = BTreeSet::new();
    let mut relative_paths = BTreeSet::new();
    let mut files = Vec::with_capacity(request.files.len());
    for (role, relative_path) in &request.files {
        ensure!(roles.insert(role.clone()), "duplicate bundle role {role}");
        validate_bundle_relative_path(relative_path)?;
        ensure!(
            relative_paths.insert(relative_path.clone()),
            "duplicate bundle path {}",
            relative_path.display()
        );
        let source = checked_bundle_source(&request.input_root, relative_path)?;
        let metadata = std::fs::metadata(&source)
            .with_context(|| format!("inspect bundle source {}", source.display()))?;
        ensure!(
            metadata.is_file(),
            "bundle source is not a file: {}",
            source.display()
        );
        ensure!(
            metadata.len() != 0,
            "bundle source is empty: {}",
            source.display()
        );
        let destination = staging.join(relative_path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create bundle directory {}", parent.display()))?;
        }
        #[cfg(target_os = "macos")]
        if role == "rootfs" {
            reflink_copy::reflink(&source, &destination).with_context(|| {
                format!(
                    "APFS block-clone rootfs {} to {}; refusing a dense fallback",
                    source.display(),
                    destination.display()
                )
            })?;
        } else {
            std::fs::copy(&source, &destination).with_context(|| {
                format!(
                    "copy bundle source {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
        }
        #[cfg(not(target_os = "macos"))]
        std::fs::copy(&source, &destination).with_context(|| {
            format!(
                "copy bundle source {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        files.push(ArtifactFileV2 {
            role: role.clone(),
            relative_path: relative_path.clone(),
            sha256: sha256_file(&destination)?,
            size_bytes: metadata.len(),
            executable: executable_roles.contains(role),
        });
    }
    ensure!(!files.is_empty(), "bundle must contain at least one file");
    ensure!(
        executable_roles.iter().all(|role| roles.contains(role)),
        "an executable role does not have a matching --file"
    );
    files.sort_by(|left, right| left.role.cmp(&right.role));
    Ok(files)
}

fn sign_bundle_manifest(manifest: &mut ArtifactBundleV2, signing_key_path: &Path) -> Result<()> {
    let payload = canonical_payload(manifest)?;
    manifest.digest = hex::encode(Sha256::digest(&payload));
    let mut key_bytes = read_signing_key(signing_key_path)?;
    let key_array: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key must contain exactly 32 bytes"))?;
    let signing_key = SigningKey::from_bytes(&key_array);
    key_bytes.fill(0);
    manifest.signature_ed25519 = BASE64.encode(signing_key.sign(&payload).to_bytes());
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn validate_bundle_relative_path(path: &Path) -> Result<()> {
    ensure!(!path.as_os_str().is_empty(), "bundle path is empty");
    ensure!(
        path.components()
            .all(|component| matches!(component, std::path::Component::Normal(_))),
        "bundle path must be strictly relative without dot or parent components: {}",
        path.display()
    );
    Ok(())
}

fn checked_bundle_source(root: &Path, relative_path: &Path) -> Result<PathBuf> {
    let mut source = root.to_owned();
    for component in relative_path.components() {
        let std::path::Component::Normal(component) = component else {
            bail!("bundle source path is not strictly relative");
        };
        source.push(component);
        let metadata = std::fs::symlink_metadata(&source)
            .with_context(|| format!("inspect bundle source component {}", source.display()))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "bundle source traverses a symlink: {}",
            source.display()
        );
    }
    Ok(source)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open bundle file {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("hash bundle file {}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn package(
    root: &Path,
    target_dir: &Path,
    runtime_dir: &Path,
    adb: &Path,
    aapt2: &Path,
    output: &Path,
) -> Result<()> {
    ensure!(
        !output.exists(),
        "package output already exists; use a fresh destination: {}",
        output.display()
    );
    std::fs::create_dir_all(output)
        .with_context(|| format!("create package directory {}", output.display()))?;
    for name in [
        "hd.exe",
        "hdctl.exe",
        "hd-host.exe",
        "hd-worker.exe",
        "hd-device-sim.exe",
        "hd-adb-bridge.exe",
        "hd-casimir-adapter.exe",
        "hd-rootcanal-adapter.exe",
        "hd-frame-producer.exe",
        "hd-uwb-adapter.exe",
        "hd-modem-adapter.exe",
        "hd-network-adapter.exe",
        "hd-audio-adapter.exe",
        "hd-camera-adapter.exe",
    ] {
        copy_package_file(&target_dir.join(name), &output.join(name))?;
    }
    #[cfg(windows)]
    {
        let loader = windows_webview2_loader(target_dir)?;
        let destination = output.join("WebView2Loader.dll");
        copy_package_file(&loader, &destination)?;
    }
    for name in [
        "crosvm.exe",
        "vm.exe",
        "virtmgr.exe",
        "libbinder-rpc.dll",
        "libgfxstream_backend.dll",
        "libEGL.dll",
        "libGLESv2.dll",
        "vulkan-1.dll",
        "libslirp-0.dll",
        "libgcc_s_seh-1.dll",
        "libstdc++-6.dll",
        "libwinpthread-1.dll",
    ] {
        copy_package_file(&runtime_dir.join(name), &output.join(name))?;
    }
    ensure!(
        adb.file_name().is_some_and(|name| name == "adb.exe"),
        "--adb must name the pinned Windows platform-tools adb.exe"
    );
    ensure!(
        aapt2.file_name().is_some_and(|name| name == "aapt2.exe"),
        "--aapt2 must name the pinned Android build-tools aapt2.exe"
    );
    let adb_root = adb.parent().context("--adb has no parent directory")?;
    copy_package_file(adb, &output.join("adb.exe"))?;
    for name in ["AdbWinApi.dll", "AdbWinUsbApi.dll"] {
        copy_package_file(&adb_root.join(name), &output.join(name))?;
    }
    copy_package_file(aapt2, &output.join("aapt2.exe"))?;
    for name in ["README.md", "LICENSE", "AGENTS.md"] {
        copy_package_file(&root.join(name), &output.join(name))?;
    }
    let web_dist = root.join("web").join("dist");
    if !web_dist.join("index.html").is_file() {
        bail!(
            "HD WebView assets are missing at {}; run npm run build in web first",
            web_dist.display()
        );
    }
    copy_tree(&web_dist, &output.join("ui"))?;
    package_third_party_notices(root, adb_root, output)?;
    copy_tree(&root.join("automation"), &output.join("automation"))?;
    copy_tree(&root.join("docs"), &output.join("docs"))?;
    #[cfg(windows)]
    {
        verify_windows_package_policy(output)?;
        for name in PACKAGED_WINDOWS_HELP_PROBES {
            verify_packaged_windows_tool(output, name, &["--help"])?;
        }
        verify_packaged_windows_tool(output, "adb.exe", &["version"])?;
        verify_packaged_windows_tool(output, "aapt2.exe", &["version"])?;
    }
    Ok(())
}

fn package_third_party_notices(root: &Path, adb_root: &Path, output: &Path) -> Result<()> {
    let notices = output.join("THIRD_PARTY_NOTICES");
    std::fs::create_dir_all(&notices)?;
    copy_package_file(
        &adb_root.join("NOTICE.txt"),
        &notices.join("android-platform-tools-NOTICE.txt"),
    )?;
    let workspace_root = root
        .parent()
        .context("HD root has no workspace parent for third-party notices")?;
    copy_package_file(
        &workspace_root
            .join("external")
            .join("crosvm")
            .join("win_audio")
            .join("third_party")
            .join("r8brain")
            .join("LICENSE"),
        &notices.join("r8brain-free-src-LICENSE.txt"),
    )?;
    Ok(())
}

fn copy_package_file(source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("inspect package source {}", source.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "package source is not a regular non-symlink file: {}",
        source.display()
    );
    std::fs::copy(source, destination)
        .with_context(|| format!("copy {} to {}", source.display(), destination.display()))?;
    Ok(())
}

#[cfg(windows)]
fn verify_windows_package_policy(output: &Path) -> Result<()> {
    let mut directories = vec![output.to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("read packaged Windows directory {}", directory.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                directories.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            ensure!(
                !name.contains("swiftshader"),
                "packaged Windows runtime must not include SwiftShader: {}",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
fn verify_packaged_windows_tool(output: &Path, name: &str, arguments: &[&str]) -> Result<()> {
    let executable = output.join(name);
    let system_root = std::env::var_os("SystemRoot").context("SystemRoot is not set")?;
    let system_path = Path::new(&system_root).join("System32");
    let probe = Command::new(&executable)
        .args(arguments)
        .current_dir(output)
        .env_clear()
        .env("SystemRoot", &system_root)
        .env("WINDIR", &system_root)
        .env("PATH", system_path)
        .output()
        .with_context(|| format!("run packaged tool probe {}", executable.display()))?;
    ensure!(
        probe.status.success(),
        "packaged tool probe failed for {}: stdout={}; stderr={}",
        executable.display(),
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr)
    );
    Ok(())
}

#[cfg(windows)]
fn windows_webview2_loader(target_dir: &Path) -> Result<PathBuf> {
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "x86",
        "aarch64" => "arm64",
        architecture => bail!("unsupported Windows WebView2 loader architecture {architecture}"),
    };
    let build_dir = target_dir.join("build");
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(&build_dir)
        .with_context(|| format!("read Cargo build directory {}", build_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir()
            || !entry
                .file_name()
                .to_string_lossy()
                .starts_with("webview2-com-sys-")
        {
            continue;
        }
        let candidate = entry
            .path()
            .join("out")
            .join(architecture)
            .join("WebView2Loader.dll");
        if candidate.is_file() {
            candidates.push(candidate);
        }
    }
    candidates.sort();
    ensure!(
        !candidates.is_empty(),
        "MinGW hd.exe dynamically imports WebView2Loader.dll, but Cargo produced no loader below {}",
        build_dir.display()
    );
    let expected_digest = sha256_file(&candidates[0])?;
    for candidate in candidates.iter().skip(1) {
        ensure!(
            sha256_file(candidate)? == expected_digest,
            "multiple non-identical WebView2Loader.dll candidates exist below {}; clean stale Cargo build outputs before packaging",
            build_dir.display()
        );
    }
    Ok(candidates.remove(0))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)
        .with_context(|| format!("create package directory {}", destination.display()))?;
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("read package source {}", source.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target)?;
        } else {
            bail!("unsupported package entry {}", entry.path().display());
        }
    }
    Ok(())
}
