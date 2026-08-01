#![allow(unsafe_code)]
#![cfg_attr(not(windows), allow(clippy::unused_async))]

#[cfg(not(windows))]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Parser;
#[cfg(windows)]
use hd_core::FormalComponentLaunchV2;
#[cfg(not(windows))]
use hd_core::FrameTransportKindV2;
use hd_platform::FrameInteropProbeV2;

#[cfg(windows)]
mod windows;

#[derive(Debug, Parser)]
#[command(about = "Formal HD Vulkan external-memory frame broker")]
struct Arguments {
    #[arg(long)]
    probe_v2: bool,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    serve_v2: bool,
    #[arg(long)]
    launch: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    match (
        arguments.probe_v2,
        arguments.json,
        arguments.serve_v2,
        arguments.launch.as_deref(),
    ) {
        (true, true, false, None) => {
            println!("{}", serde_json::to_string(&probe())?);
            Ok(())
        }
        (false, false, true, Some(path)) => serve(path).await,
        _ => bail!("select --probe-v2 --json or --serve-v2 --launch <path>"),
    }
}

#[cfg(windows)]
fn probe() -> FrameInteropProbeV2 {
    windows::probe()
}

#[cfg(not(windows))]
fn probe() -> FrameInteropProbeV2 {
    FrameInteropProbeV2 {
        component_protocol_version: hd_core::COMPONENT_PROTOCOL_VERSION,
        service_lifecycle: false,
        transport: expected_transport(),
        memory_export: false,
        explicit_sync: false,
        same_adapter: false,
        validation_clean: false,
        detail: "hd-frame-producer currently implements the Windows Vulkan transport".to_owned(),
        properties: BTreeMap::new(),
    }
}

#[cfg(windows)]
async fn serve(path: &Path) -> Result<()> {
    let launch_bytes = hd_platform::read_regular_nofollow_limited(path, 64 * 1024)?;
    let launch: FormalComponentLaunchV2 = serde_json::from_slice(&launch_bytes)?;
    windows::serve(path, launch, launch_bytes).await
}

#[cfg(not(windows))]
async fn serve(path: &Path) -> Result<()> {
    let _ = hd_platform::read_regular_nofollow_limited(path, 64 * 1024)?;
    bail!("hd-frame-producer service is unavailable on this platform")
}

#[cfg(not(windows))]
const fn expected_transport() -> FrameTransportKindV2 {
    #[cfg(target_os = "linux")]
    {
        FrameTransportKindV2::VulkanDmaBuf
    }
    #[cfg(target_os = "macos")]
    {
        FrameTransportKindV2::MetalIoSurface
    }
}
