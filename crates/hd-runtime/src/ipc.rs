use std::sync::Arc;

#[cfg(not(windows))]
use std::path::PathBuf;
#[cfg(windows)]
use std::time::Duration;

use hd_core::{ControlRequestV1, ControlResponseV1};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::Supervisor;

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

pub fn control_endpoint() -> Result<String, IpcError> {
    #[cfg(windows)]
    {
        Ok(format!(
            r"\\.\pipe\bscp-hd-control-{}",
            windows_current_sid()?
        ))
    }
    #[cfg(not(windows))]
    {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Ok(runtime
            .join(format!("bscp-hd-{}.sock", current_uid()))
            .to_string_lossy()
            .into_owned())
    }
}

pub async fn run_control_server(
    supervisor: Arc<Supervisor>,
    endpoint: &str,
) -> Result<(), IpcError> {
    #[cfg(windows)]
    {
        run_windows_server(supervisor, endpoint).await
    }
    #[cfg(not(windows))]
    {
        run_unix_server(supervisor, endpoint).await
    }
}

pub async fn send_request(
    endpoint: &str,
    request: &ControlRequestV1,
) -> Result<ControlResponseV1, IpcError> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let mut last_error = None;
        for _ in 0..40 {
            match ClientOptions::new().open(endpoint) {
                Ok(mut client) => return exchange(&mut client, request).await,
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
    #[cfg(not(windows))]
    {
        let mut stream = tokio::net::UnixStream::connect(endpoint)
            .await
            .map_err(|source| IpcError::Connect {
                endpoint: endpoint.to_owned(),
                source,
            })?;
        exchange(&mut stream, request).await
    }
}

async fn exchange<S>(
    stream: &mut S,
    request: &ControlRequestV1,
) -> Result<ControlResponseV1, IpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(request).map_err(IpcError::Encode)?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(IpcError::MessageTooLarge(bytes.len()));
    }
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(IpcError::Io)?;
    stream.flush().await.map_err(IpcError::Io)?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .await
        .map_err(IpcError::Io)?;
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(IpcError::MessageTooLarge(line.len()));
    }
    serde_json::from_str(&line).map_err(IpcError::Decode)
}

async fn serve_connection<S>(stream: &mut S, supervisor: &Arc<Supervisor>) -> Result<(), IpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut line = String::new();
    BufReader::new(&mut *stream)
        .read_line(&mut line)
        .await
        .map_err(IpcError::Io)?;
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(IpcError::MessageTooLarge(line.len()));
    }
    let request: ControlRequestV1 = serde_json::from_str(&line).map_err(IpcError::Decode)?;
    let response = supervisor.handle(request).await;
    let mut bytes = serde_json::to_vec(&response).map_err(IpcError::Encode)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(IpcError::Io)?;
    stream.flush().await.map_err(IpcError::Io)
}

#[cfg(windows)]
async fn run_windows_server(supervisor: Arc<Supervisor>, endpoint: &str) -> Result<(), IpcError> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut first = true;
    while !supervisor.should_shutdown() {
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        let mut server = options.create(endpoint).map_err(|source| IpcError::Bind {
            endpoint: endpoint.to_owned(),
            source,
        })?;
        first = false;
        server.connect().await.map_err(IpcError::Io)?;
        if let Err(error) = serve_connection(&mut server, &supervisor).await {
            tracing::warn!(%error, "control connection failed");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
async fn run_unix_server(supervisor: Arc<Supervisor>, endpoint: &str) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    let path = PathBuf::from(endpoint);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|source| IpcError::Bind {
            endpoint: endpoint.to_owned(),
            source,
        })?;
    }
    let listener = UnixListener::bind(&path).map_err(|source| IpcError::Bind {
        endpoint: endpoint.to_owned(),
        source,
    })?;
    let _path_guard = UnixSocketPathGuard(path.clone());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        IpcError::Bind {
            endpoint: endpoint.to_owned(),
            source,
        }
    })?;
    while !supervisor.should_shutdown() {
        let (mut stream, _) = listener.accept().await.map_err(IpcError::Io)?;
        if let Err(error) = serve_connection(&mut stream, &supervisor).await {
            tracing::warn!(%error, "control connection failed");
        }
    }
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_current_sid() -> Result<String, IpcError> {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo handle; token points to valid writable memory.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        // SAFETY: GetLastError has no preconditions.
        return Err(IpcError::Identity(unsafe { GetLastError() }));
    }
    let result = (|| {
        let mut bytes = 0_u32;
        // SAFETY: the null buffer/zero length probe is the documented sizing call.
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &raw mut bytes);
        }
        if bytes == 0 {
            // SAFETY: GetLastError has no preconditions.
            return Err(IpcError::Identity(unsafe { GetLastError() }));
        }
        let words = (bytes as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        // SAFETY: buffer is sized by the probe and remains live for the TOKEN_USER/SID access.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                bytes,
                &raw mut bytes,
            )
        } == 0
        {
            // SAFETY: GetLastError has no preconditions.
            return Err(IpcError::Identity(unsafe { GetLastError() }));
        }
        // SAFETY: GetTokenInformation populated a TOKEN_USER at the start of buffer.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut sid_text = std::ptr::null_mut();
        // SAFETY: user.User.Sid is valid while buffer lives; sid_text is an output pointer.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut sid_text) } == 0 {
            // SAFETY: GetLastError has no preconditions.
            return Err(IpcError::Identity(unsafe { GetLastError() }));
        }
        let mut length = 0;
        // SAFETY: ConvertSidToStringSidW returns a null-terminated LocalAlloc string.
        while unsafe { *sid_text.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: the string is valid for `length` UTF-16 units.
        let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_text, length) });
        // SAFETY: the API contract requires LocalFree for this allocation.
        unsafe {
            LocalFree(sid_text.cast());
        }
        Ok(sid)
    })();
    // SAFETY: token is a real token handle opened above and is closed exactly once.
    unsafe {
        CloseHandle(token);
    }
    result
}

#[cfg(not(windows))]
#[allow(unsafe_code)]
fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not retain pointers.
    unsafe { libc::geteuid() }
}

#[cfg(not(windows))]
struct UnixSocketPathGuard(PathBuf);

#[cfg(not(windows))]
impl Drop for UnixSocketPathGuard {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.0.display(), %error, "failed to remove Unix IPC socket");
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
    #[cfg(windows)]
    #[error("failed to obtain current Windows SID (error {0})")]
    Identity(u32),
}
