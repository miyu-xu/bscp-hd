use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::io::AsyncReadExt as _;

use super::{FormalContext, write_location};

const FIXED_LOCATION_COMMAND: &[u8] = b"CMD_GET_LOCATION";

pub(super) async fn run(context: Arc<FormalContext>) -> Result<()> {
    let endpoint = context
        .guest_endpoints
        .get("location")
        .context("fixed location Guest endpoint is unavailable")?;
    let mut guest_output = tokio::fs::OpenOptions::new()
        .read(true)
        .open(&endpoint.guest_output)
        .await
        .context("open fixed location Guest output")?;
    let mut guest_input = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&endpoint.guest_input)
        .await
        .context("open fixed location Guest input")?;
    let mut pending = Vec::new();
    let mut chunk = [0_u8; 256];
    let mut location_updates = context.location_updates.subscribe();
    let mut guest_subscribed = false;
    loop {
        tokio::select! {
            changed = location_updates.changed(), if guest_subscribed => {
                changed.context("fixed location update channel closed")?;
                let location = location_updates.borrow().clone();
                write_location(&context, &mut guest_input, &location)
                    .await
                    .context("push fixed location update to Guest")?;
            }
            read_result = guest_output.read(&mut chunk) => {
                let read = read_result.context("read fixed location Guest output")?;
                if read == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                pending.extend_from_slice(&chunk[..read]);
                while let Some(offset) = pending
                    .windows(FIXED_LOCATION_COMMAND.len())
                    .position(|window| window == FIXED_LOCATION_COMMAND)
                {
                    pending.drain(..offset + FIXED_LOCATION_COMMAND.len());
                    guest_subscribed = true;
                    let location = context.simulator.lock().await.state().location.clone();
                    write_location(&context, &mut guest_input, &location)
                        .await
                        .context("answer fixed location Guest subscription")?;
                }
                if pending.len() > FIXED_LOCATION_COMMAND.len() * 4 {
                    let keep = FIXED_LOCATION_COMMAND.len().saturating_sub(1);
                    pending.drain(..pending.len().saturating_sub(keep));
                }
            }
        }
    }
}
