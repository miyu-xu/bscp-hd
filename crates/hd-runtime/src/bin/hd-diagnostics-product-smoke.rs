use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, ensure};
use hd_core::{DeviceCapabilitiesV2, HostCapabilitiesV2, InstanceRecordV2, InstanceSpecV2};
use hd_platform::DataPaths;
use hd_runtime::{DiagnosticCollector, DiagnosticInputsV2, sha256_file};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

fn main() -> Result<()> {
    let output = output_path()?;
    let temporary = std::env::temp_dir()
        .canonicalize()
        .context("canonicalize temporary directory")?
        .join(format!("hd-diagnostics-product-smoke-{}", Uuid::new_v4()));
    let paths = DataPaths::from_root(temporary.clone());
    paths
        .ensure()
        .context("create diagnostic smoke data root")?;
    let result = run_smoke(&paths);
    let cleanup = std::fs::remove_dir_all(&temporary);
    let evidence = result?;
    cleanup.context("remove diagnostic smoke data root")?;
    let bytes = serde_json::to_vec_pretty(&evidence)?;
    if let Some(path) = output {
        hd_platform::write_owner_only(&path, &bytes)
            .with_context(|| format!("write diagnostic smoke evidence {}", path.display()))?;
    } else {
        println!("{}", String::from_utf8(bytes)?);
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn run_smoke(paths: &DataPaths) -> Result<serde_json::Value> {
    let private_marker = "diagnostic-private-marker";
    let host_log = paths.logs.join("host-product-smoke.jsonl");
    hd_platform::write_owner_only(
        &host_log,
        format!(
            "{{\"event\":\"smoke\",\"password\":\"{private_marker}\",\"authorization\":\"Bearer {private_marker}\"}}\n"
        )
        .as_bytes(),
    )?;

    let stale = paths
        .diagnostics
        .join(format!("diag-stale.tar.tmp-{}", Uuid::new_v4()));
    let recent = paths
        .diagnostics
        .join(format!("diag-recent.tar.tmp-{}", Uuid::new_v4()));
    hd_platform::write_owner_only(&stale, b"stale")?;
    hd_platform::write_owner_only(&recent, b"recent")?;
    let stale_file = std::fs::OpenOptions::new().write(true).open(&stale)?;
    stale_file.set_times(
        std::fs::FileTimes::new().set_modified(
            SystemTime::now()
                .checked_sub(Duration::from_hours(2))
                .context("calculate stale timestamp")?,
        ),
    )?;

    let instance_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let capture_id = Uuid::new_v4();
    let capture_member = PathBuf::from(format!(
        "run/{run_id}/components/rootcanal-hci-{capture_id}.btsnoop"
    ));
    let capture_path = paths
        .run_dir(instance_id, run_id)
        .join("components")
        .join(format!("rootcanal-hci-{capture_id}.btsnoop"));
    std::fs::create_dir_all(
        capture_path
            .parent()
            .context("Bluetooth HCI capture parent is unavailable")?,
    )?;
    let mut capture_bytes = b"btsnoop\0\0\0\0\x01\0\0\x03\xea".to_vec();
    capture_bytes.extend_from_slice(&[
        0, 0, 0, 4, // original length
        0, 0, 0, 4, // included length
        0, 0, 0, 1, // controller-to-Guest direction
        0, 0, 0, 0, // dropped packets
        0, 0, 0, 0, 0, 0, 0, 1, // timestamp
        0x04, 0x0e, 0x01, 0xff, // H4 event containing non-UTF-8 data
    ]);
    hd_platform::write_owner_only(&capture_path, &capture_bytes)?;
    let mut instance = InstanceRecordV2::new(InstanceSpecV2 {
        id: instance_id,
        name: "Diagnostics HCI product smoke".to_owned(),
        ..InstanceSpecV2::default()
    });
    instance.active_run_id = Some(run_id);

    let collector = DiagnosticCollector::new(paths.clone());
    let mut latest = None;
    for _ in 0..12 {
        latest = Some(collector.collect(diagnostic_inputs(Some(instance.clone())))?);
    }
    let latest = latest.context("diagnostic collector returned no bundle")?;
    ensure!(latest.path.is_file(), "latest diagnostic bundle is missing");
    ensure!(
        sha256_file(&latest.path)? == latest.archive_sha256,
        "reported archive digest does not match the file"
    );
    ensure!(
        !stale.exists(),
        "stale diagnostic temporary was not removed"
    );
    ensure!(
        recent.is_file(),
        "a recent diagnostic temporary owned by another collection was removed"
    );

    let bundles = std::fs::read_dir(&paths.diagnostics)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("diag-") && name.ends_with(".tar.zst"))
        })
        .collect::<Vec<_>>();
    ensure!(
        bundles.len() == 10,
        "diagnostic retention kept {}, expected 10",
        bundles.len()
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(
            std::fs::metadata(&latest.path)?.permissions().mode() & 0o777 == 0o600,
            "diagnostic bundle permissions are not owner-only"
        );
    }

    let file = std::fs::File::open(&latest.path)?;
    let decoder = zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    let mut manifest = None;
    let mut archive_text = String::new();
    let mut archived_capture = None;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        archive_text.push_str(&String::from_utf8_lossy(&bytes));
        if path == Path::new("diagnostic-manifest-v2.json") {
            manifest = Some(serde_json::from_slice::<serde_json::Value>(&bytes)?);
        }
        if path == capture_member {
            archived_capture = Some(bytes);
        }
    }
    ensure!(
        !archive_text.contains(private_marker),
        "diagnostic redaction leaked a private marker"
    );
    let manifest = manifest.context("diagnostic manifest is missing")?;
    ensure!(
        archived_capture.as_deref() == Some(capture_bytes.as_slice()),
        "diagnostics changed the bounded Bluetooth HCI capture bytes"
    );
    ensure!(
        manifest
            .get("bundle_id")
            .and_then(serde_json::Value::as_str)
            == Some(latest.bundle_id.to_string().as_str()),
        "diagnostic manifest bundle identity does not match the response"
    );

    Ok(json!({
        "schema_version": 1,
        "gate": "diagnostics-product-contract",
        "status": "pass",
        "host_only_supported": true,
        "atomic_bundle_present": latest.path.is_file(),
        "archive_sha256": latest.archive_sha256,
        "manifest_sha256": latest.manifest_sha256,
        "size_bytes": latest.size_bytes,
        "retained_bundle_count": bundles.len(),
        "stale_temporary_removed": !stale.exists(),
        "concurrent_temporary_preserved": recent.is_file(),
        "redaction_verified": !archive_text.contains(private_marker),
        "bounded_bluetooth_hci_capture_preserved": true,
        "bluetooth_hci_capture_size_bytes": capture_bytes.len(),
    }))
}

fn diagnostic_inputs(instance: Option<InstanceRecordV2>) -> DiagnosticInputsV2 {
    DiagnosticInputsV2 {
        capabilities: HostCapabilitiesV2 {
            schema_version: 2,
            generated_at: OffsetDateTime::UNIX_EPOCH,
            platform: "product-smoke".to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            fingerprint: "diagnostic-product-smoke".to_owned(),
            verified: false,
            certified: false,
            development_bypass: false,
            probes: Vec::new(),
            devices: DeviceCapabilitiesV2 {
                profile: "smoke".to_owned(),
                devices: Vec::new(),
            },
        },
        instance,
        operations: Vec::new(),
        leases: Vec::new(),
        migrations: Vec::new(),
        worker_checks: Vec::new(),
        guest_log: None,
    }
}

fn output_path() -> Result<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    let Some(argument) = args.next() else {
        return Ok(None);
    };
    ensure!(
        argument == "--output",
        "usage: hd-diagnostics-product-smoke [--output PATH]"
    );
    let path = args
        .next()
        .map(PathBuf::from)
        .context("--output requires a path")?;
    ensure!(args.next().is_none(), "unexpected extra arguments");
    Ok(Some(path))
}
