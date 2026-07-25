use std::time::Duration;

use hd_core::{DeviceControlRequestV2, DeviceControlResponseV2};
use thiserror::Error;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};

const MAX_DEVICE_CONTROL_BYTES: usize = 64 * 1024;
const DEVICE_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn send_device_control_request(
    endpoint: &str,
    request: &DeviceControlRequestV2,
) -> Result<DeviceControlResponseV2, DeviceIpcError> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;

        let mut last_error = None;
        for _ in 0..100 {
            match ClientOptions::new().open(endpoint) {
                Ok(mut client) => return timeout_exchange(&mut client, request).await,
                Err(error) => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        Err(DeviceIpcError::Connect {
            endpoint: endpoint.to_owned(),
            source: last_error.unwrap_or_else(|| std::io::Error::other("connection retry ended")),
        })
    }
    #[cfg(unix)]
    {
        let mut stream = tokio::net::UnixStream::connect(endpoint)
            .await
            .map_err(|source| DeviceIpcError::Connect {
                endpoint: endpoint.to_owned(),
                source,
            })?;
        timeout_exchange(&mut stream, request).await
    }
}

async fn timeout_exchange<S>(
    stream: &mut S,
    request: &DeviceControlRequestV2,
) -> Result<DeviceControlResponseV2, DeviceIpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(DEVICE_CONTROL_TIMEOUT, exchange(stream, request))
        .await
        .map_err(|_| DeviceIpcError::Timeout)?
}

async fn exchange<S>(
    stream: &mut S,
    request: &DeviceControlRequestV2,
) -> Result<DeviceControlResponseV2, DeviceIpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(request).map_err(DeviceIpcError::Encode)?;
    if bytes.len() > MAX_DEVICE_CONTROL_BYTES {
        return Err(DeviceIpcError::MessageTooLarge(bytes.len()));
    }
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(DeviceIpcError::Io)?;
    stream.flush().await.map_err(DeviceIpcError::Io)?;

    let mut response = Vec::new();
    let mut reader = BufReader::new(stream).take((MAX_DEVICE_CONTROL_BYTES + 1) as u64);
    let read = reader
        .read_until(b'\n', &mut response)
        .await
        .map_err(DeviceIpcError::Io)?;
    if read == 0 {
        return Err(DeviceIpcError::UnexpectedEof);
    }
    if response.len() > MAX_DEVICE_CONTROL_BYTES + 1 || !response.ends_with(b"\n") {
        return Err(DeviceIpcError::MessageTooLarge(response.len()));
    }
    response.pop();
    serde_json::from_slice(&response).map_err(DeviceIpcError::Decode)
}

#[derive(Debug, Error)]
pub enum DeviceIpcError {
    #[error("failed to connect device component endpoint {endpoint}: {source}")]
    Connect {
        endpoint: String,
        #[source]
        source: std::io::Error,
    },
    #[error("device component exchange timed out")]
    Timeout,
    #[error("device component closed the endpoint before responding")]
    UnexpectedEof,
    #[error("device component message exceeded the limit: {0} bytes")]
    MessageTooLarge(usize),
    #[error("device component request encoding failed: {0}")]
    Encode(serde_json::Error),
    #[error("device component response decoding failed: {0}")]
    Decode(serde_json::Error),
    #[error("device component I/O failed: {0}")]
    Io(std::io::Error),
}
