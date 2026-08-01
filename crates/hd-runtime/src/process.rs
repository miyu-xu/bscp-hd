use std::fs::OpenOptions;

use async_trait::async_trait;
use hd_platform::{
    PlatformError, ProcessContainment, ProcessExit, ProcessSpec, ProcessSupervisor,
    configure_managed_command, contain_process, resume_managed_process,
};
use tokio::process::{Child, Command};

#[derive(Debug, Clone, Copy, Default)]
pub struct TokioProcessSupervisor;

#[derive(Debug)]
pub struct ManagedProcess {
    child: Child,
    containment: ProcessContainment,
    pid: u32,
}

impl ManagedProcess {
    pub const fn id(&self) -> u32 {
        self.pid
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    pub fn try_wait(&mut self) -> Result<Option<ProcessExit>, PlatformError> {
        self.child
            .try_wait()
            .map(|status| {
                status.map(|status| ProcessExit {
                    code: status.code(),
                    success: status.success(),
                })
            })
            .map_err(|error| PlatformError::Process(format!("poll process {}: {error}", self.pid)))
    }
}

#[async_trait]
impl ProcessSupervisor for TokioProcessSupervisor {
    type Handle = ManagedProcess;

    async fn spawn(&self, spec: &ProcessSpec) -> Result<Self::Handle, PlatformError> {
        let stdout = open_log(&spec.stdout_path)?;
        let stderr = open_log(&spec.stderr_path)?;
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.arguments)
            .envs(&spec.environment)
            .current_dir(&spec.working_directory)
            .stdin(std::process::Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(spec.kill_on_drop);
        configure_managed_command(command.as_std_mut())?;
        tracing::info!(
            event = "process.spawn.started",
            executable = %spec.executable.display(),
            argument_count = spec.arguments.len(),
            working_directory = %spec.working_directory.display(),
            "starting managed process"
        );
        let mut child = command.spawn().map_err(|source| PlatformError::Io {
            operation: "spawn managed process",
            path: spec.executable.clone(),
            source,
        })?;
        let pid = child
            .id()
            .ok_or_else(|| PlatformError::Process("spawned process has no PID".to_owned()))?;
        let containment = match contain_process(pid) {
            Ok(containment) => containment,
            Err(error) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(error);
            }
        };
        if let Err(error) = resume_managed_process(pid) {
            // Dropping the Windows kill-on-close containment terminates the suspended tree.
            tracing::debug!(
                containment_pid = containment.process_id(),
                "releasing failed process containment"
            );
            #[cfg(windows)]
            drop(containment);
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(error);
        }
        tracing::info!(
            event = "process.spawn.succeeded",
            pid,
            containment_pid = containment.process_id(),
            "managed process started"
        );
        Ok(ManagedProcess {
            child,
            containment,
            pid,
        })
    }

    async fn terminate(&self, handle: &mut Self::Handle) -> Result<(), PlatformError> {
        tracing::info!(event = "process.terminate.started", pid = handle.pid);
        handle.child.start_kill().map_err(|error| {
            PlatformError::Process(format!("kill process {}: {error}", handle.pid))
        })?;
        let status = handle.child.wait().await.map_err(|error| {
            PlatformError::Process(format!("wait process {}: {error}", handle.pid))
        })?;
        tracing::info!(
            event = "process.terminate.succeeded",
            pid = handle.pid,
            exit_code = status.code(),
            "managed process terminated"
        );
        Ok(())
    }

    async fn wait(&self, handle: &mut Self::Handle) -> Result<ProcessExit, PlatformError> {
        let status = handle.child.wait().await.map_err(|error| {
            PlatformError::Process(format!("wait process {}: {error}", handle.pid))
        })?;
        let _containment_pid = handle.containment.process_id();
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
    if !path.exists() {
        hd_platform::write_owner_only(path, &[])?;
    }
    OpenOptions::new()
        .create(false)
        .append(true)
        .open(path)
        .map_err(|source| PlatformError::Io {
            operation: "open process log",
            path: path.to_owned(),
            source,
        })
}
