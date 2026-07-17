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
    let manifest_offset = read_manifest_directory_entry(file, path, &directory).await?;
    validate_manifest_local_header(file, path, manifest_offset, directory.offset).await
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

async fn read_manifest_directory_entry(
    file: &mut tokio::fs::File,
    path: &Path,
    directory: &ZipDirectory,
) -> Result<u64, UploadError> {
    file.seek(SeekFrom::Start(directory.offset))
        .await
        .map_err(|source| upload_io("seek APK central directory", path, source))?;
    let mut consumed = 0_u64;
    let mut manifest_local_offset = None;
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
        if name == b"AndroidManifest.xml"
            && manifest_local_offset
                .replace(u64::from(local_offset))
                .is_some()
        {
            return Err(UploadError::InvalidApkStructure(
                "APK contains duplicate AndroidManifest.xml entries",
            ));
        }
    }
    if consumed != directory.size {
        return Err(UploadError::InvalidApkStructure(
            "ZIP central directory size does not match its entries",
        ));
    }
    manifest_local_offset.ok_or(UploadError::InvalidApkStructure(
        "APK does not contain AndroidManifest.xml",
    ))
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
