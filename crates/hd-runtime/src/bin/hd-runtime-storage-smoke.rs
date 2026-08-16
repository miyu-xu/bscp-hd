use std::io::{Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use hd_core::{InstanceSpecV2, ObservedStateV2, RunResultV2};
use hd_platform::DataPaths;
use hd_runtime::{
    RUN_LOG_MAX_BYTES, RUN_LOG_RETAINED_TAIL_BYTES, RUN_RETENTION_MAX_BYTES,
    RUN_RETENTION_MAX_COUNT, RUNTIME_DISK_HEADROOM_BYTES, RunRetentionReportV2,
    RuntimeDiskRequirementMode, available_runtime_disk_bytes, enforce_run_retention,
    remove_finished_run_ephemeral_artifacts, rotate_oversized_run_logs, runtime_disk_requirement,
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
    let mut newest_finished_run = None;
    for index in 0..total_runs {
        newest_finished_run = Some(write_finished_run(paths, instance.id, index)?);
    }
    let finished_run = newest_finished_run.context("finished run fixture is missing")?;
    let finished_log = finished_run.join("logcat-hvc2.txt");
    let finished_marker = b"HD-FINISHED-RUN-TAIL";
    write_oversized_log(&finished_log, finished_marker)?;
    let (
        legacy_ephemeral_initrd,
        legacy_extra_idsig_0,
        legacy_extra_idsig_7,
        out_of_range_extra_idsig,
    ) = write_legacy_ephemeral_fixtures(&finished_run)?;

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
    verify_finished_log_compaction(&retention, &finished_run, &finished_log, finished_marker)?;
    ensure!(
        retention.removed_ephemeral
            == [
                legacy_ephemeral_initrd.clone(),
                legacy_extra_idsig_0.clone(),
                legacy_extra_idsig_7.clone(),
            ]
            && !legacy_ephemeral_initrd.exists()
            && !legacy_extra_idsig_0.exists()
            && !legacy_extra_idsig_7.exists()
            && out_of_range_extra_idsig.is_file(),
        "pre-start retention did not remove a legacy finalized launch artifact"
    );
    let (removed_current_ephemeral, current_ephemeral_initrd, current_extra_idsig) =
        verify_current_ephemeral_cleanup(&finished_run, &out_of_range_extra_idsig)?;

    let (active_dir, active_ephemeral_initrd, rotated) =
        verify_active_run_storage(paths, instance.id)?;
    let available = available_runtime_disk_bytes(paths)?;
    ensure!(available > 0, "runtime disk availability was not reported");
    let initial_requirement = runtime_disk_requirement(paths, Some(&instance));
    ensure!(
        initial_requirement.mode == RuntimeDiskRequirementMode::NewInstanceStorage,
        "new Android instance did not require initial storage provisioning"
    );
    let disk = paths.disk_overlay(instance.id);
    std::fs::File::create(&disk)?.set_len(1)?;
    let restart_requirement = runtime_disk_requirement(paths, Some(&instance));
    ensure!(
        restart_requirement.mode == RuntimeDiskRequirementMode::ExistingInstanceStorage
            && restart_requirement.required_free_bytes == RUNTIME_DISK_HEADROOM_BYTES,
        "existing Android storage was counted as a new allocation"
    );
    Ok(json!({
        "schema_version": 1,
        "gate": "runtime-storage-smoke",
        "status": "pass",
        "retention": retention,
        "retention_limits": {
            "max_count": RUN_RETENTION_MAX_COUNT,
            "max_bytes": RUN_RETENTION_MAX_BYTES,
            "log_max_bytes": RUN_LOG_MAX_BYTES,
            "retained_log_tail_bytes": RUN_LOG_RETAINED_TAIL_BYTES,
        },
        "log_rotation": rotated,
        "available_disk_bytes": available,
        "initial_disk_requirement": initial_requirement,
        "restart_disk_requirement": restart_requirement,
        "active_run_protected": !active_dir.join("result.json").exists()
            && active_ephemeral_initrd.is_file(),
        "finished_run_ephemeral_cleanup": {
            "legacy_removed_by_retention": retention.removed_ephemeral,
            "current_removed_after_finish": removed_current_ephemeral,
            "reproducible_initrd_removed": !legacy_ephemeral_initrd.exists()
                && !current_ephemeral_initrd.exists(),
            "microdroid_extra_idsigs_removed": !legacy_extra_idsig_0.exists()
                && !legacy_extra_idsig_7.exists()
                && !current_extra_idsig.exists(),
            "out_of_range_idsig_preserved": out_of_range_extra_idsig.is_file(),
        },
    }))
}

fn verify_active_run_storage(
    paths: &DataPaths,
    instance_id: Uuid,
) -> Result<(PathBuf, PathBuf, Vec<hd_runtime::RotatedRunLogV2>)> {
    let active_dir = paths.run_dir(instance_id, Uuid::new_v4());
    std::fs::create_dir_all(&active_dir)?;
    let active_ephemeral_initrd = active_dir.join("initrd-android-hd.img");
    let active_extra_idsig = active_dir.join("microdroid-extra-2.idsig");
    std::fs::write(&active_ephemeral_initrd, b"active patched initrd")?;
    std::fs::write(&active_extra_idsig, b"active extra idsig")?;
    ensure!(
        remove_finished_run_ephemeral_artifacts(&active_dir).is_err()
            && active_ephemeral_initrd.is_file()
            && active_extra_idsig.is_file(),
        "active run ephemeral artifact was not protected"
    );
    let log = active_dir.join("logcat-hvc2.txt");
    let marker = b"HD-STORAGE-SMOKE-TAIL";
    write_oversized_log(&log, marker)?;
    let rotated = rotate_oversized_run_logs(&active_dir)?;
    ensure!(rotated.len() == 1, "oversized run log was not rotated");
    ensure!(
        std::fs::metadata(&log)?.len() == 0,
        "active log was not truncated"
    );
    let previous_bytes = std::fs::read(active_dir.join("logcat-hvc2.txt.previous"))?;
    ensure!(
        previous_bytes.ends_with(marker),
        "rotated log tail did not preserve the newest bytes"
    );
    Ok((active_dir, active_ephemeral_initrd, rotated))
}

fn write_oversized_log(path: &Path, marker: &[u8]) -> Result<()> {
    let file = std::fs::File::create(path)?;
    file.set_len(RUN_LOG_MAX_BYTES + 1)?;
    drop(file);
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::End(-i64::try_from(marker.len())?))?;
    file.write_all(marker)?;
    Ok(())
}

fn verify_finished_log_compaction(
    retention: &RunRetentionReportV2,
    run_dir: &Path,
    log: &Path,
    marker: &[u8],
) -> Result<()> {
    ensure!(
        retention.retained_bytes <= RUN_RETENTION_MAX_BYTES,
        "retention exceeded its byte budget"
    );
    ensure!(
        retention.compacted.len() == 1
            && retention.compacted[0].path == log
            && retention.compacted[0].previous_bytes == RUN_LOG_MAX_BYTES + 1
            && retention.compacted[0].retained_tail_bytes == RUN_LOG_RETAINED_TAIL_BYTES,
        "retention did not compact the finalized oversized log"
    );
    ensure!(
        std::fs::metadata(log)?.len() == 0,
        "finalized oversized log was not truncated"
    );
    let previous = std::fs::read(run_dir.join("logcat-hvc2.txt.previous"))?;
    ensure!(
        previous.ends_with(marker),
        "finalized log compaction did not preserve the newest bytes"
    );
    Ok(())
}

fn write_finished_run(paths: &DataPaths, instance_id: Uuid, index: usize) -> Result<PathBuf> {
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
    Ok(run_dir)
}

fn write_legacy_ephemeral_fixtures(run_dir: &Path) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf)> {
    let initrd = run_dir.join("initrd-android-hd.img");
    let extra_idsig_0 = run_dir.join("microdroid-extra-0.idsig");
    let extra_idsig_7 = run_dir.join("microdroid-extra-7.idsig");
    let out_of_range_extra_idsig = run_dir.join("microdroid-extra-8.idsig");
    std::fs::write(&initrd, b"legacy reproducible patched initrd")?;
    std::fs::write(&extra_idsig_0, b"legacy reproducible extra idsig zero")?;
    std::fs::write(&extra_idsig_7, b"legacy reproducible extra idsig seven")?;
    std::fs::write(
        &out_of_range_extra_idsig,
        b"must not match the bounded extra idsig whitelist",
    )?;
    Ok((
        initrd,
        extra_idsig_0,
        extra_idsig_7,
        out_of_range_extra_idsig,
    ))
}

fn verify_current_ephemeral_cleanup(
    run_dir: &Path,
    out_of_range_extra_idsig: &Path,
) -> Result<(Vec<PathBuf>, PathBuf, PathBuf)> {
    let initrd = run_dir.join("initrd-android-hd.img");
    let extra_idsig = run_dir.join("microdroid-extra-1.idsig");
    std::fs::write(&initrd, b"current reproducible patched initrd")?;
    std::fs::write(&extra_idsig, b"current reproducible extra idsig")?;
    let removed = remove_finished_run_ephemeral_artifacts(run_dir)?;
    ensure!(
        removed == [initrd.clone(), extra_idsig.clone()]
            && !initrd.exists()
            && !extra_idsig.exists()
            && out_of_range_extra_idsig.is_file(),
        "finished run ephemeral launch artifact was not removed"
    );
    Ok((removed, initrd, extra_idsig))
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
