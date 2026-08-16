use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use hd_runtime::inspect_microdroid_payload_apk;
use uuid::Uuid;
use zip::CompressionMethod;
use zip::write::FileOptions;

const CONFIG_PATH: &str = "assets/vm_config.json";
const APK_SIGNATURE_SCHEME_V3_BLOCK_ID: u32 = 0xf053_68c0;
const APK_SIGNING_BLOCK_MAGIC: &[u8; 16] = b"APK Sig Block 42";

#[tokio::main]
async fn main() -> Result<()> {
    let system_temp = std::fs::canonicalize(std::env::temp_dir())
        .context("canonicalize the system temporary directory")?;
    let root = system_temp.join(format!(
        "hd-microdroid-payload-inspection-{}",
        Uuid::new_v4()
    ));
    hd_platform::ensure_owner_only_directory(&root)?;
    let _guard = TempRoot(root.clone());

    let stored = write_payload(
        &root,
        "stored.apk",
        br#"{"apexes":[],"extra_apks":[{"path":"first.apk"},{"path":"second.apk"}],"task":{"type":"MicrodroidLauncher","command":"/bin/payload"}}"#,
        CompressionMethod::Stored,
    )?;
    let stored_inspection = inspect_microdroid_payload_apk(&stored).await?;
    ensure!(
        stored_inspection.declared_extra_apk_count == 2,
        "stored Payload config did not report two extra APKs"
    );

    let deflated = write_payload(
        &root,
        "deflated.apk",
        br#"{"apexes":[],"task":{"type":"MicrodroidLauncher","command":"/bin/payload"}}"#,
        CompressionMethod::Deflated,
    )?;
    let deflated_inspection = inspect_microdroid_payload_apk(&deflated).await?;
    ensure!(
        deflated_inspection.declared_extra_apk_count == 0,
        "deflated Payload config did not default to zero extra APKs"
    );

    let malformed = write_payload(
        &root,
        "malformed.apk",
        br#"{"extra_apks":["#,
        CompressionMethod::Stored,
    )?;
    assert_rejected(&malformed, "decode assets/vm_config.json").await?;

    let too_many_config = serde_json::json!({
        "apexes": [],
        "extra_apks": (0..9)
            .map(|index| serde_json::json!({"path": format!("extra-{index}.apk")}))
            .collect::<Vec<_>>(),
    });
    let too_many = write_payload(
        &root,
        "too-many.apk",
        &serde_json::to_vec(&too_many_config)?,
        CompressionMethod::Deflated,
    )?;
    assert_rejected(&too_many, "supports at most 8").await?;

    let empty_path = write_payload(
        &root,
        "empty-path.apk",
        br#"{"apexes":[],"extra_apks":[{"path":""}]}"#,
        CompressionMethod::Stored,
    )?;
    assert_rejected(&empty_path, "empty extra APK path").await?;

    let extra_apex = write_payload(
        &root,
        "extra-apex.apk",
        br#"{"apexes":[{"name":"com.example.untrusted"}]}"#,
        CompressionMethod::Stored,
    )?;
    assert_rejected(
        &extra_apex,
        "unsupported APEX modules: com.example.untrusted",
    )
    .await?;

    let staged_apex = write_payload(
        &root,
        "staged-apex.apk",
        br#"{"apexes":[],"prefer_staged":true}"#,
        CompressionMethod::Stored,
    )?;
    assert_rejected(&staged_apex, "does not use Host-staged APEX state").await?;

    let hugepages = write_payload(
        &root,
        "hugepages.apk",
        br#"{"apexes":[],"hugepages":true}"#,
        CompressionMethod::Stored,
    )?;
    assert_rejected(&hugepages, "no effective macOS crosvm implementation").await?;

    println!(
        "Microdroid Payload inspection smoke passed: stored/deflated counts and bounded APK/APEX/platform negative configs"
    );
    Ok(())
}

async fn assert_rejected(path: &Path, expected: &str) -> Result<()> {
    let error = inspect_microdroid_payload_apk(path)
        .await
        .expect_err("negative Microdroid Payload fixture must be rejected")
        .to_string();
    ensure!(
        error.contains(expected),
        "unexpected rejection for {}: {error}",
        path.display()
    );
    Ok(())
}

fn write_payload(
    root: &Path,
    name: &str,
    config: &[u8],
    compression: CompressionMethod,
) -> Result<PathBuf> {
    let cursor = Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    archive.start_file(
        CONFIG_PATH,
        FileOptions::default().compression_method(compression),
    )?;
    archive.write_all(config)?;
    let bytes = archive.finish()?.into_inner();
    let signed = insert_v3_signing_block(bytes)?;
    let path = root.join(name);
    hd_platform::write_owner_only(&path, &signed)?;
    Ok(path)
}

fn insert_v3_signing_block(mut apk: Vec<u8>) -> Result<Vec<u8>> {
    let eocd = apk
        .windows(4)
        .rposition(|window| window == [0x50, 0x4b, 0x05, 0x06])
        .context("fixture ZIP has no end-of-central-directory record")?;
    ensure!(
        apk.len() >= eocd + 22,
        "fixture ZIP end record is truncated"
    );
    let directory_offset = usize::try_from(u32::from_le_bytes(
        apk[eocd + 16..eocd + 20]
            .try_into()
            .expect("EOCD directory offset has four bytes"),
    ))?;
    ensure!(
        directory_offset <= eocd,
        "fixture ZIP directory offset is invalid"
    );

    let pair_value_bytes = 4_u64;
    let block_size = 8 + pair_value_bytes + 24;
    let mut signing_block = Vec::with_capacity(44);
    signing_block.extend_from_slice(&block_size.to_le_bytes());
    signing_block.extend_from_slice(&pair_value_bytes.to_le_bytes());
    signing_block.extend_from_slice(&APK_SIGNATURE_SCHEME_V3_BLOCK_ID.to_le_bytes());
    signing_block.extend_from_slice(&block_size.to_le_bytes());
    signing_block.extend_from_slice(APK_SIGNING_BLOCK_MAGIC);
    ensure!(
        signing_block.len() == 44,
        "fixture APK signing block has an unexpected size"
    );

    apk.splice(directory_offset..directory_offset, signing_block);
    let shifted_eocd = eocd + 44;
    let shifted_directory = u32::try_from(directory_offset + 44)?;
    apk[shifted_eocd + 16..shifted_eocd + 20].copy_from_slice(&shifted_directory.to_le_bytes());
    Ok(apk)
}

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
