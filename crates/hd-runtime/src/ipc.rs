#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;

use hd_core::{WorkerRequestV2, WorkerResponseV2};
use thiserror::Error;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::sync::watch;

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);

pub async fn run_worker_server<F, Fut>(
    endpoint: &str,
    handler: F,
    shutdown: watch::Receiver<bool>,
) -> Result<(), IpcError>
where
    F: Fn(WorkerRequestV2) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = WorkerResponseV2> + Send + 'static,
{
    #[cfg(windows)]
    {
        run_windows_server(endpoint, handler, shutdown).await
    }
    #[cfg(unix)]
    {
        run_unix_server(endpoint, handler, shutdown).await
    }
}

pub async fn send_worker_request(
    endpoint: &str,
    request: &WorkerRequestV2,
) -> Result<WorkerResponseV2, IpcError> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let mut last_error = None;
        for _ in 0..60 {
            match ClientOptions::new().open(endpoint) {
                Ok(mut client) => return timeout_exchange(&mut client, request).await,
                Err(error) => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        Err(IpcError::Connect {
            endpoint: endpoint.to_owned(),
            source: last_error.unwrap_or_else(|| std::io::Error::other("connection retry ended")),
        })
    }
    #[cfg(unix)]
    {
        let mut stream = tokio::net::UnixStream::connect(endpoint)
            .await
            .map_err(|source| IpcError::Connect {
                endpoint: endpoint.to_owned(),
                source,
            })?;
        timeout_exchange(&mut stream, request).await
    }
}

async fn timeout_exchange<S>(
    stream: &mut S,
    request: &WorkerRequestV2,
) -> Result<WorkerResponseV2, IpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let timeout = match &request.command {
        hd_core::WorkerCommandV2::Start { .. } => Duration::from_mins(8),
        hd_core::WorkerCommandV2::InstallApk { .. } => Duration::from_mins(6),
        hd_core::WorkerCommandV2::Stop {
            graceful_timeout_ms,
            ..
        } => {
            Duration::from_millis(u64::from(*graceful_timeout_ms)).saturating_add(EXCHANGE_TIMEOUT)
        }
        _ => EXCHANGE_TIMEOUT,
    };
    tokio::time::timeout(timeout, exchange(stream, request))
        .await
        .map_err(|_| IpcError::Timeout)?
}

async fn exchange<S>(
    stream: &mut S,
    request: &WorkerRequestV2,
) -> Result<WorkerResponseV2, IpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_message(stream, request).await?;
    let bytes = read_message(stream).await?;
    serde_json::from_slice(&bytes).map_err(IpcError::Decode)
}

async fn serve_connection<S, F, Fut>(mut stream: S, handler: F) -> Result<(), IpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Fn(WorkerRequestV2) -> Fut,
    Fut: std::future::Future<Output = WorkerResponseV2>,
{
    let bytes = read_message(&mut stream).await?;
    let request: WorkerRequestV2 = serde_json::from_slice(&bytes).map_err(IpcError::Decode)?;
    let response = handler(request).await;
    write_message(&mut stream, &response).await
}

async fn write_message<S, T>(stream: &mut S, value: &T) -> Result<(), IpcError>
where
    S: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut bytes = serde_json::to_vec(value).map_err(IpcError::Encode)?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(IpcError::MessageTooLarge(bytes.len()));
    }
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(IpcError::Io)?;
    stream.flush().await.map_err(IpcError::Io)
}

async fn read_message<S>(stream: &mut S) -> Result<Vec<u8>, IpcError>
where
    S: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut reader = BufReader::new(stream).take((MAX_MESSAGE_BYTES + 1) as u64);
    let read = reader
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(IpcError::Io)?;
    if read == 0 {
        return Err(IpcError::UnexpectedEof);
    }
    if bytes.len() > MAX_MESSAGE_BYTES + 1 || !bytes.ends_with(b"\n") {
        return Err(IpcError::MessageTooLarge(bytes.len()));
    }
    bytes.pop();
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(IpcError::MessageTooLarge(bytes.len()));
    }
    Ok(bytes)
}

#[cfg(windows)]
async fn run_windows_server<F, Fut>(
    endpoint: &str,
    handler: F,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), IpcError>
where
    F: Fn(WorkerRequestV2) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = WorkerResponseV2> + Send + 'static,
{
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut first = true;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        let server = hd_platform::create_owner_only_named_pipe(&options, endpoint)
            .map_err(IpcError::Platform)?;
        first = false;
        tokio::select! {
            result = server.connect() => {
                result.map_err(IpcError::Io)?;
                let connection_handler = handler.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(server, connection_handler).await {
                        tracing::warn!(event = "ipc.connection.failed", error_code = error.code(), %error, "worker IPC connection failed");
                    }
                });
            }
            changed = shutdown.changed() => {
                changed.map_err(|_| IpcError::ShutdownChannel)?;
            }
        }
    }
}

#[cfg(unix)]
async fn run_unix_server<F, Fut>(
    endpoint: &str,
    handler: F,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), IpcError>
where
    F: Fn(WorkerRequestV2) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = WorkerResponseV2> + Send + 'static,
{
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
    use tokio::net::UnixListener;

    let path = PathBuf::from(endpoint);
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !metadata.file_type().is_socket() {
            return Err(IpcError::UnsafeStaleEndpoint(path));
        }
        std::fs::remove_file(&path).map_err(|source| IpcError::Bind {
            endpoint: endpoint.to_owned(),
            source,
        })?;
    }
    let listener = UnixListener::bind(&path).map_err(|source| IpcError::Bind {
        endpoint: endpoint.to_owned(),
        source,
    })?;
    let _guard = UnixSocketPathGuard(path.clone());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        IpcError::Bind {
            endpoint: endpoint.to_owned(),
            source,
        }
    })?;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(IpcError::Io)?;
                let connection_handler = handler.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(stream, connection_handler).await {
                        tracing::warn!(event = "ipc.connection.failed", error_code = error.code(), %error, "worker IPC connection failed");
                    }
                });
            }
            changed = shutdown.changed() => {
                changed.map_err(|_| IpcError::ShutdownChannel)?;
            }
        }
    }
}

#[cfg(unix)]
struct UnixSocketPathGuard(PathBuf);

#[cfg(unix)]
impl Drop for UnixSocketPathGuard {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(event = "ipc.cleanup.failed", path = %self.0.display(), %error, "failed to remove IPC socket");
        }
    }
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("failed to bind local IPC endpoint {endpoint}: {source}")]
    Bind {
        endpoint: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to connect local IPC endpoint {endpoint}: {source}")]
    Connect {
        endpoint: String,
        #[source]
        source: std::io::Error,
    },
    #[error("IPC I/O failed: {0}")]
    Io(std::io::Error),
    #[error("failed to encode IPC message: {0}")]
    Encode(serde_json::Error),
    #[error("failed to decode IPC message: {0}")]
    Decode(serde_json::Error),
    #[error("IPC message exceeds limit: {0} bytes")]
    MessageTooLarge(usize),
    #[error("IPC peer closed before sending a complete message")]
    UnexpectedEof,
    #[error("IPC exchange timed out")]
    Timeout,
    #[error("IPC shutdown channel closed unexpectedly")]
    ShutdownChannel,
    #[cfg(unix)]
    #[error("refusing to replace a non-socket IPC path: {0}")]
    UnsafeStaleEndpoint(PathBuf),
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
}

impl IpcError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Bind { .. } => "ipc_bind",
            Self::Connect { .. } => "ipc_connect",
            Self::Io(_) => "ipc_io",
            Self::Encode(_) => "ipc_encode",
            Self::Decode(_) => "ipc_decode",
            Self::MessageTooLarge(_) => "ipc_message_too_large",
            Self::UnexpectedEof => "ipc_unexpected_eof",
            Self::Timeout => "ipc_timeout",
            Self::ShutdownChannel => "ipc_shutdown_channel",
            #[cfg(unix)]
            Self::UnsafeStaleEndpoint(_) => "ipc_unsafe_stale_endpoint",
            Self::Platform(_) => "ipc_platform",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_message_is_rejected_before_write() {
        let (mut left, _right) = tokio::io::duplex(MAX_MESSAGE_BYTES + 16);
        let value = "x".repeat(MAX_MESSAGE_BYTES + 1);
        let error = write_message(&mut left, &value)
            .await
            .expect_err("size limit");
        assert!(matches!(error, IpcError::MessageTooLarge(_)));
    }
}
