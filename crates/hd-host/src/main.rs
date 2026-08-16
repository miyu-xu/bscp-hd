use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Parser;
use hd_platform::DataPaths;
use hd_runtime::{
    HostService, clear_host_startup_failure, install_bundled_release_materials,
    record_host_startup_failure, run_host_http,
};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(about = "HD authenticated per-user host control plane")]
struct Arguments {
    #[arg(long)]
    data_root: Option<PathBuf>,
    #[arg(long)]
    worker_executable: Option<PathBuf>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    startup_attempt_id: Option<Uuid>,
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
    if let Some(startup_attempt_id) = arguments.startup_attempt_id
        && let Err(error) = clear_host_startup_failure(&paths, startup_attempt_id)
    {
        let error = anyhow::Error::msg(error).context("clear stale Host startup failure");
        report_startup_failure(
            &paths,
            arguments.startup_attempt_id,
            "startup_state_invalid",
            &error,
        );
        return Err(error);
    }
    let installed_release_materials = match install_bundled_release_materials(&paths)
        .map_err(anyhow::Error::msg)
        .context("install bundled release trust materials")
    {
        Ok(installed) => installed,
        Err(error) => {
            report_startup_failure(
                &paths,
                arguments.startup_attempt_id,
                "release_materials_invalid",
                &error,
            );
            return Err(error);
        }
    };
    if !installed_release_materials.is_empty() {
        tracing::info!(
            event = "release.materials.installed",
            count = installed_release_materials.len(),
            "installed signed release trust materials on first launch"
        );
    }

    let host = match HostService::open(paths.clone(), arguments.worker_executable)
        .await
        .context("open HD host service")
    {
        Ok(host) => host,
        Err(error) => {
            report_startup_failure(
                &paths,
                arguments.startup_attempt_id,
                "host_service_initialization_failed",
                &error,
            );
            return Err(error);
        }
    };
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

fn report_startup_failure(
    paths: &DataPaths,
    startup_attempt_id: Option<Uuid>,
    code: &'static str,
    error: &anyhow::Error,
) {
    tracing::error!(
        event = "host.startup.failed",
        error_code = code,
        error = %error,
        "HD Host startup failed"
    );
    let Some(startup_attempt_id) = startup_attempt_id else {
        return;
    };
    if let Err(record_error) = record_host_startup_failure(paths, startup_attempt_id, code, error) {
        tracing::error!(
            event = "host.startup.failure_record.failed",
            error_code = code,
            error = %record_error,
            "persist HD Host startup failure failed"
        );
    }
}
