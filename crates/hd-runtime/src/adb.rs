use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use hd_core::{
    AdbConfigV2, BatteryStateV2, KeyActionV2, NetworkConditionV2, OrientationV2, SensorInjectionV2,
};
use thiserror::Error;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_STATE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_RETRY_DELAY: Duration = Duration::from_secs(2);
const CONNECT_OFFLINE_RETRY_DELAY: Duration = Duration::from_secs(5);
const POWEROFF_TIMEOUT: Duration = Duration::from_secs(10);
const INSTALL_TIMEOUT: Duration = Duration::from_mins(5);
const READY_TIMEOUT: Duration = Duration::from_mins(5);
const ORIENTATION_VERIFY_TIMEOUT: Duration = Duration::from_secs(5);
const ORIENTATION_RETRY_DELAY: Duration = Duration::from_millis(250);
const OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;

/// Bounded ADB adapter. Callers can select only the fixed operations below; arbitrary shell
/// strings are intentionally not exposed by the production API.
#[derive(Debug, Clone)]
pub struct AdbClient {
    executable: PathBuf,
    aapt2: Option<PathBuf>,
    sensor_injector: Option<PathBuf>,
}

impl AdbClient {
    pub fn new(executable: PathBuf, aapt2: Option<PathBuf>) -> Self {
        Self {
            executable,
            aapt2,
            sensor_injector: None,
        }
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

    #[must_use]
    pub fn with_sensor_injector(mut self, executable: Option<PathBuf>) -> Self {
        self.sensor_injector = executable;
        self
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub async fn connect(&self, serial: &str) -> Result<(), AdbError> {
        validate_serial(serial)?;
        let started = Instant::now();
        let mut last_error = None;
        tracing::info!(event = "adb.connect.started", %serial, "connecting ADB");
        loop {
            let connect_error = match self
                .run(
                    vec!["connect".to_owned(), serial.to_owned()],
                    CONNECT_STATE_TIMEOUT,
                )
                .await
            {
                Ok(output) => {
                    let detail = String::from_utf8_lossy(&output).trim().to_owned();
                    tracing::debug!(event = "adb.connect.attempt", %serial, detail);
                    None
                }
                Err(error) => {
                    let detail = error.to_string();
                    tracing::debug!(event = "adb.connect.retry", %serial, %error);
                    Some(detail)
                }
            };
            if let Some(error) = connect_error {
                last_error = Some(error);
            }
            match self
                .run(
                    vec!["-s".to_owned(), serial.to_owned(), "get-state".to_owned()],
                    CONNECT_STATE_TIMEOUT,
                )
                .await
            {
                Ok(output) => {
                    let state = String::from_utf8_lossy(&output).trim().to_owned();
                    if state == "device" {
                        tracing::info!(
                            event = "adb.connect.succeeded",
                            %serial,
                            elapsed_ms = started.elapsed().as_millis(),
                            "ADB connected"
                        );
                        return Ok(());
                    }
                    if state == "offline" {
                        self.reset_offline_transport(serial).await;
                        last_error = Some("adb get-state returned offline".to_owned());
                        if started.elapsed() >= READY_TIMEOUT {
                            return Err(AdbError::ConnectTimeout {
                                serial: serial.to_owned(),
                                last_error: last_error.unwrap_or_else(|| {
                                    "ADB connection has not been attempted yet".to_owned()
                                }),
                            });
                        }
                        tokio::time::sleep(CONNECT_OFFLINE_RETRY_DELAY).await;
                        continue;
                    }
                    last_error = Some(format!("adb get-state returned {state:?}"));
                }
                Err(error) => {
                    let offline = is_offline_adb_error(&error);
                    let previous = last_error.take();
                    last_error = Some(previous.map_or_else(
                        || error.to_string(),
                        |connect| format!("{connect}; {error}"),
                    ));
                    tracing::debug!(event = "adb.connect.state_unavailable", %serial, %error);
                    if offline {
                        self.reset_offline_transport(serial).await;
                        if started.elapsed() >= READY_TIMEOUT {
                            return Err(AdbError::ConnectTimeout {
                                serial: serial.to_owned(),
                                last_error: last_error.unwrap_or_else(|| {
                                    "ADB connection has not been attempted yet".to_owned()
                                }),
                            });
                        }
                        tokio::time::sleep(CONNECT_OFFLINE_RETRY_DELAY).await;
                        continue;
                    }
                }
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(AdbError::ConnectTimeout {
                    serial: serial.to_owned(),
                    last_error: last_error
                        .unwrap_or_else(|| "ADB connection has not been attempted yet".to_owned()),
                });
            }
            tokio::time::sleep(CONNECT_RETRY_DELAY).await;
        }
    }

    /// Requests the fixed Android power-off operation. This deliberately does not expose a
    /// general shell surface to Host callers.
    pub async fn power_off(&self, serial: &str) -> Result<(), AdbError> {
        validate_serial(serial)?;
        self.run(
            vec![
                "-s".to_owned(),
                serial.to_owned(),
                "shell".to_owned(),
                "reboot".to_owned(),
                "-p".to_owned(),
            ],
            POWEROFF_TIMEOUT,
        )
        .await?;
        tracing::info!(event = "adb.power_off.requested", %serial, "requested Android power off");
        Ok(())
    }

    async fn reset_offline_transport(&self, serial: &str) {
        tracing::warn!(
            event = "adb.connect.offline_transport_reset",
            %serial,
            "ADB transport is offline; resetting only the instance transport before retry"
        );
        if let Err(error) = self
            .run(
                vec!["disconnect".to_owned(), serial.to_owned()],
                CONNECT_STATE_TIMEOUT,
            )
            .await
        {
            tracing::debug!(
                event = "adb.connect.offline_disconnect_ignored",
                %serial,
                %error,
                "ignoring ADB disconnect failure while resetting offline transport"
            );
        }
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
        let expected = orientation.android_rotation().to_string();
        self.shell(
            serial,
            &["settings", "put", "system", "accelerometer_rotation", "0"],
        )
        .await?;

        // Writing user_rotation directly races WindowManager's boot-time rotation policy.  Use
        // its supported shell surface to establish a rotation lock, then wait for the backing
        // setting to converge.  Reasserting the lock is intentional: Android can finish applying
        // the initial sensor policy just after ADB becomes Ready and otherwise overwrite the first
        // request (observed during repeated cold starts).
        let deadline = Instant::now() + ORIENTATION_VERIFY_TIMEOUT;
        loop {
            self.shell(serial, &["wm", "user-rotation", "lock", &expected])
                .await?;
            let actual = self
                .shell(serial, &["settings", "get", "system", "user_rotation"])
                .await?;
            if actual.trim() == expected {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(AdbError::OrientationVerification {
                    expected,
                    actual: actual.trim().to_owned(),
                });
            }
            tokio::time::sleep(ORIENTATION_RETRY_DELAY).await;
        }
    }

    pub async fn send_key(&self, serial: &str, key: KeyActionV2) -> Result<(), AdbError> {
        let key_code = match key {
            KeyActionV2::Home => "3",
            KeyActionV2::Recent => "187",
            KeyActionV2::Back => "4",
            KeyActionV2::Power => "26",
            KeyActionV2::VolumeUp => "24",
            KeyActionV2::VolumeDown => "25",
        };
        self.shell(serial, &["input", "keyevent", key_code]).await?;
        Ok(())
    }

    /// Applies the fixed Android battery simulation surface and verifies every requested field.
    /// The public Host API remains typed; callers cannot provide arbitrary shell arguments.
    pub async fn set_battery(
        &self,
        serial: &str,
        battery: &BatteryStateV2,
    ) -> Result<(), AdbError> {
        let level = battery.level_percent.to_string();
        let temperature = battery.temperature_deci_celsius.to_string();
        let powered = if battery.charging { "1" } else { "0" };
        let status = if battery.charging { "2" } else { "3" };
        for arguments in [
            ["dumpsys", "battery", "set", "level", &level],
            ["dumpsys", "battery", "set", "temp", &temperature],
            ["dumpsys", "battery", "set", "ac", powered],
            ["dumpsys", "battery", "set", "usb", "0"],
            ["dumpsys", "battery", "set", "wireless", "0"],
            ["dumpsys", "battery", "set", "status", status],
        ] {
            self.shell(serial, &arguments).await?;
        }
        let actual = self.shell(serial, &["dumpsys", "battery"]).await?;
        let expected_powered = battery.charging.to_string();
        if battery_field(&actual, "AC powered") != Some(expected_powered.as_str())
            || battery_field(&actual, "USB powered") != Some("false")
            || battery_field(&actual, "Wireless powered") != Some("false")
            || battery_field(&actual, "status") != Some(status)
            || battery_field(&actual, "level") != Some(level.as_str())
            || battery_field(&actual, "temperature") != Some(temperature.as_str())
        {
            return Err(AdbError::BatteryVerification {
                expected: format!(
                    "charging={}, status={status}, level={level}, temperature={temperature}",
                    battery.charging
                ),
                actual,
            });
        }
        Ok(())
    }

    /// Applies network emulation to the fixed Android phone-profile data interface and verifies
    /// the resulting qdisc. `su 0` is available only in the signed userdebug Guest profile; the
    /// Host API remains typed and does not expose an arbitrary privileged shell surface.
    pub async fn set_network_condition(
        &self,
        serial: &str,
        condition: &NetworkConditionV2,
    ) -> Result<(), AdbError> {
        let latency = format!("{}ms", condition.latency_ms);
        let loss = netem_loss(condition.loss_basis_points);
        let mut arguments = vec![
            "su".to_owned(),
            "0".to_owned(),
            "tc".to_owned(),
            "qdisc".to_owned(),
            "replace".to_owned(),
            "dev".to_owned(),
            "eth1".to_owned(),
            "root".to_owned(),
            "netem".to_owned(),
            "delay".to_owned(),
            latency.clone(),
            "loss".to_owned(),
            loss.clone(),
        ];
        let rate = condition.bandwidth_kbps.map(|value| format!("{value}kbit"));
        if let Some(rate) = &rate {
            arguments.extend(["rate".to_owned(), rate.clone()]);
        }
        let argument_refs = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        self.shell(serial, &argument_refs).await?;

        let actual = self
            .shell(serial, &["su", "0", "tc", "qdisc", "show", "dev", "eth1"])
            .await?;
        let latency_value = format!("{}.{:01}ms", condition.latency_ms, 0);
        let loss_value = netem_loss(condition.loss_basis_points);
        // `tc` omits zero-valued netem fields from `qdisc show` (for example the
        // default condition is rendered as just `qdisc netem ... limit 1000`).
        // Treat an omitted zero as zero while retaining exact checks for every
        // non-zero value.
        let latency_matches =
            condition.latency_ms == 0 || actual.contains(&format!("delay {latency_value}"));
        let loss_matches =
            condition.loss_basis_points == 0 || actual.contains(&format!("loss {loss_value}"));
        if !actual.contains("qdisc netem")
            || !latency_matches
            || !loss_matches
            || condition
                .bandwidth_kbps
                .is_some_and(|expected| netem_rate_kbps(&actual) != Some(u64::from(expected)))
        {
            return Err(AdbError::NetworkVerification {
                expected: format!(
                    "latency={latency}, loss={loss}, bandwidth={:?}",
                    condition.bandwidth_kbps
                ),
                actual,
            });
        }
        Ok(())
    }

    /// Uses Android 15 `SensorService`'s HAL-bypass replay mode so all five fixed profile sensors
    /// are injected into the framework even when the selected Guest uses the AOSP example HAL.
    pub async fn inject_sensor(
        &self,
        serial: &str,
        injection: &SensorInjectionV2,
    ) -> Result<(), AdbError> {
        const REMOTE: &str = "/data/local/tmp/hd-sensor-injector";

        validate_serial(serial)?;
        let executable = self
            .sensor_injector
            .as_ref()
            .ok_or(AdbError::SensorInjectorUnavailable)?;
        if !executable.is_file() {
            return Err(AdbError::SensorInjectorInvalid(executable.clone()));
        }
        self.run(
            vec![
                "-s".to_owned(),
                serial.to_owned(),
                "push".to_owned(),
                executable.to_string_lossy().into_owned(),
                REMOTE.to_owned(),
            ],
            COMMAND_TIMEOUT,
        )
        .await?;
        self.shell(serial, &["chmod", "0755", REMOTE]).await?;
        self.shell(
            serial,
            &[
                "su",
                "0",
                "dumpsys",
                "sensorservice",
                "hal_bypass_replay_data_injection",
                "dev.hd.sensor_injector",
            ],
        )
        .await?;

        let duration = injection.duration_ms.to_string();
        let values = injection
            .values_microunits
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let mut arguments = vec![
            "su",
            "0",
            REMOTE,
            injection.sensor.as_str(),
            duration.as_str(),
        ];
        arguments.extend(values.iter().map(String::as_str));
        let injection_result = self.shell(serial, &arguments).await;
        let reset_result = self
            .shell(serial, &["su", "0", "dumpsys", "sensorservice", "enable"])
            .await;
        injection_result?;
        reset_result?;

        let actual = self.shell(serial, &["dumpsys", "sensorservice"]).await?;
        if !actual.contains("Mode : NORMAL") {
            return Err(AdbError::SensorVerification(actual));
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

    pub async fn screenshot(&self, serial: &str) -> Result<Vec<u8>, AdbError> {
        validate_serial(serial)?;
        let bytes = self
            .run(
                vec![
                    "-s".to_owned(),
                    serial.to_owned(),
                    "exec-out".to_owned(),
                    "screencap".to_owned(),
                    "-p".to_owned(),
                ],
                COMMAND_TIMEOUT,
            )
            .await?;
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(AdbError::ScreenshotFormat);
        }
        Ok(bytes)
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
    hd_platform::configure_transient_command(command.as_std_mut()).map_err(|source| {
        AdbError::Spawn {
            executable: executable.to_owned(),
            source: std::io::Error::other(source),
        }
    })?;
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
        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);
        let output = match (stdout.trim(), stderr.trim()) {
            ("", stderr) => stderr.to_owned(),
            (stdout, "") => stdout.to_owned(),
            (stdout, stderr) => format!("{stderr}\n{stdout}"),
        };
        Err(AdbError::Command {
            args,
            code: status.code(),
            output,
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
        return Err(std::io::Error::other("command output exceeded 16 MiB"));
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

fn battery_field<'a>(output: &'a str, field: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix(field)
            .and_then(|value| value.strip_prefix(':'))
            .map(str::trim)
    })
}

fn netem_loss(loss_basis_points: u16) -> String {
    let mut value = format!("{}.{:02}", loss_basis_points / 100, loss_basis_points % 100);
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    value.push('%');
    value
}

fn netem_rate_kbps(output: &str) -> Option<u64> {
    let mut fields = output.split_whitespace();
    while let Some(field) = fields.next() {
        if field.eq_ignore_ascii_case("rate") {
            return fields.next().and_then(rate_token_kbps);
        }
    }
    None
}

fn rate_token_kbps(token: &str) -> Option<u64> {
    let normalized = token.to_ascii_lowercase();
    let (number, scale) = if let Some(number) = normalized.strip_suffix("kbit") {
        (number, 1_u64)
    } else if let Some(number) = normalized.strip_suffix("mbit") {
        (number, 1_000_u64)
    } else if let Some(number) = normalized.strip_suffix("gbit") {
        (number, 1_000_000_u64)
    } else {
        return None;
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let denominator = 10_u64.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let whole = whole.parse::<u64>().ok()?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().ok()?
    };
    let scaled = whole
        .checked_mul(denominator)?
        .checked_add(fraction)?
        .checked_mul(scale)?;
    (scaled % denominator == 0).then_some(scaled / denominator)
}

fn is_offline_adb_error(error: &AdbError) -> bool {
    error.to_string().to_ascii_lowercase().contains("offline")
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
    #[error("command {args:?} failed (code {code:?}): {output}")]
    Command {
        args: Vec<String>,
        code: Option<i32>,
        output: String,
    },
    #[error("ADB transport for {serial} did not become online: {last_error}")]
    ConnectTimeout { serial: String, last_error: String },
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
    #[error("battery verification failed: expected {expected}, actual {actual}")]
    BatteryVerification { expected: String, actual: String },
    #[error("network condition verification failed: expected {expected}, actual {actual}")]
    NetworkVerification { expected: String, actual: String },
    #[error("the verified host bundle has no Android sensor injector")]
    SensorInjectorUnavailable,
    #[error("Android sensor injector is not a regular file: {0}")]
    SensorInjectorInvalid(PathBuf),
    #[error("sensor injection cleanup verification failed: {0}")]
    SensorVerification(String),
    #[error("guest screenshot did not return a PNG image")]
    ScreenshotFormat,
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
