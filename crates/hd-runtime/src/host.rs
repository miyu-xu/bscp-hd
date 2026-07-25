use std::collections::{BTreeMap, HashMap};
use std::io::{Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs2::FileExt as _;
use hd_core::{
    AcquireDisplaySessionRequestV2, ActionRequestV2, ApiErrorV2, CreateInstanceRequestV2,
    DesiredStateV2, DisplaySessionV2, DisplayViewportV2, FrameReadyMarkerV2, HostCapabilitiesV2,
    HostEventKindV2, HostEventV2, InstanceActionV2, InstanceRecordV2, InstanceSummaryV2,
    NativeDisplayTargetV2, ObservedStateV2, OperationKindV2, OperationRecordV2, OperationStateV2,
    PreparedNativeDisplayV2, ReconcileReportV2, ReleaseDisplaySessionRequestV2, RestartPolicyV2,
    ScreenshotRecordV2, StopModeV2, UpdateDisplaySessionRequestV2, UpdateInstanceRequestV2,
    WORKER_PROTOCOL_VERSION, WorkerCommandV2, WorkerDescriptorV2, WorkerIdentityV2,
    WorkerPayloadV2, WorkerRequestV2, WorkerResponseV2, WorkerStatusV2,
};
use hd_platform::{DataPaths, executable_name, process_identity_is_alive};
use parking_lot::Mutex as ParkingMutex;
use subtle::ConstantTimeEq as _;
use time::OffsetDateTime;
use tokio::sync::{Mutex, RwLock, broadcast, watch};
use uuid::Uuid;

use crate::{
    CapabilityDiscovery, DiagnosticCollector, DiagnosticError, DiagnosticInputsV2, IpcError,
    LeaseError, LeaseManager, PersistentStore, StoreError, send_worker_request, worker_endpoint,
};

const WORKER_START_TIMEOUT: Duration = Duration::from_secs(15);
const SECRET_BYTES: usize = 32;
const MAX_AUTOMATIC_RUNTIME_RESTARTS: u8 = 3;

#[derive(Debug, Default)]
struct RuntimeRestartState {
    attempts: u8,
    last_failed_revision: u64,
}

#[derive(Debug)]
struct ActiveHostDisplaySession {
    session: DisplaySessionV2,
    target: NativeDisplayTargetV2,
    viewport: DisplayViewportV2,
}

#[derive(Debug)]
pub struct HostService {
    paths: DataPaths,
    store: PersistentStore,
    leases: LeaseManager,
    discovery: CapabilityDiscovery,
    capabilities: RwLock<HostCapabilitiesV2>,
    worker_executable: PathBuf,
    instance_operations: Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
    runtime_restarts: ParkingMutex<HashMap<Uuid, RuntimeRestartState>>,
    display_sessions: Mutex<HashMap<Uuid, ActiveHostDisplaySession>>,
    events: broadcast::Sender<HostEventV2>,
    shutdown: watch::Sender<bool>,
    started_at: OffsetDateTime,
    _data_lock: ParkingMutex<std::fs::File>,
}

struct WorkerPresence {
    persisted_identity: Option<WorkerIdentityV2>,
    persisted_alive: bool,
    descriptor_alive: bool,
    lock_held: bool,
}

impl WorkerPresence {
    const fn may_be_live(&self) -> bool {
        self.persisted_alive || self.descriptor_alive || self.lock_held
    }
}

impl HostService {
    pub async fn open(
        paths: DataPaths,
        worker_executable: Option<PathBuf>,
    ) -> Result<Arc<Self>, HostError> {
        paths.ensure()?;
        let data_lock = acquire_data_lock(&paths)?;
        let store = PersistentStore::open(&paths.database())?;
        store.migrate_legacy_instances(&paths)?;
        let leases = LeaseManager::new(store.clone(), paths.clone())?;
        let discovery = CapabilityDiscovery::discover_defaults(paths.clone(), None);
        let capabilities = discovery.discover(None).await.capabilities;
        let worker_executable = worker_executable
            .or_else(sibling_worker)
            .unwrap_or_else(|| PathBuf::from(executable_name("hd-worker")));
        let (events, _) = broadcast::channel(1024);
        let (shutdown, _) = watch::channel(false);
        let service = Arc::new(Self {
            paths,
            store,
            leases,
            discovery,
            capabilities: RwLock::new(capabilities),
            worker_executable,
            instance_operations: Mutex::new(HashMap::new()),
            runtime_restarts: ParkingMutex::new(HashMap::new()),
            display_sessions: Mutex::new(HashMap::new()),
            events,
            shutdown,
            started_at: OffsetDateTime::now_utc(),
            _data_lock: ParkingMutex::new(data_lock),
        });
        service.recover_operations()?;
        service.reconcile().await?;
        service.spawn_runtime_refresh_monitor();
        Ok(service)
    }

    pub fn paths(&self) -> &DataPaths {
        &self.paths
    }

    pub fn store(&self) -> &PersistentStore {
        &self.store
    }

    pub fn started_at(&self) -> OffsetDateTime {
        self.started_at
    }

    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// Stops only the host control plane. Per-instance workers and their VM process trees remain
    /// alive and can be authenticated again by the next host process.
    pub fn detach(&self) {
        let _ = self.shutdown.send(true);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HostEventV2> {
        self.events.subscribe()
    }

    pub async fn capabilities(
        &self,
        instance_id: Option<Uuid>,
    ) -> Result<HostCapabilitiesV2, HostError> {
        if let Some(instance_id) = instance_id {
            let record = self
                .store
                .get_instance(instance_id)?
                .ok_or(HostError::InstanceNotFound(instance_id))?;
            let result = self.discovery.discover(Some(&record.spec)).await;
            self.emit(HostEventKindV2::Capabilities(result.capabilities.clone()))?;
            Ok(result.capabilities)
        } else {
            Ok(self.capabilities.read().await.clone())
        }
    }

    pub fn list_instances(&self) -> Result<Vec<InstanceSummaryV2>, HostError> {
        Ok(self
            .store
            .list_instances()?
            .iter()
            .map(InstanceSummaryV2::from)
            .collect())
    }

    pub fn get_instance(&self, id: Uuid) -> Result<InstanceRecordV2, HostError> {
        self.store
            .get_instance(id)?
            .ok_or(HostError::InstanceNotFound(id))
    }

    pub fn create_instance(
        &self,
        request: CreateInstanceRequestV2,
    ) -> Result<InstanceRecordV2, HostError> {
        let record = self.store.create_instance(request.spec)?;
        self.emit(HostEventKindV2::Instance((&record).into()))?;
        Ok(record)
    }

    pub async fn update_instance(
        &self,
        id: Uuid,
        request: UpdateInstanceRequestV2,
    ) -> Result<InstanceRecordV2, HostError> {
        let operation_lock = {
            let mut locks = self.instance_operations.lock().await;
            Arc::clone(locks.entry(id).or_insert_with(|| Arc::new(Mutex::new(()))))
        };
        let _guard = operation_lock.lock().await;
        if request.spec.id != id {
            return Err(HostError::InstanceMismatch);
        }
        let current = self.get_instance(id)?;
        if current.status.observed.is_active() {
            return Err(HostError::Busy(
                "stop the instance before changing its specification",
            ));
        }
        let record = self
            .store
            .update_instance(request.expected_revision, request.spec)?;
        self.emit(HostEventKindV2::Instance((&record).into()))?;
        Ok(record)
    }

    pub fn operation(&self, id: Uuid) -> Result<OperationRecordV2, HostError> {
        self.store
            .get_operation(id)?
            .ok_or(HostError::OperationNotFound(id))
    }

    pub fn list_operations(&self) -> Result<Vec<OperationRecordV2>, HostError> {
        self.store.list_operations().map_err(HostError::Store)
    }

    fn recover_operations(self: &Arc<Self>) -> Result<(), HostError> {
        for mut operation in self.store.list_operations()? {
            match operation.state {
                OperationStateV2::Queued | OperationStateV2::Running => {
                    operation.state = OperationStateV2::Cancelled;
                    operation.finished_at = Some(OffsetDateTime::now_utc());
                    operation.progress_per_mille = 1000;
                    operation.error = Some(ApiErrorV2::new(
                        "host_restarted",
                        "pending operation was cancelled after host restart; instance state was independently reconciled",
                    ));
                    self.store.put_operation(&operation)?;
                    self.emit(HostEventKindV2::Operation(operation.clone()))?;
                    tracing::warn!(
                        event = "operation.recovered.cancelled",
                        operation_id = %operation.id,
                        instance_id = ?operation.instance_id,
                        "cancelled a pending operation left by a previous host"
                    );
                }
                OperationStateV2::Succeeded
                | OperationStateV2::Failed
                | OperationStateV2::Cancelled => {}
            }
        }
        Ok(())
    }

    pub fn create_operation(
        self: &Arc<Self>,
        instance_id: Uuid,
        kind: OperationKindV2,
        idempotency_key: &str,
    ) -> Result<OperationRecordV2, HostError> {
        self.get_instance(instance_id)?;
        let (operation, created) =
            self.store
                .create_operation_idempotent(Some(instance_id), kind, idempotency_key)?;
        if created {
            self.emit(HostEventKindV2::Operation(operation.clone()))?;
            let host = Arc::clone(self);
            let operation_id = operation.id;
            tokio::spawn(async move {
                host.execute_operation(operation_id).await;
            });
        }
        Ok(operation)
    }

    pub async fn action(
        &self,
        instance_id: Uuid,
        request: ActionRequestV2,
    ) -> Result<WorkerStatusV2, HostError> {
        request.action.validate()?;
        let rotated_orientation = match &request.action {
            InstanceActionV2::Rotate { orientation } => Some(*orientation),
            _ => None,
        };
        let operation_lock = {
            let mut locks = self.instance_operations.lock().await;
            Arc::clone(
                locks
                    .entry(instance_id)
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = operation_lock.lock().await;
        let record = self.get_instance(instance_id)?;
        if record.status.observed != ObservedStateV2::Ready {
            return Err(HostError::Busy("typed actions require Ready"));
        }
        let response = self
            .call_worker(
                instance_id,
                WorkerCommandV2::Action {
                    action: request.action,
                },
            )
            .await?;
        ensure_worker_success(response)?;
        let status = self.refresh_from_worker(instance_id).await?;
        if let Some(orientation) = rotated_orientation {
            let mut record = self.get_instance(instance_id)?;
            if record.spec.display.orientation != orientation {
                record.spec.display.orientation = orientation;
                record.status.revision = record.status.revision.saturating_add(1);
                record.status.updated_at = OffsetDateTime::now_utc();
                self.persist_instance(&record)?;
            }
        }
        Ok(status)
    }

    pub async fn acquire_display_session(
        &self,
        instance_id: Uuid,
        request: AcquireDisplaySessionRequestV2,
    ) -> Result<DisplaySessionV2, HostError> {
        if !request.viewport.is_valid() {
            return Err(HostError::DisplaySession(
                "viewport is outside supported bounds".to_owned(),
            ));
        }
        if !process_identity_is_alive(request.target.owner()) {
            return Err(HostError::DisplaySession(
                "Player process identity is not alive".to_owned(),
            ));
        }
        let record = self.get_instance(instance_id)?;
        let generation = record.frame_generation;
        let worker_active = record.status.observed.is_active() && record.worker.is_some();
        let previous = self.display_sessions.lock().await.remove(&instance_id);
        if worker_active && let Some(previous) = previous {
            // A Worker accepts a single target. Detach the old target before assigning the new
            // Player so a stale hidden Player cannot retain the crosvm child window.
            let _ = self
                .call_worker(
                    instance_id,
                    WorkerCommandV2::DetachDisplay {
                        session_id: previous.session.id,
                        generation: previous.session.generation,
                    },
                )
                .await;
        }
        let session_id = Uuid::new_v4();
        let token = random_session_token()?;
        if worker_active {
            let response = self
                .call_worker(
                    instance_id,
                    WorkerCommandV2::AttachDisplay {
                        session_id,
                        generation,
                        target: request.target.clone(),
                        viewport: request.viewport.clone(),
                    },
                )
                .await?;
            ensure_worker_success(response)?;
        }
        let session = DisplaySessionV2 {
            id: session_id,
            instance_id,
            worker_endpoint: worker_endpoint(instance_id)?,
            session_token: token,
            generation,
            expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(15),
        };
        self.display_sessions.lock().await.insert(
            instance_id,
            ActiveHostDisplaySession {
                session: session.clone(),
                target: request.target,
                viewport: request.viewport,
            },
        );
        Ok(session)
    }

    pub async fn update_display_session(
        &self,
        instance_id: Uuid,
        request: UpdateDisplaySessionRequestV2,
    ) -> Result<DisplaySessionV2, HostError> {
        if !request.viewport.is_valid() {
            return Err(HostError::DisplaySession(
                "viewport is outside supported bounds".to_owned(),
            ));
        }
        let record = self.get_instance(instance_id)?;
        let worker_active = record.status.observed.is_active() && record.worker.is_some();
        let mut sessions = self.display_sessions.lock().await;
        let active = sessions
            .get_mut(&instance_id)
            .ok_or_else(|| HostError::DisplaySession("display session was not found".to_owned()))?;
        if !tokens_equal(&active.session.session_token, &request.session_token) {
            return Err(HostError::DisplaySession(
                "display session authentication failed".to_owned(),
            ));
        }
        if !process_identity_is_alive(active.target.owner()) {
            return Err(HostError::DisplaySession(
                "Player process identity is no longer alive".to_owned(),
            ));
        }
        if request.viewport.revision > active.viewport.revision {
            if !worker_active {
                active.viewport = request.viewport;
                active.session.expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(15);
                return Ok(active.session.clone());
            }
            let response = self
                .call_worker(
                    instance_id,
                    WorkerCommandV2::ResizeDisplay {
                        session_id: active.session.id,
                        generation: active.session.generation,
                        viewport: request.viewport.clone(),
                    },
                )
                .await?;
            ensure_worker_success(response)?;
            active.viewport = request.viewport;
        }
        active.session.expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(15);
        Ok(active.session.clone())
    }

    pub async fn release_display_session(
        &self,
        instance_id: Uuid,
        request: ReleaseDisplaySessionRequestV2,
    ) -> Result<(), HostError> {
        let mut sessions = self.display_sessions.lock().await;
        let active = sessions
            .get(&instance_id)
            .ok_or_else(|| HostError::DisplaySession("display session was not found".to_owned()))?;
        if !tokens_equal(&active.session.session_token, &request.session_token) {
            return Err(HostError::DisplaySession(
                "display session authentication failed".to_owned(),
            ));
        }
        let active = sessions
            .remove(&instance_id)
            .expect("session checked above");
        drop(sessions);
        let record = self.get_instance(instance_id)?;
        if !record.status.observed.is_active() || record.worker.is_none() {
            return Ok(());
        }
        let response = self
            .call_worker(
                instance_id,
                WorkerCommandV2::DetachDisplay {
                    session_id: active.session.id,
                    generation: active.session.generation,
                },
            )
            .await?;
        ensure_worker_success(response)?;
        Ok(())
    }

    pub async fn capture_screenshot(
        &self,
        instance_id: Uuid,
    ) -> Result<ScreenshotRecordV2, HostError> {
        let record = self.get_instance(instance_id)?;
        if record.status.observed != ObservedStateV2::Ready {
            return Err(HostError::Busy("screenshots require Ready"));
        }
        let directory = self.paths.screenshot_directory();
        std::fs::create_dir_all(&directory).map_err(|source| HostError::Io {
            operation: "create screenshot directory",
            path: directory.clone(),
            source,
        })?;
        let output_path =
            directory.join(format!("{}-{}.png", instance_id, Uuid::new_v4().simple()));
        let response = self
            .call_worker(
                instance_id,
                WorkerCommandV2::CaptureScreenshot { output_path },
            )
            .await?;
        match ensure_worker_success(response)? {
            Some(WorkerPayloadV2::Screenshot(record)) => Ok(record),
            _ => Err(HostError::WorkerProtocol(
                "worker screenshot response had an unexpected payload".to_owned(),
            )),
        }
    }

    pub async fn reconcile(self: &Arc<Self>) -> Result<Vec<ReconcileReportV2>, HostError> {
        let mut reports = Vec::new();
        for mut record in self.store.list_instances()? {
            let before = record.status.observed;
            let mut actions = Vec::new();
            let mut reconnected = false;
            let mut worker_alive_unverified = false;
            let presence = self.worker_presence(&record)?;
            if presence.may_be_live() {
                match self.reconnect_worker(record.spec.id).await {
                    Ok((status, descriptor_recovered))
                        if !presence.persisted_alive
                            || presence.persisted_identity.as_ref() == Some(&status.identity) =>
                    {
                        record.status.force_reconciled(
                            status.observed,
                            status.last_error.as_ref().map(|error| error.code.clone()),
                            status
                                .last_error
                                .as_ref()
                                .map(|error| error.message.clone()),
                        );
                        sync_runtime_fields(&mut record, &status);
                        self.leases.verify_instance(record.spec.id)?;
                        actions.push(if descriptor_recovered {
                            "authenticated_worker_descriptor_recovered".to_owned()
                        } else {
                            "authenticated_worker_reconnected".to_owned()
                        });
                        reconnected = true;
                    }
                    Ok(_) | Err(_) => {
                        worker_alive_unverified = true;
                        actions.push("live_worker_unreachable_identity_retained".to_owned());
                    }
                }
            } else if record.worker.take().is_some() || presence.descriptor_alive {
                actions.push("stale_worker_identity_removed".to_owned());
            }
            if !reconnected && record.status.observed.is_active() {
                let target = if worker_alive_unverified
                    || record.status.desired == DesiredStateV2::Running
                        && matches!(record.spec.restart_policy, RestartPolicyV2::OnFailure)
                {
                    ObservedStateV2::Recovering
                } else {
                    ObservedStateV2::Stopped
                };
                record.status.force_reconciled(
                    target,
                    Some(if worker_alive_unverified {
                        "worker_unreachable".to_owned()
                    } else {
                        "worker_lost".to_owned()
                    }),
                    Some(if worker_alive_unverified {
                        "worker process is alive but authenticated status is unavailable; identity and leases are retained"
                            .to_owned()
                    } else {
                        "authenticated worker could not be reconnected".to_owned()
                    }),
                );
                if worker_alive_unverified {
                    self.leases.verify_instance(record.spec.id)?;
                } else {
                    record.active_run_id = None;
                    record.adb_serial = None;
                    self.leases.release_instance(record.spec.id)?;
                }
            } else if !reconnected
                && !worker_alive_unverified
                && presence.persisted_identity.is_some()
            {
                record.worker = None;
                record.active_run_id = None;
                record.adb_serial = None;
                self.leases.release_instance(record.spec.id)?;
            }
            self.store.put_instance(&record)?;
            self.emit(HostEventKindV2::Instance((&record).into()))?;
            reports.push(ReconcileReportV2 {
                desired: record.status.desired,
                observed_before: before,
                observed_after: record.status.observed,
                worker_reconnected: reconnected,
                actions,
            });
            if !reconnected
                && !worker_alive_unverified
                && record.status.observed == ObservedStateV2::Recovering
                && record.status.desired == DesiredStateV2::Running
            {
                let key = format!("reconcile-{}", record.status.revision);
                let _ = self.create_operation(record.spec.id, OperationKindV2::Start, &key);
            }
        }
        Ok(reports)
    }

    fn worker_presence(&self, record: &InstanceRecordV2) -> Result<WorkerPresence, HostError> {
        let persisted_identity = record.worker.clone();
        let persisted_alive = persisted_identity
            .as_ref()
            .is_some_and(process_identity_is_alive);
        let descriptor_alive = self
            .read_worker_descriptor(record.spec.id)
            .ok()
            .is_some_and(|descriptor| process_identity_is_alive(&descriptor.identity));
        Ok(WorkerPresence {
            persisted_identity,
            persisted_alive,
            descriptor_alive,
            lock_held: worker_instance_lock_held(&self.paths, record.spec.id)?,
        })
    }

    pub async fn request_shutdown(self: &Arc<Self>, stop_all: bool) -> Result<(), HostError> {
        let active = self
            .store
            .list_instances()?
            .into_iter()
            .filter(|record| record.status.observed.is_active())
            .collect::<Vec<_>>();
        if !active.is_empty() && !stop_all {
            return Err(HostError::Busy("active instances require stop_all=true"));
        }
        if stop_all {
            for record in active {
                let key = format!("host-shutdown-{}", Uuid::new_v4());
                let operation = self.create_operation(
                    record.spec.id,
                    OperationKindV2::Stop {
                        mode: StopModeV2::Graceful,
                        graceful_timeout_ms: 20_000,
                    },
                    &key,
                )?;
                self.wait_operation(operation.id, Duration::from_secs(45))
                    .await?;
            }
            // `stop_all` is also the controlled deployment/maintenance boundary. A stopped VM's
            // detached Worker normally survives Host exit, but retaining it here would keep the
            // Worker executable locked on Windows and make an in-place runtime upgrade
            // impossible. Shut down only authenticated idle Workers after every active instance
            // has reached its terminal stop state.
            for record in self.store.list_instances()? {
                if let Some(identity) = record.worker.as_ref()
                    && process_identity_is_alive(identity)
                {
                    self.shutdown_idle_worker(record.spec.id, identity).await?;
                }
            }
        }
        let _ = self.shutdown.send(true);
        Ok(())
    }

    pub async fn collect_diagnostics(
        &self,
        instance_id: Option<Uuid>,
        include_guest_logs: bool,
    ) -> Result<hd_core::DiagnosticBundleResponseV2, HostError> {
        let instance = instance_id.map(|id| self.get_instance(id)).transpose()?;
        let capabilities = self.capabilities(instance_id).await?;
        let mut worker_checks = Vec::new();
        let mut guest_log = None;
        if let Some(record) = &instance
            && record.worker.is_some()
        {
            match self
                .call_worker(record.spec.id, WorkerCommandV2::Diagnose)
                .await
            {
                Ok(response) => match ensure_worker_success(response) {
                    Ok(Some(WorkerPayloadV2::Diagnostics(checks))) => worker_checks = checks,
                    Ok(_) => worker_checks.push(diagnostic_failure(
                        "worker.diagnostics.protocol",
                        "worker diagnostics payload mismatch",
                    )),
                    Err(error) => worker_checks.push(diagnostic_failure(
                        "worker.diagnostics.request",
                        &error.to_string(),
                    )),
                },
                Err(error) => worker_checks.push(diagnostic_failure(
                    "worker.diagnostics.connection",
                    &error.to_string(),
                )),
            }
            if include_guest_logs {
                match self
                    .call_worker(record.spec.id, WorkerCommandV2::CollectGuestLogs)
                    .await
                {
                    Ok(response) => match ensure_worker_success(response) {
                        Ok(Some(WorkerPayloadV2::GuestLog(file))) => {
                            guest_log = Some(file.relative_path);
                        }
                        Ok(_) => worker_checks.push(diagnostic_failure(
                            "guest.log.protocol",
                            "guest log payload mismatch",
                        )),
                        Err(error) => worker_checks
                            .push(diagnostic_failure("guest.log.request", &error.to_string())),
                    },
                    Err(error) => worker_checks.push(diagnostic_failure(
                        "guest.log.connection",
                        &error.to_string(),
                    )),
                }
            }
        }
        let operations = self.store.list_operations()?;
        let leases = self.store.list_leases()?;
        let migrations = self.store.list_migrations()?;
        DiagnosticCollector::new(self.paths.clone())
            .collect(DiagnosticInputsV2 {
                capabilities,
                instance,
                operations,
                leases,
                migrations,
                worker_checks,
                guest_log,
            })
            .map_err(HostError::Diagnostic)
    }

    async fn wait_operation(&self, id: Uuid, timeout: Duration) -> Result<(), HostError> {
        let started = Instant::now();
        loop {
            let operation = self.operation(id)?;
            match operation.state {
                OperationStateV2::Succeeded => return Ok(()),
                OperationStateV2::Failed | OperationStateV2::Cancelled => {
                    return Err(HostError::OperationFailed(operation.error.map_or_else(
                        || "operation failed".to_owned(),
                        |error| error.message,
                    )));
                }
                OperationStateV2::Queued | OperationStateV2::Running => {}
            }
            if started.elapsed() >= timeout {
                return Err(HostError::OperationTimeout(id));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_operation(self: Arc<Self>, operation_id: Uuid) {
        let mut operation = match self.operation(operation_id) {
            Ok(operation) => operation,
            Err(error) => {
                tracing::error!(event = "operation.load.failed", %operation_id, %error);
                return;
            }
        };
        let Some(instance_id) = operation.instance_id else {
            return;
        };
        let lock = {
            let mut locks = self.instance_operations.lock().await;
            Arc::clone(
                locks
                    .entry(instance_id)
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = lock.lock().await;
        operation.state = OperationStateV2::Running;
        operation.started_at = Some(OffsetDateTime::now_utc());
        operation.progress_per_mille = 50;
        if let Err(error) = self.store.put_operation(&operation) {
            tracing::error!(event = "operation.persist.failed", %operation_id, %error);
            return;
        }
        let _ = self.emit(HostEventKindV2::Operation(operation.clone()));
        tracing::info!(
            event = "operation.started",
            %operation_id,
            %instance_id,
            kind = ?operation.kind,
            "host operation started"
        );
        let result = match operation.kind.clone() {
            OperationKindV2::Start => {
                self.start_instance_with_progress(instance_id, operation_id)
                    .await
            }
            OperationKindV2::Stop {
                mode,
                graceful_timeout_ms,
            } => {
                self.stop_instance(
                    instance_id,
                    mode,
                    Duration::from_millis(u64::from(graceful_timeout_ms)),
                )
                .await
            }
            OperationKindV2::Restart => {
                self.update_operation_progress(operation_id, 150);
                if let Err(error) = self
                    .stop_instance(instance_id, StopModeV2::Graceful, Duration::from_secs(20))
                    .await
                {
                    Err(error)
                } else {
                    self.update_operation_progress(operation_id, 200);
                    self.start_instance_with_progress(instance_id, operation_id)
                        .await
                }
            }
            OperationKindV2::Pause => {
                self.simple_worker_operation(instance_id, WorkerCommandV2::Pause)
                    .await
            }
            OperationKindV2::Resume => {
                self.simple_worker_operation(instance_id, WorkerCommandV2::Resume)
                    .await
            }
            OperationKindV2::Reconfigure { display, adb } => {
                self.reconfigure_instance(instance_id, display, adb).await
            }
            OperationKindV2::InstallApk { upload_id, sha256 } => {
                self.install_apk(instance_id, upload_id, &sha256).await
            }
            OperationKindV2::CollectDiagnostics { include_guest_logs } => self
                .collect_diagnostics(Some(instance_id), include_guest_logs)
                .await
                .map(|bundle| {
                    operation
                        .result
                        .insert("bundle_id".to_owned(), bundle.bundle_id.to_string());
                    operation
                        .result
                        .insert("manifest_sha256".to_owned(), bundle.manifest_sha256);
                    operation
                        .result
                        .insert("archive_sha256".to_owned(), bundle.archive_sha256);
                }),
            OperationKindV2::Delete => self.delete_instance(instance_id).await,
        };
        operation.finished_at = Some(OffsetDateTime::now_utc());
        operation.progress_per_mille = 1000;
        match result {
            Ok(()) => operation.state = OperationStateV2::Succeeded,
            Err(error) => {
                operation.state = OperationStateV2::Failed;
                operation.error = Some(error.api_error());
            }
        }
        if let Err(error) = self.store.put_operation(&operation) {
            tracing::error!(event = "operation.persist.failed", %operation_id, %error);
            return;
        }
        let _ = self.emit(HostEventKindV2::Operation(operation.clone()));
        tracing::info!(
            event = "operation.finished",
            %operation_id,
            %instance_id,
            state = ?operation.state,
            "host operation finished"
        );
    }

    async fn start_instance_with_progress(
        &self,
        instance_id: Uuid,
        operation_id: Uuid,
    ) -> Result<(), HostError> {
        let start = self.start_instance(instance_id);
        tokio::pin!(start);
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_millis(500),
            Duration::from_millis(500),
        );
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                result = &mut start => return result,
                _ = interval.tick() => {
                    let Ok(status) = self.refresh_from_worker(instance_id).await else {
                        continue;
                    };
                    let progress = start_progress(status.observed);
                    let Ok(mut operation) = self.operation(operation_id) else {
                        continue;
                    };
                    if operation.state == OperationStateV2::Running
                        && progress > operation.progress_per_mille
                    {
                        operation.progress_per_mille = progress;
                        if let Err(error) = self.store.put_operation(&operation) {
                            tracing::warn!(
                                event = "operation.progress.persist.failed",
                                %operation_id,
                                %error,
                                "failed to persist optional operation progress"
                            );
                            continue;
                        }
                        let _ = self.emit(HostEventKindV2::Operation(operation));
                    }
                }
            }
        }
    }

    fn update_operation_progress(&self, operation_id: Uuid, progress: u16) {
        let Ok(mut operation) = self.operation(operation_id) else {
            return;
        };
        if operation.state != OperationStateV2::Running || progress <= operation.progress_per_mille
        {
            return;
        }
        operation.progress_per_mille = progress;
        if let Err(error) = self.store.put_operation(&operation) {
            tracing::warn!(
                event = "operation.progress.persist.failed",
                %operation_id,
                %error,
                "failed to persist optional operation progress"
            );
            return;
        }
        let _ = self.emit(HostEventKindV2::Operation(operation));
    }

    #[allow(clippy::too_many_lines)]
    async fn start_instance(&self, id: Uuid) -> Result<(), HostError> {
        let mut record = self.get_instance(id)?;
        self.guard_existing_worker(&mut record).await?;
        if record.status.observed.is_active()
            && record.status.observed != ObservedStateV2::Recovering
        {
            return Err(HostError::Busy("instance is already active"));
        }
        record.status.set_desired(DesiredStateV2::Running);
        transition_record(&mut record, ObservedStateV2::Preparing, None)?;
        self.persist_instance(&record)?;
        let start_result: Result<(), HostError> = async {
            let discovery = self.discovery.discover(Some(&record.spec)).await;
            self.emit(HostEventKindV2::Capabilities(
                discovery.capabilities.clone(),
            ))?;
            if !discovery.capabilities.can_start() {
                record.status.force_reconciled(
                    ObservedStateV2::Blocked,
                    Some("capability_blocked".to_owned()),
                    Some("required host capability is unavailable".to_owned()),
                );
                self.persist_instance(&record)?;
                return Err(HostError::CapabilityBlocked);
            }
            self.leases.release_instance(id)?;
            record.frame_generation = record
                .frame_generation
                .max(frame_generation_high_water(&self.paths, id));
            let frame_generation = record
                .frame_generation
                .checked_add(1)
                .ok_or(LeaseError::FrameGenerationExhausted)?;
            let reserved = self
                .leases
                .reserve_start(&record.spec, None, frame_generation)?;
            transition_record(&mut record, ObservedStateV2::StartingWorker, None)?;
            self.persist_instance(&record)?;
            let mut descriptor = self.ensure_worker(&record).await?;
            let bound = self.leases.bind_worker_identity(id, &descriptor.identity)?;
            record.worker = Some(descriptor.identity.clone());
            let mut run_id = Uuid::new_v4();
            record.active_run_id = Some(run_id);
            self.persist_instance(&record)?;
            let initial_display = {
                let mut sessions = self.display_sessions.lock().await;
                let prepared = sessions.get_mut(&id).and_then(|active| {
                    if !process_identity_is_alive(active.target.owner()) {
                        return None;
                    }
                    active.session.generation = frame_generation;
                    active.session.expires_at =
                        OffsetDateTime::now_utc() + time::Duration::seconds(15);
                    Some(PreparedNativeDisplayV2 {
                        session_id: active.session.id,
                        target: active.target.clone(),
                        viewport: active.viewport.clone(),
                    })
                });
                if prepared.is_none()
                    && sessions
                        .get(&id)
                        .is_some_and(|active| !process_identity_is_alive(active.target.owner()))
                {
                    sessions.remove(&id);
                }
                prepared
            };
            let mut response = self
                .call_worker(
                    id,
                    WorkerCommandV2::Start {
                        spec: Box::new(record.spec.clone()),
                        run_id,
                        leases: if bound.is_empty() { reserved } else { bound },
                        capabilities_fingerprint: discovery.capabilities.fingerprint.clone(),
                        initial_display: initial_display.clone(),
                    },
                )
                .await?;
            if response
                .error
                .as_ref()
                .is_some_and(|error| error.code == "capability_changed")
            {
                tracing::warn!(
                    event = "worker.capability_changed.replacing_idle_worker",
                    instance_id = %id,
                    worker_pid = descriptor.identity.pid,
                    "replacing an idle worker that inherited a stale host environment"
                );
                self.shutdown_idle_worker(id, &descriptor.identity).await?;
                record.worker = None;
                record.active_run_id = None;
                self.persist_instance(&record)?;

                descriptor = self.ensure_worker(&record).await?;
                let rebound = self.leases.bind_worker_identity(id, &descriptor.identity)?;
                record.worker = Some(descriptor.identity.clone());
                run_id = Uuid::new_v4();
                record.active_run_id = Some(run_id);
                self.persist_instance(&record)?;
                response = self
                    .call_worker(
                        id,
                        WorkerCommandV2::Start {
                            spec: Box::new(record.spec.clone()),
                            run_id,
                            leases: rebound,
                            capabilities_fingerprint: discovery.capabilities.fingerprint,
                            initial_display,
                        },
                    )
                    .await?;
            }
            if let Err(error) = ensure_worker_success(response) {
                let _ = self.refresh_from_worker(id).await;
                return Err(error);
            }
            let status = self.refresh_from_worker(id).await?;
            if status.observed != ObservedStateV2::Ready {
                return Err(HostError::WorkerProtocol(
                    "worker acknowledged start without reaching Ready".to_owned(),
                ));
            }
            Ok(())
        }
        .await;
        match start_result {
            Ok(()) => Ok(()),
            Err(error) => {
                if self.reconcile_start_failure(id, &error).await? {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn guard_existing_worker(&self, record: &mut InstanceRecordV2) -> Result<(), HostError> {
        let id = record.spec.id;
        let persisted_identity = record.worker.clone();
        let persisted_alive = persisted_identity
            .as_ref()
            .is_some_and(process_identity_is_alive);
        let descriptor_alive = self
            .read_worker_descriptor(id)
            .ok()
            .is_some_and(|descriptor| process_identity_is_alive(&descriptor.identity));
        let worker_lock_held = worker_instance_lock_held(&self.paths, id)?;
        if persisted_alive || descriptor_alive || worker_lock_held {
            match self.reconnect_worker(id).await {
                Ok((status, _)) => {
                    if persisted_alive && persisted_identity.as_ref() != Some(&status.identity) {
                        return Err(HostError::WorkerProtocol(
                            "live worker identity differs from the persisted exact identity"
                                .to_owned(),
                        ));
                    }
                    sync_runtime_fields(record, &status);
                    if status.observed.is_active()
                        || status.cleanup_pending
                        || status.child_pid.is_some()
                    {
                        record.status.force_reconciled(
                            ObservedStateV2::Recovering,
                            Some("live_worker_owns_runtime".to_owned()),
                            Some(
                                "start refused until the existing worker runtime is stopped"
                                    .to_owned(),
                            ),
                        );
                        self.persist_instance(record)?;
                        return Err(HostError::Busy(
                            "an existing worker still owns runtime resources; stop it before start",
                        ));
                    }
                }
                Err(error) if persisted_alive || worker_lock_held => {
                    record.status.force_reconciled(
                        ObservedStateV2::Recovering,
                        Some("worker_unreachable".to_owned()),
                        Some(format!(
                            "a Worker holds the instance lock but authenticated status is unavailable: {error}"
                        )),
                    );
                    self.persist_instance(record)?;
                    return Err(HostError::Busy(
                        "the existing worker lock is held; start cannot safely replace it",
                    ));
                }
                Err(_) => {}
            }
        }
        Ok(())
    }

    async fn reconcile_start_failure(
        &self,
        id: Uuid,
        error: &HostError,
    ) -> Result<bool, HostError> {
        let worker_status = self.ping_worker(id).await.ok();
        if let Some(status) = &worker_status
            && status.observed == ObservedStateV2::Ready
        {
            self.refresh_from_worker(id).await?;
            tracing::warn!(
                event = "instance.start.response.recovered",
                instance_id = %id,
                error_code = error.code(),
                "worker reached Ready despite a lost or rejected start response"
            );
            return Ok(true);
        }
        let mut record = self.get_instance(id)?;
        if let Some(status) = worker_status {
            sync_runtime_fields(&mut record, &status);
            if status.observed.is_active() || status.cleanup_pending || status.child_pid.is_some() {
                record.status.force_reconciled(
                    ObservedStateV2::Recovering,
                    Some(error.code().to_owned()),
                    Some(format!("start result is uncertain: {error}")),
                );
                self.persist_instance(&record)?;
                return Ok(false);
            }
        } else if record
            .worker
            .as_ref()
            .is_some_and(process_identity_is_alive)
        {
            record.status.force_reconciled(
                ObservedStateV2::Recovering,
                Some(error.code().to_owned()),
                Some(format!(
                    "worker is alive but start status is unavailable: {error}"
                )),
            );
            self.persist_instance(&record)?;
            return Ok(false);
        } else {
            record.worker = None;
        }
        let blocked = matches!(error, HostError::CapabilityBlocked)
            || matches!(
                error,
                HostError::Lease(
                    LeaseError::CpuCapacity { .. }
                        | LeaseError::MemoryCapacity { .. }
                        | LeaseError::GpuCapacity(_)
                        | LeaseError::GuestCidExhausted
                        | LeaseError::AdbPortUnavailable(_)
                        | LeaseError::AdbPortExhausted
                        | LeaseError::PortIo { .. }
                )
            )
            || matches!(
                error,
                HostError::WorkerRejected(api)
                    if matches!(
                        api.code.as_str(),
                        "capability_blocked"
                            | "capability_changed"
                            | "readiness_unavailable"
                            | "frame_handshake"
                            | "artifacts"
                    )
            );
        record.status.force_reconciled(
            if blocked {
                ObservedStateV2::Blocked
            } else {
                ObservedStateV2::Failed
            },
            Some(error.code().to_owned()),
            Some(error.to_string()),
        );
        self.leases.release_instance(id)?;
        self.persist_instance(&record)?;
        Ok(false)
    }

    async fn stop_instance(
        &self,
        id: Uuid,
        mode: StopModeV2,
        timeout: Duration,
    ) -> Result<(), HostError> {
        let mut record = self.get_instance(id)?;
        record.status.set_desired(DesiredStateV2::Stopped);
        self.runtime_restarts.lock().remove(&id);
        // Persist the user's desired state before consulting the worker.  If the worker is already
        // Stopped (notably after a failed start), the old flow skipped the transition branch and
        // refresh_from_worker reloaded the stale desired=Running record.  A successful stop then
        // exposed the contradictory Running/Stopped pair and the runner could not reliably clean
        // up a failed cycle.
        self.persist_instance(&record)?;
        if record.worker.is_none() {
            record
                .status
                .force_reconciled(ObservedStateV2::Stopped, None, None);
            record.active_run_id = None;
            record.adb_serial = None;
            self.leases.release_instance(id)?;
            self.persist_instance(&record)?;
            return Ok(());
        }
        if record.status.observed != ObservedStateV2::Stopped {
            if record
                .status
                .observed
                .can_transition_to(ObservedStateV2::Stopping)
            {
                transition_record(&mut record, ObservedStateV2::Stopping, None)?;
                self.persist_instance(&record)?;
            }
            let response = self
                .call_worker(
                    id,
                    WorkerCommandV2::Stop {
                        mode,
                        graceful_timeout_ms: u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX),
                    },
                )
                .await?;
            ensure_worker_success(response)?;
        }
        let worker_status = self.refresh_from_worker(id).await?;
        if worker_status.observed != ObservedStateV2::Stopped
            || worker_status.cleanup_pending
            || worker_status.child_pid.is_some()
        {
            return Err(HostError::WorkerProtocol(
                "worker acknowledged stop without proving child and endpoint cleanup".to_owned(),
            ));
        }
        let mut record = self.get_instance(id)?;
        record
            .status
            .force_reconciled(ObservedStateV2::Stopped, None, None);
        record.active_run_id = None;
        record.adb_serial = None;
        record.host_fps_milli = None;
        self.leases.release_instance(id)?;
        self.persist_instance(&record)
    }

    async fn simple_worker_operation(
        &self,
        id: Uuid,
        command: WorkerCommandV2,
    ) -> Result<(), HostError> {
        let response = self.call_worker(id, command).await?;
        let operation_result = ensure_worker_success(response).map(|_| ());
        let refresh_result = self.refresh_from_worker(id).await.map(|_| ());
        operation_result.and(refresh_result)
    }

    async fn reconfigure_instance(
        &self,
        id: Uuid,
        display: hd_core::DisplayConfigV2,
        adb: hd_core::AdbConfigV2,
    ) -> Result<(), HostError> {
        let response = self
            .call_worker(
                id,
                WorkerCommandV2::Reconfigure {
                    display: display.clone(),
                    adb: adb.clone(),
                },
            )
            .await?;
        ensure_worker_success(response)?;
        let mut record = self.get_instance(id)?;
        record.spec.display = display;
        record.spec.adb = adb;
        record.status.revision = record.status.revision.saturating_add(1);
        record.status.updated_at = OffsetDateTime::now_utc();
        self.persist_instance(&record)
    }

    async fn install_apk(&self, id: Uuid, upload_id: Uuid, sha256: &str) -> Result<(), HostError> {
        let upload = self
            .store
            .get_upload(upload_id)?
            .ok_or(HostError::UploadNotFound(upload_id))?;
        if upload.sha256 != sha256 {
            return Err(HostError::UploadDigestMismatch);
        }
        let response = self
            .call_worker(
                id,
                WorkerCommandV2::InstallApk {
                    upload_path: upload.path,
                    sha256: sha256.to_owned(),
                },
            )
            .await?;
        ensure_worker_success(response).map(|_| ())
    }

    async fn delete_instance(&self, id: Uuid) -> Result<(), HostError> {
        let mut record = self.get_instance(id)?;
        if record.status.observed.is_active() && record.status.observed != ObservedStateV2::Deleting
        {
            return Err(HostError::Busy("stop the instance before deletion"));
        }
        if record
            .status
            .observed
            .can_transition_to(ObservedStateV2::Deleting)
        {
            transition_record(&mut record, ObservedStateV2::Deleting, None)?;
            self.persist_instance(&record)?;
        }
        if let Some(identity) = record.worker.clone()
            && process_identity_is_alive(&identity)
        {
            let response = self.call_worker(id, WorkerCommandV2::Shutdown).await?;
            ensure_worker_success(response)?;
            wait_for_process_exit(&identity, Duration::from_secs(5)).await;
            if process_identity_is_alive(&identity) {
                hd_platform::terminate_process_identity(&identity)?;
                wait_for_process_exit(&identity, Duration::from_secs(5)).await;
            }
            if process_identity_is_alive(&identity) {
                return Err(HostError::WorkerShutdownTimeout(id));
            }
        }
        self.leases.release_instance(id)?;
        remove_regular_file_if_present(&self.paths.disk_overlay(id))?;
        remove_scoped_directory_if_safe(&self.paths.instances, id, "delete instance directory")?;
        remove_scoped_directory_if_safe(&self.paths.runs, id, "delete instance run history")?;
        remove_scoped_directory_if_safe(&self.paths.workers, id, "delete instance worker data")?;
        self.store.delete_instance_record(id)?;
        Ok(())
    }

    async fn ensure_worker(
        &self,
        record: &InstanceRecordV2,
    ) -> Result<WorkerDescriptorV2, HostError> {
        if let Some(descriptor) = self.try_reuse_worker(record).await? {
            return Ok(descriptor);
        }
        if !self.worker_executable.is_file() && self.worker_executable.components().count() > 1 {
            return Err(HostError::WorkerExecutable(self.worker_executable.clone()));
        }
        let worker_dir = self.paths.worker_dir(record.spec.id);
        std::fs::create_dir_all(&worker_dir).map_err(|source| HostError::Io {
            operation: "create worker directory",
            path: worker_dir,
            source,
        })?;
        let secret = random_secret()?;
        hd_platform::write_owner_only(
            &self.paths.worker_secret(record.spec.id),
            secret.as_bytes(),
        )?;
        let endpoint = worker_endpoint(record.spec.id)?;
        let nonce = Uuid::new_v4();
        let arguments = vec![
            "--data-root".to_owned(),
            self.paths.root.to_string_lossy().into_owned(),
            "--instance-id".to_owned(),
            record.spec.id.to_string(),
            "--nonce".to_owned(),
            nonce.to_string(),
            "--endpoint".to_owned(),
            endpoint,
        ];
        tracing::info!(
            event = "worker.spawn.started",
            instance_id = %record.spec.id,
            executable = %self.worker_executable.display(),
            "starting detached worker"
        );
        let spawned_pid = hd_platform::spawn_detached(
            &self.worker_executable,
            &arguments,
            &BTreeMap::new(),
            &self.paths.root,
        )?;
        let spawned_identity =
            wait_for_process_identity(spawned_pid, nonce, Duration::from_secs(2)).await?;
        self.wait_spawned_worker(record.spec.id, spawned_identity)
            .await
    }

    async fn shutdown_idle_worker(
        &self,
        id: Uuid,
        identity: &WorkerIdentityV2,
    ) -> Result<(), HostError> {
        let status = self.ping_worker(id).await?;
        if status.identity != *identity {
            return Err(HostError::WorkerIdentity);
        }
        if status.observed.is_active() || status.cleanup_pending || status.child_pid.is_some() {
            return Err(HostError::Busy(
                "the stale worker still owns runtime resources",
            ));
        }
        let response = self.call_worker(id, WorkerCommandV2::Shutdown).await?;
        ensure_worker_success(response)?;
        wait_for_process_exit(identity, Duration::from_secs(5)).await;
        if process_identity_is_alive(identity) {
            hd_platform::terminate_process_identity(identity)?;
            wait_for_process_exit(identity, Duration::from_secs(5)).await;
        }
        if process_identity_is_alive(identity) {
            return Err(HostError::WorkerShutdownTimeout(id));
        }
        Ok(())
    }

    async fn try_reuse_worker(
        &self,
        record: &InstanceRecordV2,
    ) -> Result<Option<WorkerDescriptorV2>, HostError> {
        let descriptor_alive = self
            .read_worker_descriptor(record.spec.id)
            .ok()
            .is_some_and(|descriptor| process_identity_is_alive(&descriptor.identity));
        let persisted_alive = record
            .worker
            .as_ref()
            .is_some_and(process_identity_is_alive);
        let worker_lock_held = worker_instance_lock_held(&self.paths, record.spec.id)?;
        if persisted_alive || descriptor_alive || worker_lock_held {
            if let Ok((status, _)) = self.reconnect_worker(record.spec.id).await {
                if persisted_alive && record.worker.as_ref() != Some(&status.identity) {
                    return Err(HostError::WorkerIdentity);
                }
                if status.observed.is_active()
                    || status.cleanup_pending
                    || status.child_pid.is_some()
                {
                    return Err(HostError::Busy(
                        "the existing worker still owns runtime resources",
                    ));
                }
                return self.read_worker_descriptor(record.spec.id).map(Some);
            }
            if worker_lock_held {
                return Err(HostError::Busy(
                    "the per-instance worker lock is already held",
                ));
            }
        }
        Ok(None)
    }

    async fn wait_spawned_worker(
        &self,
        instance_id: Uuid,
        spawned_identity: WorkerIdentityV2,
    ) -> Result<WorkerDescriptorV2, HostError> {
        let started = Instant::now();
        loop {
            if let Ok(descriptor) = self.read_worker_descriptor(instance_id)
                && descriptor.identity == spawned_identity
                && process_identity_is_alive(&descriptor.identity)
                && let Ok(status) = self.ping_worker(instance_id).await
                && status.identity == descriptor.identity
            {
                tracing::info!(
                    event = "worker.spawn.succeeded",
                    %instance_id,
                    pid = descriptor.identity.pid,
                    "detached worker authenticated"
                );
                return Ok(descriptor);
            }
            if started.elapsed() >= WORKER_START_TIMEOUT {
                if process_identity_is_alive(&spawned_identity)
                    && let Err(error) = hd_platform::terminate_process_identity(&spawned_identity)
                {
                    tracing::error!(
                        event = "worker.spawn.cleanup.failed",
                        %instance_id,
                        pid = spawned_identity.pid,
                        %error,
                        "failed to terminate an unauthenticated worker"
                    );
                }
                return Err(HostError::WorkerStartTimeout(instance_id));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn ping_worker(&self, id: Uuid) -> Result<WorkerStatusV2, HostError> {
        let descriptor = self.read_worker_descriptor(id)?;
        let response = self.call_worker(id, WorkerCommandV2::Ping).await?;
        match ensure_worker_success(response)? {
            Some(WorkerPayloadV2::Pong(status)) if status.identity == descriptor.identity => {
                Ok(status)
            }
            Some(WorkerPayloadV2::Pong(_)) => Err(HostError::WorkerIdentity),
            _ => Err(HostError::WorkerProtocol(
                "Ping response payload mismatch".to_owned(),
            )),
        }
    }

    async fn reconnect_worker(&self, id: Uuid) -> Result<(WorkerStatusV2, bool), HostError> {
        if let Ok(status) = self.ping_worker(id).await {
            return Ok((status, false));
        }
        let status = self.ping_worker_endpoint(id).await?;
        if !process_identity_is_alive(&status.identity) {
            return Err(HostError::WorkerIdentity);
        }
        let descriptor = WorkerDescriptorV2 {
            protocol_version: WORKER_PROTOCOL_VERSION,
            instance_id: id,
            identity: status.identity.clone(),
            endpoint: worker_endpoint(id)?,
            secret_path: self.paths.worker_secret(id),
            started_at: OffsetDateTime::now_utc(),
        };
        let descriptor_bytes = serde_json::to_vec_pretty(&descriptor).map_err(HostError::Json)?;
        hd_platform::write_owner_only(&self.paths.worker_descriptor(id), &descriptor_bytes)?;
        tracing::warn!(
            event = "worker.descriptor.recovered",
            instance_id = %id,
            pid = status.identity.pid,
            "reconstructed a Worker descriptor after authenticated endpoint recovery"
        );
        Ok((status, true))
    }

    async fn ping_worker_endpoint(&self, id: Uuid) -> Result<WorkerStatusV2, HostError> {
        let endpoint = worker_endpoint(id)?;
        let token = read_secret(&self.paths.worker_secret(id))?;
        let request_id = Uuid::new_v4();
        let response = send_worker_request(
            &endpoint,
            &WorkerRequestV2 {
                protocol_version: WORKER_PROTOCOL_VERSION,
                request_id,
                instance_id: id,
                bearer_token: token,
                command: WorkerCommandV2::Ping,
            },
        )
        .await?;
        if response.protocol_version != WORKER_PROTOCOL_VERSION || response.request_id != request_id
        {
            return Err(HostError::WorkerProtocol(
                "worker endpoint recovery response version or request id mismatch".to_owned(),
            ));
        }
        match ensure_worker_success(response)? {
            Some(WorkerPayloadV2::Pong(status)) => Ok(status),
            _ => Err(HostError::WorkerProtocol(
                "worker endpoint recovery Ping payload mismatch".to_owned(),
            )),
        }
    }

    async fn refresh_from_worker(&self, id: Uuid) -> Result<WorkerStatusV2, HostError> {
        let response = self.call_worker(id, WorkerCommandV2::Status).await?;
        let Some(WorkerPayloadV2::Status(status)) = ensure_worker_success(response)? else {
            return Err(HostError::WorkerProtocol(
                "Status payload mismatch".to_owned(),
            ));
        };
        let mut record = self.get_instance(id)?;
        let previous = record.clone();
        let error_code = status.last_error.as_ref().map(|error| error.code.clone());
        let reason = status
            .last_error
            .as_ref()
            .map(|error| error.message.clone());
        if record.status.observed != status.observed
            || record.status.error_code != error_code
            || record.status.reason != reason
        {
            record
                .status
                .force_reconciled(status.observed, error_code, reason);
        }
        sync_runtime_fields(&mut record, &status);
        if record != previous {
            self.persist_instance(&record)?;
        }
        Ok(status)
    }

    fn spawn_runtime_refresh_monitor(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(host) = weak.upgrade() else {
                    break;
                };
                host.expire_display_sessions().await;
                let Ok(instances) = host.store.list_instances() else {
                    continue;
                };
                for instance in instances {
                    if instance.status.observed.is_active() {
                        host.refresh_runtime_instance(instance.spec.id).await;
                    } else if instance.status.observed == ObservedStateV2::Failed {
                        host.schedule_runtime_restart(&instance, "terminal_start_failure");
                    }
                }
            }
        });
    }

    async fn expire_display_sessions(&self) {
        let now = OffsetDateTime::now_utc();
        let expired = {
            let mut sessions = self.display_sessions.lock().await;
            let expired_ids = sessions
                .iter()
                .filter_map(|(instance_id, active)| {
                    (active.session.expires_at <= now
                        || !process_identity_is_alive(active.target.owner()))
                    .then_some(*instance_id)
                })
                .collect::<Vec<_>>();
            expired_ids
                .into_iter()
                .filter_map(|instance_id| {
                    sessions
                        .remove(&instance_id)
                        .map(|session| (instance_id, session))
                })
                .collect::<Vec<_>>()
        };
        for (instance_id, active) in expired {
            if let Err(error) = self
                .call_worker(
                    instance_id,
                    WorkerCommandV2::DetachDisplay {
                        session_id: active.session.id,
                        generation: active.session.generation,
                    },
                )
                .await
            {
                tracing::debug!(
                    event = "display_session.expire.detach_failed",
                    %instance_id,
                    %error,
                    "expired Player display session could not be detached"
                );
            } else {
                tracing::info!(
                    event = "display_session.expired",
                    %instance_id,
                    "expired Player display session was detached"
                );
            }
        }
    }

    async fn refresh_runtime_instance(self: &Arc<Self>, instance_id: Uuid) {
        match self.refresh_from_worker(instance_id).await {
            Ok(status) if status.observed == ObservedStateV2::Ready => {
                self.runtime_restarts.lock().remove(&instance_id);
            }
            Ok(status) if status.observed == ObservedStateV2::Failed => {
                let Ok(record) = self.get_instance(instance_id) else {
                    return;
                };
                self.schedule_runtime_restart(&record, "runtime_failure");
            }
            Ok(_) => {}
            Err(error) => self.handle_runtime_refresh_error(instance_id, &error),
        }
    }

    fn handle_runtime_refresh_error(self: &Arc<Self>, instance_id: Uuid, error: &HostError) {
        let Ok(mut record) = self.get_instance(instance_id) else {
            return;
        };
        if !record.status.observed.is_active()
            || record.status.observed == ObservedStateV2::Recovering
                && record.status.error_code.as_deref() == Some("worker_lost")
        {
            return;
        }
        let Some(worker_alive) = record.worker.as_ref().map(process_identity_is_alive) else {
            // Preparing/StartingWorker legitimately precedes descriptor and exact identity
            // persistence. The in-flight start operation owns that interval; absence is not loss.
            return;
        };
        if worker_alive {
            tracing::warn!(
                event = "instance.runtime_refresh.transient_failure",
                instance_id = %record.spec.id,
                %error,
                "live Worker did not answer this refresh tick"
            );
            return;
        }

        let restart = should_restart_after_failure(&record);
        record.status.force_reconciled(
            if restart {
                ObservedStateV2::Recovering
            } else {
                ObservedStateV2::Failed
            },
            Some("worker_lost".to_owned()),
            Some(format!(
                "Worker identity exited during an active runtime: {error}"
            )),
        );
        record.worker = None;
        record.active_run_id = None;
        record.adb_serial = None;
        record.host_fps_milli = None;
        if let Err(release_error) = self.leases.release_instance(record.spec.id) {
            tracing::error!(
                event = "instance.worker_lost.lease_release_failed",
                instance_id = %record.spec.id,
                %release_error,
                "failed to release leases after exact Worker identity exit"
            );
            return;
        }
        if let Err(persist_error) = self.persist_instance(&record) {
            tracing::error!(
                event = "instance.worker_lost.persist_failed",
                instance_id = %record.spec.id,
                %persist_error,
                "failed to persist Worker loss recovery state"
            );
            return;
        }
        self.schedule_runtime_restart(&record, "worker_lost");
    }

    fn schedule_runtime_restart(self: &Arc<Self>, record: &InstanceRecordV2, cause: &str) {
        if !should_restart_after_failure(record) {
            return;
        }
        let attempt = {
            let mut restarts = self.runtime_restarts.lock();
            let state = restarts.entry(record.spec.id).or_default();
            if state.last_failed_revision == record.status.revision {
                return;
            }
            if state.attempts >= MAX_AUTOMATIC_RUNTIME_RESTARTS {
                tracing::error!(
                    event = "instance.runtime_failure.restart_exhausted",
                    instance_id = %record.spec.id,
                    attempts = state.attempts,
                    %cause,
                    "automatic runtime recovery reached its bounded retry limit"
                );
                state.last_failed_revision = record.status.revision;
                return;
            }
            state.attempts = state.attempts.saturating_add(1);
            state.last_failed_revision = record.status.revision;
            state.attempts
        };
        let key = format!("{cause}-restart-{}-{attempt}", record.status.revision);
        tracing::warn!(
            event = "instance.runtime_failure.restart_scheduled",
            instance_id = %record.spec.id,
            error_code = record.status.error_code.as_deref().unwrap_or(cause),
            %attempt,
            %cause,
            "runtime failure scheduled a new start under restart_policy=on_failure"
        );
        if let Err(error) = self.create_operation(record.spec.id, OperationKindV2::Start, &key) {
            tracing::error!(
                event = "instance.runtime_failure.restart_schedule_failed",
                instance_id = %record.spec.id,
                %cause,
                %error,
                "failed to schedule runtime recovery"
            );
        }
    }

    async fn call_worker(
        &self,
        id: Uuid,
        command: WorkerCommandV2,
    ) -> Result<WorkerResponseV2, HostError> {
        let descriptor = self.read_worker_descriptor(id)?;
        if descriptor.instance_id != id
            || descriptor.protocol_version != WORKER_PROTOCOL_VERSION
            || !process_identity_is_alive(&descriptor.identity)
        {
            return Err(HostError::WorkerIdentity);
        }
        let token = read_secret(&descriptor.secret_path)?;
        let request_id = Uuid::new_v4();
        let response = send_worker_request(
            &descriptor.endpoint,
            &WorkerRequestV2 {
                protocol_version: WORKER_PROTOCOL_VERSION,
                request_id,
                instance_id: id,
                bearer_token: token,
                command,
            },
        )
        .await?;
        if response.protocol_version != WORKER_PROTOCOL_VERSION || response.request_id != request_id
        {
            return Err(HostError::WorkerProtocol(
                "worker response version or request id mismatch".to_owned(),
            ));
        }
        Ok(response)
    }

    fn read_worker_descriptor(&self, id: Uuid) -> Result<WorkerDescriptorV2, HostError> {
        let path = self.paths.worker_descriptor(id);
        let bytes = read_regular_limited(&path, 64 * 1024)?;
        serde_json::from_slice(&bytes).map_err(HostError::Json)
    }

    fn persist_instance(&self, record: &InstanceRecordV2) -> Result<(), HostError> {
        self.store.put_instance(record)?;
        self.emit(HostEventKindV2::Instance(record.into()))
    }

    fn emit(&self, kind: HostEventKindV2) -> Result<(), HostError> {
        let event = HostEventV2 {
            sequence: self.store.next_event_sequence()?,
            timestamp: OffsetDateTime::now_utc(),
            trace_id: Uuid::new_v4(),
            kind,
        };
        let _ = self.events.send(event);
        Ok(())
    }
}

const fn start_progress(state: ObservedStateV2) -> u16 {
    match state {
        ObservedStateV2::Preparing => 100,
        ObservedStateV2::StartingWorker => 200,
        ObservedStateV2::LaunchingGuest => 350,
        ObservedStateV2::NegotiatingDisplay => 500,
        ObservedStateV2::GuestBooting => 650,
        ObservedStateV2::AdbConnecting => 800,
        ObservedStateV2::Ready => 950,
        _ => 50,
    }
}

fn ensure_worker_success(response: WorkerResponseV2) -> Result<Option<WorkerPayloadV2>, HostError> {
    if response.ok {
        Ok(response.payload)
    } else {
        let error = response
            .error
            .unwrap_or_else(|| ApiErrorV2::new("worker_unknown", "worker returned no error"));
        Err(HostError::WorkerRejected(error))
    }
}

fn sync_runtime_fields(record: &mut InstanceRecordV2, status: &WorkerStatusV2) {
    record.worker = Some(status.identity.clone());
    record.active_run_id = status.run_id;
    record.adb_serial.clone_from(&status.adb_serial);
    record.frame_generation = record.frame_generation.max(status.frame_generation);
    record.host_fps_milli =
        (status.frame_metrics.fps_milli > 0).then_some(status.frame_metrics.fps_milli);
}

fn should_restart_after_failure(record: &InstanceRecordV2) -> bool {
    record.status.desired == DesiredStateV2::Running
        && matches!(record.spec.restart_policy, RestartPolicyV2::OnFailure)
}

fn frame_generation_high_water(paths: &DataPaths, instance_id: Uuid) -> u64 {
    let run_root = paths.runs.join(instance_id.to_string());
    let Ok(entries) = std::fs::read_dir(run_root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let marker_path = entry.path().join("frame-ready-v2.json");
            let bytes = hd_platform::read_regular_nofollow_limited(&marker_path, 64 * 1024).ok()?;
            let marker = serde_json::from_slice::<FrameReadyMarkerV2>(&bytes).ok()?;
            (marker.instance_id == instance_id).then_some(marker.generation)
        })
        .max()
        .unwrap_or(0)
}

fn diagnostic_failure(id: &str, detail: &str) -> hd_core::DiagnosticCheckV2 {
    hd_core::DiagnosticCheckV2 {
        id: id.to_owned(),
        status: hd_core::DiagnosticStatusV2::Fail,
        detail: detail.to_owned(),
        fields: BTreeMap::new(),
    }
}

fn transition_record(
    record: &mut InstanceRecordV2,
    next: ObservedStateV2,
    error: Option<&HostError>,
) -> Result<(), HostError> {
    record.status.transition(
        next,
        error.map(|value| value.code().to_owned()),
        error.map(ToString::to_string),
    )?;
    Ok(())
}

fn acquire_data_lock(paths: &DataPaths) -> Result<std::fs::File, HostError> {
    let path = paths.host_lock();
    let mut file = hd_platform::open_owner_only_rw(&path)?;
    file.try_lock_exclusive().map_err(|source| HostError::Io {
        operation: "lock host data root",
        path: path.clone(),
        source,
    })?;
    file.set_len(0).map_err(|source| HostError::Io {
        operation: "truncate host lock",
        path: path.clone(),
        source,
    })?;
    file.rewind().map_err(|source| HostError::Io {
        operation: "rewind host lock",
        path: path.clone(),
        source,
    })?;
    writeln!(file, "pid={}", std::process::id()).map_err(|source| HostError::Io {
        operation: "write host lock",
        path,
        source,
    })?;
    file.sync_all().map_err(|source| HostError::Io {
        operation: "sync host lock",
        path: paths.host_lock(),
        source,
    })?;
    Ok(file)
}

fn worker_instance_lock_held(paths: &DataPaths, id: Uuid) -> Result<bool, HostError> {
    let path = paths.worker_lock(id);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(HostError::Io {
                operation: "inspect worker instance lock",
                path,
                source,
            });
        }
    };
    if !metadata.file_type().is_file() {
        return Err(HostError::WorkerProtocol(
            "worker instance lock is not a regular file".to_owned(),
        ));
    }
    let file = hd_platform::open_owner_only_rw(&path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            fs2::FileExt::unlock(&file).map_err(|source| HostError::Io {
                operation: "unlock worker instance probe",
                path,
                source,
            })?;
            Ok(false)
        }
        Err(error) => {
            tracing::debug!(
                event = "worker.lock.held",
                instance_id = %id,
                %error,
                "per-instance worker lock is held or cannot be safely acquired"
            );
            Ok(true)
        }
    }
}

fn random_secret() -> Result<String, HostError> {
    let mut bytes = [0_u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| HostError::Random(error.to_string()))?;
    Ok(hex::encode(bytes))
}

fn random_session_token() -> Result<String, HostError> {
    random_secret()
}

fn tokens_equal(expected: &str, supplied: &str) -> bool {
    expected.len() == supplied.len()
        && expected.as_bytes().ct_eq(supplied.as_bytes()).unwrap_u8() == 1
}

fn read_secret(path: &Path) -> Result<String, HostError> {
    let bytes = read_regular_limited(path, 256)?;
    let value = String::from_utf8(bytes).map_err(HostError::SecretUtf8)?;
    let value = value.trim().to_owned();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HostError::WorkerIdentity);
    }
    Ok(value)
}

fn read_regular_limited(path: &Path, maximum: u64) -> Result<Vec<u8>, HostError> {
    hd_platform::read_regular_nofollow_limited(path, maximum).map_err(HostError::Platform)
}

fn sibling_worker() -> Option<PathBuf> {
    let path = std::env::current_exe()
        .ok()?
        .parent()?
        .join(executable_name("hd-worker"));
    path.is_file().then_some(path)
}

async fn wait_for_process_identity(
    pid: u32,
    nonce: Uuid,
    timeout: Duration,
) -> Result<WorkerIdentityV2, HostError> {
    let started = Instant::now();
    loop {
        match hd_platform::process_start_marker(pid) {
            Ok(process_start_marker) => {
                return Ok(WorkerIdentityV2 {
                    pid,
                    process_start_marker,
                    nonce,
                });
            }
            Err(error) if started.elapsed() < timeout => {
                tracing::debug!(
                    event = "worker.identity.waiting",
                    pid,
                    %error,
                    "waiting for spawned worker process identity"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(HostError::Platform(error)),
        }
    }
}

async fn wait_for_process_exit(identity: &WorkerIdentityV2, timeout: Duration) {
    let started = Instant::now();
    while process_identity_is_alive(identity) && started.elapsed() < timeout {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn remove_regular_file_if_present(path: &Path) -> Result<(), HostError> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(HostError::UnsafeFile(path.to_owned()));
    }
    std::fs::remove_file(path).map_err(|source| HostError::Io {
        operation: "delete instance disk",
        path: path.to_owned(),
        source,
    })
}

fn remove_scoped_directory_if_safe(
    root: &Path,
    id: Uuid,
    operation: &'static str,
) -> Result<(), HostError> {
    let path = root.join(id.to_string());
    if path.parent() != Some(root)
        || path.file_name().and_then(|name| name.to_str()) != Some(id.to_string().as_str())
    {
        return Err(HostError::UnsafeFile(path.clone()));
    }
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(HostError::Io {
                operation: "inspect instance directory",
                path: path.clone(),
                source,
            });
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(HostError::UnsafeFile(path.clone()));
    }
    std::fs::remove_dir_all(&path).map_err(|source| HostError::Io {
        operation,
        path,
        source,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("instance {0} was not found")]
    InstanceNotFound(Uuid),
    #[error("operation {0} was not found")]
    OperationNotFound(Uuid),
    #[error("upload {0} was not found")]
    UploadNotFound(Uuid),
    #[error("request instance does not match its path")]
    InstanceMismatch,
    #[error("host is busy: {0}")]
    Busy(&'static str),
    #[error("required host capability is blocked")]
    CapabilityBlocked,
    #[error("worker executable is missing: {0}")]
    WorkerExecutable(PathBuf),
    #[error("worker {0} did not authenticate before the startup deadline")]
    WorkerStartTimeout(Uuid),
    #[error("worker {0} did not exit before the shutdown deadline")]
    WorkerShutdownTimeout(Uuid),
    #[error("worker identity validation failed")]
    WorkerIdentity,
    #[error("worker protocol violation: {0}")]
    WorkerProtocol(String),
    #[error("worker rejected the command: {0:?}")]
    WorkerRejected(ApiErrorV2),
    #[error("display session failed: {0}")]
    DisplaySession(String),
    #[error("upload digest does not match the operation")]
    UploadDigestMismatch,
    #[error("operation failed: {0}")]
    OperationFailed(String),
    #[error("operation {0} timed out")]
    OperationTimeout(Uuid),
    #[error("secure file is unsafe or exceeds its size limit: {0}")]
    UnsafeFile(PathBuf),
    #[error("secure secret is not UTF-8: {0}")]
    SecretUtf8(std::string::FromUtf8Error),
    #[error("secure random generation failed: {0}")]
    Random(String),
    #[error(transparent)]
    Action(#[from] hd_core::ActionValidationError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Lease(#[from] LeaseError),
    #[error(transparent)]
    Ipc(#[from] IpcError),
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
    #[error(transparent)]
    State(#[from] hd_core::StateTransitionError),
    #[error(transparent)]
    Diagnostic(#[from] DiagnosticError),
    #[error("failed to decode JSON: {0}")]
    Json(serde_json::Error),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl HostError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InstanceNotFound(_) => "instance_not_found",
            Self::OperationNotFound(_) => "operation_not_found",
            Self::UploadNotFound(_) => "upload_not_found",
            Self::InstanceMismatch => "instance_mismatch",
            Self::Busy(_) => "busy",
            Self::CapabilityBlocked => "capability_blocked",
            Self::WorkerExecutable(_) => "worker_executable",
            Self::WorkerStartTimeout(_) => "worker_start_timeout",
            Self::WorkerShutdownTimeout(_) => "worker_shutdown_timeout",
            Self::WorkerIdentity => "worker_identity",
            Self::WorkerProtocol(_) => "worker_protocol",
            Self::WorkerRejected(_) => "worker_rejected",
            Self::DisplaySession(_) => "display_session",
            Self::UploadDigestMismatch => "upload_digest_mismatch",
            Self::OperationFailed(_) => "operation_failed",
            Self::OperationTimeout(_) => "operation_timeout",
            Self::UnsafeFile(_) => "unsafe_file",
            Self::SecretUtf8(_) => "secret_utf8",
            Self::Random(_) => "random",
            Self::Action(_) => "action_invalid",
            Self::Store(_) => "store",
            Self::Lease(error) => error.code(),
            Self::Ipc(_) => "ipc",
            Self::Platform(_) => "platform",
            Self::State(_) => "state",
            Self::Diagnostic(_) => "diagnostic",
            Self::Json(_) => "json",
            Self::Io { .. } => "io",
        }
    }

    pub fn api_error(&self) -> ApiErrorV2 {
        ApiErrorV2::new(self.code(), self.to_string()).retryable(matches!(
            self,
            Self::Busy(_)
                | Self::WorkerStartTimeout(_)
                | Self::WorkerShutdownTimeout(_)
                | Self::Ipc(_)
        ))
    }
}
