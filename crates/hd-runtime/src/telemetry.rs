use hd_core::GpuStatsEventV1;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

const MAX_EVENT_BYTES: usize = 64 * 1024;

pub async fn run_gpu_stats_listener(
    endpoint: String,
    sender: mpsc::Sender<GpuStatsEventV1>,
) -> Result<(), TelemetryError> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        loop {
            let server = ServerOptions::new()
                .first_pipe_instance(false)
                .reject_remote_clients(true)
                .create(&endpoint)
                .map_err(TelemetryError::Io)?;
            server.connect().await.map_err(TelemetryError::Io)?;
            let mut lines = BufReader::new(server).lines();
            while let Some(line) = lines.next_line().await.map_err(TelemetryError::Io)? {
                decode_and_send(&line, &sender).await?;
            }
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use tokio::net::UnixListener;

        let path = PathBuf::from(&endpoint);
        if path.exists() {
            std::fs::remove_file(&path).map_err(TelemetryError::Io)?;
        }
        let listener = UnixListener::bind(&path).map_err(TelemetryError::Io)?;
        let _path_guard = TelemetrySocketPathGuard(path.clone());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(TelemetryError::Io)?;
        loop {
            let (stream, _) = listener.accept().await.map_err(TelemetryError::Io)?;
            let mut lines = BufReader::new(stream).lines();
            while let Some(line) = lines.next_line().await.map_err(TelemetryError::Io)? {
                decode_and_send(&line, &sender).await?;
            }
        }
    }
}

#[cfg(not(windows))]
struct TelemetrySocketPathGuard(std::path::PathBuf);

#[cfg(not(windows))]
impl Drop for TelemetrySocketPathGuard {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.0.display(), %error, "failed to remove GPU telemetry socket");
        }
    }
}

async fn decode_and_send(
    line: &str,
    sender: &mpsc::Sender<GpuStatsEventV1>,
) -> Result<(), TelemetryError> {
    if line.len() > MAX_EVENT_BYTES {
        return Err(TelemetryError::TooLarge(line.len()));
    }
    let event = serde_json::from_str(line).map_err(TelemetryError::Decode)?;
    sender
        .send(event)
        .await
        .map_err(|_| TelemetryError::ReceiverClosed)
}

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("GPU telemetry I/O failed: {0}")]
    Io(std::io::Error),
    #[error("GPU telemetry JSON is invalid: {0}")]
    Decode(serde_json::Error),
    #[error("GPU telemetry event exceeds limit: {0} bytes")]
    TooLarge(usize),
    #[error("GPU telemetry receiver closed")]
    ReceiverClosed,
}
