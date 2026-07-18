use std::path::{Path, PathBuf};

use hd_core::{AdbConfig, InstanceAction, Orientation};
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct AdbClient {
    executable: PathBuf,
}

impl AdbClient {
    pub fn from_config(config: &AdbConfig) -> Self {
        let executable = config
            .adb_path
            .clone()
            .or_else(|| std::env::var_os("HD_ADB_PATH").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("adb"));
        Self { executable }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub async fn connect(&self, serial: &str, auto_root: bool) -> Result<(), AdbError> {
        self.run(["connect", serial]).await?;
        self.run(["-s", serial, "wait-for-device"]).await?;
        if auto_root {
            self.run(["-s", serial, "root"]).await?;
            self.run(["-s", serial, "wait-for-device"]).await?;
        }
        Ok(())
    }

    pub async fn action(&self, serial: &str, action: InstanceAction) -> Result<(), AdbError> {
        let key_code = match action {
            InstanceAction::Home => "3",
            InstanceAction::Recent => "187",
            InstanceAction::Back => "4",
            InstanceAction::Power => "26",
            InstanceAction::VolumeUp => "24",
            InstanceAction::VolumeDown => "25",
            InstanceAction::Rotate => {
                return Err(AdbError::InvalidOperation(
                    "rotate requires an explicit target orientation",
                ));
            }
        };
        self.run(["-s", serial, "shell", "input", "keyevent", key_code])
            .await
    }

    pub async fn install(&self, serial: &str, apk: &Path) -> Result<(), AdbError> {
        let args = vec![
            "-s".to_owned(),
            serial.to_owned(),
            "install".to_owned(),
            "-r".to_owned(),
            apk.to_string_lossy().into_owned(),
        ];
        self.run_owned(args).await
    }

    pub async fn set_orientation(
        &self,
        serial: &str,
        orientation: Orientation,
    ) -> Result<(), AdbError> {
        let rotation = match orientation {
            Orientation::Landscape => "1",
            Orientation::Portrait => "0",
        };
        self.run([
            "-s",
            serial,
            "shell",
            "settings",
            "put",
            "system",
            "accelerometer_rotation",
            "0",
        ])
        .await?;
        self.run([
            "-s",
            serial,
            "shell",
            "settings",
            "put",
            "system",
            "user_rotation",
            rotation,
        ])
        .await
    }

    async fn run<const N: usize>(&self, args: [&str; N]) -> Result<(), AdbError> {
        self.run_owned(args.into_iter().map(str::to_owned).collect())
            .await
    }

    async fn run_owned(&self, args: Vec<String>) -> Result<(), AdbError> {
        let output = Command::new(&self.executable)
            .args(&args)
            .output()
            .await
            .map_err(|source| AdbError::Spawn {
                executable: self.executable.clone(),
                source,
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(AdbError::Command {
            args,
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

#[derive(Debug, Error)]
pub enum AdbError {
    #[error("failed to start adb at {executable}: {source}")]
    Spawn {
        executable: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("adb command {args:?} failed (code {code:?}): {stderr}")]
    Command {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
    #[error("invalid adb operation: {0}")]
    InvalidOperation(&'static str),
}
