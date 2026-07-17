use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hd_platform::{DiskProvisionMethodV2, DiskProvisionResultV2, DiskProvisioner, PlatformError};

#[derive(Debug, Clone, Copy, Default)]
pub struct NativeDiskProvisioner;

#[async_trait]
impl DiskProvisioner for NativeDiskProvisioner {
    async fn provision(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<DiskProvisionResultV2, PlatformError> {
        let source = source.to_owned();
        let destination = destination.to_owned();
        tokio::task::spawn_blocking(move || provision(&source, &destination))
            .await
            .map_err(|error| PlatformError::Process(format!("disk task join failed: {error}")))?
    }
}

fn provision(source: &Path, destination: &Path) -> Result<DiskProvisionResultV2, PlatformError> {
    let source_metadata = regular_file_metadata(source, "read base disk metadata")?;
    if destination.exists() {
        let destination_metadata = regular_file_metadata(destination, "read overlay metadata")?;
        if destination_metadata.len() != source_metadata.len() {
            return Err(PlatformError::Process(format!(
                "existing overlay {} has {} bytes, expected {}",
                destination.display(),
                destination_metadata.len(),
                source_metadata.len()
            )));
        }
        return Ok(DiskProvisionResultV2 {
            path: destination.to_owned(),
            method: DiskProvisionMethodV2::ExistingVerified,
            bytes: destination_metadata.len(),
        });
    }
    let parent = destination.parent().ok_or_else(|| {
        PlatformError::Process(format!("overlay has no parent: {}", destination.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(|source_error| PlatformError::Io {
        operation: "create overlay directory",
        path: parent.to_owned(),
        source: source_error,
    })?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("overlay"),
        uuid::Uuid::new_v4()
    ));
    let mut temporary_guard = TemporaryDisk::new(temporary.clone());
    let method = if reflink_copy::reflink(source, &temporary).is_ok() {
        DiskProvisionMethodV2::BlockClone
    } else {
        let available = fs2::available_space(parent).map_err(|source_error| PlatformError::Io {
            operation: "query overlay free space",
            path: parent.to_owned(),
            source: source_error,
        })?;
        if available < source_metadata.len().saturating_add(1024 * 1024 * 1024) {
            return Err(PlatformError::Process(format!(
                "insufficient disk space for {} byte overlay; {} bytes available",
                source_metadata.len(),
                available
            )));
        }
        std::fs::copy(source, &temporary).map_err(|source_error| PlatformError::Io {
            operation: "copy private overlay",
            path: temporary.clone(),
            source: source_error,
        })?;
        DiskProvisionMethodV2::FullCopy
    };
    let temporary_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary)
        .map_err(|source_error| PlatformError::Io {
            operation: "open temporary overlay",
            path: temporary.clone(),
            source: source_error,
        })?;
    temporary_file
        .sync_all()
        .map_err(|source_error| PlatformError::Io {
            operation: "sync temporary overlay",
            path: temporary.clone(),
            source: source_error,
        })?;
    drop(temporary_file);
    let copied_metadata = regular_file_metadata(&temporary, "verify temporary overlay")?;
    if copied_metadata.len() != source_metadata.len() {
        let _ = std::fs::remove_file(&temporary);
        return Err(PlatformError::Process(format!(
            "overlay copy length mismatch: expected {}, actual {}",
            source_metadata.len(),
            copied_metadata.len()
        )));
    }
    hd_platform::replace_file(&temporary, destination)?;
    temporary_guard.disarm();
    Ok(DiskProvisionResultV2 {
        path: destination.to_owned(),
        method,
        bytes: copied_metadata.len(),
    })
}

#[derive(Debug)]
struct TemporaryDisk {
    path: PathBuf,
    armed: bool,
}

impl TemporaryDisk {
    const fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryDisk {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn regular_file_metadata(
    path: &Path,
    operation: &'static str,
) -> Result<std::fs::Metadata, PlatformError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| PlatformError::Io {
        operation,
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(PlatformError::Process(format!(
            "{} is not a regular non-symlink file",
            path.display()
        )));
    }
    Ok(metadata)
}

pub fn unique_disk_path(root: &Path, instance_id: uuid::Uuid) -> PathBuf {
    root.join(format!("{instance_id}.img"))
}
