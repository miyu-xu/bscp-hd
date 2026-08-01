use std::io::{Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use hd_core::{InstanceSpecV2, ObservedStateV2, RunResultV2};
use hd_platform::DataPaths;
use hd_runtime::{
    RUN_LOG_MAX_BYTES, RUN_RETENTION_MAX_COUNT, available_runtime_disk_bytes,
    enforce_run_retention, rotate_oversized_run_logs,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

fn main() -> Result<()> {
    let output = output_path()?;
    let temporary = std::env::temp_dir()
        .canonicalize()
        .context("canonicalize temporary directory")?
        .join(format!("hd-runtime-storage-smoke-{}", Uuid::new_v4()));
    let paths = DataPaths::from_root(temporary.clone());
    paths
        .ensure()
        .context("create temporary runtime data root")?;
    let result = run_smoke(&paths);
    let cleanup = std::fs::remove_dir_all(&temporary);
    let evidence = result?;
    cleanup.context("remove temporary runtime storage smoke directory")?;
    let bytes = serde_json::to_vec_pretty(&evidence)?;
    if let Some(path) = output {
        hd_platform::write_owner_only(&path, &bytes)
            .with_context(|| format!("write storage smoke evidence {}", path.display()))?;
    } else {
        println!("{}", String::from_utf8(bytes)?);
    }
    Ok(())
}

fn run_smoke(paths: &DataPaths) -> Result<serde_json::Value> {
    let instance = InstanceSpecV2::default();
    let total_runs = RUN_RETENTION_MAX_COUNT + 3;
    for index in 0..total_runs {
        write_finished_run(paths, instance.id, index)?;
    }
    let retention = enforce_run_retention(paths, instance.id)?;
    ensure!(
        retention.retained_count == RUN_RETENTION_MAX_COUNT,
        "retention kept {}, expected {}",
        retention.retained_count,
        RUN_RETENTION_MAX_COUNT
    );
    ensure!(
        retention.pruned.len() == total_runs - RUN_RETENTION_MAX_COUNT,
        "retention pruned the wrong number of runs"
    );

    let active_run = Uuid::new_v4();
    let active_dir = paths.run_dir(instance.id, active_run);
    std::fs::create_dir_all(&active_dir)?;
    let log = active_dir.join("logcat-hvc2.txt");
    let file = std::fs::File::create(&log)?;
    file.set_len(RUN_LOG_MAX_BYTES + 1)?;
    drop(file);
    let marker = b"HD-STORAGE-SMOKE-TAIL";
    let mut file = std::fs::OpenOptions::new().write(true).open(&log)?;
    file.seek(SeekFrom::End(-i64::try_from(marker.len())?))?;
    file.write_all(marker)?;
    drop(file);
    let rotated = rotate_oversized_run_logs(&active_dir)?;
    ensure!(rotated.len() == 1, "oversized run log was not rotated");
    ensure!(
        std::fs::metadata(&log)?.len() == 0,
        "active log was not truncated"
    );
    let previous = active_dir.join("logcat-hvc2.txt.previous");
    let previous_bytes = std::fs::read(&previous)?;
    ensure!(
        previous_bytes.ends_with(marker),
        "rotated log tail did not preserve the newest bytes"
    );
    let available = available_runtime_disk_bytes(paths)?;
    ensure!(available > 0, "runtime disk availability was not reported");
    Ok(json!({
        "schema_version": 1,
        "gate": "runtime-storage-smoke",
        "status": "pass",
        "retention": retention,
        "log_rotation": rotated,
        "available_disk_bytes": available,
        "active_run_protected": !active_dir.join("result.json").exists()
            && active_dir.exists(),
    }))
}

fn write_finished_run(paths: &DataPaths, instance_id: Uuid, index: usize) -> Result<()> {
    let run_id = Uuid::new_v4();
    let run_dir = paths.run_dir(instance_id, run_id);
    std::fs::create_dir_all(&run_dir)?;
    let result = RunResultV2 {
        schema_version: 2,
        run_id,
        instance_id,
        started_at: OffsetDateTime::UNIX_EPOCH,
        finished_at: Some(OffsetDateTime::UNIX_EPOCH),
        final_state: ObservedStateV2::Stopped,
        exit_code: Some(0),
        error_code: None,
        reason: None,
    };
    hd_platform::write_owner_only(
        &run_dir.join("result.json"),
        &serde_json::to_vec_pretty(&result)?,
    )?;
    hd_platform::write_owner_only(
        &run_dir.join("crosvm.stdout.log"),
        format!("finalized run {index}\n").as_bytes(),
    )?;
    Ok(())
}

fn output_path() -> Result<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    let Some(argument) = args.next() else {
        return Ok(None);
    };
    ensure!(
        argument == "--output",
        "usage: hd-runtime-storage-smoke [--output PATH]"
    );
    let path = args
        .next()
        .map(PathBuf::from)
        .context("--output requires a path")?;
    ensure!(args.next().is_none(), "unexpected extra arguments");
    ensure!(
        path.is_absolute() || path.parent().is_some_and(Path::exists),
        "output path must be absolute or have an existing parent"
    );
    Ok(Some(path))
}
