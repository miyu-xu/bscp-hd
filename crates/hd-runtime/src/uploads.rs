use std::io::Read as _;
use std::path::{Path, PathBuf};

use hd_core::UploadRecordV2;
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _, SeekFrom};
use uuid::Uuid;

const MAX_APK_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ZIP_COMMENT_BYTES: u64 = u16::MAX as u64;
const END_OF_CENTRAL_DIRECTORY_BYTES: u64 = 22;
const MAX_CENTRAL_DIRECTORY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ZIP_ENTRY_NAME_BYTES: usize = 4096;
const APK_SIGNING_BLOCK_FOOTER_LEN: usize = 24;
const APK_SIGNING_BLOCK_FOOTER_BYTES: u64 = 24;
const MAX_APK_SIGNING_BLOCK_BYTES: u64 = 64 * 1024 * 1024;
const APK_SIGNING_BLOCK_MAGIC: &[u8; 16] = b"APK Sig Block 42";
const APK_SIGNATURE_SCHEME_V3_BLOCK_ID: u32 = 0xf053_68c0;
const APK_SIGNATURE_SCHEME_V31_BLOCK_ID: u32 = 0x1b93_ad61;
const MAX_MICRODROID_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_MICRODROID_CONFIG_CAPACITY: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicrodroidPayloadInspectionV2 {
    pub declared_extra_apk_count: u8,
}

#[derive(Debug, serde::Deserialize)]
struct MicrodroidPayloadConfigInspection {
    #[serde(default)]
    apexes: Vec<MicrodroidApexInspection>,
    #[serde(default)]
    prefer_staged: bool,
    #[serde(default)]
    extra_apks: Vec<MicrodroidExtraApkInspection>,
    #[serde(default)]
    hugepages: bool,
}

#[derive(Debug, serde::Deserialize)]
struct MicrodroidApexInspection {
    name: String,
}

#[derive(Debug, serde::Deserialize)]
struct MicrodroidExtraApkInspection {
    path: String,
}

pub async fn store_apk_upload<R>(
    paths: &hd_platform::DataPaths,
    store: &crate::PersistentStore,
    file_name: &str,
    expected_sha256: &str,
    mut reader: R,
) -> Result<UploadRecordV2, UploadError>
where
    R: AsyncRead + Unpin,
{
    validate_file_name(file_name)?;
    validate_digest(expected_sha256)?;
    std::fs::create_dir_all(&paths.uploads).map_err(|source| UploadError::Io {
        operation: "create upload directory",
        path: paths.uploads.clone(),
        source,
    })?;
    let id = Uuid::new_v4();
    let destination = paths.upload_path(id);
    let temporary = paths.uploads.join(format!(".{id}.uploading"));
    let mut guard = UploadGuard(Some(temporary.clone()));
    hd_platform::write_owner_only(&temporary, &[])?;
    let mut output = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .open(&temporary)
        .await
        .map_err(|source| UploadError::Io {
            operation: "open upload staging file",
            path: temporary.clone(),
            source,
        })?;
    let mut hash = Sha256::new();
    let mut total = 0_u64;
    let mut prefix = Vec::with_capacity(4);
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(UploadError::ReadBody)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_APK_BYTES {
            return Err(UploadError::TooLarge(total));
        }
        if prefix.len() < 4 {
            let needed = 4 - prefix.len();
            prefix.extend_from_slice(&buffer[..read.min(needed)]);
        }
        hash.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .await
            .map_err(|source| UploadError::Io {
                operation: "write upload staging file",
                path: temporary.clone(),
                source,
            })?;
    }
    if total < 4 || prefix != [b'P', b'K', 3, 4] {
        return Err(UploadError::NotApk);
    }
    let actual_sha256 = hex::encode(hash.finalize());
    if actual_sha256 != expected_sha256 {
        return Err(UploadError::DigestMismatch {
            expected: expected_sha256.to_owned(),
            actual: actual_sha256,
        });
    }
    output.flush().await.map_err(|source| UploadError::Io {
        operation: "flush upload staging file",
        path: temporary.clone(),
        source,
    })?;
    output.sync_all().await.map_err(|source| UploadError::Io {
        operation: "sync upload staging file",
        path: temporary.clone(),
        source,
    })?;
    validate_apk_structure(&mut output, &temporary, total).await?;
    drop(output);
    hd_platform::replace_file(&temporary, &destination)?;
    guard.disarm();
    let mut destination_guard = UploadGuard(Some(destination.clone()));
    let upload = UploadRecordV2 {
        id,
        file_name: file_name.to_owned(),
        path: destination,
        sha256: expected_sha256.to_owned(),
        size_bytes: total,
        created_at: OffsetDateTime::now_utc(),
    };
    store.put_upload(&upload)?;
    destination_guard.disarm();
    Ok(upload)
}

async fn validate_apk_structure(
    file: &mut tokio::fs::File,
    path: &Path,
    size: u64,
) -> Result<(), UploadError> {
    let directory = read_zip_directory(file, path, size).await?;
    let manifest_offset = read_required_directory_entry(
        file,
        path,
        &directory,
        b"AndroidManifest.xml",
        "APK contains duplicate AndroidManifest.xml entries",
        "APK does not contain AndroidManifest.xml",
    )
    .await?;
    validate_manifest_local_header(file, path, manifest_offset, directory.offset).await
}

pub async fn validate_microdroid_payload_apk(path: &Path) -> Result<(), UploadError> {
    inspect_microdroid_payload_apk(path).await.map(|_| ())
}

pub async fn inspect_microdroid_payload_apk(
    path: &Path,
) -> Result<MicrodroidPayloadInspectionV2, UploadError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|source| upload_io("inspect Microdroid Payload APK", path, source))?;
    if !metadata.is_file() || metadata.len() > MAX_APK_BYTES {
        return Err(UploadError::InvalidApkStructure(
            "Microdroid Payload must be a regular APK within the upload limit",
        ));
    }
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|source| upload_io("open Microdroid Payload APK", path, source))?;
    let directory = read_zip_directory(&mut file, path, metadata.len()).await?;
    read_required_directory_entry(
        &mut file,
        path,
        &directory,
        b"assets/vm_config.json",
        "Microdroid Payload contains duplicate assets/vm_config.json entries",
        "Microdroid Payload APK does not contain assets/vm_config.json",
    )
    .await?;
    validate_apk_v3_signature_block(&mut file, path, directory.offset).await?;
    let config_path = path.to_owned();
    tokio::task::spawn_blocking(move || inspect_microdroid_payload_config(&config_path))
        .await
        .map_err(|error| {
            UploadError::InvalidMicrodroidPayloadConfig(format!(
                "config inspection task failed: {error}"
            ))
        })?
}

fn inspect_microdroid_payload_config(
    path: &Path,
) -> Result<MicrodroidPayloadInspectionV2, UploadError> {
    let source = std::fs::File::open(path)
        .map_err(|error| invalid_microdroid_config(format!("open APK: {error}")))?;
    let mut archive = zip::ZipArchive::new(source)
        .map_err(|error| invalid_microdroid_config(format!("open APK ZIP: {error}")))?;
    let mut config = archive.by_name("assets/vm_config.json").map_err(|error| {
        invalid_microdroid_config(format!("open assets/vm_config.json: {error}"))
    })?;
    if config.size() > MAX_MICRODROID_CONFIG_BYTES {
        return Err(invalid_microdroid_config(format!(
            "assets/vm_config.json exceeds the {MAX_MICRODROID_CONFIG_BYTES}-byte limit"
        )));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(config.size()).unwrap_or(MAX_MICRODROID_CONFIG_CAPACITY),
    );
    config
        .by_ref()
        .take(MAX_MICRODROID_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            invalid_microdroid_config(format!("read assets/vm_config.json: {error}"))
        })?;
    if bytes.len() as u64 > MAX_MICRODROID_CONFIG_BYTES {
        return Err(invalid_microdroid_config(format!(
            "assets/vm_config.json expands beyond the {MAX_MICRODROID_CONFIG_BYTES}-byte limit"
        )));
    }
    let config: MicrodroidPayloadConfigInspection =
        serde_json::from_slice(&bytes).map_err(|error| {
            invalid_microdroid_config(format!("decode assets/vm_config.json: {error}"))
        })?;
    if !config.apexes.is_empty() {
        let names = config
            .apexes
            .iter()
            .map(|apex| apex.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(invalid_microdroid_config(format!(
            "assets/vm_config.json requests unsupported APEX modules: {names}; HD only uses the signed runtime closure"
        )));
    }
    if config.prefer_staged {
        return Err(invalid_microdroid_config(
            "assets/vm_config.json requests prefer_staged APEX resolution; HD does not use Host-staged APEX state"
                .to_owned(),
        ));
    }
    if config.hugepages {
        return Err(invalid_microdroid_config(
            "assets/vm_config.json requests hugepages, which have no effective macOS crosvm implementation"
                .to_owned(),
        ));
    }
    if config.extra_apks.len() > hd_core::MAX_MICRODROID_EXTRA_APKS {
        return Err(invalid_microdroid_config(format!(
            "assets/vm_config.json declares {} extra APKs; HD supports at most {}",
            config.extra_apks.len(),
            hd_core::MAX_MICRODROID_EXTRA_APKS
        )));
    }
    if config.extra_apks.iter().any(|extra| extra.path.is_empty()) {
        return Err(invalid_microdroid_config(
            "assets/vm_config.json contains an empty extra APK path".to_owned(),
        ));
    }
    Ok(MicrodroidPayloadInspectionV2 {
        declared_extra_apk_count: u8::try_from(config.extra_apks.len())
            .expect("the Microdroid extra APK product limit fits in u8"),
    })
}

fn invalid_microdroid_config(message: String) -> UploadError {
    UploadError::InvalidMicrodroidPayloadConfig(message)
}

pub async fn validate_microdroid_extra_apk(path: &Path) -> Result<(), UploadError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|source| upload_io("inspect Microdroid extra APK", path, source))?;
    if !metadata.is_file() || metadata.len() > MAX_APK_BYTES {
        return Err(UploadError::InvalidApkStructure(
            "Microdroid extra APK must be a regular APK within the upload limit",
        ));
    }
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|source| upload_io("open Microdroid extra APK", path, source))?;
    let directory = read_zip_directory(&mut file, path, metadata.len()).await?;
    let manifest_offset = read_required_directory_entry(
        &mut file,
        path,
        &directory,
        b"AndroidManifest.xml",
        "Microdroid extra APK contains duplicate AndroidManifest.xml entries",
        "Microdroid extra APK does not contain AndroidManifest.xml",
    )
    .await?;
    validate_manifest_local_header(&mut file, path, manifest_offset, directory.offset).await?;
    validate_apk_v3_signature_block(&mut file, path, directory.offset).await
}

async fn validate_apk_v3_signature_block(
    file: &mut tokio::fs::File,
    path: &Path,
    directory_offset: u64,
) -> Result<(), UploadError> {
    let (pairs_offset, pairs_bytes) =
        apk_signing_pairs_region(file, path, directory_offset).await?;
    file.seek(SeekFrom::Start(pairs_offset))
        .await
        .map_err(|source| upload_io("seek APK signing pairs", path, source))?;
    let mut consumed = 0_u64;
    let mut has_v3 = false;
    while consumed < pairs_bytes {
        if pairs_bytes - consumed < 12 {
            return Err(UploadError::InvalidApkStructure(
                "APK signing block pair is truncated",
            ));
        }
        let mut length_bytes = [0_u8; 8];
        file.read_exact(&mut length_bytes)
            .await
            .map_err(|source| upload_io("read APK signing pair length", path, source))?;
        let value_bytes = read_u64(&length_bytes).ok_or(UploadError::InvalidApkStructure(
            "APK signing pair length is invalid",
        ))?;
        if value_bytes < 4 || value_bytes > pairs_bytes - consumed - 8 {
            return Err(UploadError::InvalidApkStructure(
                "APK signing pair escapes the signing block",
            ));
        }
        let mut id_bytes = [0_u8; 4];
        file.read_exact(&mut id_bytes)
            .await
            .map_err(|source| upload_io("read APK signing pair id", path, source))?;
        let id = read_u32(&id_bytes).ok_or(UploadError::InvalidApkStructure(
            "APK signing pair id is invalid",
        ))?;
        if matches!(
            id,
            APK_SIGNATURE_SCHEME_V3_BLOCK_ID | APK_SIGNATURE_SCHEME_V31_BLOCK_ID
        ) {
            if has_v3 {
                return Err(UploadError::InvalidApkStructure(
                    "APK contains duplicate v3 signing blocks",
                ));
            }
            has_v3 = true;
        }
        file.seek(SeekFrom::Current(i64::try_from(value_bytes - 4).map_err(
            |_| UploadError::InvalidApkStructure("APK signing pair offset overflows"),
        )?))
        .await
        .map_err(|source| upload_io("skip APK signing pair", path, source))?;
        consumed =
            consumed
                .checked_add(8 + value_bytes)
                .ok_or(UploadError::InvalidApkStructure(
                    "APK signing block pair length overflows",
                ))?;
    }
    if !has_v3 {
        return Err(UploadError::InvalidApkStructure(
            "Microdroid Payload requires an APK Signature Scheme v3 or v3.1 signing block",
        ));
    }
    Ok(())
}

async fn apk_signing_pairs_region(
    file: &mut tokio::fs::File,
    path: &Path,
    directory_offset: u64,
) -> Result<(u64, u64), UploadError> {
    if directory_offset < APK_SIGNING_BLOCK_FOOTER_BYTES + 8 {
        return Err(UploadError::InvalidApkStructure(
            "Microdroid Payload requires an APK Signature Scheme v3 or v3.1 signing block",
        ));
    }
    let footer_offset = directory_offset - APK_SIGNING_BLOCK_FOOTER_BYTES;
    file.seek(SeekFrom::Start(footer_offset))
        .await
        .map_err(|source| upload_io("seek APK signing block footer", path, source))?;
    let mut footer = [0_u8; APK_SIGNING_BLOCK_FOOTER_LEN];
    file.read_exact(&mut footer)
        .await
        .map_err(|source| upload_io("read APK signing block footer", path, source))?;
    if &footer[8..] != APK_SIGNING_BLOCK_MAGIC {
        return Err(UploadError::InvalidApkStructure(
            "Microdroid Payload requires an APK Signature Scheme v3 or v3.1 signing block",
        ));
    }
    let block_size = read_u64(&footer[..8]).ok_or(UploadError::InvalidApkStructure(
        "APK signing block size is invalid",
    ))?;
    let total_size = block_size
        .checked_add(8)
        .ok_or(UploadError::InvalidApkStructure(
            "APK signing block size overflows",
        ))?;
    if block_size < APK_SIGNING_BLOCK_FOOTER_BYTES
        || total_size > directory_offset
        || total_size > MAX_APK_SIGNING_BLOCK_BYTES
    {
        return Err(UploadError::InvalidApkStructure(
            "APK signing block is outside the accepted size contract",
        ));
    }
    let block_offset =
        directory_offset
            .checked_sub(total_size)
            .ok_or(UploadError::InvalidApkStructure(
                "APK signing block offset underflows",
            ))?;
    file.seek(SeekFrom::Start(block_offset))
        .await
        .map_err(|source| upload_io("seek APK signing block", path, source))?;
    let mut leading_size = [0_u8; 8];
    file.read_exact(&mut leading_size)
        .await
        .map_err(|source| upload_io("read APK signing block size", path, source))?;
    if read_u64(&leading_size) != Some(block_size) {
        return Err(UploadError::InvalidApkStructure(
            "APK signing block size fields do not match",
        ));
    }
    Ok((
        block_offset + 8,
        block_size - APK_SIGNING_BLOCK_FOOTER_BYTES,
    ))
}

#[derive(Debug, Clone, Copy)]
struct ZipDirectory {
    offset: u64,
    size: u64,
    entries: u16,
}

async fn read_zip_directory(
    file: &mut tokio::fs::File,
    path: &Path,
    size: u64,
) -> Result<ZipDirectory, UploadError> {
    if size < END_OF_CENTRAL_DIRECTORY_BYTES {
        return Err(UploadError::InvalidApkStructure(
            "ZIP end record is missing",
        ));
    }
    let tail_size = size.min(END_OF_CENTRAL_DIRECTORY_BYTES + MAX_ZIP_COMMENT_BYTES);
    file.seek(SeekFrom::Start(size - tail_size))
        .await
        .map_err(|source| upload_io("seek APK ZIP tail", path, source))?;
    let mut tail = vec![
        0_u8;
        usize::try_from(tail_size).map_err(|_| {
            UploadError::InvalidApkStructure("ZIP tail length does not fit in memory")
        })?
    ];
    file.read_exact(&mut tail)
        .await
        .map_err(|source| upload_io("read APK ZIP tail", path, source))?;
    let eocd_size = usize::try_from(END_OF_CENTRAL_DIRECTORY_BYTES)
        .map_err(|_| UploadError::InvalidApkStructure("ZIP end record size overflows"))?;
    let eocd_index = (0..=tail.len() - eocd_size)
        .rev()
        .find(|index| {
            tail[*index..].starts_with(&[0x50, 0x4b, 0x05, 0x06])
                && read_u16(&tail[*index + 20..*index + 22])
                    .is_some_and(|comment| *index + eocd_size + usize::from(comment) == tail.len())
        })
        .ok_or(UploadError::InvalidApkStructure(
            "ZIP end record is malformed",
        ))?;
    let eocd = &tail[eocd_index..eocd_index + eocd_size];
    let disk = read_u16(&eocd[4..6]).unwrap_or(u16::MAX);
    let directory_disk = read_u16(&eocd[6..8]).unwrap_or(u16::MAX);
    let entries_on_disk = read_u16(&eocd[8..10]).unwrap_or(u16::MAX);
    let entries = read_u16(&eocd[10..12]).unwrap_or(u16::MAX);
    let directory_size = u64::from(read_u32(&eocd[12..16]).unwrap_or(u32::MAX));
    let directory_offset = u64::from(read_u32(&eocd[16..20]).unwrap_or(u32::MAX));
    if disk != 0
        || directory_disk != 0
        || entries == 0
        || entries == u16::MAX
        || entries_on_disk != entries
        || directory_size == u64::from(u32::MAX)
        || directory_offset == u64::from(u32::MAX)
    {
        return Err(UploadError::InvalidApkStructure(
            "multi-disk or ZIP64 APK archives are not accepted",
        ));
    }
    if directory_size > MAX_CENTRAL_DIRECTORY_BYTES {
        return Err(UploadError::InvalidApkStructure(
            "ZIP central directory exceeds the validation limit",
        ));
    }
    let directory_end =
        directory_offset
            .checked_add(directory_size)
            .ok_or(UploadError::InvalidApkStructure(
                "ZIP central directory overflows",
            ))?;
    let eocd_offset = size - tail_size
        + u64::try_from(eocd_index)
            .map_err(|_| UploadError::InvalidApkStructure("ZIP end record offset overflows"))?;
    if directory_end > eocd_offset {
        return Err(UploadError::InvalidApkStructure(
            "ZIP central directory escapes the archive",
        ));
    }
    Ok(ZipDirectory {
        offset: directory_offset,
        size: directory_size,
        entries,
    })
}

async fn read_required_directory_entry(
    file: &mut tokio::fs::File,
    path: &Path,
    directory: &ZipDirectory,
    required_name: &[u8],
    duplicate_error: &'static str,
    missing_error: &'static str,
) -> Result<u64, UploadError> {
    file.seek(SeekFrom::Start(directory.offset))
        .await
        .map_err(|source| upload_io("seek APK central directory", path, source))?;
    let mut consumed = 0_u64;
    let mut required_local_offset = None;
    for _ in 0..directory.entries {
        let mut header = [0_u8; 46];
        file.read_exact(&mut header)
            .await
            .map_err(|source| upload_io("read APK central directory entry", path, source))?;
        if !header.starts_with(&[0x50, 0x4b, 0x01, 0x02]) {
            return Err(UploadError::InvalidApkStructure(
                "ZIP central directory signature is invalid",
            ));
        }
        let name_length = usize::from(read_u16(&header[28..30]).unwrap_or(u16::MAX));
        let extra_length = u64::from(read_u16(&header[30..32]).unwrap_or(u16::MAX));
        let comment_length = u64::from(read_u16(&header[32..34]).unwrap_or(u16::MAX));
        let start_disk = read_u16(&header[34..36]).unwrap_or(u16::MAX);
        let local_offset = read_u32(&header[42..46]).unwrap_or(u32::MAX);
        if name_length == 0 || name_length > MAX_ZIP_ENTRY_NAME_BYTES || start_disk != 0 {
            return Err(UploadError::InvalidApkStructure(
                "ZIP entry metadata is outside the APK contract",
            ));
        }
        let entry_size = 46_u64
            .checked_add(name_length as u64)
            .and_then(|value| value.checked_add(extra_length))
            .and_then(|value| value.checked_add(comment_length))
            .ok_or(UploadError::InvalidApkStructure(
                "ZIP entry length overflows",
            ))?;
        consumed = consumed
            .checked_add(entry_size)
            .ok_or(UploadError::InvalidApkStructure(
                "ZIP directory length overflows",
            ))?;
        if consumed > directory.size {
            return Err(UploadError::InvalidApkStructure(
                "ZIP entry escapes the central directory",
            ));
        }
        let mut name = vec![0_u8; name_length];
        file.read_exact(&mut name)
            .await
            .map_err(|source| upload_io("read APK ZIP entry name", path, source))?;
        file.seek(SeekFrom::Current(
            i64::try_from(extra_length + comment_length).map_err(|_| {
                UploadError::InvalidApkStructure("ZIP entry metadata offset overflows")
            })?,
        ))
        .await
        .map_err(|source| upload_io("skip APK ZIP entry metadata", path, source))?;
        if name == required_name
            && required_local_offset
                .replace(u64::from(local_offset))
                .is_some()
        {
            return Err(UploadError::InvalidApkStructure(duplicate_error));
        }
    }
    if consumed != directory.size {
        return Err(UploadError::InvalidApkStructure(
            "ZIP central directory size does not match its entries",
        ));
    }
    required_local_offset.ok_or(UploadError::InvalidApkStructure(missing_error))
}

async fn validate_manifest_local_header(
    file: &mut tokio::fs::File,
    path: &Path,
    manifest_offset: u64,
    directory_offset: u64,
) -> Result<(), UploadError> {
    if manifest_offset >= directory_offset {
        return Err(UploadError::InvalidApkStructure(
            "APK manifest local header is outside file data",
        ));
    }
    file.seek(SeekFrom::Start(manifest_offset))
        .await
        .map_err(|source| upload_io("seek APK manifest local header", path, source))?;
    let mut local_header = [0_u8; 30];
    file.read_exact(&mut local_header)
        .await
        .map_err(|source| upload_io("read APK manifest local header", path, source))?;
    if !local_header.starts_with(&[0x50, 0x4b, 0x03, 0x04]) {
        return Err(UploadError::InvalidApkStructure(
            "APK manifest local header signature is invalid",
        ));
    }
    let local_name_length = usize::from(read_u16(&local_header[26..28]).unwrap_or(u16::MAX));
    if local_name_length != b"AndroidManifest.xml".len() {
        return Err(UploadError::InvalidApkStructure(
            "APK manifest local name differs from the central directory",
        ));
    }
    let mut local_name = vec![0_u8; local_name_length];
    file.read_exact(&mut local_name)
        .await
        .map_err(|source| upload_io("read APK manifest local name", path, source))?;
    if local_name != b"AndroidManifest.xml" {
        return Err(UploadError::InvalidApkStructure(
            "APK manifest local name is invalid",
        ));
    }
    Ok(())
}

fn read_u16(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

fn upload_io(operation: &'static str, path: &Path, source: std::io::Error) -> UploadError {
    UploadError::Io {
        operation,
        path: path.to_owned(),
        source,
    }
}

fn validate_file_name(file_name: &str) -> Result<(), UploadError> {
    let path = Path::new(file_name);
    if file_name.is_empty()
        || file_name.len() > 255
        || path.file_name().and_then(|value| value.to_str()) != Some(file_name)
        || !file_name.to_ascii_lowercase().ends_with(".apk")
        || file_name.chars().any(char::is_control)
    {
        return Err(UploadError::InvalidFileName);
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), UploadError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(UploadError::InvalidDigest);
    }
    Ok(())
}

struct UploadGuard(Option<PathBuf>);

impl UploadGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("APK file name must be a basename ending in .apk")]
    InvalidFileName,
    #[error("X-Content-SHA256 must be a lowercase SHA-256 digest")]
    InvalidDigest,
    #[error("upload body read failed: {0}")]
    ReadBody(std::io::Error),
    #[error("APK upload exceeds the {MAX_APK_BYTES} byte limit: {0}")]
    TooLarge(u64),
    #[error("upload is not an APK ZIP archive")]
    NotApk,
    #[error("upload is not a structurally valid APK: {0}")]
    InvalidApkStructure(&'static str),
    #[error("Microdroid Payload config is invalid: {0}")]
    InvalidMicrodroidPayloadConfig(String),
    #[error("APK digest mismatch: expected {expected}, actual {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] crate::StoreError),
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_names_cannot_escape_the_upload_root() {
        assert!(validate_file_name("sample.apk").is_ok());
        assert!(validate_file_name("../sample.apk").is_err());
    }
}
