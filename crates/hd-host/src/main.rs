use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Parser;
use hd_platform::DataPaths;
use hd_runtime::{HostService, run_host_http};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(about = "HD authenticated per-user host control plane")]
struct Arguments {
    #[arg(long)]
    data_root: Option<PathBuf>,
    #[arg(long)]
    worker_executable: Option<PathBuf>,
    #[arg(long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let paths = match arguments.data_root {
        Some(root) => DataPaths::resolve(root).context("resolve HD data directory")?,
        None => DataPaths::discover().context("discover HD data directory")?,
    };
    paths.ensure().context("create HD data directories")?;
    let appender = tracing_appender::rolling::daily(&paths.logs, "host-v2.jsonl");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _log_guard = guard;
    tracing_subscriber::fmt()
        .json()
        .with_writer(writer)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let host = HostService::open(paths, arguments.worker_executable)
        .await
        .context("open HD host service")?;
    tracing::info!(
        event = "host.started",
        pid = std::process::id(),
        "HD host started"
    );
    let mut server = tokio::spawn(run_host_http(Arc::clone(&host), arguments.port));
    tokio::select! {
        joined = &mut server => {
            joined.context("join HD HTTP server")??;
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("wait for host shutdown signal")?;
            host.detach();
            server.await.context("join detached HD HTTP server")??;
        }
    }
    tracing::info!(event = "host.stopped", "HD host control plane stopped");
    Ok(())
}
