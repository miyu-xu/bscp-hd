use std::path::{Path, PathBuf};

use hd_core::{
    GuestKindV2, InstanceRecordV2, InstanceStorageTransactionV2, ObservedStateV2,
    PowerwashBackupReasonV2, PowerwashBackupV2,
};
use hd_platform::{DataPaths, PlatformError, ensure_owner_only_directory};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{ArtifactError, PersistentStore, StoreError, sha256_file};

const SHA256_HEX_LENGTH: usize = 64;

pub fn validate_storage_confirmation(
    record: &InstanceRecordV2,
    expected_revision: u64,
    confirmation_name: &str,
) -> Result<(), PowerwashError> {
    if record.spec.guest_kind != GuestKindV2::Android {
        return Err(PowerwashError::AndroidOnly);
    }
    if record.status.revision != expected_revision {
        return Err(PowerwashError::RevisionConflict {
            expected: expected_revision,
            actual: record.status.revision,
        });
    }
    if confirmation_name != record.spec.name {
        return Err(PowerwashError::ConfirmationMismatch);
    }
    if !matches!(
        record.status.observed,
        ObservedStateV2::Defined | ObservedStateV2::Stopped
    ) || record.active_run_id.is_some()
        || record.adb_ready
    {
        return Err(PowerwashError::InstanceNotStopped);
    }
    if record.storage_transaction.is_some() {
        return Err(PowerwashError::TransactionPending);
    }
    Ok(())
}

pub fn powerwash(
    paths: &DataPaths,
    store: &PersistentStore,
    record: &mut InstanceRecordV2,
    operation_id: Uuid,
) -> Result<PowerwashBackupV2, PowerwashError> {
    ensure_storage_idle(record)?;
    if record.powerwash_backup.is_some() {
        return Err(PowerwashError::BackupAlreadyAvailable);
    }
    let overlay = paths.disk_overlay(record.spec.id);
    if !overlay.exists() {
        return Err(PowerwashError::OverlayNotFound(overlay));
    }
    let (size_bytes, sha256) = inspect_regular_with_digest(&overlay)?;
    let backup = PowerwashBackupV2 {
        id: operation_id,
        reason: PowerwashBackupReasonV2::Powerwash,
        source_revision: record.status.revision,
        size_bytes,
        sha256,
        created_at: OffsetDateTime::now_utc(),
    };
    record.storage_transaction = Some(InstanceStorageTransactionV2::Powerwash {
        operation_id,
        backup: backup.clone(),
    });
    persist_mutation(store, record)?;

    let backup_path = paths.powerwash_backup(record.spec.id, backup.id);
    if let Err(error) = move_regular_file(&overlay, &backup_path, backup.size_bytes) {
        let _ = recover_record(paths, store, record);
        return Err(error);
    }
    record.powerwash_backup = Some(backup.clone());
    record.storage_transaction = None;
    persist_mutation(store, record)?;
    Ok(backup)
}

pub fn restore_powerwash(
    paths: &DataPaths,
    store: &PersistentStore,
    record: &mut InstanceRecordV2,
    backup_id: Uuid,
    operation_id: Uuid,
) -> Result<Option<PowerwashBackupV2>, PowerwashError> {
    ensure_storage_idle(record)?;
    let source = record
        .powerwash_backup
        .clone()
        .ok_or(PowerwashError::BackupNotFound(backup_id))?;
    if source.id != backup_id {
        return Err(PowerwashError::BackupNotFound(backup_id));
    }
    let source_path = paths.powerwash_backup(record.spec.id, source.id);
    verify_backup(&source_path, &source)?;

    let overlay = paths.disk_overlay(record.spec.id);
    let rollback = if overlay.exists() {
        let (size_bytes, sha256) = inspect_regular_with_digest(&overlay)?;
        Some(PowerwashBackupV2 {
            id: operation_id,
            reason: PowerwashBackupReasonV2::RestoreRollback,
            source_revision: record.status.revision,
            size_bytes,
            sha256,
            created_at: OffsetDateTime::now_utc(),
        })
    } else {
        None
    };
    record.storage_transaction = Some(InstanceStorageTransactionV2::RestorePowerwash {
        operation_id,
        source: source.clone(),
        rollback: rollback.clone(),
    });
    persist_mutation(store, record)?;

    if let Some(rollback) = &rollback {
        let rollback_path = paths.powerwash_backup(record.spec.id, rollback.id);
        if let Err(error) = move_regular_file(&overlay, &rollback_path, rollback.size_bytes) {
            let _ = recover_record(paths, store, record);
            return Err(error);
        }
    }
    if let Err(error) = move_regular_file(&source_path, &overlay, source.size_bytes) {
        let _ = recover_record(paths, store, record);
        return Err(error);
    }
    record.powerwash_backup.clone_from(&rollback);
    record.storage_transaction = None;
    persist_mutation(store, record)?;
    Ok(rollback)
}

pub fn discard_powerwash_backup(
    paths: &DataPaths,
    store: &PersistentStore,
    record: &mut InstanceRecordV2,
    backup_id: Uuid,
    operation_id: Uuid,
) -> Result<PowerwashBackupV2, PowerwashError> {
    ensure_storage_idle(record)?;
    let backup = record
        .powerwash_backup
        .clone()
        .ok_or(PowerwashError::BackupNotFound(backup_id))?;
    if backup.id != backup_id {
        return Err(PowerwashError::BackupNotFound(backup_id));
    }
    let backup_path = paths.powerwash_backup(record.spec.id, backup.id);
    verify_backup(&backup_path, &backup)?;
    record.storage_transaction = Some(InstanceStorageTransactionV2::DiscardPowerwashBackup {
        operation_id,
        backup: backup.clone(),
    });
    persist_mutation(store, record)?;
    if let Err(error) = remove_regular_file(&backup_path) {
        let _ = recover_record(paths, store, record);
        return Err(error);
    }
    record.powerwash_backup = None;
    record.storage_transaction = None;
    persist_mutation(store, record)?;
    remove_empty_backup_directory(paths, record.spec.id);
    Ok(backup)
}

pub fn recover_storage_transactions(
    paths: &DataPaths,
    store: &PersistentStore,
) -> Result<u64, PowerwashError> {
    let mut recovered = 0_u64;
    let mut first_error = None;
    for mut record in store.list_instances()? {
        if record.storage_transaction.is_some() {
            match recover_record(paths, store, &mut record) {
                Ok(()) => recovered = recovered.saturating_add(1),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
    }
    first_error.map_or(Ok(recovered), Err)
}

fn recover_record(
    paths: &DataPaths,
    store: &PersistentStore,
    record: &mut InstanceRecordV2,
) -> Result<(), PowerwashError> {
    let Some(transaction) = record.storage_transaction.clone() else {
        return Ok(());
    };
    let overlay = paths.disk_overlay(record.spec.id);
    match transaction {
        InstanceStorageTransactionV2::Powerwash { backup, .. } => {
            let backup_path = paths.powerwash_backup(record.spec.id, backup.id);
            match (overlay.exists(), backup_path.exists()) {
                (true, false) => {
                    record.storage_transaction = None;
                }
                (false, true) => {
                    verify_backup(&backup_path, &backup)?;
                    record.powerwash_backup = Some(backup);
                    record.storage_transaction = None;
                }
                state => return mark_ambiguous(store, record, "powerwash", state),
            }
        }
        InstanceStorageTransactionV2::RestorePowerwash {
            source, rollback, ..
        } => {
            let source_path = paths.powerwash_backup(record.spec.id, source.id);
            let rollback_path = rollback
                .as_ref()
                .map(|backup| paths.powerwash_backup(record.spec.id, backup.id));
            let state = (
                overlay.exists(),
                source_path.exists(),
                rollback_path.as_ref().is_some_and(|path| path.exists()),
            );
            match (rollback.as_ref(), state) {
                (Some(_), (true, true, false)) | (None, (false, true, false)) => {
                    record.storage_transaction = None;
                }
                (Some(rollback), (false, true, true)) => {
                    let rollback_path = rollback_path.as_ref().expect("rollback path exists");
                    verify_backup(rollback_path, rollback)?;
                    move_regular_file(rollback_path, &overlay, rollback.size_bytes)?;
                    record.storage_transaction = None;
                }
                (Some(rollback), (true, false, true)) => {
                    verify_backup(&overlay, &source)?;
                    record.powerwash_backup = Some(rollback.clone());
                    record.storage_transaction = None;
                }
                (None, (true, false, false)) => {
                    verify_backup(&overlay, &source)?;
                    record.powerwash_backup = None;
                    record.storage_transaction = None;
                }
                _ => return mark_ambiguous_restore(store, record, state),
            }
        }
        InstanceStorageTransactionV2::DiscardPowerwashBackup { backup, .. } => {
            let backup_path = paths.powerwash_backup(record.spec.id, backup.id);
            if backup_path.exists() {
                verify_backup(&backup_path, &backup)?;
            } else {
                record.powerwash_backup = None;
            }
            record.storage_transaction = None;
        }
    }
    persist_mutation(store, record)
}

fn ensure_storage_idle(record: &InstanceRecordV2) -> Result<(), PowerwashError> {
    if record.storage_transaction.is_some() {
        return Err(PowerwashError::TransactionPending);
    }
    if record.worker.is_some() || record.active_run_id.is_some() || record.adb_ready {
        return Err(PowerwashError::WorkerStillAttached);
    }
    Ok(())
}

fn persist_mutation(
    store: &PersistentStore,
    record: &mut InstanceRecordV2,
) -> Result<(), PowerwashError> {
    record.status.revision = record.status.revision.saturating_add(1);
    record.status.updated_at = OffsetDateTime::now_utc();
    store.put_instance(record)?;
    Ok(())
}

fn inspect_regular_with_digest(path: &Path) -> Result<(u64, String), PowerwashError> {
    let metadata = regular_metadata(path)?;
    if metadata.len() == 0 {
        return Err(PowerwashError::EmptyOverlay(path.to_owned()));
    }
    Ok((metadata.len(), sha256_file(path)?))
}

fn verify_backup(path: &Path, backup: &PowerwashBackupV2) -> Result<(), PowerwashError> {
    if backup.sha256.len() != SHA256_HEX_LENGTH
        || !backup.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(PowerwashError::InvalidDigest(backup.id));
    }
    let metadata = regular_metadata(path)?;
    if metadata.len() != backup.size_bytes {
        return Err(PowerwashError::BackupSizeMismatch {
            id: backup.id,
            expected: backup.size_bytes,
            actual: metadata.len(),
        });
    }
    let actual = sha256_file(path)?;
    if actual != backup.sha256 {
        return Err(PowerwashError::BackupDigestMismatch(backup.id));
    }
    Ok(())
}

fn regular_metadata(path: &Path) -> Result<std::fs::Metadata, PowerwashError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| PowerwashError::Io {
        operation: "inspect powerwash storage file",
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(PowerwashError::UnsafePath(path.to_owned()));
    }
    Ok(metadata)
}

fn move_regular_file(source: &Path, destination: &Path, size: u64) -> Result<(), PowerwashError> {
    let source_metadata = regular_metadata(source)?;
    if source_metadata.len() != size {
        return Err(PowerwashError::SourceChanged(source.to_owned()));
    }
    if std::fs::symlink_metadata(destination).is_ok() {
        return Err(PowerwashError::DestinationExists(destination.to_owned()));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| PowerwashError::UnsafePath(destination.to_owned()))?;
    ensure_owner_only_directory(parent)?;
    std::fs::rename(source, destination).map_err(|source_error| PowerwashError::Io {
        operation: "atomically move powerwash storage file",
        path: destination.to_owned(),
        source: source_error,
    })?;
    let moved = regular_metadata(destination)?;
    if moved.len() != size {
        return Err(PowerwashError::SourceChanged(destination.to_owned()));
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(destination)
        .and_then(|file| file.sync_all())
        .map_err(|source_error| PowerwashError::Io {
            operation: "sync powerwash storage file",
            path: destination.to_owned(),
            source: source_error,
        })?;
    sync_directory(parent)?;
    if let Some(source_parent) = source.parent()
        && source_parent != parent
    {
        sync_directory(source_parent)?;
    }
    Ok(())
}

fn remove_regular_file(path: &Path) -> Result<(), PowerwashError> {
    regular_metadata(path)?;
    std::fs::remove_file(path).map_err(|source| PowerwashError::Io {
        operation: "discard powerwash backup",
        path: path.to_owned(),
        source,
    })?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), PowerwashError> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| PowerwashError::Io {
            operation: "sync powerwash directory",
            path: path.to_owned(),
            source,
        })
}

#[cfg(windows)]
#[allow(clippy::unnecessary_wraps)]
fn sync_directory(_path: &Path) -> Result<(), PowerwashError> {
    Ok(())
}

fn remove_empty_backup_directory(paths: &DataPaths, instance_id: Uuid) {
    let directory = paths.powerwash_backup_dir(instance_id);
    let _ = std::fs::remove_dir(&directory);
    if let Some(parent) = directory.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

fn mark_ambiguous(
    store: &PersistentStore,
    record: &mut InstanceRecordV2,
    transaction: &str,
    state: (bool, bool),
) -> Result<(), PowerwashError> {
    mark_ambiguous_state(
        store,
        record,
        format!("{transaction} overlay={} backup={}", state.0, state.1),
    )
}

fn mark_ambiguous_restore(
    store: &PersistentStore,
    record: &mut InstanceRecordV2,
    state: (bool, bool, bool),
) -> Result<(), PowerwashError> {
    mark_ambiguous_state(
        store,
        record,
        format!(
            "restore overlay={} source={} rollback={}",
            state.0, state.1, state.2
        ),
    )
}

fn mark_ambiguous_state(
    store: &PersistentStore,
    record: &mut InstanceRecordV2,
    detail: String,
) -> Result<(), PowerwashError> {
    record.status.observed = ObservedStateV2::Blocked;
    record.status.error_code = Some("powerwash_recovery_ambiguous".to_owned());
    record.status.reason = Some(format!(
        "storage recovery found an ambiguous powerwash transaction; no file was removed ({detail})"
    ));
    persist_mutation(store, record)?;
    Err(PowerwashError::AmbiguousRecovery(detail))
}

#[derive(Debug, Error)]
pub enum PowerwashError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Platform(#[from] PlatformError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error("powerwash is only available for Android instances")]
    AndroidOnly,
    #[error("instance revision changed: expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("powerwash confirmation must exactly match the instance name")]
    ConfirmationMismatch,
    #[error("powerwash requires a Defined or Stopped instance with no active run")]
    InstanceNotStopped,
    #[error("a storage transaction is already pending")]
    TransactionPending,
    #[error("the instance worker is still attached to storage")]
    WorkerStillAttached,
    #[error("restore or discard the existing powerwash backup before creating another")]
    BackupAlreadyAvailable,
    #[error("powerwash backup {0} was not found")]
    BackupNotFound(Uuid),
    #[error("powerwash backup {0} has an invalid digest")]
    InvalidDigest(Uuid),
    #[error("powerwash backup {id} size differs: expected {expected}, actual {actual}")]
    BackupSizeMismatch {
        id: Uuid,
        expected: u64,
        actual: u64,
    },
    #[error("powerwash backup {0} digest differs")]
    BackupDigestMismatch(Uuid),
    #[error("powerwash storage file is not a regular no-follow file: {0}")]
    UnsafePath(PathBuf),
    #[error("powerwash source changed while preparing the transaction: {0}")]
    SourceChanged(PathBuf),
    #[error("powerwash destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("powerwash cannot operate on an empty overlay: {0}")]
    EmptyOverlay(PathBuf),
    #[error("the instance has no userdata overlay to powerwash: {0}")]
    OverlayNotFound(PathBuf),
    #[error("ambiguous powerwash recovery state: {0}")]
    AmbiguousRecovery(String),
    #[error("{operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl PowerwashError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::AndroidOnly => "powerwash_android_only",
            Self::RevisionConflict { .. } => "revision_conflict",
            Self::ConfirmationMismatch => "powerwash_confirmation_mismatch",
            Self::InstanceNotStopped => "powerwash_instance_not_stopped",
            Self::TransactionPending => "powerwash_transaction_pending",
            Self::WorkerStillAttached => "powerwash_worker_attached",
            Self::BackupAlreadyAvailable => "powerwash_backup_exists",
            Self::BackupNotFound(_) => "powerwash_backup_not_found",
            Self::InvalidDigest(_)
            | Self::BackupSizeMismatch { .. }
            | Self::BackupDigestMismatch(_) => "powerwash_backup_integrity",
            Self::UnsafePath(_) | Self::DestinationExists(_) => "powerwash_path_unsafe",
            Self::SourceChanged(_) => "powerwash_source_changed",
            Self::EmptyOverlay(_) => "powerwash_overlay_empty",
            Self::OverlayNotFound(_) => "powerwash_overlay_not_found",
            Self::AmbiguousRecovery(_) => "powerwash_recovery_ambiguous",
            Self::Store(_) => "powerwash_store",
            Self::Platform(_) => "powerwash_platform",
            Self::Artifact(_) => "powerwash_artifact",
            Self::Io { .. } => "powerwash_io",
        }
    }
}
