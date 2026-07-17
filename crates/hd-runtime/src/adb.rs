use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use hd_core::{AdbConfigV2, OrientationV2};
use thiserror::Error;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
const READY_TIMEOUT: Duration = Duration::from_secs(300);
const OUTPUT_LIMIT: u64 = 1024 * 1024;

/// Bounded ADB adapter. Callers can select only the fixed operations below; arbitrary shell
/// strings are intentionally not exposed by the production API.
#[derive(Debug, Clone)]
pub struct AdbClient {
    executable: PathBuf,
    aapt2: Option<PathBuf>,
}

impl AdbClient {
    pub fn new(executable: PathBuf, aapt2: Option<PathBuf>) -> Self {
        Self { executable, aapt2 }
    }

    pub fn from_config(config: &AdbConfigV2, bundled: Option<&Path>) -> Self {
        let executable = config
            .executable
            .clone()
            .or_else(|| bundled.map(Path::to_owned))
            .unwrap_or_else(|| PathBuf::from(hd_platform::executable_name("adb")));
        Self::new(executable, None)
    }

    #[must_use]
    pub fn with_aapt2(mut self, executable: Option<PathBuf>) -> Self {
        self.aapt2 = executable;
        self
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub async fn connect(&self, serial: &str) -> Result<(), AdbError> {
        validate_serial(serial)?;
        tracing::info!(event = "adb.connect.started", %serial, "connecting ADB");
        self.run(
            vec!["connect".to_owned(), serial.to_owned()],
            COMMAND_TIMEOUT,
        )
        .await?;
        self.run(
            vec![
                "-s".to_owned(),
                serial.to_owned(),
                "wait-for-device".to_owned(),
            ],
            COMMAND_TIMEOUT,
        )
        .await?;
        tracing::info!(event = "adb.connect.succeeded", %serial, "ADB connected");
        Ok(())
    }

    /// Ready is a single conjunction: Android boot completed, boot animation exited, package
    /// manager responds, and `SurfaceFlinger` is registered. No earlier state is reported as Ready.
    pub async fn wait_ready(&self, serial: &str) -> Result<(), AdbError> {
        validate_serial(serial)?;
        let started = Instant::now();
        tracing::info!(event = "adb.readiness.started", %serial, "waiting for guest readiness");
        loop {
            let boot_completed = self.getprop(serial, "sys.boot_completed").await;
            let boot_animation = self.getprop(serial, "service.bootanim.exit").await;
            let package_manager = self
                .shell(serial, &["cmd", "package", "path", "android"])
                .await;
            let surface_flinger = self
                .shell(serial, &["service", "check", "SurfaceFlinger"])
                .await;
            if boot_completed.as_ref().is_ok_and(|value| value == "1")
                && boot_animation.as_ref().is_ok_and(|value| value == "1")
                && package_manager
                    .as_ref()
                    .is_ok_and(|value| value.contains("package:"))
                && surface_flinger
                    .as_ref()
                    .is_ok_and(|value| value.contains("found"))
            {
                tracing::info!(
                    event = "adb.readiness.succeeded",
                    %serial,
                    elapsed_ms = started.elapsed().as_millis(),
                    "guest readiness confirmed"
                );
                return Ok(());
            }
            if started.elapsed() >= READY_TIMEOUT {
                tracing::error!(
                    event = "adb.readiness.failed",
                    error_code = "adb_readiness_timeout",
                    %serial,
                    "guest readiness timed out"
                );
                return Err(AdbError::ReadinessTimeout {
                    serial: serial.to_owned(),
                    boot_completed: boot_completed.ok(),
                    boot_animation: boot_animation.ok(),
                    package_manager: package_manager.ok(),
                    surface_flinger: surface_flinger.ok(),
                });
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    pub async fn install_and_verify(&self, serial: &str, apk: &Path) -> Result<String, AdbError> {
        validate_serial(serial)?;
        if !apk.is_file() {
            return Err(AdbError::InvalidApk(apk.to_owned()));
        }
        let package_name = self.package_name(apk).await?;
        tracing::info!(
            event = "adb.install.started",
            %serial,
            package = %package_name,
            "installing APK"
        );
        self.run(
            vec![
                "-s".to_owned(),
                serial.to_owned(),
                "install".to_owned(),
                "-r".to_owned(),
                "--no-streaming".to_owned(),
                apk.to_string_lossy().into_owned(),
            ],
            INSTALL_TIMEOUT,
        )
        .await?;
        let installed = self
            .shell(serial, &["cmd", "package", "path", &package_name])
            .await?;
        if !installed.lines().any(|line| line.starts_with("package:")) {
            return Err(AdbError::InstallVerification(package_name));
        }
        tracing::info!(
            event = "adb.install.succeeded",
            %serial,
            package = %package_name,
            "APK installation verified"
        );
        Ok(package_name)
    }

    pub async fn set_orientation(
        &self,
        serial: &str,
        orientation: OrientationV2,
    ) -> Result<(), AdbError> {
        self.shell(
            serial,
            &["settings", "put", "system", "accelerometer_rotation", "0"],
        )
        .await?;
        self.shell(
            serial,
            &[
                "settings",
                "put",
                "system",
                "user_rotation",
                &orientation.android_rotation().to_string(),
            ],
        )
        .await?;
        let actual = self
            .shell(serial, &["settings", "get", "system", "user_rotation"])
            .await?;
        let expected = orientation.android_rotation().to_string();
        if actual.trim() != expected {
            return Err(AdbError::OrientationVerification {
                expected,
                actual: actual.trim().to_owned(),
            });
        }
        Ok(())
    }

    pub async fn guest_logcat(&self, serial: &str) -> Result<Vec<u8>, AdbError> {
        self.run(
            vec![
                "-s".to_owned(),
                serial.to_owned(),
                "logcat".to_owned(),
                "-d".to_owned(),
                "-t".to_owned(),
                "2000".to_owned(),
            ],
            COMMAND_TIMEOUT,
        )
        .await
    }

    async fn getprop(&self, serial: &str, property: &str) -> Result<String, AdbError> {
        self.shell(serial, &["getprop", property])
            .await
            .map(|value| value.trim().to_owned())
    }

    async fn shell(&self, serial: &str, arguments: &[&str]) -> Result<String, AdbError> {
        validate_serial(serial)?;
        let mut args = vec!["-s".to_owned(), serial.to_owned(), "shell".to_owned()];
        args.extend(arguments.iter().map(|value| (*value).to_owned()));
        self.run(args, COMMAND_TIMEOUT)
            .await
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
    }

    async fn package_name(&self, apk: &Path) -> Result<String, AdbError> {
        let executable = self.aapt2.as_ref().ok_or(AdbError::Aapt2Unavailable)?;
        let output = run_capped(
            executable,
            vec![
                "dump".to_owned(),
                "badging".to_owned(),
                apk.to_string_lossy().into_owned(),
            ],
            COMMAND_TIMEOUT,
        )
        .await?;
        let text = String::from_utf8_lossy(&output);
        let first = text
            .lines()
            .find(|line| line.starts_with("package: "))
            .ok_or_else(|| AdbError::InvalidPackageMetadata(apk.to_owned()))?;
        let name = first
            .split_whitespace()
            .find_map(|part| {
                part.strip_prefix("name='")
                    .and_then(|v| v.strip_suffix('\''))
            })
            .ok_or_else(|| AdbError::InvalidPackageMetadata(apk.to_owned()))?;
        if !valid_package_name(name) {
            return Err(AdbError::InvalidPackageMetadata(apk.to_owned()));
        }
        Ok(name.to_owned())
    }

    async fn run(&self, args: Vec<String>, timeout: Duration) -> Result<Vec<u8>, AdbError> {
        run_capped(&self.executable, args, timeout).await
    }
}

async fn run_capped(
    executable: &Path,
    args: Vec<String>,
    timeout: Duration,
) -> Result<Vec<u8>, AdbError> {
    let mut command = Command::new(executable);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|source| AdbError::Spawn {
        executable: executable.to_owned(),
        source,
    })?;
    let stdout = child.stdout.take().ok_or(AdbError::Capture)?;
    let stderr = child.stderr.take().ok_or(AdbError::Capture)?;
    let operation = async move {
        let stdout_task = read_limited(stdout);
        let stderr_task = read_limited(stderr);
        let (stdout, stderr) = tokio::try_join!(stdout_task, stderr_task)?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, stdout, stderr))
    };
    let (status, stdout, stderr) = tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| AdbError::Timeout { args: args.clone() })?
        .map_err(|source| AdbError::Io {
            args: args.clone(),
            source,
        })?;
    if status.success() {
        Ok(stdout)
    } else {
        Err(AdbError::Command {
            args,
            code: status.code(),
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        })
    }
}

async fn read_limited<R: tokio::io::AsyncRead + Unpin>(reader: R) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        return Err(std::io::Error::other("command output exceeded 1 MiB"));
    }
    Ok(bytes)
}

fn validate_serial(serial: &str) -> Result<(), AdbError> {
    let Some(port) = serial.strip_prefix("127.0.0.1:") else {
        return Err(AdbError::InvalidSerial(serial.to_owned()));
    };
    if port.parse::<u16>().is_err() || port == "0" {
        return Err(AdbError::InvalidSerial(serial.to_owned()));
    }
    Ok(())
}

fn valid_package_name(name: &str) -> bool {
    name.len() <= 255
        && name.contains('.')
        && name.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
}

#[derive(Debug, Error)]
pub enum AdbError {
    #[error("failed to start command at {executable}: {source}")]
    Spawn {
        executable: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to capture command output")]
    Capture,
    #[error("command {args:?} I/O failed: {source}")]
    Io {
        args: Vec<String>,
        #[source]
        source: std::io::Error,
    },
    #[error("command {args:?} timed out")]
    Timeout { args: Vec<String> },
    #[error("command {args:?} failed (code {code:?}): {stderr}")]
    Command {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
    #[error("ADB serial must be an IPv4 loopback endpoint: {0}")]
    InvalidSerial(String),
    #[error("APK is not a regular file: {0}")]
    InvalidApk(PathBuf),
    #[error("aapt2 from the verified host bundle is required")]
    Aapt2Unavailable,
    #[error("APK package metadata is invalid: {0}")]
    InvalidPackageMetadata(PathBuf),
    #[error("package {0} was not present after adb install")]
    InstallVerification(String),
    #[error("orientation verification failed: expected {expected}, actual {actual}")]
    OrientationVerification { expected: String, actual: String },
    #[error(
        "guest readiness timed out for {serial}; boot={boot_completed:?}, animation={boot_animation:?}, package={package_manager:?}, surface={surface_flinger:?}"
    )]
    ReadinessTimeout {
        serial: String,
        boot_completed: Option<String>,
        boot_animation: Option<String>,
        package_manager: Option<String>,
        surface_flinger: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_serials_are_accepted() {
        assert!(validate_serial("127.0.0.1:6520").is_ok());
        assert!(validate_serial("192.168.1.2:5555").is_err());
    }

    #[test]
    fn package_names_are_strict() {
        assert!(valid_package_name("com.example.app"));
        assert!(!valid_package_name("bad-name"));
    }
}
