use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Parser;
use hd_platform::DataPaths;
use hd_runtime::{WorkerService, run_worker_server};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(about = "Per-instance HD VM worker")]
struct Arguments {
    #[arg(long)]
    data_root: PathBuf,
    #[arg(long)]
    instance_id: Uuid,
    #[arg(long)]
    nonce: Uuid,
    #[arg(long)]
    endpoint: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let paths = DataPaths::resolve(arguments.data_root).context("resolve HD data directory")?;
    paths.ensure().context("create HD data directories")?;
    let log_directory = paths.logs.join("workers");
    std::fs::create_dir_all(&log_directory).context("create worker log directory")?;
    let appender = tracing_appender::rolling::daily(
        &log_directory,
        format!("{}.jsonl", arguments.instance_id),
    );
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _log_guard = guard;
    tracing_subscriber::fmt()
        .json()
        .with_writer(writer)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let worker = WorkerService::open(
        paths,
        arguments.instance_id,
        arguments.nonce,
        arguments.endpoint,
    )
    .context("open worker service")?;
    tracing::info!(
        event = "worker.started",
        instance_id = %arguments.instance_id,
        pid = std::process::id(),
        "worker is accepting authenticated local IPC"
    );
    let server_worker = Arc::clone(&worker);
    let endpoint = worker.endpoint().to_owned();
    let shutdown = worker.shutdown_receiver();
    let server = tokio::spawn(async move {
        run_worker_server(
            &endpoint,
            move |request| Arc::clone(&server_worker).handle(request),
            shutdown,
        )
        .await
    });
    tokio::select! {
        joined = server => {
            joined.context("join worker IPC server")??;
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("wait for worker shutdown signal")?;
            worker.shutdown_gracefully().await.context("stop worker after signal")?;
        }
    }
    tracing::info!(
        event = "worker.stopped",
        instance_id = %arguments.instance_id,
        "worker stopped"
    );
    Ok(())
}
