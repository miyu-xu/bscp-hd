use std::fs::OpenOptions;
use std::process::Stdio;

use async_trait::async_trait;
use hd_platform::{PlatformError, ProcessExit, ProcessSpec, ProcessSupervisor};
use tokio::process::{Child, Command};

#[derive(Debug, Default, Clone, Copy)]
pub struct TokioProcessSupervisor;

#[async_trait]
impl ProcessSupervisor for TokioProcessSupervisor {
    type Handle = Child;

    async fn spawn(&self, spec: &ProcessSpec) -> Result<Self::Handle, PlatformError> {
        let stdout = open_log(&spec.stdout_path)?;
        let stderr = open_log(&spec.stderr_path)?;
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.arguments)
            .envs(&spec.environment)
            .current_dir(&spec.working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true);
        command.spawn().map_err(|error| {
            PlatformError::Process(format!("spawn {}: {error}", spec.executable.display()))
        })
    }

    async fn terminate(&self, handle: &mut Self::Handle) -> Result<(), PlatformError> {
        match handle.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => handle
                .kill()
                .await
                .map_err(|error| PlatformError::Process(format!("terminate child: {error}"))),
            Err(error) => Err(PlatformError::Process(format!(
                "query child before terminate: {error}"
            ))),
        }
    }

    async fn wait(&self, handle: &mut Self::Handle) -> Result<ProcessExit, PlatformError> {
        let status = handle
            .wait()
            .await
            .map_err(|error| PlatformError::Process(format!("wait for child: {error}")))?;
        Ok(ProcessExit {
            code: status.code(),
            success: status.success(),
        })
    }
}

fn open_log(path: &std::path::Path) -> Result<std::fs::File, PlatformError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| PlatformError::Io {
            operation: "create process log directory",
            path: parent.to_owned(),
            source,
        })?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| PlatformError::Io {
            operation: "open process log",
            path: path.to_owned(),
            source,
        })
}
