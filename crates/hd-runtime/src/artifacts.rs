use std::path::{Path, PathBuf};

use hd_core::{ArtifactConfig, DiagnosticCheckV1, DiagnosticStatus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactFingerprintV1 {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

pub async fn validate_artifacts(
    config: &ArtifactConfig,
) -> Result<Vec<ArtifactFingerprintV1>, ArtifactError> {
    let mut files = vec![
        ("kernel", config.kernel.clone()),
        ("initrd", config.initrd.clone()),
        ("rootfs", config.rootfs.clone()),
        ("android_fstab", config.android_fstab.clone()),
    ];
    if let Some(path) = &config.system_image {
        files.push(("system_image", path.clone()));
    }
    if let Some(path) = &config.vendor_image {
        files.push(("vendor_image", path.clone()));
    }

    let mut fingerprints = Vec::with_capacity(files.len());
    for (name, path) in files {
        let expected = config.expected_sha256.get(name).cloned();
        let path_for_hash = path.clone();
        let name_owned = name.to_owned();
        let fingerprint =
            tokio::task::spawn_blocking(move || fingerprint_file(&name_owned, &path_for_hash))
                .await
                .map_err(|error| ArtifactError::Worker(error.to_string()))??;
        if let Some(expected) = expected
            && !fingerprint.sha256.eq_ignore_ascii_case(expected.trim())
        {
            return Err(ArtifactError::ChecksumMismatch {
                name: name.to_owned(),
                expected,
                actual: fingerprint.sha256,
            });
        }
        fingerprints.push(fingerprint);
    }
    Ok(fingerprints)
}

pub fn artifact_diagnostics(config: &ArtifactConfig) -> Vec<DiagnosticCheckV1> {
    [
        ("kernel", &config.kernel),
        ("initrd", &config.initrd),
        ("rootfs", &config.rootfs),
        ("android_fstab", &config.android_fstab),
    ]
    .into_iter()
    .map(|(name, path)| match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() > 0 => DiagnosticCheckV1 {
            name: format!("artifact.{name}"),
            status: DiagnosticStatus::Pass,
            detail: format!("{} ({} bytes)", path.display(), metadata.len()),
        },
        Ok(_) => DiagnosticCheckV1 {
            name: format!("artifact.{name}"),
            status: DiagnosticStatus::Fail,
            detail: format!("{} is not a non-empty regular file", path.display()),
        },
        Err(error) => DiagnosticCheckV1 {
            name: format!("artifact.{name}"),
            status: DiagnosticStatus::Blocked,
            detail: format!("{}: {error}", path.display()),
        },
    })
    .collect()
}

fn fingerprint_file(name: &str, path: &Path) -> Result<ArtifactFingerprintV1, ArtifactError> {
    let metadata = std::fs::metadata(path).map_err(|source| ArtifactError::Io {
        operation: "read metadata",
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(ArtifactError::InvalidFile(path.to_owned()));
    }
    let mut file = std::fs::File::open(path).map_err(|source| ArtifactError::Io {
        operation: "open",
        path: path.to_owned(),
        source,
    })?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|source| ArtifactError::Io {
        operation: "hash",
        path: path.to_owned(),
        source,
    })?;
    Ok(ArtifactFingerprintV1 {
        name: name.to_owned(),
        path: path.to_owned(),
        bytes: metadata.len(),
        sha256: hex::encode(hasher.finalize()),
    })
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("artifact is not a non-empty regular file: {0}")]
    InvalidFile(PathBuf),
    #[error("checksum mismatch for {name}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    #[error("artifact validation worker failed: {0}")]
    Worker(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fingerprints_are_reproducible() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("artifact.bin");
        std::fs::write(&path, b"hd").expect("write");
        let first = fingerprint_file("test", &path).expect("fingerprint");
        let second = fingerprint_file("test", &path).expect("fingerprint");
        assert_eq!(first, second);
        assert_eq!(first.bytes, 2);
    }
}
