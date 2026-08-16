use anyhow::{Context as _, Result, ensure};
use hd_runtime::{AdbClient, AdbError, MAX_ANDROID_BUGREPORT_BYTES, sha256_file};
use serde_json::json;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.get(2).and_then(|value| value.to_str()) == Some("bugreport") {
        let output = args.get(3).context("fake ADB bugreport output path")?;
        if output.to_string_lossy().contains("oversized") {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(output)
                .context("open oversized fake bugreport")?;
            file.set_len(MAX_ANDROID_BUGREPORT_BYTES + 1)
                .context("size oversized fake bugreport")?;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            return Ok(());
        }
        std::fs::write(output, minimal_bugreport_zip()).context("write fake ADB bugreport")?;
        return Ok(());
    }

    let temporary = std::env::temp_dir()
        .canonicalize()
        .context("canonicalize temporary directory")?
        .join(format!(
            "hd-android-bugreport-product-smoke-{}",
            Uuid::new_v4()
        ));
    hd_platform::ensure_owner_only_directory(&temporary)?;
    let output = temporary.join("android-bugreport-smoke.zip");
    let executable = std::env::current_exe().context("resolve smoke executable")?;
    let adb = AdbClient::new(executable, None);
    let result = adb
        .collect_android_bugreport("127.0.0.1:6520", &output)
        .await;
    let evidence = match result {
        Ok(size_bytes) => {
            ensure!(size_bytes > 22, "bugreport ZIP is unexpectedly empty");
            ensure!(
                size_bytes <= MAX_ANDROID_BUGREPORT_BYTES,
                "bugreport exceeded product bound"
            );
            let metadata = std::fs::symlink_metadata(&output)?;
            ensure!(
                metadata.file_type().is_file(),
                "bugreport is not a regular file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                ensure!(
                    metadata.permissions().mode() & 0o777 == 0o600,
                    "bugreport is not owner-only"
                );
            }
            let oversized = temporary.join("android-bugreport-oversized.zip");
            ensure!(
                matches!(
                    adb.collect_android_bugreport("127.0.0.1:6520", &oversized)
                        .await,
                    Err(AdbError::BugreportSize(size)) if size > MAX_ANDROID_BUGREPORT_BYTES
                ),
                "oversized bugreport was not terminated at the product boundary"
            );
            let _ = std::fs::remove_file(&oversized);
            json!({
                "schema_version": 1,
                "status": "pass",
                "typed_fixed_adb_operation": true,
                "caller_path_surface": false,
                "max_bytes": MAX_ANDROID_BUGREPORT_BYTES,
                "size_bytes": size_bytes,
                "sha256": sha256_file(&output)?,
                "zip_local_header": true,
                "zip_end_of_central_directory": true,
                "owner_only": true
                ,"oversized_generation_terminated": true
            })
        }
        Err(error) => return Err(error).context("collect bounded Android bugreport"),
    };
    std::fs::remove_dir_all(&temporary).context("remove bugreport smoke directory")?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    Ok(())
}

fn minimal_bugreport_zip() -> Vec<u8> {
    let name = b"bugreport-test.txt";
    let mut bytes = Vec::new();
    push_u32(&mut bytes, 0x0403_4b50);
    push_u16(&mut bytes, 20);
    for _ in 0..4 {
        push_u16(&mut bytes, 0);
    }
    for _ in 0..3 {
        push_u32(&mut bytes, 0);
    }
    push_u16(&mut bytes, u16::try_from(name.len()).unwrap_or(0));
    push_u16(&mut bytes, 0);
    bytes.extend_from_slice(name);
    let central_offset = u32::try_from(bytes.len()).unwrap_or(0);
    push_u32(&mut bytes, 0x0201_4b50);
    push_u16(&mut bytes, 20);
    push_u16(&mut bytes, 20);
    for _ in 0..4 {
        push_u16(&mut bytes, 0);
    }
    for _ in 0..3 {
        push_u32(&mut bytes, 0);
    }
    push_u16(&mut bytes, u16::try_from(name.len()).unwrap_or(0));
    for _ in 0..4 {
        push_u16(&mut bytes, 0);
    }
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    bytes.extend_from_slice(name);
    let central_size = u32::try_from(bytes.len()).unwrap_or(0) - central_offset;
    push_u32(&mut bytes, 0x0605_4b50);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 1);
    push_u32(&mut bytes, central_size);
    push_u32(&mut bytes, central_offset);
    push_u16(&mut bytes, 0);
    bytes
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}
