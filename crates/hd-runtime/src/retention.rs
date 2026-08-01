use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use hd_core::RunResultV2;
use hd_platform::DataPaths;
use serde::Serialize;
use uuid::Uuid;
use walkdir::WalkDir;

pub const DISK_LOW_WATERMARK_BYTES: u64 = 10 * 1024 * 1024 * 1024;
pub const RUN_RETENTION_MAX_COUNT: usize = 40;
pub const RUN_RETENTION_MIN_COUNT: usize = 5;
pub const RUN_RETENTION_MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const RUN_LOG_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const RUN_LOG_RETAINED_TAIL_BYTES: u64 = 16 * 1024 * 1024;
const LOG_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrunedRunV2 {
    pub run_id: Uuid,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunRetentionReportV2 {
    pub retained_count: usize,
    pub retained_bytes: u64,
    pub pruned: Vec<PrunedRunV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RotatedRunLogV2 {
    pub path: PathBuf,
    pub previous_bytes: u64,
    pub retained_tail_bytes: u64,
}

#[derive(Debug)]
struct FinishedRun {
    run_id: Uuid,
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

pub fn enforce_run_retention(
    paths: &DataPaths,
    instance_id: Uuid,
) -> Result<RunRetentionReportV2, RuntimeStorageError> {
    let instance_root = paths.runs.join(instance_id.to_string());
    let metadata = match std::fs::symlink_metadata(&instance_root) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RunRetentionReportV2 {
                retained_count: 0,
                retained_bytes: 0,
                pruned: Vec::new(),
            });
        }
        Err(source) => {
            return Err(RuntimeStorageError::Io {
                operation: "inspect instance run root",
                path: instance_root,
                source,
            });
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(RuntimeStorageError::UnsafePath(instance_root));
    }

    let mut runs = Vec::new();
    for entry in std::fs::read_dir(&instance_root).map_err(|source| RuntimeStorageError::Io {
        operation: "scan instance runs",
        path: instance_root.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| RuntimeStorageError::Io {
            operation: "read instance run entry",
            path: instance_root.clone(),
            source,
        })?;
        let Some(run_id) = entry
            .file_name()
            .to_str()
            .and_then(|name| Uuid::parse_str(name).ok())
        else {
            continue;
        };
        let path = entry.path();
        if path.parent() != Some(instance_root.as_path()) {
            return Err(RuntimeStorageError::UnsafePath(path));
        }
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|source| RuntimeStorageError::Io {
                operation: "inspect run directory",
                path: path.clone(),
                source,
            })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(RuntimeStorageError::UnsafePath(path));
        }
        let Some(result) = read_finished_result(&path)? else {
            continue;
        };
        if result.instance_id != instance_id || result.run_id != run_id {
            return Err(RuntimeStorageError::RunIdentity(path));
        }
        runs.push(FinishedRun {
            run_id,
            bytes: directory_size(&path)?,
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            path,
        });
    }
    runs.sort_by_key(|run| (run.modified, run.run_id));
    let mut retained_bytes = runs
        .iter()
        .fold(0_u64, |total, run| total.saturating_add(run.bytes));
    let mut pruned = Vec::new();
    while runs.len() > RUN_RETENTION_MIN_COUNT
        && (runs.len() > RUN_RETENTION_MAX_COUNT || retained_bytes > RUN_RETENTION_MAX_BYTES)
    {
        let run = runs.remove(0);
        remove_finished_run(&instance_root, &run)?;
        retained_bytes = retained_bytes.saturating_sub(run.bytes);
        pruned.push(PrunedRunV2 {
            run_id: run.run_id,
            bytes: run.bytes,
        });
    }
    Ok(RunRetentionReportV2 {
        retained_count: runs.len(),
        retained_bytes,
        pruned,
    })
}

pub fn available_runtime_disk_bytes(paths: &DataPaths) -> Result<u64, RuntimeStorageError> {
    fs2::available_space(&paths.root).map_err(|source| RuntimeStorageError::Io {
        operation: "query runtime free disk space",
        path: paths.root.clone(),
        source,
    })
}

pub fn rotate_oversized_run_logs(
    run_dir: &Path,
) -> Result<Vec<RotatedRunLogV2>, RuntimeStorageError> {
    validate_run_dir(run_dir)?;
    let mut rotated = Vec::new();
    for entry in WalkDir::new(run_dir).follow_links(false) {
        let entry = entry.map_err(RuntimeStorageError::Walk)?;
        if !entry.file_type().is_file() || !is_bounded_run_log(entry.path()) {
            continue;
        }
        let path = entry.path();
        let metadata =
            std::fs::symlink_metadata(path).map_err(|source| RuntimeStorageError::Io {
                operation: "inspect run log",
                path: path.to_owned(),
                source,
            })?;
        if metadata.file_type().is_symlink() || metadata.len() <= RUN_LOG_MAX_BYTES {
            continue;
        }
        rotated.push(rotate_run_log(path, metadata.len())?);
    }
    Ok(rotated)
}

pub fn spawn_run_log_maintenance(run_dir: PathBuf) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LOG_MAINTENANCE_INTERVAL).await;
            if run_dir.join("result.json").is_file() {
                return;
            }
            let maintenance_dir = run_dir.clone();
            match tokio::task::spawn_blocking(move || rotate_oversized_run_logs(&maintenance_dir))
                .await
            {
                Ok(Ok(rotated)) => {
                    for log in rotated {
                        tracing::info!(
                            event = "runtime.log.rotated",
                            path = %log.path.display(),
                            previous_bytes = log.previous_bytes,
                            retained_tail_bytes = log.retained_tail_bytes,
                            "rotated oversized active run log"
                        );
                    }
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        event = "runtime.log.maintenance.failed",
                        error_code = "runtime_storage",
                        %error,
                        "active run log maintenance failed"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        event = "runtime.log.maintenance.failed",
                        error_code = "runtime_storage_task",
                        %error,
                        "active run log maintenance task failed"
                    );
                }
            }
        }
    });
}

fn read_finished_result(run_dir: &Path) -> Result<Option<RunResultV2>, RuntimeStorageError> {
    let path = run_dir.join("result.json");
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RuntimeStorageError::Io {
                operation: "inspect run result",
                path,
                source,
            });
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(RuntimeStorageError::UnsafePath(path));
    }
    let bytes = std::fs::read(&path).map_err(|source| RuntimeStorageError::Io {
        operation: "read run result",
        path: path.clone(),
        source,
    })?;
    let result = serde_json::from_slice::<RunResultV2>(&bytes)
        .map_err(|source| RuntimeStorageError::Json { path, source })?;
    Ok(result.finished_at.is_some().then_some(result))
}

fn directory_size(path: &Path) -> Result<u64, RuntimeStorageError> {
    let mut bytes = 0_u64;
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(RuntimeStorageError::Walk)?;
        if entry.file_type().is_symlink() {
            return Err(RuntimeStorageError::UnsafePath(entry.path().to_owned()));
        }
        if entry.file_type().is_file() {
            bytes = bytes.saturating_add(
                entry
                    .metadata()
                    .map_err(|source| RuntimeStorageError::WalkMetadata {
                        path: entry.path().to_owned(),
                        source,
                    })?
                    .len(),
            );
        }
    }
    Ok(bytes)
}

fn remove_finished_run(root: &Path, run: &FinishedRun) -> Result<(), RuntimeStorageError> {
    if run.path.parent() != Some(root)
        || run.path.file_name().and_then(|name| name.to_str())
            != Some(run.run_id.to_string().as_str())
    {
        return Err(RuntimeStorageError::UnsafePath(run.path.clone()));
    }
    let metadata =
        std::fs::symlink_metadata(&run.path).map_err(|source| RuntimeStorageError::Io {
            operation: "inspect retained run before pruning",
            path: run.path.clone(),
            source,
        })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(RuntimeStorageError::UnsafePath(run.path.clone()));
    }
    std::fs::remove_dir_all(&run.path).map_err(|source| RuntimeStorageError::Io {
        operation: "prune finalized run",
        path: run.path.clone(),
        source,
    })
}

fn validate_run_dir(run_dir: &Path) -> Result<(), RuntimeStorageError> {
    let metadata =
        std::fs::symlink_metadata(run_dir).map_err(|source| RuntimeStorageError::Io {
            operation: "inspect active run directory",
            path: run_dir.to_owned(),
            source,
        })?;
    if !run_dir.is_absolute() || !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(RuntimeStorageError::UnsafePath(run_dir.to_owned()));
    }
    Ok(())
}

fn is_bounded_run_log(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["log", "txt", "stdout", "stderr"]
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

fn rotate_run_log(
    path: &Path,
    previous_bytes: u64,
) -> Result<RotatedRunLogV2, RuntimeStorageError> {
    let mut source = File::open(path).map_err(|source| RuntimeStorageError::Io {
        operation: "open oversized run log",
        path: path.to_owned(),
        source,
    })?;
    let retained_tail_bytes = previous_bytes.min(RUN_LOG_RETAINED_TAIL_BYTES);
    source
        .seek(SeekFrom::End(
            -i64::try_from(retained_tail_bytes).unwrap_or(i64::MAX),
        ))
        .map_err(|source| RuntimeStorageError::Io {
            operation: "seek oversized run log tail",
            path: path.to_owned(),
            source,
        })?;
    let mut tail = Vec::with_capacity(usize::try_from(retained_tail_bytes).unwrap_or(0));
    source
        .read_to_end(&mut tail)
        .map_err(|source| RuntimeStorageError::Io {
            operation: "read oversized run log tail",
            path: path.to_owned(),
            source,
        })?;
    let previous_path = path.with_file_name(format!(
        "{}.previous",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("run.log")
    ));
    hd_platform::write_owner_only(&previous_path, &tail)?;
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| RuntimeStorageError::Io {
            operation: "truncate oversized active run log",
            path: path.to_owned(),
            source,
        })?;
    Ok(RotatedRunLogV2 {
        path: path.to_owned(),
        previous_bytes,
        retained_tail_bytes: u64::try_from(tail.len()).unwrap_or(u64::MAX),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeStorageError {
    #[error("runtime storage path is unsafe: {0}")]
    UnsafePath(PathBuf),
    #[error("run result identity does not match its directory: {0}")]
    RunIdentity(PathBuf),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("decode run result {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("walk runtime storage: {0}")]
    Walk(#[source] walkdir::Error),
    #[error("read runtime storage metadata for {path}: {source}")]
    WalkMetadata {
        path: PathBuf,
        #[source]
        source: walkdir::Error,
    },
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
}
