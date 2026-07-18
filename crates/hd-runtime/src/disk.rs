use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hd_platform::{DiskProvisionMethod, DiskProvisionResult, DiskProvisioner, PlatformError};

#[derive(Debug, Default, Clone, Copy)]
pub struct NativeDiskProvisioner;

#[async_trait]
impl DiskProvisioner for NativeDiskProvisioner {
    async fn provision_full_copy(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<DiskProvisionResult, PlatformError> {
        let source = source.to_owned();
        let destination = destination.to_owned();
        tokio::task::spawn_blocking(move || provision(&source, &destination))
            .await
            .map_err(|error| PlatformError::Process(error.to_string()))?
    }
}

fn provision(source: &Path, destination: &Path) -> Result<DiskProvisionResult, PlatformError> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|source_error| PlatformError::Io {
            operation: "create disk directory",
            path: parent.to_owned(),
            source: source_error,
        })?;
    }
    let partial = destination.with_extension("img.partial");
    if partial.exists() {
        std::fs::remove_file(&partial).map_err(|source_error| PlatformError::Io {
            operation: "remove stale partial disk",
            path: partial.clone(),
            source: source_error,
        })?;
    }
    let result = (|| {
        let copy_result =
            reflink_copy::reflink_or_copy(source, &partial).map_err(|source_error| {
                PlatformError::Io {
                    operation: "provision private disk",
                    path: partial.clone(),
                    source: source_error,
                }
            })?;
        let source_bytes = std::fs::metadata(source)
            .map_err(|source_error| PlatformError::Io {
                operation: "read source disk metadata",
                path: source.to_owned(),
                source: source_error,
            })?
            .len();
        let bytes = std::fs::metadata(&partial)
            .map_err(|source_error| PlatformError::Io {
                operation: "read provisioned disk metadata",
                path: partial.clone(),
                source: source_error,
            })?
            .len();
        if bytes != source_bytes {
            return Err(PlatformError::Process(format!(
                "private disk size mismatch: source={source_bytes}, destination={bytes}"
            )));
        }
        std::fs::rename(&partial, destination).map_err(|source_error| PlatformError::Io {
            operation: "commit private disk",
            path: destination.to_owned(),
            source: source_error,
        })?;
        Ok(DiskProvisionResult {
            path: destination.to_owned(),
            method: if copy_result.is_none() {
                DiskProvisionMethod::BlockClone
            } else {
                DiskProvisionMethod::FullCopyFallback
            },
            bytes,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(partial);
    }
    result
}

pub fn unique_disk_path(root: &Path, instance_id: uuid::Uuid) -> PathBuf {
    root.join(instance_id.to_string())
        .join("aggregate_android.img")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn provisioned_disk_has_independent_path_and_contents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source.img");
        let destination = dir.path().join("instance/disk.img");
        std::fs::write(&source, b"base-image").expect("source");
        let result = NativeDiskProvisioner
            .provision_full_copy(&source, &destination)
            .await
            .expect("provision");
        assert_ne!(source, result.path);
        assert_eq!(std::fs::read(destination).expect("read"), b"base-image");
    }
}
