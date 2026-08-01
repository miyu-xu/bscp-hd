use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::FormalContext;

const UPDATE_HAL_COMMAND: u32 = 2;
const HOST_SENSOR_MASK: u32 = (1 << 0) | (1 << 1) | (1 << 2);
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

pub(super) async fn run(context: Arc<FormalContext>) -> Result<()> {
    let endpoint = context
        .guest_endpoints
        .get("sensors")
        .context("sensor Guest endpoint is unavailable")?;
    let mut guest_output = tokio::fs::OpenOptions::new()
        .read(true)
        .open(&endpoint.guest_output)
        .await
        .context("open sensor Guest output")?;
    let mut guest_input = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&endpoint.guest_input)
        .await
        .context("open sensor Guest input")?;
    let mut pending = Vec::new();
    let mut line_pending = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut activated = false;
    let mut framed = true;
    let mut report_interval = tokio::time::interval(Duration::from_millis(200));
    report_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            read_result = guest_output.read(&mut chunk) => {
                let read = read_result?;
                if read > 0 {
                    pending.extend_from_slice(&chunk[..read]);
                    for request_framed in take_list_requests(&mut pending, &mut line_pending) {
                        framed = request_framed;
                        write_payload(
                            &mut guest_input,
                            format!("{HOST_SENSOR_MASK}\n").as_bytes(),
                            framed,
                        )
                        .await?;
                        activated = true;
                        tracing::info!("sensor Guest HAL activated");
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            _ = report_interval.tick(), if activated => {
                write_sensor_reports(&context, &mut guest_input, framed).await?;
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
    guest_input: &mut tokio::fs::File,
    framed: bool,
) -> Result<()> {
    let state = context.simulator.lock().await.state().clone();
    let reports: [(&str, &str, &[i64]); 5] = [
        ("accelerometer", "acceleration", &[0, 9_806_650, 0]),
        ("gyroscope", "gyroscope", &[0, 0, 0]),
        ("magnetometer", "magnetic", &[0, 5_900_000, -48_400_000]),
        ("light", "light", &[0]),
        ("proximity", "proximity", &[2_500_000]),
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
        write_payload(guest_input, payload.as_bytes(), framed).await?;
    }
    Ok(())
}

fn take_list_requests(pending: &mut Vec<u8>, line_pending: &mut Vec<u8>) -> Vec<bool> {
    let mut requests = Vec::new();
    loop {
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
                requests.push(true);
            }
            continue;
        }
        line_pending.append(pending);
        while let Some(newline) = line_pending.iter().position(|byte| *byte == b'\n') {
            let line = line_pending.drain(..=newline).collect::<Vec<_>>();
            if is_list_request(&line) {
                requests.push(false);
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
    guest_input: &mut tokio::fs::File,
    payload: &[u8],
    framed: bool,
) -> Result<()> {
    if framed {
        let command = UPDATE_HAL_COMMAND | (1 << 31);
        let payload_len =
            u32::try_from(payload.len()).context("sensor payload length exceeds u32")?;
        guest_input.write_all(&command.to_le_bytes()).await?;
        guest_input.write_all(&payload_len.to_le_bytes()).await?;
    }
    guest_input.write_all(payload).await?;
    guest_input.flush().await?;
    Ok(())
}
