use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use hd_core::{
    InstanceSpecV2, InstanceStorageTransactionV2, ObservedStateV2, PowerwashBackupReasonV2,
    PowerwashBackupV2,
};
use hd_platform::DataPaths;
use hd_runtime::{
    PersistentStore, discard_powerwash_backup, powerwash, recover_storage_transactions,
    restore_powerwash, sha256_file, validate_storage_confirmation,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

const ORIGINAL: &[u8] = b"HD-POWERWASH-ORIGINAL-USERDATA";
const CLEAN: &[u8] = b"HD-POWERWASH-CLEAN-USERDATA";

fn main() -> Result<()> {
    let output = std::env::var_os("HD_SMOKE_OUTPUT").map(PathBuf::from);
    let temporary = std::env::temp_dir()
        .canonicalize()
        .context("canonicalize temporary directory")?
        .join(format!("hd-powerwash-smoke-{}", Uuid::new_v4()));
    let paths = DataPaths::from_root(temporary.clone());
    paths.ensure()?;
    let store = PersistentStore::open(&paths.database())?;
    let evidence = run_smoke(&paths, &store);
    drop(store);
    std::fs::remove_dir_all(&temporary).context("remove temporary powerwash data root")?;
    let evidence = evidence?;
    let bytes = serde_json::to_vec_pretty(&evidence)?;
    if let Some(output) = output {
        ensure!(output.is_absolute(), "HD_SMOKE_OUTPUT must be absolute");
        hd_platform::write_owner_only(&output, &bytes)?;
    } else {
        println!("{}", String::from_utf8(bytes)?);
    }
    Ok(())
}

fn run_smoke(paths: &DataPaths, store: &PersistentStore) -> Result<serde_json::Value> {
    let mut record = stopped_android(store, "Powerwash QA")?;
    let overlay = paths.disk_overlay(record.spec.id);
    write_overlay(&overlay, ORIGINAL)?;

    ensure!(
        validate_storage_confirmation(
            &record,
            record.status.revision.saturating_add(1),
            &record.spec.name,
        )
        .is_err()
            && validate_storage_confirmation(&record, record.status.revision, "wrong-name")
                .is_err(),
        "stale revision or incorrect instance-name confirmation was accepted"
    );
    let mut microdroid = stopped_android(store, "temporary")?;
    microdroid.spec = InstanceSpecV2::microdroid("Microdroid QA");
    ensure!(
        validate_storage_confirmation(
            &microdroid,
            microdroid.status.revision,
            &microdroid.spec.name,
        )
        .is_err(),
        "Microdroid powerwash was accepted"
    );

    let first_operation = Uuid::new_v4();
    let original_backup = powerwash(paths, store, &mut record, first_operation)?;
    ensure!(
        !overlay.exists()
            && record.powerwash_backup.as_ref() == Some(&original_backup)
            && record.storage_transaction.is_none()
            && std::fs::read(paths.powerwash_backup(record.spec.id, original_backup.id))?
                == ORIGINAL,
        "powerwash did not atomically retain original userdata"
    );
    ensure!(
        powerwash(paths, store, &mut record, Uuid::new_v4()).is_err(),
        "a second powerwash replaced the only recoverable backup"
    );

    write_overlay(&overlay, CLEAN)?;
    let rollback = restore_powerwash(
        paths,
        store,
        &mut record,
        original_backup.id,
        Uuid::new_v4(),
    )?
    .context("restore did not retain displaced clean userdata")?;
    ensure!(
        std::fs::read(&overlay)? == ORIGINAL
            && std::fs::read(paths.powerwash_backup(record.spec.id, rollback.id))? == CLEAN
            && record.powerwash_backup.as_ref() == Some(&rollback),
        "restore did not swap userdata and rollback backup without loss"
    );
    let restored_again = restore_powerwash(paths, store, &mut record, rollback.id, Uuid::new_v4())?
        .context("second restore did not preserve displaced userdata")?;
    ensure!(
        std::fs::read(&overlay)? == CLEAN
            && std::fs::read(paths.powerwash_backup(record.spec.id, restored_again.id))?
                == ORIGINAL,
        "second restore did not provide reversible swap semantics"
    );

    let crash_commit = verify_crash_commit_recovery(paths, store)?;
    let crash_rollback = verify_crash_rollback_recovery(paths, store)?;
    let discarded =
        discard_powerwash_backup(paths, store, &mut record, restored_again.id, Uuid::new_v4())?;
    ensure!(
        record.powerwash_backup.is_none()
            && !paths
                .powerwash_backup(record.spec.id, discarded.id)
                .exists(),
        "discard did not remove only the explicitly selected backup"
    );

    let mut unsafe_record = stopped_android(store, "Unsafe path QA")?;
    std::fs::create_dir_all(paths.disk_overlay(unsafe_record.spec.id))?;
    ensure!(
        powerwash(paths, store, &mut unsafe_record, Uuid::new_v4()).is_err(),
        "a non-regular userdata path was accepted"
    );

    Ok(json!({
        "schema_version": 1,
        "gate": "powerwash-product-smoke",
        "status": "pass",
        "android_only": true,
        "stopped_only": true,
        "exact_name_confirmation": true,
        "revision_bound": true,
        "single_backup": true,
        "owner_scoped_atomic_move": true,
        "sha256_integrity": true,
        "reversible_restore_swap": true,
        "crash_commit_recovery": crash_commit,
        "crash_rollback_recovery": crash_rollback,
        "explicit_discard": true,
        "unsafe_path_rejected": true,
    }))
}

fn verify_crash_commit_recovery(paths: &DataPaths, store: &PersistentStore) -> Result<bool> {
    let mut record = stopped_android(store, "Crash commit QA")?;
    let overlay = paths.disk_overlay(record.spec.id);
    write_overlay(&overlay, ORIGINAL)?;
    let backup = backup_record(
        &overlay,
        record.status.revision,
        Uuid::new_v4(),
        PowerwashBackupReasonV2::Powerwash,
    )?;
    record.storage_transaction = Some(InstanceStorageTransactionV2::Powerwash {
        operation_id: backup.id,
        backup: backup.clone(),
    });
    store.put_instance(&record)?;
    let backup_path = paths.powerwash_backup(record.spec.id, backup.id);
    hd_platform::ensure_owner_only_directory(
        backup_path.parent().context("backup path has no parent")?,
    )?;
    std::fs::rename(&overlay, &backup_path)?;
    ensure!(recover_storage_transactions(paths, store)? == 1);
    let recovered = store
        .get_instance(record.spec.id)?
        .context("recovered instance missing")?;
    ensure!(
        recovered.storage_transaction.is_none()
            && recovered.powerwash_backup.as_ref() == Some(&backup)
            && std::fs::read(&backup_path)? == ORIGINAL,
        "crash-after-move recovery did not commit the existing backup"
    );
    Ok(true)
}

fn verify_crash_rollback_recovery(paths: &DataPaths, store: &PersistentStore) -> Result<bool> {
    let mut record = stopped_android(store, "Crash rollback QA")?;
    let overlay = paths.disk_overlay(record.spec.id);
    write_overlay(&overlay, ORIGINAL)?;
    let source = powerwash(paths, store, &mut record, Uuid::new_v4())?;
    write_overlay(&overlay, CLEAN)?;
    let rollback = backup_record(
        &overlay,
        record.status.revision,
        Uuid::new_v4(),
        PowerwashBackupReasonV2::RestoreRollback,
    )?;
    record.storage_transaction = Some(InstanceStorageTransactionV2::RestorePowerwash {
        operation_id: rollback.id,
        source: source.clone(),
        rollback: Some(rollback.clone()),
    });
    store.put_instance(&record)?;
    let rollback_path = paths.powerwash_backup(record.spec.id, rollback.id);
    std::fs::rename(&overlay, &rollback_path)?;
    ensure!(recover_storage_transactions(paths, store)? == 1);
    let recovered = store
        .get_instance(record.spec.id)?
        .context("rollback-recovered instance missing")?;
    ensure!(
        recovered.storage_transaction.is_none()
            && recovered.powerwash_backup.as_ref() == Some(&source)
            && std::fs::read(&overlay)? == CLEAN
            && !rollback_path.exists(),
        "crash between restore renames did not roll the current userdata back"
    );
    Ok(true)
}

fn stopped_android(store: &PersistentStore, name: &str) -> Result<hd_core::InstanceRecordV2> {
    let spec = InstanceSpecV2 {
        name: name.to_owned(),
        ..InstanceSpecV2::default()
    };
    let mut record = store.create_instance(spec)?;
    record.status.observed = ObservedStateV2::Stopped;
    record.frame_generation = 1;
    store.put_instance(&record)?;
    Ok(record)
}

fn backup_record(
    path: &Path,
    source_revision: u64,
    id: Uuid,
    reason: PowerwashBackupReasonV2,
) -> Result<PowerwashBackupV2> {
    Ok(PowerwashBackupV2 {
        id,
        reason,
        source_revision,
        size_bytes: std::fs::metadata(path)?.len(),
        sha256: sha256_file(path)?,
        created_at: OffsetDateTime::now_utc(),
    })
}

fn write_overlay(path: &Path, bytes: &[u8]) -> Result<()> {
    hd_platform::write_owner_only(path, bytes)?;
    Ok(())
}
