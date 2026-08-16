use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use super::FormalContext;

const UPDATE_HAL_COMMAND: u32 = 2;
const HOST_SENSOR_MASK: u32 = (1 << 0) | (1 << 1) | (1 << 2);
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
enum WireFormat {
    Qemud,
    Transport,
    Line,
}

#[cfg(unix)]
pub(super) async fn run(context: Arc<FormalContext>) -> Result<()> {
    let endpoint = context
        .guest_endpoints
        .get("sensors")
        .context("sensor Guest endpoint is unavailable")?;
    let guest_output = tokio::fs::OpenOptions::new()
        .read(true)
        .open(&endpoint.guest_output)
        .await
        .context("open sensor Guest output")?;
    let guest_input = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&endpoint.guest_input)
        .await
        .context("open sensor Guest input")?;
    run_streams(context, guest_output, guest_input).await
}

#[cfg(windows)]
pub(super) async fn run(context: Arc<FormalContext>) -> Result<()> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let endpoint = context
        .guest_endpoints
        .get("sensors")
        .context("sensor Guest endpoint is unavailable")?;
    let guest_output = super::open_guest_pipe(|| {
        ClientOptions::new()
            .read(true)
            .write(false)
            .open(&endpoint.guest_output)
    })
    .await
    .context("open sensor Guest output")?;
    let guest_input = super::open_guest_pipe(|| {
        ClientOptions::new()
            .read(false)
            .write(true)
            .open(&endpoint.guest_input)
    })
    .await
    .context("open sensor Guest input")?;
    run_streams(context, guest_output, guest_input).await
}

async fn run_streams<R, W>(
    context: Arc<FormalContext>,
    mut guest_output: R,
    mut guest_input: W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut pending = Vec::new();
    let mut line_pending = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut activated = false;
    let mut wire_format = WireFormat::Transport;
    let mut report_interval = tokio::time::interval(Duration::from_millis(200));
    report_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            read_result = guest_output.read(&mut chunk) => {
                let read = read_result?;
                if read > 0 {
                    pending.extend_from_slice(&chunk[..read]);
                    for request_format in take_list_requests(&mut pending, &mut line_pending) {
                        wire_format = request_format;
                        let response = match wire_format {
                            WireFormat::Qemud => HOST_SENSOR_MASK.to_string(),
                            WireFormat::Transport | WireFormat::Line => {
                                format!("{HOST_SENSOR_MASK}\n")
                            }
                        };
                        write_payload(&mut guest_input, response.as_bytes(), wire_format).await?;
                        activated = true;
                        tracing::info!(?wire_format, "sensor Guest HAL activated");
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            _ = report_interval.tick(), if activated => {
                write_sensor_reports(&context, &mut guest_input, wire_format).await?;
            }
        }
    }
}

fn format_microunits(value: i64) -> String {
    let magnitude = value.unsigned_abs();
    let whole = magnitude / 1_000_000;
    let fraction = magnitude % 1_000_000;
    if value.is_negative() {
        format!("-{whole}.{fraction:06}")
    } else {
        format!("{whole}.{fraction:06}")
    }
}

async fn write_sensor_reports(
    context: &FormalContext,
    guest_input: &mut (impl AsyncWrite + Unpin),
    wire_format: WireFormat,
) -> Result<()> {
    let state = context.simulator.lock().await.state().clone();
    let reports: [(&str, &str, &[i64]); 3] = [
        ("accelerometer", "acceleration", &[0, 9_806_650, 0]),
        ("gyroscope", "gyroscope", &[0, 0, 0]),
        ("magnetometer", "magnetic", &[0, 5_900_000, -48_400_000]),
    ];
    for (sensor, protocol_name, defaults) in reports {
        let values = state
            .sensors
            .get(sensor)
            .map_or(defaults, |injection| injection.values_microunits.as_slice());
        let payload = format!(
            "{protocol_name}:{}\n",
            values
                .iter()
                .map(|value| format_microunits(*value))
                .collect::<Vec<_>>()
                .join(":")
        );
        write_payload(guest_input, payload.as_bytes(), wire_format).await?;
    }
    Ok(())
}

fn take_list_requests(pending: &mut Vec<u8>, line_pending: &mut Vec<u8>) -> Vec<WireFormat> {
    let mut requests = Vec::new();
    loop {
        if pending.len() >= 4 && pending[..4].iter().all(u8::is_ascii_hexdigit) {
            let header = std::str::from_utf8(&pending[..4]).expect("ASCII hex frame header");
            let payload_size =
                usize::from_str_radix(header, 16).expect("validated hex frame header");
            let total = 4 + payload_size;
            if pending.len() < total {
                break;
            }
            let payload = pending[4..total].to_vec();
            pending.drain(..total);
            if is_list_request(&payload) {
                requests.push(WireFormat::Qemud);
            }
            continue;
        }
        if pending.len() < 8 {
            break;
        }
        let command_word = u32::from_le_bytes(pending[0..4].try_into().expect("fixed header"));
        let payload_size =
            u32::from_le_bytes(pending[4..8].try_into().expect("fixed header")) as usize;
        let plausible = command_word & 0x7fff_ffff <= 0xffff && payload_size <= MAX_PAYLOAD_BYTES;
        if plausible {
            let total = 8 + payload_size;
            if pending.len() < total {
                break;
            }
            let payload = pending[8..total].to_vec();
            pending.drain(..total);
            if is_list_request(&payload) {
                requests.push(WireFormat::Transport);
            }
            continue;
        }
        line_pending.append(pending);
        while let Some(newline) = line_pending.iter().position(|byte| *byte == b'\n') {
            let line = line_pending.drain(..=newline).collect::<Vec<_>>();
            if is_list_request(&line) {
                requests.push(WireFormat::Line);
            }
        }
        break;
    }
    requests
}

fn is_list_request(payload: &[u8]) -> bool {
    String::from_utf8_lossy(payload)
        .trim()
        .to_ascii_lowercase()
        .starts_with("list-sensors")
}

async fn write_payload(
    guest_input: &mut (impl AsyncWrite + Unpin),
    payload: &[u8],
    wire_format: WireFormat,
) -> Result<()> {
    match wire_format {
        WireFormat::Qemud => {
            anyhow::ensure!(
                payload.len() <= u16::MAX.into(),
                "sensor qemud payload exceeds u16"
            );
            let header = format!("{:04x}", payload.len());
            guest_input.write_all(header.as_bytes()).await?;
        }
        WireFormat::Transport => {
            let command = UPDATE_HAL_COMMAND | (1 << 31);
            let payload_len =
                u32::try_from(payload.len()).context("sensor payload length exceeds u32")?;
            guest_input.write_all(&command.to_le_bytes()).await?;
            guest_input.write_all(&payload_len.to_le_bytes()).await?;
        }
        WireFormat::Line => {}
    }
    guest_input.write_all(payload).await?;
    guest_input.flush().await?;
    Ok(())
}
