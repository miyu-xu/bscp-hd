use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use hd_core::{TrackpadEventV2, TrackpadPhaseV2};
use hd_platform::VmBackend as _;
use hd_runtime::CrosvmBackend;
use tokio::io::AsyncReadExt as _;
use uuid::Uuid;

const DOWN_REPORT_BYTES: usize = 40;

#[tokio::main]
async fn main() -> Result<()> {
    let endpoint = smoke_endpoint();
    let backend = CrosvmBackend::new(PathBuf::from("crosvm-trackpad-contract-smoke"));
    backend
        .prepare_trackpad_endpoint(&endpoint)
        .await
        .context("prepare owner-only trackpad endpoint")?;

    let reader_endpoint = endpoint.clone();
    let reader = tokio::spawn(async move { read_report(&reader_endpoint).await });
    backend
        .send_trackpad(
            &endpoint,
            TrackpadEventV2 {
                phase: TrackpadPhaseV2::Down,
                x: 2_500,
                y: 7_500,
            },
        )
        .await
        .context("send normalized trackpad event")?;
    let report = reader.await.context("join trackpad reader")??;
    validate_down_report(&report)?;

    #[cfg(unix)]
    if let Err(error) = std::fs::remove_file(&endpoint)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error).context("remove trackpad smoke socket");
    }

    println!(
        "{}",
        serde_json::json!({
            "schema_version": 1,
            "gate": "trackpad-smoke",
            "status": "pass",
            "owner_only_endpoint": true,
            "ordered_virtio_input": true,
            "phase": "down",
            "x": 2500,
            "y": 7500,
            "bytes": report.len(),
        })
    );
    Ok(())
}

fn smoke_endpoint() -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\bscp-hd-trackpad-smoke-{}", Uuid::new_v4())
    }
    #[cfg(unix)]
    {
        PathBuf::from("/tmp")
            .join(format!(
                "hd-tp-{}.sock",
                &Uuid::new_v4().simple().to_string()[..12]
            ))
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(windows)]
async fn read_report(endpoint: &str) -> Result<[u8; DOWN_REPORT_BYTES]> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut last_error = None;
    let mut pipe = None;
    for _ in 0..100 {
        match ClientOptions::new().open(endpoint) {
            Ok(connected) => {
                pipe = Some(connected);
                break;
            }
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    let mut pipe = pipe.with_context(|| {
        format!(
            "connect trackpad named pipe: {}",
            last_error.map_or_else(|| "retry exhausted".to_owned(), |error| error.to_string())
        )
    })?;
    let mut report = [0_u8; DOWN_REPORT_BYTES];
    tokio::time::timeout(Duration::from_secs(2), pipe.read_exact(&mut report))
        .await
        .context("trackpad report timed out")?
        .context("read trackpad report")?;
    Ok(report)
}

#[cfg(unix)]
async fn read_report(endpoint: &str) -> Result<[u8; DOWN_REPORT_BYTES]> {
    let mut stream = tokio::net::UnixStream::connect(endpoint)
        .await
        .context("connect trackpad smoke socket")?;
    let mut report = [0_u8; DOWN_REPORT_BYTES];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut report))
        .await
        .context("trackpad report timed out")??;
    Ok(report)
}

fn validate_down_report(report: &[u8; DOWN_REPORT_BYTES]) -> Result<()> {
    let events = report
        .chunks_exact(8)
        .map(|event| {
            (
                u16::from_le_bytes([event[0], event[1]]),
                u16::from_le_bytes([event[2], event[3]]),
                i32::from_le_bytes([event[4], event[5], event[6], event[7]]),
            )
        })
        .collect::<Vec<_>>();
    ensure!(
        events
            == [
                (3, 0, 2_500),
                (3, 1, 7_500),
                (1, 0x145, 1),
                (1, 0x14a, 1),
                (0, 0, 0),
            ],
        "trackpad virtio-input report is not ordered or little-endian: {events:?}"
    );
    Ok(())
}
