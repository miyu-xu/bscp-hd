use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use hd_platform::DataPaths;
use hd_runtime::PersistentStore;
use serde_json::json;
use uuid::Uuid;

fn main() -> Result<()> {
    let output = output_path()?;
    let temporary = std::env::temp_dir()
        .canonicalize()
        .context("canonicalize temporary directory")?
        .join(format!("hd-migration-product-smoke-{}", Uuid::new_v4()));
    let paths = DataPaths::from_root(temporary.clone());
    paths.ensure().context("create migration smoke data root")?;
    let result = run_smoke(&paths);
    let cleanup = std::fs::remove_dir_all(&temporary);
    let evidence = result?;
    cleanup.context("remove migration smoke data root")?;
    let bytes = serde_json::to_vec_pretty(&evidence)?;
    if let Some(path) = output {
        hd_platform::write_owner_only(&path, &bytes)
            .with_context(|| format!("write migration smoke evidence {}", path.display()))?;
    } else {
        println!("{}", String::from_utf8(bytes)?);
    }
    Ok(())
}

fn run_smoke(paths: &DataPaths) -> Result<serde_json::Value> {
    let id = Uuid::new_v4();
    let source = legacy_config(id);
    let source_bytes = serde_json::to_vec_pretty(&source)?;
    let instance_dir = paths.instance_dir(id);
    std::fs::create_dir_all(&instance_dir)?;
    hd_platform::write_owner_only(&paths.legacy_instance_config(id), &source_bytes)?;

    let store = PersistentStore::open(&paths.database())?;
    let first = store.migrate_legacy_instances(paths)?;
    ensure!(first.len() == 1, "valid legacy instance was not migrated");
    let migrated = store
        .get_instance(id)?
        .context("migrated instance is missing from the persistent store")?;
    ensure!(
        migrated.spec.id == id && migrated.spec.schema_version == 2,
        "migrated instance identity or schema version is incorrect"
    );
    ensure!(
        std::fs::read(paths.migration_backup(id))? == source_bytes,
        "migration backup is not byte-for-byte identical to the source"
    );
    ensure!(
        store.migrate_legacy_instances(paths)?.is_empty(),
        "second migration pass was not idempotent"
    );

    let mismatch_directory_id = Uuid::new_v4();
    let mismatch_dir = paths.instance_dir(mismatch_directory_id);
    std::fs::create_dir_all(&mismatch_dir)?;
    hd_platform::write_owner_only(
        &mismatch_dir.join("instance.json"),
        &serde_json::to_vec_pretty(&legacy_config(Uuid::new_v4()))?,
    )?;
    let mismatch_rejected = store
        .migrate_legacy_instances(paths)
        .is_err_and(|error| error.to_string().contains("contains configuration for"));
    ensure!(
        mismatch_rejected,
        "directory/configuration UUID mismatch was not rejected"
    );
    std::fs::remove_dir_all(&mismatch_dir)?;

    #[cfg(unix)]
    let symlink_rejected = {
        use std::os::unix::fs::symlink;
        let external = paths.root.with_file_name(format!(
            "hd-migration-product-smoke-external-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&external)?;
        let external_id = Uuid::new_v4();
        hd_platform::write_owner_only(
            &external.join("instance.json"),
            &serde_json::to_vec_pretty(&legacy_config(external_id))?,
        )?;
        let linked = paths.instance_dir(external_id);
        symlink(&external, &linked)?;
        let rejected = store
            .migrate_legacy_instances(paths)
            .is_err_and(|error| error.to_string().contains("symbolic link"));
        std::fs::remove_file(linked)?;
        std::fs::remove_dir_all(external)?;
        rejected
    };
    #[cfg(not(unix))]
    let symlink_rejected = true;
    ensure!(
        symlink_rejected,
        "symbolic-link legacy instance directory was not rejected"
    );

    Ok(json!({
        "schema_version": 1,
        "gate": "legacy-migration-product-contract",
        "status": "pass",
        "migrated_instance_id": id,
        "source_version": first[0].source_version,
        "target_version": first[0].target_version,
        "byte_exact_backup": std::fs::read(paths.migration_backup(id))? == source_bytes,
        "idempotent": store.migrate_legacy_instances(paths)?.is_empty(),
        "directory_identity_mismatch_rejected": mismatch_rejected,
        "symlink_escape_rejected": symlink_rejected,
    }))
}

fn legacy_config(id: Uuid) -> serde_json::Value {
    json!({
        "schema_version": 1,
        "id": id,
        "name": "Migrated Android",
        "cpu_count": 4,
        "memory_mib": 4096,
        "display": {
            "width": 1080,
            "height": 1920,
            "dpi": 420,
            "refresh_rate_hz": 60,
            "orientation": "portrait",
            "vsync": "on",
            "show_host_fps": false
        },
        "adb": {
            "enabled": true,
            "auto_port": true,
            "host_port": null,
            "adb_path": null,
            "auto_root": false
        },
        "artifacts": {
            "kernel": "",
            "initrd": "",
            "rootfs": "",
            "android_fstab": "",
            "system_image": null,
            "vendor_image": null,
            "expected_sha256": {}
        },
        "extra_kernel_args": []
    })
}

fn output_path() -> Result<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    let Some(argument) = args.next() else {
        return Ok(None);
    };
    ensure!(
        argument == "--output",
        "usage: hd-migration-product-smoke [--output PATH]"
    );
    let path = args
        .next()
        .map(PathBuf::from)
        .context("--output requires a path")?;
    ensure!(args.next().is_none(), "unexpected extra arguments");
    Ok(Some(path))
}
