use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Parser, Subcommand};
use ed25519_dalek::{Signer as _, SigningKey};
use hd_core::{
    ApiErrorV2, CONTROL_PROTOCOL_VERSION, CreateInstanceRequestV2, DiagnosticRequestV2,
    FRAME_PROTOCOL_VERSION, HOST_CERTIFICATION_VERSION, HostCertificationV2, InstanceSpecV2,
    LeaseKindV2, LeaseV2, ObservedStateV2, OperationKindV2, StopModeV2, WORKER_PROTOCOL_VERSION,
    WorkerCommandV2, WorkerDescriptorV2, WorkerPayloadV2, WorkerRequestV2,
};
use hd_platform::DataPaths;
use hd_runtime::{
    ArtifactTrustStore, HostClientV2, HostService, LeaseManager, PersistentStore, run_host_http,
    send_worker_request, worker_endpoint,
};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

mod process;

const CERTIFICATION_EVIDENCE: [&str; 8] = [
    "hd_quality",
    "host_worker_smoke",
    "http_security_smoke",
    "lease_recovery_smoke",
    "diagnostic_smoke",
    "real_guest",
    "zero_copy",
    "device_profile_conformance",
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
    Quality,
    CheckPortable,
    Smoke,
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
    /// Sign an exact platform/bundle certification after all eight evidence gates pass.
    Certify {
        #[arg(long)]
        data_root: PathBuf,
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
        #[arg(long, default_value_t = 14, value_parser = 1..=31)]
        validity_days: u64,
        #[arg(long = "evidence", value_parser = parse_evidence, num_args = 8)]
        evidence: Vec<(String, PathBuf)>,
    },
}

fn main() {
    if let Err(error) = run_main() {
        eprintln!("xtask failed: {error:#}");
        std::process::exit(1);
    }
}

fn run_main() -> Result<()> {
    let root = workspace_root()?;
    match Cli::parse().command {
        Task::ProcessCheck => process::process_check(&root),
        Task::Quality => quality(&root),
        Task::CheckPortable => {
            require_windows_gnu()?;
            let target = quality_target_args();
            let mut arguments = vec!["check", "--workspace", "--all-targets"];
            arguments.extend(target.iter().copied());
            run(&root, "cargo", &arguments)
        }
        Task::Smoke => {
            require_windows_gnu()?;
            smoke(&root)
        }
        Task::PeAudit { bin_dir, objdump } => pe_audit(&bin_dir, &objdump),
        Task::Package { target_dir, output } => package(&root, &target_dir, &output),
        Task::AiCycle { task, output } => {
            require_windows_gnu()?;
            process::ai_cycle(&root, &task, &output)
        }
        Task::Readback {
            task,
            output,
            gate_reports,
        } => process::readback(&root, &task, &output, &gate_reports),
        Task::Certify {
            data_root,
            guest_digest,
            host_digest,
            capability_fingerprint,
            signer_key_id,
            signing_key,
            validity_days,
            evidence,
        } => certify(
            &data_root,
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

fn quality(root: &Path) -> Result<()> {
    require_windows_gnu()?;
    process::process_check(root)?;
    run(root, "git", &["diff", "--check"])?;
    run(root, "git", &["diff", "--cached", "--check"])?;
    run(root, "cargo", &["fmt", "--all", "--", "--check"])?;
    let target = quality_target_args();
    let mut check = vec!["check", "--workspace", "--all-targets"];
    check.extend(target.iter().copied());
    run(root, "cargo", &check)?;
    let mut clippy = vec!["clippy", "--workspace", "--all-targets"];
    clippy.extend(target.iter().copied());
    clippy.extend(["--", "-D", "warnings"]);
    run(root, "cargo", &clippy)?;
    let mut build = vec!["build", "--workspace", "--bins"];
    build.extend(target.iter().copied());
    run(root, "cargo", &build)?;
    smoke(root)
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

#[allow(clippy::too_many_lines)]
fn smoke(root: &Path) -> Result<()> {
    let worker_executable = ensure_worker_binary(root)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hd-smoke")
        .build()
        .context("create smoke runtime")?;
    runtime.block_on(async {
        let temporary = tempfile::tempdir().context("create smoke data directory")?;
        lease_multi_instance_smoke(temporary.path())?;
        let paths = DataPaths::from_root(temporary.path().join("data"));
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
        let client = HostClientV2::connect(paths.clone()).await?;
        let descriptor = client.descriptor().clone();

        let health = client.health().await?;
        ensure!(health.pid == std::process::id(), "health PID mismatch");
        let first_capabilities = client.capabilities(None).await?;
        let second_capabilities = client.capabilities(None).await?;
        ensure!(
            first_capabilities.fingerprint == second_capabilities.fingerprint,
            "capability fingerprint changed across consecutive dynamic resource samples"
        );
        let migrated = client.get_instance(legacy_id).await?;
        ensure!(
            migrated.spec.schema_version == 2,
            "legacy config was not migrated"
        );
        ensure!(
            paths.migration_backup(legacy_id).is_file(),
            "migration backup is missing"
        );

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

        let apk = temporary.path().join("contract.apk");
        std::fs::write(&apk, minimal_apk()?)?;
        let upload = client.upload_apk(&apk).await?;
        ensure!(upload.path.is_file(), "streamed APK upload is missing");

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
        worker_process_smoke(&worker_executable).await?;
        println!("HD V2 contract, process separation and blocked-release smoke passed");
        Ok(())
    })
}

fn lease_multi_instance_smoke(root: &Path) -> Result<()> {
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
        "/v2/instances",
        "/v2/instances/{id}",
        "/v2/instances/{id}/operations",
        "/v2/instances/{id}/actions",
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

#[allow(clippy::too_many_lines)]
async fn worker_process_smoke(worker_executable: &Path) -> Result<()> {
    let temporary = tempfile::tempdir()?;
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
    let ping = send_worker_request(
        &endpoint,
        &WorkerRequestV2 {
            protocol_version: WORKER_PROTOCOL_VERSION,
            request_id: ping_id,
            instance_id,
            bearer_token: secret.clone(),
            command: WorkerCommandV2::Ping,
        },
    )
    .await?;
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
    drop(host);
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
    worker_guard.disarm();
    Ok(())
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
    let required = CERTIFICATION_EVIDENCE.into_iter().collect::<BTreeSet<_>>();
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
        capability_fingerprint: capability_fingerprint.to_owned(),
        guest_bundle_digest: guest_digest.to_owned(),
        host_bundle_digest: host_digest.to_owned(),
        device_profile: "hd-phone-android15-v2".to_owned(),
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
        if path.extension().and_then(|value| value.to_str()) != Some("exe") {
            continue;
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
        let imports = String::from_utf8_lossy(&output.stdout).to_ascii_uppercase();
        for forbidden in ["VCRUNTIME", "MSVCP", "CONCRT", "MFC"] {
            ensure!(
                !imports.contains(forbidden),
                "{} imports forbidden MSVC runtime {forbidden}",
                path.display()
            );
        }
        audited = audited.saturating_add(1);
    }
    ensure!(
        audited > 0,
        "no .exe files found under {}",
        bin_dir.display()
    );
    println!("PE audit passed for {audited} executables");
    Ok(())
}

fn package(root: &Path, target_dir: &Path, output: &Path) -> Result<()> {
    std::fs::create_dir_all(output)
        .with_context(|| format!("create package directory {}", output.display()))?;
    for name in [
        "hd.exe",
        "hdctl.exe",
        "hd-host.exe",
        "hd-worker.exe",
        "hd-device-sim.exe",
    ] {
        let source = target_dir.join(name);
        let destination = output.join(name);
        std::fs::copy(&source, &destination)
            .with_context(|| format!("copy {} to {}", source.display(), destination.display()))?;
    }
    for name in ["README.md", "LICENSE", "AGENTS.md"] {
        std::fs::copy(root.join(name), output.join(name))?;
    }
    copy_tree(&root.join("automation"), &output.join("automation"))?;
    copy_tree(&root.join("docs"), &output.join("docs"))
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
