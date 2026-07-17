use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs2::FileExt as _;
use hd_core::{
    AdbModeV2, ApiErrorV2, COMPONENT_PROTOCOL_VERSION, DeviceSerialEndpointV2,
    FRAME_PROTOCOL_VERSION, FormalComponentConfigurationV2, FormalComponentLaunchV2,
    FormalComponentReadyV2, FrameMetricsV2, FrameReadyMarkerV2, InstanceActionV2, InstanceSpecV2,
    LaunchPlanV2, LeaseKindV2, LeaseV2, ObservedStateV2, RunManifestV2, RunResultV2, StopModeV2,
    WORKER_PROTOCOL_VERSION, WorkerCommandV2, WorkerDescriptorV2, WorkerIdentityV2,
    WorkerPayloadV2, WorkerRequestV2, WorkerResponseV2, WorkerStatusV2,
};
use hd_device_sim::{
    DeviceCommandV2, DeviceRequestV2, DeviceSimulatorV2, MAX_DEVICE_MESSAGE_BYTES,
};
use hd_platform::{
    DataPaths, DiskProvisioner as _, ProcessSpec, ProcessSupervisor as _, VmBackend as _,
    VmLaunchContextV2, process_identity_is_alive,
};
use sha2::Digest as _;
use subtle::ConstantTimeEq as _;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

use crate::{
    AdbClient, CapabilityDiscovery, CrosvmBackend, ManagedProcess, NativeDiskProvisioner,
    RunJournalV2, TokioProcessSupervisor, expected_frame_transport,
};

const FRAME_READY_TIMEOUT: Duration = Duration::from_secs(90);
const COMPONENT_READY_TIMEOUT: Duration = Duration::from_secs(30);
const DEVICE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

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
    backend: Option<CrosvmBackend>,
    launch: Option<LaunchPlanV2>,
    adb: Option<AdbClient>,
    journal: Option<Arc<RunJournalV2>>,
    started_at: Option<OffsetDateTime>,
    device_simulator: DeviceSimulatorV2,
    #[cfg(unix)]
    device_output_sockets: Vec<tokio::net::UnixDatagram>,
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
                    frame_generation: 0,
                    frame_metrics: FrameMetricsV2::default(),
                    last_error: None,
                },
                active_spec: None,
                process: None,
                components: Vec::new(),
                backend: None,
                launch: None,
                adb: None,
                journal: None,
                started_at: None,
                device_simulator: DeviceSimulatorV2::default(),
                #[cfg(unix)]
                device_output_sockets: Vec::new(),
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
        self.mutable.lock().await.status.clone()
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
        let result = match request.command {
            WorkerCommandV2::Ping => Ok(WorkerPayloadV2::Pong(self.status().await)),
            WorkerCommandV2::Status => Ok(WorkerPayloadV2::Status(self.status().await)),
            WorkerCommandV2::Start {
                spec,
                run_id,
                leases,
                capabilities_fingerprint,
            } => self
                .start(*spec, run_id, leases, &capabilities_fingerprint)
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
        };
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
    async fn start(
        self: &Arc<Self>,
        spec: InstanceSpecV2,
        run_id: Uuid,
        leases: Vec<LeaseV2>,
        expected_capabilities: &str,
    ) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        spec.validate()?;
        if spec.id != self.instance_id {
            return Err(WorkerError::InstanceMismatch);
        }
        let frame_generation =
            validate_start_leases(&leases, &self.identity, &spec, &self.paths, &self.endpoint)?;
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
            let frame_tool = discovery.frame_tool.ok_or_else(|| {
                WorkerError::CapabilityBlocked(vec!["display.zero_copy".to_owned()])
            })?;
            let adb_bridge = discovery.adb_bridge;
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
                mutable.device_output_sockets = endpoints.output_sockets;
            }
            let backend = CrosvmBackend::new(discovery.crosvm.clone());
            backend
                .prepare_keyboard_endpoint(&endpoints.keyboard)
                .await?;
            let context = VmLaunchContextV2 {
                spec: spec.clone(),
                run_id,
                guest_cid,
                run_dir: run_dir.clone(),
                disk_overlay: overlay,
                artifacts: bundles.artifacts.clone(),
                control_endpoint: endpoints.control,
                frame_endpoint: endpoints.frame,
                keyboard_endpoint: endpoints.keyboard,
                device_endpoints: endpoints.devices,
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
            self.transition(ObservedStateV2::LaunchingGuest, None)
                .await?;
            let process = TokioProcessSupervisor
                .spawn(&ProcessSpec {
                    executable: launch.executable.clone(),
                    arguments: launch.arguments.clone(),
                    environment: launch.environment.clone(),
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

            self.transition(ObservedStateV2::NegotiatingDisplay, None)
                .await?;
            self.wait_frame_ready(&run_dir, run_id, frame_generation)
                .await?;
            self.transition(ObservedStateV2::GuestBooting, None).await?;

            if let Some(serial) = &launch.adb_serial {
                self.transition(ObservedStateV2::AdbConnecting, None)
                    .await?;
                let bridge = adb_bridge
                    .ok_or_else(|| WorkerError::CapabilityBlocked(vec!["adb.bridge".to_owned()]))?;
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
                        },
                    },
                    &journal,
                )
                .await?;
                let adb = AdbClient::new(discovery.adb, None)
                    .with_aapt2(bundles.artifacts.host_tools.get("aapt2").cloned());
                adb.connect(serial).await.map_err(WorkerError::Adb)?;
                adb.wait_ready(serial).await.map_err(WorkerError::Adb)?;
                self.ensure_components_alive().await?;
                self.mutable.lock().await.adb = Some(adb);
            } else {
                return Err(WorkerError::ReadinessUnavailable);
            }
            self.transition(ObservedStateV2::Ready, None).await?;
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
        let (process, backend, launch) = {
            let mut mutable = self.mutable.lock().await;
            (
                mutable.process.take(),
                mutable.backend.clone(),
                mutable.launch.clone(),
            )
        };
        let mut retained_process = None;
        let mut cleanup_error = None;
        if let Some(mut process) = process {
            let mut exited = false;
            if matches!(mode, StopModeV2::Graceful)
                && let (Some(backend), Some(launch)) = (&backend, &launch)
            {
                match backend.power_button(&launch.control_endpoint).await {
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
        mutable.components.clear();
        mutable.journal = None;
        #[cfg(unix)]
        mutable.device_output_sockets.clear();
        Ok(())
    }

    async fn pause(&self) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        if self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::Busy("pause requires Ready"));
        }
        self.transition(ObservedStateV2::Pausing, None).await?;
        let (backend, endpoint) = self.backend_control().await?;
        backend.pause(&endpoint).await?;
        self.transition(ObservedStateV2::Paused, None).await
    }

    async fn resume(&self) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        if self.status().await.observed != ObservedStateV2::Paused {
            return Err(WorkerError::Busy("resume requires Paused"));
        }
        self.transition(ObservedStateV2::Resuming, None).await?;
        let (backend, endpoint) = self.backend_control().await?;
        backend.resume(&endpoint).await?;
        self.transition(ObservedStateV2::Ready, None).await
    }

    async fn reconfigure(
        &self,
        display: hd_core::DisplayConfigV2,
        adb_config: hd_core::AdbConfigV2,
    ) -> Result<(), WorkerError> {
        let _operation = self.operation.lock().await;
        let (mut spec, backend, endpoint, adb, serial) = {
            let mutable = self.mutable.lock().await;
            (
                mutable.active_spec.clone().ok_or(WorkerError::NotRunning)?,
                mutable.backend.clone().ok_or(WorkerError::NotRunning)?,
                mutable
                    .launch
                    .as_ref()
                    .map(|plan| plan.control_endpoint.clone())
                    .ok_or(WorkerError::NotRunning)?,
                mutable.adb.clone(),
                mutable.status.adb_serial.clone(),
            )
        };
        if display.vsync != spec.display.vsync || adb_config != spec.adb {
            return Err(WorkerError::RestartRequired);
        }
        let previous = spec.display.clone();
        backend.replace_display(&endpoint, &display).await?;
        if display.orientation != previous.orientation
            && let (Some(adb), Some(serial)) = (&adb, serial.as_deref())
            && let Err(error) = adb.set_orientation(serial, display.orientation).await
        {
            backend.replace_display(&endpoint, &previous).await?;
            return Err(WorkerError::Adb(error));
        }
        spec.display = display;
        self.mutable.lock().await.active_spec = Some(spec);
        Ok(())
    }

    async fn action(&self, action: InstanceActionV2) -> Result<(), WorkerError> {
        if self.status().await.observed != ObservedStateV2::Ready {
            return Err(WorkerError::NotReady);
        }
        match action {
            InstanceActionV2::Key { key } => {
                let (backend, endpoint) = {
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
                backend.send_key(&endpoint, key).await?;
            }
            InstanceActionV2::Rotate { orientation } => {
                let (mut display, adb) = {
                    let mutable = self.mutable.lock().await;
                    let spec = mutable
                        .active_spec
                        .as_ref()
                        .ok_or(WorkerError::NotRunning)?;
                    (spec.display.clone(), spec.adb.clone())
                };
                display.orientation = orientation;
                self.reconfigure(display, adb).await?;
            }
            InstanceActionV2::SetLocation { location } => {
                self.device_action("mcu-control", DeviceCommandV2::SetLocation { location })
                    .await?;
            }
            InstanceActionV2::SetBattery { battery } => {
                self.device_action("mcu-control", DeviceCommandV2::SetBattery { battery })
                    .await?;
            }
            InstanceActionV2::SetNetworkCondition { condition } => {
                self.device_action(
                    "mcu-control",
                    DeviceCommandV2::SetNetworkCondition { condition },
                )
                .await?;
            }
            InstanceActionV2::InjectSensor { injection } => {
                self.device_action("sensors", DeviceCommandV2::InjectSensor { injection })
                    .await?;
            }
            InstanceActionV2::BluetoothPeer { .. } => {
                return Err(WorkerError::ExternalDeviceControl("rootcanal"));
            }
            InstanceActionV2::NfcTag { .. } => {
                return Err(WorkerError::ExternalDeviceControl("casimir"));
            }
        }
        Ok(())
    }

    async fn device_action(&self, role: &str, command: DeviceCommandV2) -> Result<(), WorkerError> {
        let request = DeviceRequestV2 {
            protocol_version: hd_device_sim::DEVICE_SIM_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            command,
        };
        let (endpoint, candidate, response) = {
            let mutable = self.mutable.lock().await;
            let endpoint = mutable
                .launch
                .as_ref()
                .and_then(|plan| plan.device_endpoints.get(role))
                .cloned()
                .ok_or(WorkerError::DeviceEndpoint(role.to_owned()))?;
            let mut candidate = mutable.device_simulator.clone();
            let response = candidate.handle(request.clone());
            (endpoint, candidate, response)
        };
        if !response.ok {
            return Err(WorkerError::DeviceRejected(
                response
                    .message
                    .unwrap_or_else(|| "device request was rejected".to_owned()),
            ));
        }
        let mut bytes = serde_json::to_vec(&request)?;
        if bytes.len() > MAX_DEVICE_MESSAGE_BYTES {
            return Err(WorkerError::DeviceMessageTooLarge);
        }
        bytes.push(b'\n');
        write_device_input(&endpoint.guest_input, &bytes).await?;
        self.mutable.lock().await.device_simulator = candidate;
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
            if cleanup_error.is_none() {
                mutable.backend = None;
                mutable.launch = None;
                #[cfg(unix)]
                mutable.device_output_sockets.clear();
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
    #[cfg(unix)]
    output_sockets: Vec<tokio::net::UnixDatagram>,
}

impl RuntimeEndpoints {
    fn create(spec: &InstanceSpecV2, run_id: Uuid) -> Result<Self, WorkerError> {
        let control = runtime_endpoint(spec.id, run_id, "vm-control", "sock")?;
        let frame = runtime_endpoint(spec.id, run_id, "frame", "sock")?;
        let keyboard = runtime_endpoint(spec.id, run_id, "keyboard", "sock")?;
        let mut devices = BTreeMap::new();
        #[cfg(unix)]
        let mut output_sockets = Vec::new();
        for role in [
            "bluetooth",
            "gnss",
            "location",
            "uwb",
            "nfc",
            "sensors",
            "mcu-control",
            "mcu-uart",
        ] {
            let enabled = match role {
                "bluetooth" => spec.devices.bluetooth,
                "gnss" | "location" => spec.devices.gnss,
                "uwb" => spec.devices.uwb,
                "nfc" => spec.devices.nfc,
                "sensors" => spec.devices.sensors,
                "mcu-control" | "mcu-uart" => spec.devices.power,
                _ => false,
            };
            if !enabled {
                continue;
            }
            let output = runtime_endpoint(spec.id, run_id, &format!("{role}-out"), "sock")?;
            let input = runtime_endpoint(spec.id, run_id, &format!("{role}-in"), "fifo")?;
            #[cfg(unix)]
            {
                let socket =
                    tokio::net::UnixDatagram::bind(&output).map_err(|source| WorkerError::Io {
                        operation: "bind device output socket",
                        path: PathBuf::from(&output),
                        source,
                    })?;
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o600)).map_err(
                    |source| WorkerError::Io {
                        operation: "secure device output socket",
                        path: PathBuf::from(&output),
                        source,
                    },
                )?;
                hd_platform::create_owner_only_fifo(Path::new(&input))?;
                output_sockets.push(socket);
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
            #[cfg(unix)]
            output_sockets,
        })
    }
}

fn runtime_endpoint(
    instance_id: Uuid,
    run_id: Uuid,
    role: &str,
    _suffix: &str,
) -> Result<String, WorkerError> {
    #[cfg(windows)]
    {
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
        let path = root.join(format!("hd-{scope}-{instance}-{run}-{role}.{_suffix}"));
        if path.as_os_str().len() >= 100 {
            return Err(WorkerError::EndpointTooLong(path));
        }
        Ok(path.to_string_lossy().into_owned())
    }
}

#[cfg(unix)]
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
            if !(metadata.file_type().is_socket() || metadata.file_type().is_fifo()) {
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

async fn write_device_input(endpoint: &str, bytes: &[u8]) -> Result<(), WorkerError> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let operation = async {
            loop {
                match ClientOptions::new().open(endpoint) {
                    Ok(mut pipe) => {
                        pipe.write_all(bytes).await?;
                        pipe.flush().await?;
                        return Ok::<_, std::io::Error>(());
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::WouldBlock
                        ) =>
                    {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        tokio::time::timeout(DEVICE_WRITE_TIMEOUT, operation)
            .await
            .map_err(|_| WorkerError::DeviceWriteTimeout)?
            .map_err(|source| WorkerError::Io {
                operation: "write device named pipe",
                path: PathBuf::from(endpoint),
                source,
            })
    }
    #[cfg(unix)]
    {
        let endpoint = endpoint.to_owned();
        let bytes = bytes.to_vec();
        tokio::time::timeout(
            DEVICE_WRITE_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new().write(true).open(&endpoint)?;
                file.write_all(&bytes)?;
                file.flush()
            }),
        )
        .await
        .map_err(|_| WorkerError::DeviceWriteTimeout)?
        .map_err(|error| WorkerError::Task(error.to_string()))?
        .map_err(|source| WorkerError::Io {
            operation: "write device FIFO",
            path: PathBuf::from(endpoint),
            source,
        })
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
    #[error("device message exceeds the protocol limit")]
    DeviceMessageTooLarge,
    #[error("device input write timed out")]
    DeviceWriteTimeout,
    #[error("typed control for {0} requires its signed formal component adapter")]
    ExternalDeviceControl(&'static str),
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
            Self::InstanceMismatch => "worker_instance_mismatch",
            Self::Busy(_) => "worker_busy",
            Self::NotRunning => "worker_not_running",
            Self::NotReady => "worker_not_ready",
            Self::RestartRequired => "restart_required",
            Self::ReadinessUnavailable => "readiness_unavailable",
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
            Self::DeviceMessageTooLarge => "device_message_too_large",
            Self::DeviceWriteTimeout => "device_write_timeout",
            Self::ExternalDeviceControl(_) => "device_control_adapter",
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
            Self::Json(_) => "json",
            Self::Io { .. } => "io",
        }
    }

    pub fn api_error(&self) -> ApiErrorV2 {
        ApiErrorV2::new(self.code(), self.to_string()).retryable(matches!(
            self,
            Self::Busy(_) | Self::CapabilityChanged { .. } | Self::DeviceWriteTimeout
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
