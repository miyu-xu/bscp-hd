use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{
    FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

pub const MICRODROID_CONSOLE_CHALLENGE_TIMEOUT: Duration = Duration::from_secs(5);
const CONSOLE_TAIL_LIMIT_BYTES: u64 = 64 * 1024;
const REQUEST_PREFIX: &str = "HD_CONSOLE_CHALLENGE_V1";
const RESPONSE_PREFIX: &str = "HD_CONSOLE_RESPONSE_V1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MicrodroidConsoleChallengeReceiptV2 {
    pub schema_version: u16,
    pub challenge_id: Uuid,
    pub nonce_sha256: String,
    pub request_size_bytes: u16,
    #[serde(with = "time::serde::rfc3339")]
    pub sent_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub responded_at: Option<OffsetDateTime>,
    pub response_verified: bool,
    pub error_code: Option<&'static str>,
}

#[derive(Debug)]
pub struct MicrodroidConsoleChallengeChannel {
    writer: std::fs::File,
    input_path: PathBuf,
    output_path: PathBuf,
    audit_path: PathBuf,
    device: u64,
    inode: u64,
    used: bool,
}

impl MicrodroidConsoleChallengeChannel {
    pub fn create(
        input_path: impl Into<PathBuf>,
        output_path: impl Into<PathBuf>,
        audit_path: impl Into<PathBuf>,
    ) -> Result<Self, MicrodroidConsoleChallengeError> {
        let input_path = input_path.into();
        let output_path = output_path.into();
        let audit_path = audit_path.into();
        if input_path.parent().is_none()
            || input_path.parent() != output_path.parent()
            || input_path.parent() != audit_path.parent()
        {
            return Err(MicrodroidConsoleChallengeError::UnsafePath(input_path));
        }
        hd_platform::create_owner_only_fifo(&input_path)?;
        let writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&input_path)
            .map_err(|source| MicrodroidConsoleChallengeError::Io {
                operation: "open console challenge FIFO",
                path: input_path.clone(),
                source,
            })?;
        let metadata = writer
            .metadata()
            .map_err(|source| MicrodroidConsoleChallengeError::Io {
                operation: "inspect console challenge FIFO",
                path: input_path.clone(),
                source,
            })?;
        if !metadata.file_type().is_fifo() || metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(MicrodroidConsoleChallengeError::UnsafePath(input_path));
        }
        Ok(Self {
            writer,
            input_path,
            output_path,
            audit_path,
            device: metadata.dev(),
            inode: metadata.ino(),
            used: false,
        })
    }

    pub fn input_path(&self) -> &Path {
        &self.input_path
    }

    pub async fn send_and_verify(
        &mut self,
        challenge_id: Uuid,
        confirmed: bool,
        timeout: Duration,
    ) -> Result<MicrodroidConsoleChallengeReceiptV2, MicrodroidConsoleChallengeError> {
        if !confirmed {
            return Err(MicrodroidConsoleChallengeError::ConfirmationRequired);
        }
        if challenge_id.is_nil() {
            return Err(MicrodroidConsoleChallengeError::NilChallengeId);
        }
        if self.used {
            return Err(MicrodroidConsoleChallengeError::AlreadyUsed);
        }
        if timeout.is_zero() || timeout > MICRODROID_CONSOLE_CHALLENGE_TIMEOUT {
            return Err(MicrodroidConsoleChallengeError::InvalidTimeout);
        }

        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce)
            .map_err(|error| MicrodroidConsoleChallengeError::Random(error.to_string()))?;
        let nonce_hex = hex::encode(nonce);
        let nonce_sha256 = hex::encode(Sha256::digest(nonce));
        let request = format!("{REQUEST_PREFIX} {challenge_id} {nonce_hex}\n");
        let expected_response = format!("{RESPONSE_PREFIX} {challenge_id} {nonce_hex}");
        let request_size_bytes = u16::try_from(request.len())
            .map_err(|_| MicrodroidConsoleChallengeError::RequestTooLarge)?;
        self.used = true;
        let sent_at = OffsetDateTime::now_utc();
        self.writer
            .write_all(request.as_bytes())
            .map_err(|source| MicrodroidConsoleChallengeError::Io {
                operation: "write console challenge",
                path: self.input_path.clone(),
                source,
            })?;
        self.writer
            .flush()
            .map_err(|source| MicrodroidConsoleChallengeError::Io {
                operation: "flush console challenge",
                path: self.input_path.clone(),
                source,
            })?;

        let started = Instant::now();
        loop {
            if console_tail_contains(&self.output_path, &expected_response)? {
                let receipt = MicrodroidConsoleChallengeReceiptV2 {
                    schema_version: 2,
                    challenge_id,
                    nonce_sha256,
                    request_size_bytes,
                    sent_at,
                    responded_at: Some(OffsetDateTime::now_utc()),
                    response_verified: true,
                    error_code: None,
                };
                self.write_audit(&receipt)?;
                return Ok(receipt);
            }
            if started.elapsed() >= timeout {
                let receipt = MicrodroidConsoleChallengeReceiptV2 {
                    schema_version: 2,
                    challenge_id,
                    nonce_sha256,
                    request_size_bytes,
                    sent_at,
                    responded_at: None,
                    response_verified: false,
                    error_code: Some("microdroid_console_challenge_timeout"),
                };
                self.write_audit(&receipt)?;
                return Err(MicrodroidConsoleChallengeError::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn write_audit(
        &self,
        receipt: &MicrodroidConsoleChallengeReceiptV2,
    ) -> Result<(), MicrodroidConsoleChallengeError> {
        if self.audit_path.exists() {
            return Err(MicrodroidConsoleChallengeError::AuditAlreadyExists(
                self.audit_path.clone(),
            ));
        }
        let bytes = serde_json::to_vec_pretty(receipt)?;
        hd_platform::write_owner_only(&self.audit_path, &bytes)?;
        Ok(())
    }

    fn remove_owned_fifo(&self) -> Result<(), MicrodroidConsoleChallengeError> {
        let metadata = match std::fs::symlink_metadata(&self.input_path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(MicrodroidConsoleChallengeError::Io {
                    operation: "inspect console challenge FIFO during cleanup",
                    path: self.input_path.clone(),
                    source,
                });
            }
        };
        if !metadata.file_type().is_fifo()
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
        {
            return Err(MicrodroidConsoleChallengeError::UnsafePath(
                self.input_path.clone(),
            ));
        }
        std::fs::remove_file(&self.input_path).map_err(|source| {
            MicrodroidConsoleChallengeError::Io {
                operation: "remove console challenge FIFO",
                path: self.input_path.clone(),
                source,
            }
        })?;
        Ok(())
    }
}

impl Drop for MicrodroidConsoleChallengeChannel {
    fn drop(&mut self) {
        if let Err(error) = self.remove_owned_fifo() {
            tracing::warn!(
                event = "microdroid.console_challenge.cleanup.failed",
                path = %self.input_path.display(),
                %error,
                "failed to remove the owned Microdroid console challenge FIFO"
            );
        }
    }
}

fn console_tail_contains(
    path: &Path,
    expected_line: &str,
) -> Result<bool, MicrodroidConsoleChallengeError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(MicrodroidConsoleChallengeError::Io {
                operation: "inspect Microdroid console output",
                path: path.to_owned(),
                source,
            });
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(MicrodroidConsoleChallengeError::UnsafePath(path.to_owned()));
    }
    let mut file = hd_platform::open_regular_read_nofollow(path)?;
    let opened_metadata =
        file.metadata()
            .map_err(|source| MicrodroidConsoleChallengeError::Io {
                operation: "inspect opened Microdroid console output",
                path: path.to_owned(),
                source,
            })?;
    let offset = opened_metadata
        .len()
        .saturating_sub(CONSOLE_TAIL_LIMIT_BYTES);
    file.seek(SeekFrom::Start(offset))
        .map_err(|source| MicrodroidConsoleChallengeError::Io {
            operation: "seek Microdroid console output",
            path: path.to_owned(),
            source,
        })?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(opened_metadata.len().saturating_sub(offset)).unwrap_or(64 * 1024),
    );
    file.take(CONSOLE_TAIL_LIMIT_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|source| MicrodroidConsoleChallengeError::Io {
            operation: "read Microdroid console output",
            path: path.to_owned(),
            source,
        })?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .any(|line| line.trim_end_matches('\r') == expected_line))
}

#[derive(Debug, thiserror::Error)]
pub enum MicrodroidConsoleChallengeError {
    #[error("explicit confirmation is required for a Microdroid console challenge")]
    ConfirmationRequired,
    #[error("Microdroid console challenge id must not be nil")]
    NilChallengeId,
    #[error("only one Microdroid console challenge is allowed per run")]
    AlreadyUsed,
    #[error("Microdroid console challenge timeout must be within the product bound")]
    InvalidTimeout,
    #[error("Microdroid console challenge request exceeded its fixed frame bound")]
    RequestTooLarge,
    #[error("Microdroid console challenge timed out before an exact Guest response")]
    Timeout,
    #[error("secure random generation failed: {0}")]
    Random(String),
    #[error("Microdroid console challenge path is unsafe: {0}")]
    UnsafePath(PathBuf),
    #[error("Microdroid console challenge audit already exists: {0}")]
    AuditAlreadyExists(PathBuf),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl MicrodroidConsoleChallengeError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ConfirmationRequired | Self::NilChallengeId | Self::InvalidTimeout => {
                "microdroid_console_challenge_invalid"
            }
            Self::AlreadyUsed => "microdroid_console_challenge_already_sent",
            Self::Timeout => "microdroid_console_challenge_timeout",
            Self::RequestTooLarge => "microdroid_console_challenge_frame_too_large",
            Self::Random(_) => "secure_random",
            Self::UnsafePath(_) => "microdroid_console_challenge_unsafe_path",
            Self::AuditAlreadyExists(_) => "microdroid_console_challenge_audit_exists",
            Self::Io { .. } | Self::Platform(_) | Self::Json(_) => {
                "microdroid_console_challenge_io"
            }
        }
    }
}
