//! Cross-platform gfxstream host screen-recorder control protocol.
//!
//! The gfxstream backend owns bounded frame readback and hardware H.264 encoding. The Worker only
//! sends commands over a per-run owner-only local endpoint and validates the resulting MP4.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use uuid::Uuid;

const REQUEST_MAGIC: u32 = 0x4844_5243;
const RESPONSE_MAGIC: u32 = 0x4844_5253;
const PROTOCOL_VERSION: u16 = 2;
const OPERATION_START: u16 = 1;
const OPERATION_STOP: u16 = 2;
const MAX_MESSAGE_BYTES: usize = 4096;
const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(windows)]
const PIPE_CONNECT_RETRY: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub(crate) struct HostRecording {
    endpoint: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HostRecordingStats {
    pub(crate) encoded_frames: u64,
    pub(crate) dropped_frames: u64,
    pub(crate) initial_static_frame: bool,
    pub(crate) initial_frame_y_direction: i32,
    pub(crate) near_black_frames: u64,
    pub(crate) max_consecutive_near_black_frames: u64,
    pub(crate) max_source_frame_gap_millis: u64,
    pub(crate) source_frame_gaps_over_100_millis: u64,
}

#[derive(Debug, Default, Deserialize)]
struct HostRecordingEvidence {
    #[serde(default)]
    initial_static_frame: bool,
    #[serde(default)]
    initial_frame_y_direction: i32,
    #[serde(default)]
    near_black_frames: u64,
    #[serde(default)]
    max_consecutive_near_black_frames: u64,
    #[serde(default)]
    max_source_frame_gap_millis: u64,
    #[serde(default)]
    source_frame_gaps_over_100_millis: u64,
}

pub(crate) fn endpoint_for_run(run_id: Uuid) -> PathBuf {
    let compact = run_id.simple().to_string();
    #[cfg(windows)]
    {
        PathBuf::from(format!(
            r"\\.\pipe\hd-rec-{}-{}",
            &compact[..16],
            std::process::id()
        ))
    }
    #[cfg(unix)]
    {
        std::env::temp_dir().join(format!(
            "hd-rec-{}-{}.sock",
            &compact[..16],
            std::process::id()
        ))
    }
}

impl HostRecording {
    pub(crate) async fn start(
        endpoint: PathBuf,
        display_id: u32,
        output_path: &Path,
        max_duration_seconds: u16,
    ) -> Result<Self, String> {
        let path = output_path
            .to_str()
            .ok_or_else(|| "host screen-recording path is not valid UTF-8".to_owned())?;
        if let Err(error) = send_command(
            &endpoint,
            OPERATION_START,
            display_id,
            u32::from(max_duration_seconds),
            path.as_bytes(),
            START_TIMEOUT,
        )
        .await
        {
            // A timeout or broken response can happen after gfxstream accepted START. STOP
            // unregisters the readback callback before finalizing the writer, so a short
            // best-effort cleanup exchange is required even if its response also times out.
            let _ = send_command(&endpoint, OPERATION_STOP, 0, 0, &[], CLEANUP_TIMEOUT).await;
            return Err(error);
        }
        Ok(Self { endpoint })
    }

    pub(crate) async fn finish(self) -> Result<HostRecordingStats, String> {
        match send_command(&self.endpoint, OPERATION_STOP, 0, 0, &[], STOP_TIMEOUT).await {
            Ok(stats) => Ok(stats),
            Err(error) => {
                // STOP is idempotent after successful finalization. Retrying covers a lost first
                // response and gives an accepted stop another bounded opportunity to unregister.
                let _ =
                    send_command(&self.endpoint, OPERATION_STOP, 0, 0, &[], CLEANUP_TIMEOUT).await;
                Err(error)
            }
        }
    }
}

#[cfg(windows)]
async fn connect_local(endpoint: &Path) -> Result<NamedPipeClient, String> {
    let endpoint = endpoint
        .to_str()
        .ok_or_else(|| "Windows host-recorder endpoint is not valid UTF-8".to_owned())?;
    if !endpoint.starts_with(r"\\.\pipe\") {
        return Err("Windows host-recorder endpoint is not a local named pipe".to_owned());
    }
    loop {
        match ClientOptions::new().open(endpoint) {
            Ok(stream) => return Ok(stream),
            Err(error) if matches!(error.raw_os_error(), Some(2 | 231)) => {
                tokio::time::sleep(PIPE_CONNECT_RETRY).await;
            }
            Err(error) => {
                return Err(format!(
                    "connect gfxstream host recorder {endpoint}: {error}"
                ));
            }
        }
    }
}

async fn send_command(
    endpoint: &Path,
    operation: u16,
    display_id: u32,
    max_duration_seconds: u32,
    path: &[u8],
    timeout: Duration,
) -> Result<HostRecordingStats, String> {
    if path.len() > MAX_MESSAGE_BYTES {
        return Err("host screen-recording path exceeds the protocol limit".to_owned());
    }
    let exchange = async {
        #[cfg(unix)]
        let mut stream = UnixStream::connect(endpoint).await.map_err(|error| {
            format!(
                "connect gfxstream host recorder {}: {error}",
                endpoint.display()
            )
        })?;
        #[cfg(windows)]
        let mut stream = connect_local(endpoint).await?;
        let mut request = Vec::with_capacity(20 + path.len());
        request.extend_from_slice(&REQUEST_MAGIC.to_le_bytes());
        request.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        request.extend_from_slice(&operation.to_le_bytes());
        request.extend_from_slice(&display_id.to_le_bytes());
        request.extend_from_slice(&max_duration_seconds.to_le_bytes());
        request.extend_from_slice(
            &u32::try_from(path.len())
                .map_err(|_| "host screen-recording path length overflow".to_owned())?
                .to_le_bytes(),
        );
        request.extend_from_slice(path);
        stream
            .write_all(&request)
            .await
            .map_err(|error| format!("send gfxstream host-recorder command: {error}"))?;

        let mut header = [0_u8; 28];
        stream
            .read_exact(&mut header)
            .await
            .map_err(|error| format!("read gfxstream host-recorder response: {error}"))?;
        let magic = u32::from_le_bytes(header[0..4].try_into().expect("fixed response header"));
        let version = u16::from_le_bytes(header[4..6].try_into().expect("fixed response header"));
        let status = u16::from_le_bytes(header[6..8].try_into().expect("fixed response header"));
        let encoded_frames =
            u64::from_le_bytes(header[8..16].try_into().expect("fixed response header"));
        let dropped_frames =
            u64::from_le_bytes(header[16..24].try_into().expect("fixed response header"));
        let message_length = usize::try_from(u32::from_le_bytes(
            header[24..28].try_into().expect("fixed response header"),
        ))
        .expect("u32 fits usize on supported hosts");
        if magic != RESPONSE_MAGIC || version != PROTOCOL_VERSION {
            return Err("gfxstream host recorder returned an incompatible response".to_owned());
        }
        if message_length > MAX_MESSAGE_BYTES {
            return Err("gfxstream host recorder response exceeds the protocol limit".to_owned());
        }
        let mut message = vec![0_u8; message_length];
        stream
            .read_exact(&mut message)
            .await
            .map_err(|error| format!("read gfxstream host-recorder message: {error}"))?;
        let message = String::from_utf8_lossy(&message).into_owned();
        if status != 0 {
            return Err(if message.is_empty() {
                "gfxstream host recorder rejected the command".to_owned()
            } else {
                message
            });
        }
        let evidence = if message.is_empty() {
            HostRecordingEvidence::default()
        } else {
            serde_json::from_str::<HostRecordingEvidence>(&message).map_err(|error| {
                format!("gfxstream host recorder returned invalid success evidence: {error}")
            })?
        };
        Ok(HostRecordingStats {
            encoded_frames,
            dropped_frames,
            initial_static_frame: evidence.initial_static_frame,
            initial_frame_y_direction: evidence.initial_frame_y_direction,
            near_black_frames: evidence.near_black_frames,
            max_consecutive_near_black_frames: evidence.max_consecutive_near_black_frames,
            max_source_frame_gap_millis: evidence.max_source_frame_gap_millis,
            source_frame_gaps_over_100_millis: evidence.source_frame_gaps_over_100_millis,
        })
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| "gfxstream host-recorder command timed out".to_owned())?
}
