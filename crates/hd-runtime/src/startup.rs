use hd_core::HostStartupFailureV1;
use hd_platform::DataPaths;
use time::OffsetDateTime;
use uuid::Uuid;

const STARTUP_FAILURE_LIMIT: u64 = 64 * 1024;

pub fn write_host_startup_failure(
    paths: &DataPaths,
    failure: &HostStartupFailureV1,
) -> Result<(), String> {
    failure.validate()?;
    let bytes = serde_json::to_vec_pretty(failure)
        .map_err(|error| format!("encode Host startup failure: {error}"))?;
    hd_platform::write_owner_only(
        &paths.host_startup_failure(failure.startup_attempt_id),
        &bytes,
    )
    .map_err(|error| format!("write Host startup failure: {error}"))
}

pub fn read_host_startup_failure(
    paths: &DataPaths,
    startup_attempt_id: Uuid,
) -> Result<Option<HostStartupFailureV1>, String> {
    let path = paths.host_startup_failure(startup_attempt_id);
    match hd_platform::read_regular_nofollow_limited(&path, STARTUP_FAILURE_LIMIT) {
        Ok(bytes) => {
            let failure: HostStartupFailureV1 = serde_json::from_slice(&bytes)
                .map_err(|error| format!("decode Host startup failure: {error}"))?;
            failure.validate()?;
            Ok(Some(failure))
        }
        Err(hd_platform::PlatformError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(format!("read Host startup failure: {error}")),
    }
}

pub fn clear_host_startup_failure(
    paths: &DataPaths,
    startup_attempt_id: Uuid,
) -> Result<(), String> {
    let path = paths.host_startup_failure(startup_attempt_id);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => std::fs::remove_file(&path)
            .map_err(|error| format!("remove Host startup failure {}: {error}", path.display())),
        Ok(_) => Err(format!(
            "Host startup failure path is not a regular file: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "inspect Host startup failure {}: {error}",
            path.display()
        )),
    }
}

pub fn bounded_startup_failure_message(error: &anyhow::Error) -> String {
    const MAXIMUM: usize = 4096;
    let message = format!("{error:#}");
    if message.len() <= MAXIMUM {
        return message;
    }
    let mut boundary = MAXIMUM.saturating_sub("…".len());
    while !message.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    format!("{}…", &message[..boundary])
}

pub fn record_host_startup_failure(
    paths: &DataPaths,
    startup_attempt_id: Uuid,
    code: &str,
    error: &anyhow::Error,
) -> Result<(), String> {
    write_host_startup_failure(
        paths,
        &HostStartupFailureV1 {
            schema_version: HostStartupFailureV1::SCHEMA_VERSION,
            startup_attempt_id,
            code: code.to_owned(),
            message: bounded_startup_failure_message(error),
            occurred_at: OffsetDateTime::now_utc(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_failure_message_is_utf8_safe_and_bounded() {
        let error = anyhow::Error::msg("界".repeat(2_000));
        let message = bounded_startup_failure_message(&error);
        assert!(message.len() <= 4_096);
        assert!(message.ends_with('…'));
    }
}
