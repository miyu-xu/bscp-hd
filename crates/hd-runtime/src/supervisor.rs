use std::collections::{BTreeMap, HashMap};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fs2::FileExt;
use hd_core::{
    CONTROL_PROTOCOL_VERSION, ControlCommandV1, ControlPayloadV1, ControlRequestV1,
    ControlResponseV1, DiagnosisV1, DiagnosticCheckV1, DiagnosticStatus, DisplayConfig,
    GPU_STATS_PROTOCOL_VERSION, InstanceAction, InstanceConfigV1, InstanceState, InstanceSummaryV1,
    LaunchPlanV1, RunResultV1, StateSnapshot,
};
use hd_platform::{
    DataPaths, DiskProvisionMethod, DiskProvisioner, PlatformDisplayLease, ProcessSpec,
    ProcessSupervisor, VmBackend,
};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::process::Child;
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{
    AdbClient, CrosvmBackend, NativeDiskProvisioner, RunJournal, RunManifestV1,
    TokioProcessSupervisor, artifact_diagnostics, validate_artifacts,
};

#[derive(Debug)]
struct InstanceRecord {
    config: InstanceConfigV1,
    state: StateSnapshot,
    display_lease: Option<PlatformDisplayLease>,
    child: Option<Child>,
    run_id: Option<Uuid>,
    run_started_at: Option<OffsetDateTime>,
    journal: Option<Arc<RunJournal>>,
    adb_serial: Option<String>,
    host_fps_milli: Option<u32>,
    telemetry_tasks: Vec<JoinHandle<()>>,
    mock_mode: bool,
}

impl InstanceRecord {
    fn new(config: InstanceConfigV1) -> Self {
        Self {
            config,
            state: StateSnapshot::default(),
            display_lease: None,
            child: None,
            run_id: None,
            run_started_at: None,
            journal: None,
            adb_serial: None,
            host_fps_milli: None,
            telemetry_tasks: Vec::new(),
            mock_mode: false,
        }
    }

    fn summary(&self) -> InstanceSummaryV1 {
        InstanceSummaryV1 {
            id: self.config.id,
            name: self.config.name.clone(),
            state: self.state.clone(),
            adb_serial: self.adb_serial.clone(),
            host_fps_milli: self.host_fps_milli,
        }
    }
}

#[derive(Debug)]
pub struct Supervisor {
    paths: DataPaths,
    _data_lock: std::fs::File,
    backend: CrosvmBackend,
    disk: NativeDiskProvisioner,
    process: TokioProcessSupervisor,
    instances: RwLock<HashMap<Uuid, InstanceRecord>>,
    shutdown: AtomicBool,
}

impl Supervisor {
    pub fn new(paths: DataPaths, backend: CrosvmBackend) -> Result<Self, SupervisorError> {
        paths.ensure()?;
        let data_lock = acquire_data_lock(&paths)?;
        let mut instances = HashMap::new();
        if paths.instances.is_dir() {
            for entry in
                std::fs::read_dir(&paths.instances).map_err(|source| SupervisorError::Io {
                    operation: "scan instance directory",
                    path: paths.instances.clone(),
                    source,
                })?
            {
                let entry = entry.map_err(|source| SupervisorError::Io {
                    operation: "read instance directory entry",
                    path: paths.instances.clone(),
                    source,
                })?;
                let config_path = entry.path().join("instance.json");
                if !config_path.is_file() {
                    continue;
                }
                match InstanceConfigV1::load(&config_path) {
                    Ok(config) => {
                        instances.insert(config.id, InstanceRecord::new(config));
                    }
                    Err(error) => {
                        tracing::warn!(path = %config_path.display(), %error, "ignoring invalid instance config");
                    }
                }
            }
        }
        Ok(Self {
            paths,
            _data_lock: data_lock,
            backend,
            disk: NativeDiskProvisioner,
            process: TokioProcessSupervisor,
            instances: RwLock::new(instances),
            shutdown: AtomicBool::new(false),
        })
    }

    pub fn paths(&self) -> &DataPaths {
        &self.paths
    }

    pub fn backend(&self) -> &CrosvmBackend {
        &self.backend
    }

    pub fn should_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    pub async fn summaries(&self) -> Vec<InstanceSummaryV1> {
        let instances = self.instances.read().await;
        let mut summaries = instances
            .values()
            .map(InstanceRecord::summary)
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        summaries
    }

    pub async fn config(&self, id: Uuid) -> Option<InstanceConfigV1> {
        self.instances
            .read()
            .await
            .get(&id)
            .map(|record| record.config.clone())
    }

    pub async fn set_display_lease(
        &self,
        id: Uuid,
        lease: Option<PlatformDisplayLease>,
    ) -> Result<(), SupervisorError> {
        let mut instances = self.instances.write().await;
        let record = instances
            .get_mut(&id)
            .ok_or(SupervisorError::UnknownInstance(id))?;
        if record.child.is_some() {
            return Err(SupervisorError::Busy(
                "cannot replace the native display lease while crosvm is running",
            ));
        }
        record.display_lease = lease;
        Ok(())
    }

    pub async fn handle(self: &Arc<Self>, request: ControlRequestV1) -> ControlResponseV1 {
        if request.protocol_version != CONTROL_PROTOCOL_VERSION {
            return ControlResponseV1::failure(
                request.request_id,
                "protocol_version",
                format!(
                    "unsupported control protocol {}, expected {}",
                    request.protocol_version, CONTROL_PROTOCOL_VERSION
                ),
            );
        }
        let request_id = request.request_id;
        let result = self.execute(request.command).await;
        match result {
            Ok(payload) => ControlResponseV1::success(request_id, payload),
            Err(error) => {
                tracing::warn!(%request_id, %error, "control request failed");
                ControlResponseV1::failure(request_id, error.code(), error.to_string())
            }
        }
    }

    async fn execute(
        self: &Arc<Self>,
        command: ControlCommandV1,
    ) -> Result<ControlPayloadV1, SupervisorError> {
        match command {
            ControlCommandV1::Ping => Ok(ControlPayloadV1::Pong),
            ControlCommandV1::List => Ok(ControlPayloadV1::Instances(self.summaries().await)),
            ControlCommandV1::Show { id } => {
                Ok(ControlPayloadV1::Instance(self.summary(id).await?))
            }
            ControlCommandV1::Create { config } => {
                let summary = self.create(config).await?;
                Ok(ControlPayloadV1::Instance(summary))
            }
            ControlCommandV1::Update { config } => {
                let summary = self.update(config).await?;
                Ok(ControlPayloadV1::Instance(summary))
            }
            ControlCommandV1::Start { id, mock } => {
                if let Err(error) = self.start(id, mock).await {
                    self.recover_start_error(id, &error).await;
                    return Err(error);
                }
                Ok(ControlPayloadV1::Instance(self.summary(id).await?))
            }
            ControlCommandV1::Stop { id } => {
                self.stop(id).await?;
                Ok(ControlPayloadV1::Instance(self.summary(id).await?))
            }
            ControlCommandV1::Delete { id } => {
                self.delete(id).await?;
                Ok(ControlPayloadV1::Empty)
            }
            ControlCommandV1::Action { id, action } => {
                self.action(id, action).await?;
                Ok(ControlPayloadV1::Instance(self.summary(id).await?))
            }
            ControlCommandV1::InstallApk { id, path } => {
                self.install_apk(id, &path).await?;
                Ok(ControlPayloadV1::Empty)
            }
            ControlCommandV1::ApplyDisplay { id, display } => {
                self.apply_display(id, display).await?;
                Ok(ControlPayloadV1::Instance(self.summary(id).await?))
            }
            ControlCommandV1::Diagnose { id } => {
                Ok(ControlPayloadV1::Diagnosis(self.diagnose(id).await?))
            }
            ControlCommandV1::Shutdown => {
                self.stop_all().await;
                self.shutdown.store(true, Ordering::Release);
                Ok(ControlPayloadV1::Empty)
            }
        }
    }

    async fn summary(&self, id: Uuid) -> Result<InstanceSummaryV1, SupervisorError> {
        self.instances
            .read()
            .await
            .get(&id)
            .map(InstanceRecord::summary)
            .ok_or(SupervisorError::UnknownInstance(id))
    }

    async fn create(&self, config: InstanceConfigV1) -> Result<InstanceSummaryV1, SupervisorError> {
        config.validate()?;
        let mut instances = self.instances.write().await;
        if instances.contains_key(&config.id) {
            return Err(SupervisorError::AlreadyExists(config.id));
        }
        ensure_unique_adb_port(&instances, &config)?;
        config.save(&self.paths.instance_config(config.id))?;
        let record = InstanceRecord::new(config);
        let summary = record.summary();
        instances.insert(summary.id, record);
        tracing::info!(instance_id = %summary.id, name = %summary.name, "instance created");
        Ok(summary)
    }

    async fn update(&self, config: InstanceConfigV1) -> Result<InstanceSummaryV1, SupervisorError> {
        config.validate()?;
        let mut instances = self.instances.write().await;
        ensure_unique_adb_port(&instances, &config)?;
        let record = instances
            .get_mut(&config.id)
            .ok_or(SupervisorError::UnknownInstance(config.id))?;
        if record.child.is_some()
            || !matches!(
                record.state.state,
                InstanceState::Defined
                    | InstanceState::Stopped
                    | InstanceState::Failed
                    | InstanceState::Blocked
            )
        {
            return Err(SupervisorError::Busy(
                "stop the instance before changing launch settings",
            ));
        }
        config.save(&self.paths.instance_config(config.id))?;
        record.config = config;
        tracing::info!(
            instance_id = %record.config.id,
            cpu_count = record.config.cpu_count,
            memory_mib = record.config.memory_mib,
            "instance launch configuration updated"
        );
        Ok(record.summary())
    }

    #[allow(clippy::too_many_lines)]
    async fn start(self: &Arc<Self>, id: Uuid, mock: bool) -> Result<(), SupervisorError> {
        tracing::info!(instance_id = %id, mock, "instance start requested");
        let (mut config, display_lease) = {
            let mut instances = self.instances.write().await;
            let record = instances
                .get_mut(&id)
                .ok_or(SupervisorError::UnknownInstance(id))?;
            if record.child.is_some()
                || matches!(
                    record.state.state,
                    InstanceState::Preparing
                        | InstanceState::Launching
                        | InstanceState::DisplayAttached
                        | InstanceState::GuestBooting
                        | InstanceState::AdbConnecting
                        | InstanceState::Ready
                        | InstanceState::Stopping
                )
            {
                return Err(SupervisorError::Busy("instance is already active"));
            }
            for task in record.telemetry_tasks.drain(..) {
                task.abort();
            }
            record.run_id = None;
            record.run_started_at = None;
            record.journal = None;
            record.adb_serial = None;
            record.host_fps_milli = None;
            record.mock_mode = mock;
            transition(record, InstanceState::Preparing, None)?;
            (record.config.clone(), record.display_lease.clone())
        };

        let run_id = Uuid::new_v4();
        let started_at = OffsetDateTime::now_utc();
        let run_dir = self.paths.run_dir(id, run_id);
        let journal = match RunJournal::create(&run_dir) {
            Ok(journal) => Arc::new(journal),
            Err(error) => {
                self.fail_start(id, InstanceState::Failed, error.to_string())
                    .await?;
                return Err(SupervisorError::Journal(error));
            }
        };
        {
            let mut instances = self.instances.write().await;
            let record = instances
                .get_mut(&id)
                .ok_or(SupervisorError::UnknownInstance(id))?;
            record.run_id = Some(run_id);
            record.run_started_at = Some(started_at);
            record.journal = Some(Arc::clone(&journal));
        }
        journal.event(
            "run",
            "created",
            Some(InstanceState::Preparing),
            BTreeMap::from([
                ("run_id".to_owned(), run_id.to_string()),
                (
                    "mode".to_owned(),
                    if mock { "mock" } else { "crosvm" }.to_owned(),
                ),
            ]),
        )?;

        if mock {
            let plan = mock_launch_plan(&config, display_lease.as_ref(), &run_dir);
            journal.write_manifest(&RunManifestV1 {
                schema_version: 1,
                run_id,
                instance: config.clone(),
                launch_plan: plan,
                artifacts: Vec::new(),
                toolchain: toolchain_fingerprint(&self.backend),
            })?;
            {
                let mut instances = self.instances.write().await;
                let record = instances
                    .get_mut(&id)
                    .ok_or(SupervisorError::UnknownInstance(id))?;
                for state in [
                    InstanceState::Launching,
                    InstanceState::DisplayAttached,
                    InstanceState::GuestBooting,
                    InstanceState::AdbConnecting,
                    InstanceState::Ready,
                ] {
                    transition(record, state, Some("mock backend".to_owned()))?;
                }
            }
            return Ok(());
        }

        let proposed_plan = match self
            .backend
            .build_launch_plan(&config, display_lease.as_ref(), &run_dir)
            .await
        {
            Ok(plan) => plan,
            Err(error) => {
                self.fail_start(id, InstanceState::Failed, error.to_string())
                    .await?;
                return Err(SupervisorError::Platform(error));
            }
        };
        journal.write_manifest(&RunManifestV1 {
            schema_version: 1,
            run_id,
            instance: config.clone(),
            launch_plan: proposed_plan,
            artifacts: Vec::new(),
            toolchain: toolchain_fingerprint(&self.backend),
        })?;

        let artifacts = match validate_artifacts(&config.artifacts).await {
            Ok(artifacts) => artifacts,
            Err(error) => {
                journal.event(
                    "preflight",
                    "artifact_validation_failed",
                    Some(InstanceState::Blocked),
                    BTreeMap::from([("error".to_owned(), error.to_string())]),
                )?;
                self.fail_start(id, InstanceState::Blocked, error.to_string())
                    .await?;
                return Err(SupervisorError::Artifacts(error));
            }
        };
        journal.event(
            "preflight",
            "artifact_validation_passed",
            Some(InstanceState::Preparing),
            BTreeMap::from([("artifact_count".to_owned(), artifacts.len().to_string())]),
        )?;

        let private_disk = crate::unique_disk_path(&self.paths.disks, id);
        if private_disk.exists() {
            let bytes = std::fs::metadata(&private_disk)
                .map_err(|source| SupervisorError::Io {
                    operation: "read existing private disk metadata",
                    path: private_disk.clone(),
                    source,
                })?
                .len();
            journal.event(
                "storage",
                "disk_reused",
                Some(InstanceState::Preparing),
                BTreeMap::from([
                    ("path".to_owned(), private_disk.display().to_string()),
                    ("bytes".to_owned(), bytes.to_string()),
                ]),
            )?;
        } else {
            let provisioned = match self
                .disk
                .provision_full_copy(&config.artifacts.rootfs, &private_disk)
                .await
            {
                Ok(provisioned) => provisioned,
                Err(error) => {
                    self.fail_start(id, InstanceState::Failed, error.to_string())
                        .await?;
                    return Err(SupervisorError::Platform(error));
                }
            };
            let mut fields = BTreeMap::new();
            fields.insert("path".to_owned(), provisioned.path.display().to_string());
            fields.insert("bytes".to_owned(), provisioned.bytes.to_string());
            fields.insert(
                "method".to_owned(),
                match provisioned.method {
                    DiskProvisionMethod::BlockClone => "block_clone",
                    DiskProvisionMethod::FullCopyFallback => "full_copy_fallback",
                }
                .to_owned(),
            );
            journal.event(
                "storage",
                "disk_provisioned",
                Some(InstanceState::Preparing),
                fields,
            )?;
        }
        config.artifacts.rootfs = private_disk;

        let plan = match self
            .backend
            .build_launch_plan(&config, display_lease.as_ref(), &run_dir)
            .await
        {
            Ok(plan) => plan,
            Err(error) => {
                self.fail_start(id, InstanceState::Failed, error.to_string())
                    .await?;
                return Err(SupervisorError::Platform(error));
            }
        };
        journal.write_manifest(&RunManifestV1 {
            schema_version: 1,
            run_id,
            instance: config.clone(),
            launch_plan: plan.clone(),
            artifacts,
            toolchain: toolchain_fingerprint(&self.backend),
        })?;

        let spec = ProcessSpec {
            executable: plan.executable.clone(),
            arguments: plan.arguments.clone(),
            environment: plan.environment.clone(),
            working_directory: plan.working_directory.clone(),
            stdout_path: run_dir.join("crosvm.stdout.log"),
            stderr_path: run_dir.join("crosvm.stderr.log"),
        };
        let child = match self.process.spawn(&spec).await {
            Ok(child) => child,
            Err(error) => {
                journal.event(
                    "process",
                    "spawn_failed",
                    Some(InstanceState::Failed),
                    BTreeMap::from([("error".to_owned(), error.to_string())]),
                )?;
                self.fail_start(id, InstanceState::Failed, error.to_string())
                    .await?;
                return Err(SupervisorError::Platform(error));
            }
        };
        journal.event(
            "process",
            "spawned",
            Some(InstanceState::Launching),
            BTreeMap::from([
                (
                    "executable".to_owned(),
                    spec.executable.display().to_string(),
                ),
                (
                    "pid".to_owned(),
                    child
                        .id()
                        .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
                ),
            ]),
        )?;

        {
            let mut instances = self.instances.write().await;
            let record = instances
                .get_mut(&id)
                .ok_or(SupervisorError::UnknownInstance(id))?;
            record.child = Some(child);
            record.adb_serial.clone_from(&plan.adb_serial);
            record.telemetry_tasks = self.spawn_telemetry_listener(plan.gpu_stats_endpoint);
            transition(record, InstanceState::Launching, None)?;
            transition(record, InstanceState::DisplayAttached, None)?;
            transition(record, InstanceState::GuestBooting, None)?;
        }
        Ok(())
    }

    async fn fail_start(
        &self,
        id: Uuid,
        state: InstanceState,
        reason: String,
    ) -> Result<(), SupervisorError> {
        let (journal, run_id, started_at, tasks) = {
            let mut instances = self.instances.write().await;
            let record = instances
                .get_mut(&id)
                .ok_or(SupervisorError::UnknownInstance(id))?;
            transition(record, state, Some(reason.clone()))?;
            let values = (
                record.journal.clone(),
                record.run_id,
                record.run_started_at,
                std::mem::take(&mut record.telemetry_tasks),
            );
            record.run_id = None;
            record.run_started_at = None;
            record.journal = None;
            record.adb_serial = None;
            record.host_fps_milli = None;
            values
        };
        for task in tasks {
            task.abort();
        }
        if let (Some(journal), Some(run_id), Some(started_at)) = (journal, run_id, started_at) {
            journal.finish(&RunResultV1 {
                schema_version: 1,
                run_id,
                instance_id: id,
                started_at,
                finished_at: Some(OffsetDateTime::now_utc()),
                final_state: state,
                exit_code: None,
                reason: Some(reason),
            })?;
        }
        Ok(())
    }

    async fn recover_start_error(&self, id: Uuid, error: &SupervisorError) {
        if matches!(
            error,
            SupervisorError::Busy(_) | SupervisorError::UnknownInstance(_)
        ) {
            return;
        }
        let reason = error.to_string();
        let recovered = {
            let mut instances = self.instances.write().await;
            let Some(record) = instances.get_mut(&id) else {
                return;
            };
            if record.state.state.is_terminal()
                || matches!(
                    record.state.state,
                    InstanceState::Stopping | InstanceState::Stopped
                )
            {
                return;
            }
            if let Err(state_error) = record
                .state
                .transition(InstanceState::Failed, Some(reason.clone()))
            {
                tracing::error!(%id, %state_error, %reason, "could not recover failed start state");
                return;
            }
            tracing::error!(
                instance_id = %id,
                revision = record.state.revision,
                %reason,
                "instance start failed"
            );
            if let Some(journal) = &record.journal
                && let Err(journal_error) = journal.event(
                    "state",
                    "transition",
                    Some(InstanceState::Failed),
                    BTreeMap::from([("reason".to_owned(), reason.clone())]),
                )
            {
                tracing::error!(%id, %journal_error, "failed to journal recovered start state");
            }
            let values = (
                record.child.take(),
                std::mem::take(&mut record.telemetry_tasks),
                record.journal.clone(),
                record.run_id,
                record.run_started_at,
            );
            record.journal = None;
            record.run_id = None;
            record.run_started_at = None;
            record.adb_serial = None;
            record.host_fps_milli = None;
            values
        };
        let (mut child, tasks, journal, run_id, started_at) = recovered;
        for task in tasks {
            task.abort();
        }
        let mut exit_code = None;
        if let Some(child) = child.as_mut() {
            if let Err(process_error) = self.process.terminate(child).await {
                tracing::error!(%id, %process_error, "failed to terminate VM after start error");
            } else {
                match self.process.wait(child).await {
                    Ok(exit) => exit_code = exit.code,
                    Err(process_error) => {
                        tracing::error!(%id, %process_error, "failed to reap VM after start error");
                    }
                }
            }
        }
        if let (Some(journal), Some(run_id), Some(started_at)) = (journal, run_id, started_at)
            && let Err(journal_error) = journal.finish(&RunResultV1 {
                schema_version: 1,
                run_id,
                instance_id: id,
                started_at,
                finished_at: Some(OffsetDateTime::now_utc()),
                final_state: InstanceState::Failed,
                exit_code,
                reason: Some(reason),
            })
        {
            tracing::error!(%id, %journal_error, "failed to finish recovered start journal");
        }
    }

    fn spawn_telemetry_listener(self: &Arc<Self>, endpoint: String) -> Vec<JoinHandle<()>> {
        let (sender, mut receiver) = mpsc::channel(64);
        let listener = tokio::spawn(async move {
            if let Err(error) = crate::run_gpu_stats_listener(endpoint, sender).await {
                tracing::warn!(%error, "GPU telemetry listener stopped");
            }
        });
        let weak = Arc::downgrade(self);
        let consumer = tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let Some(supervisor) = weak.upgrade() else {
                    break;
                };
                if event.protocol_version != GPU_STATS_PROTOCOL_VERSION {
                    tracing::warn!(
                        protocol_version = event.protocol_version,
                        "ignoring incompatible GPU telemetry event"
                    );
                    continue;
                }
                let mut instances = supervisor.instances.write().await;
                if let Some(record) = instances.get_mut(&event.instance_id)
                    && record.config.display.show_host_fps
                {
                    record.host_fps_milli = Some(event.fps_milli);
                }
            }
        });
        vec![listener, consumer]
    }

    async fn stop(&self, id: Uuid) -> Result<(), SupervisorError> {
        let (mut child, journal, run_id, started_at, tasks) = {
            let mut instances = self.instances.write().await;
            let record = instances
                .get_mut(&id)
                .ok_or(SupervisorError::UnknownInstance(id))?;
            if matches!(
                record.state.state,
                InstanceState::Defined | InstanceState::Stopped
            ) {
                return Ok(());
            }
            if matches!(
                record.state.state,
                InstanceState::Failed | InstanceState::Blocked
            ) {
                transition(record, InstanceState::Stopped, None)?;
            } else {
                transition(record, InstanceState::Stopping, None)?;
            }
            (
                record.child.take(),
                record.journal.clone(),
                record.run_id,
                record.run_started_at,
                std::mem::take(&mut record.telemetry_tasks),
            )
        };
        for task in tasks {
            task.abort();
        }
        let mut exit_code = None;
        if let Some(child) = child.as_mut() {
            self.process.terminate(child).await?;
            exit_code = self.process.wait(child).await?.code;
        }
        {
            let mut instances = self.instances.write().await;
            let record = instances
                .get_mut(&id)
                .ok_or(SupervisorError::UnknownInstance(id))?;
            if record.state.state != InstanceState::Stopped {
                transition(record, InstanceState::Stopped, None)?;
            }
            record.adb_serial = None;
            record.host_fps_milli = None;
            record.mock_mode = false;
            record.run_id = None;
            record.run_started_at = None;
            record.journal = None;
        }
        if let (Some(journal), Some(run_id), Some(started_at)) = (journal, run_id, started_at) {
            journal.finish(&RunResultV1 {
                schema_version: 1,
                run_id,
                instance_id: id,
                started_at,
                finished_at: Some(OffsetDateTime::now_utc()),
                final_state: InstanceState::Stopped,
                exit_code,
                reason: None,
            })?;
        }
        Ok(())
    }

    async fn stop_all(&self) {
        let ids = self
            .instances
            .read()
            .await
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for id in ids {
            if let Err(error) = self.stop(id).await {
                tracing::warn!(%id, %error, "failed to stop instance during shutdown");
            }
        }
    }

    async fn delete(&self, id: Uuid) -> Result<(), SupervisorError> {
        let mut instances = self.instances.write().await;
        let record = instances
            .get(&id)
            .ok_or(SupervisorError::UnknownInstance(id))?;
        if record.child.is_some() || !record.state.state.is_terminal() {
            return Err(SupervisorError::Busy(
                "stop the instance before deleting it",
            ));
        }
        instances.remove(&id);
        let instance_dir = self.paths.instance_dir(id);
        if instance_dir.is_dir() {
            std::fs::remove_dir_all(&instance_dir).map_err(|source| SupervisorError::Io {
                operation: "delete instance configuration",
                path: instance_dir,
                source,
            })?;
        }
        let disk_dir = self.paths.disks.join(id.to_string());
        if disk_dir.is_dir() {
            std::fs::remove_dir_all(&disk_dir).map_err(|source| SupervisorError::Io {
                operation: "delete instance private disk",
                path: disk_dir,
                source,
            })?;
        }
        tracing::info!(%id, "deleted instance configuration and private disk; run evidence retained");
        Ok(())
    }

    async fn action(
        self: &Arc<Self>,
        id: Uuid,
        action: InstanceAction,
    ) -> Result<(), SupervisorError> {
        if action == InstanceAction::Rotate {
            let mut display = self
                .config(id)
                .await
                .ok_or(SupervisorError::UnknownInstance(id))?
                .display;
            display.orientation = match display.orientation {
                hd_core::Orientation::Landscape => hd_core::Orientation::Portrait,
                hd_core::Orientation::Portrait => hd_core::Orientation::Landscape,
            };
            return self.apply_display(id, display).await;
        }
        let (mock_mode, serial, adb_config, journal) = {
            let instances = self.instances.read().await;
            let record = instances
                .get(&id)
                .ok_or(SupervisorError::UnknownInstance(id))?;
            (
                record.mock_mode,
                record.adb_serial.clone(),
                record.config.adb.clone(),
                record.journal.clone(),
            )
        };
        if mock_mode {
            if let Some(journal) = journal {
                journal.event(
                    "input",
                    "mock_key",
                    Some(InstanceState::Ready),
                    BTreeMap::from([("action".to_owned(), format!("{action:?}"))]),
                )?;
            }
            return Ok(());
        }
        let serial = serial.ok_or(SupervisorError::AdbUnavailable)?;
        AdbClient::from_config(&adb_config)
            .action(&serial, action)
            .await?;
        if let Some(journal) = journal {
            journal.event(
                "input",
                "adb_key",
                Some(InstanceState::Ready),
                BTreeMap::from([("action".to_owned(), format!("{action:?}"))]),
            )?;
        }
        Ok(())
    }

    async fn install_apk(&self, id: Uuid, path: &Path) -> Result<(), SupervisorError> {
        if !path.is_file() {
            return Err(SupervisorError::InvalidApk(path.to_owned()));
        }
        let (mock_mode, serial, adb_config, journal) = {
            let instances = self.instances.read().await;
            let record = instances
                .get(&id)
                .ok_or(SupervisorError::UnknownInstance(id))?;
            (
                record.mock_mode,
                record.adb_serial.clone(),
                record.config.adb.clone(),
                record.journal.clone(),
            )
        };
        if mock_mode {
            if let Some(journal) = journal {
                journal.event(
                    "adb",
                    "mock_install_apk",
                    Some(InstanceState::Ready),
                    BTreeMap::from([("path".to_owned(), path.display().to_string())]),
                )?;
            }
            return Ok(());
        }
        let serial = serial.ok_or(SupervisorError::AdbUnavailable)?;
        AdbClient::from_config(&adb_config)
            .install(&serial, path)
            .await?;
        if let Some(journal) = journal {
            journal.event(
                "adb",
                "apk_installed",
                Some(InstanceState::Ready),
                BTreeMap::from([("path".to_owned(), path.display().to_string())]),
            )?;
        }
        Ok(())
    }

    async fn apply_display(&self, id: Uuid, display: DisplayConfig) -> Result<(), SupervisorError> {
        let (config, active, mock_mode, serial) = {
            let instances = self.instances.read().await;
            let record = instances
                .get(&id)
                .ok_or(SupervisorError::UnknownInstance(id))?;
            (
                record.config.clone(),
                record.child.is_some() || record.mock_mode,
                record.mock_mode,
                record.adb_serial.clone(),
            )
        };
        let mut candidate = config.clone();
        candidate.display = display.clone();
        candidate.validate()?;

        if active && !mock_mode {
            if display.vsync != config.display.vsync {
                return Err(SupervisorError::Busy(
                    "VSync changes require an instance restart",
                ));
            }
            let surface_changed = display.width != config.display.width
                || display.height != config.display.height
                || display.dpi != config.display.dpi
                || display.refresh_rate_hz != config.display.refresh_rate_hz
                || display.orientation != config.display.orientation;
            if surface_changed {
                self.backend.replace_display(&config, &display).await?;
            }
            if display.orientation != config.display.orientation
                && let Some(serial) = serial
            {
                let adb = AdbClient::from_config(&config.adb);
                if let Err(error) = adb.set_orientation(&serial, display.orientation).await {
                    let rollback_result =
                        self.backend.replace_display(&config, &config.display).await;
                    if let Err(rollback_error) = rollback_result {
                        return Err(SupervisorError::Rollback(format!(
                            "orientation failed: {error}; display rollback failed: {rollback_error}"
                        )));
                    }
                    return Err(SupervisorError::Adb(error));
                }
            }
        }

        candidate.save(&self.paths.instance_config(id))?;
        let mut instances = self.instances.write().await;
        let record = instances
            .get_mut(&id)
            .ok_or(SupervisorError::UnknownInstance(id))?;
        record.config = candidate;
        if !display.show_host_fps {
            record.host_fps_milli = None;
        }
        if let Some(journal) = &record.journal {
            let (width, height) = display.oriented_size();
            journal.event(
                "display",
                "configuration_applied",
                Some(record.state.state),
                BTreeMap::from([
                    ("width".to_owned(), width.to_string()),
                    ("height".to_owned(), height.to_string()),
                    ("dpi".to_owned(), display.dpi.to_string()),
                    ("vsync".to_owned(), format!("{:?}", display.vsync)),
                ]),
            )?;
        }
        Ok(())
    }

    async fn diagnose(&self, id: Uuid) -> Result<DiagnosisV1, SupervisorError> {
        let config = self
            .config(id)
            .await
            .ok_or(SupervisorError::UnknownInstance(id))?;
        let mut checks = artifact_diagnostics(&config.artifacts);
        checks.push(file_check("crosvm", self.backend.executable()));
        if cfg!(windows) {
            checks.push(gfxstream_backend_check(self.backend.executable()));
        }
        let adb = AdbClient::from_config(&config.adb);
        checks.push(command_check("adb", adb.executable()));
        checks.push(DiagnosticCheckV1 {
            name: "adb_bridge".to_owned(),
            status: if !config.adb.enabled {
                DiagnosticStatus::Pass
            } else if config.adb.host_port.is_some() {
                DiagnosticStatus::Warn
            } else {
                DiagnosticStatus::Blocked
            },
            detail: if !config.adb.enabled {
                "ADB disabled by instance configuration".to_owned()
            } else if let Some(port) = config.adb.host_port {
                format!(
                    "host port {port} configured; guest forwarding/readiness still requires M3 integration"
                )
            } else {
                "automatic host-port allocation and guest forwarding require M3 integration"
                    .to_owned()
            },
        });
        checks.push(DiagnosticCheckV1 {
            name: "windows_abi".to_owned(),
            status: if cfg!(all(windows, target_env = "gnu")) || !cfg!(windows) {
                DiagnosticStatus::Pass
            } else {
                DiagnosticStatus::Fail
            },
            detail: if cfg!(windows) {
                format!(
                    "target_env={}; arch={}",
                    if cfg!(target_env = "gnu") {
                        "gnu"
                    } else {
                        "non-gnu"
                    },
                    std::env::consts::ARCH
                )
            } else {
                "native portable check".to_owned()
            },
        });
        Ok(DiagnosisV1 {
            instance_id: id,
            checks,
        })
    }
}

fn acquire_data_lock(paths: &DataPaths) -> Result<std::fs::File, SupervisorError> {
    let path = paths.root.join("supervisor.lock");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|source| SupervisorError::Io {
            operation: "open supervisor data lock",
            path: path.clone(),
            source,
        })?;
    FileExt::try_lock_exclusive(&file).map_err(|source| SupervisorError::Io {
        operation: "lock supervisor data root (another HD process may be running)",
        path: path.clone(),
        source,
    })?;
    file.set_len(0).map_err(|source| SupervisorError::Io {
        operation: "truncate supervisor data lock",
        path: path.clone(),
        source,
    })?;
    file.rewind().map_err(|source| SupervisorError::Io {
        operation: "rewind supervisor data lock",
        path: path.clone(),
        source,
    })?;
    writeln!(file, "pid={}", std::process::id()).map_err(|source| SupervisorError::Io {
        operation: "write supervisor data lock",
        path,
        source,
    })?;
    Ok(file)
}

fn ensure_unique_adb_port(
    instances: &HashMap<Uuid, InstanceRecord>,
    config: &InstanceConfigV1,
) -> Result<(), SupervisorError> {
    if !config.adb.enabled || config.adb.auto_port {
        return Ok(());
    }
    let Some(port) = config.adb.host_port else {
        return Ok(());
    };
    if let Some(conflict) = instances.values().find(|record| {
        record.config.id != config.id
            && record.config.adb.enabled
            && !record.config.adb.auto_port
            && record.config.adb.host_port == Some(port)
    }) {
        return Err(SupervisorError::AdbPortConflict {
            port,
            instance_id: conflict.config.id,
        });
    }
    Ok(())
}

fn transition(
    record: &mut InstanceRecord,
    state: InstanceState,
    reason: Option<String>,
) -> Result<(), SupervisorError> {
    record.state.transition(state, reason.clone())?;
    tracing::info!(
        instance_id = %record.config.id,
        ?state,
        revision = record.state.revision,
        reason,
        "instance state transition"
    );
    if let Some(journal) = &record.journal {
        journal.event(
            "state",
            "transition",
            Some(state),
            reason
                .map(|value| BTreeMap::from([("reason".to_owned(), value)]))
                .unwrap_or_default(),
        )?;
    }
    Ok(())
}

fn mock_launch_plan(
    config: &InstanceConfigV1,
    display: Option<&PlatformDisplayLease>,
    run_dir: &Path,
) -> LaunchPlanV1 {
    LaunchPlanV1 {
        schema_version: 1,
        instance_id: config.id,
        executable: PathBuf::from("mock-vm-backend"),
        arguments: Vec::new(),
        environment: BTreeMap::new(),
        working_directory: run_dir.to_owned(),
        display_lease: display.map(|lease| lease.contract.clone()),
        control_endpoint: "mock://control".to_owned(),
        gpu_stats_endpoint: "mock://gpu-stats".to_owned(),
        adb_serial: None,
    }
}

fn toolchain_fingerprint(backend: &CrosvmBackend) -> BTreeMap<String, String> {
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
                } else if cfg!(target_env = "msvc") {
                    "msvc"
                } else {
                    "native"
                }
            ),
        ),
        (
            "crosvm".to_owned(),
            backend.executable().display().to_string(),
        ),
    ])
}

fn file_check(name: &str, path: &Path) -> DiagnosticCheckV1 {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => DiagnosticCheckV1 {
            name: name.to_owned(),
            status: DiagnosticStatus::Pass,
            detail: path.display().to_string(),
        },
        Ok(_) => DiagnosticCheckV1 {
            name: name.to_owned(),
            status: DiagnosticStatus::Fail,
            detail: format!("{} is not a file", path.display()),
        },
        Err(error) => DiagnosticCheckV1 {
            name: name.to_owned(),
            status: DiagnosticStatus::Blocked,
            detail: format!("{}: {error}", path.display()),
        },
    }
}

fn command_check(name: &str, path: &Path) -> DiagnosticCheckV1 {
    if path.components().count() == 1 {
        DiagnosticCheckV1 {
            name: name.to_owned(),
            status: DiagnosticStatus::Warn,
            detail: format!("{} will be resolved from PATH at launch", path.display()),
        }
    } else {
        file_check(name, path)
    }
}

fn gfxstream_backend_check(crosvm: &Path) -> DiagnosticCheckV1 {
    let directory = crosvm.parent().unwrap_or_else(|| Path::new("."));
    let candidates = [
        directory.join("libgfxstream_backend.dll"),
        directory.join("gfxstream_backend.dll"),
    ];
    if let Some(path) = candidates.iter().find(|path| path.is_file()) {
        DiagnosticCheckV1 {
            name: "gfxstream_backend".to_owned(),
            status: DiagnosticStatus::Pass,
            detail: path.display().to_string(),
        }
    } else {
        DiagnosticCheckV1 {
            name: "gfxstream_backend".to_owned(),
            status: DiagnosticStatus::Blocked,
            detail: format!(
                "expected {} or {}",
                candidates[0].display(),
                candidates[1].display()
            ),
        }
    }
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("unknown instance {0}")]
    UnknownInstance(Uuid),
    #[error("instance {0} already exists")]
    AlreadyExists(Uuid),
    #[error("instance is busy: {0}")]
    Busy(&'static str),
    #[error("ADB is not connected for this instance")]
    AdbUnavailable,
    #[error("ADB host port {port} is already assigned to instance {instance_id}")]
    AdbPortConflict { port: u16, instance_id: Uuid },
    #[error("APK is not a regular file: {0}")]
    InvalidApk(PathBuf),
    #[error("display rollback failed: {0}")]
    Rollback(String),
    #[error(transparent)]
    Config(#[from] hd_core::ConfigError),
    #[error(transparent)]
    State(#[from] hd_core::StateTransitionError),
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
    #[error(transparent)]
    Artifacts(#[from] crate::ArtifactError),
    #[error(transparent)]
    Journal(#[from] crate::JournalError),
    #[error(transparent)]
    Adb(#[from] crate::AdbError),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl SupervisorError {
    fn code(&self) -> &'static str {
        match self {
            Self::UnknownInstance(_) => "unknown_instance",
            Self::AlreadyExists(_) => "already_exists",
            Self::Busy(_) => "busy",
            Self::AdbUnavailable | Self::Adb(_) => "adb",
            Self::AdbPortConflict { .. } => "adb_port_conflict",
            Self::InvalidApk(_) => "invalid_apk",
            Self::Rollback(_) => "rollback",
            Self::Config(_) => "config",
            Self::State(_) => "state",
            Self::Platform(_) => "platform",
            Self::Artifacts(_) => "artifacts",
            Self::Journal(_) => "journal",
            Self::Io { .. } => "io",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supervisor() -> (tempfile::TempDir, Arc<Supervisor>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = DataPaths::from_root(dir.path().join("data"));
        let supervisor = Supervisor::new(paths, CrosvmBackend::new(PathBuf::from("crosvm")))
            .expect("supervisor");
        (dir, Arc::new(supervisor))
    }

    #[tokio::test]
    async fn mock_instance_reaches_ready_and_stops() {
        let (_dir, supervisor) = supervisor();
        let config = InstanceConfigV1::default();
        let id = config.id;
        supervisor.create(config).await.expect("create");
        supervisor.start(id, true).await.expect("start");
        assert_eq!(
            supervisor.summary(id).await.expect("summary").state.state,
            InstanceState::Ready
        );
        supervisor.stop(id).await.expect("stop");
        assert_eq!(
            supervisor.summary(id).await.expect("summary").state.state,
            InstanceState::Stopped
        );
    }

    #[tokio::test]
    async fn real_start_is_blocked_by_missing_external_artifacts() {
        let (_dir, supervisor) = supervisor();
        let config = InstanceConfigV1::default();
        let id = config.id;
        supervisor.create(config).await.expect("create");
        assert!(supervisor.start(id, false).await.is_err());
        assert_eq!(
            supervisor.summary(id).await.expect("summary").state.state,
            InstanceState::Blocked
        );
        supervisor.stop(id).await.expect("stop blocked instance");
        assert_eq!(
            supervisor.summary(id).await.expect("summary").state.state,
            InstanceState::Stopped
        );
    }

    #[tokio::test]
    async fn static_adb_ports_are_unique_across_instances() {
        let (_dir, supervisor) = supervisor();
        let mut first = InstanceConfigV1::default();
        first.adb.auto_port = false;
        first.adb.host_port = Some(6520);
        supervisor.create(first).await.expect("create first");

        let mut second = InstanceConfigV1::default();
        second.adb.auto_port = false;
        second.adb.host_port = Some(6520);
        let error = supervisor
            .create(second)
            .await
            .expect_err("duplicate ADB port must be rejected");

        assert!(matches!(
            error,
            SupervisorError::AdbPortConflict { port: 6520, .. }
        ));
    }
}
