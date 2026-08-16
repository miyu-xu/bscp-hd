use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use hd_core::{
    AdbConfigV2, BatteryStateV2, DisplayConfigV2, KeyActionV2, NetworkConditionV2, OrientationV2,
    SensorInjectionV2, SensorMotionFrameV2,
};
use thiserror::Error;
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, Command};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
// A cold Windows SDK adb server can take more than five seconds to fork, initialize USB, and
// acknowledge its launcher. Keep connect/state probes bounded while allowing that supported path
// to complete instead of repeatedly terminating the daemon during startup.
const CONNECT_STATE_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_RETRY_DELAY: Duration = Duration::from_secs(2);
const CONNECT_OFFLINE_RETRY_DELAY: Duration = Duration::from_secs(5);
const POWEROFF_TIMEOUT: Duration = Duration::from_secs(10);
const POWEROFF_ROOT_RETRY_DELAY: Duration = Duration::from_millis(100);
const INSTALL_TIMEOUT: Duration = Duration::from_mins(5);
const READY_TIMEOUT: Duration = Duration::from_mins(5);
const READY_STABLE_SAMPLES: u8 = 2;
const READY_STABLE_SAMPLE_DELAY: Duration = Duration::from_millis(500);
const READY_RETRY_DELAY: Duration = Duration::from_secs(2);
const READY_PACKAGE_HANDLER_TIMEOUT: Duration = Duration::from_secs(7);
const READY_PACKAGE_HANDLER_TIMEOUT_MILLIS: &str = "5000";
const ORIENTATION_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const ORIENTATION_REASSERT_DELAY: Duration = Duration::from_secs(4);
const ORIENTATION_RETRY_DELAY: Duration = Duration::from_millis(250);
const ORIENTATION_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const SCREEN_RECORDING_START_TIMEOUT: Duration = Duration::from_secs(10);
const SCREEN_RECORDING_START_ATTEMPTS: u8 = 2;
const SCREEN_RECORDING_STOP_TIMEOUT: Duration = Duration::from_secs(15);
const SCREEN_RECORDING_GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const SCREEN_RECORDING_CLIENT_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
const SCREEN_RECORDING_STOP_RETRY_DELAY: Duration = Duration::from_millis(100);
const SCREEN_RECORDING_TRANSPORT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(20);
const BUGREPORT_TIMEOUT: Duration = Duration::from_mins(10);
pub const MAX_ANDROID_BUGREPORT_BYTES: u64 = 256 * 1024 * 1024;
const NETWORK_VALIDATE_TIMEOUT: Duration = Duration::from_secs(30);
const NETWORK_VALIDATE_RETRY_DELAY: Duration = Duration::from_secs(1);
const OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;

fn readiness_retry_delay(sample_ready: bool) -> Duration {
    if sample_ready {
        READY_STABLE_SAMPLE_DELAY
    } else {
        READY_RETRY_DELAY
    }
}

/// Bounded ADB adapter. Callers can select only the fixed operations below; arbitrary shell
/// strings are intentionally not exposed by the production API.
#[derive(Debug, Clone)]
pub struct AdbClient {
    executable: PathBuf,
    aapt2: Option<PathBuf>,
    sensor_injector: Option<PathBuf>,
}

#[derive(Debug)]
pub struct AdbScreenRecording {
    child: Child,
    remote_path: String,
    remote_pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidNetworkHealth {
    pub active_network: Option<String>,
    pub interface: Option<String>,
    pub validated: bool,
    pub has_dns: bool,
    pub has_default_route: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidDeviceRuntimeHealth {
    pub id: String,
    pub installed: bool,
    pub configured: bool,
    pub running: bool,
    pub controllable: bool,
    pub verified: bool,
    pub detail: String,
}

impl AndroidNetworkHealth {
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.active_network.is_some() && self.validated && self.has_dns && self.has_default_route
    }

    #[must_use]
    pub fn detail(&self) -> String {
        format!(
            "active_network={}, interface={}, validated={}, dns={}, default_route={}",
            self.active_network.as_deref().unwrap_or("none"),
            self.interface.as_deref().unwrap_or("unknown"),
            self.validated,
            self.has_dns,
            self.has_default_route
        )
    }
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

    /// Generate one full AOSP dumpstate archive at a Host-selected path. The public Host API never
    /// accepts a path or shell fragment from callers; Worker validates the managed directory before
    /// reaching this fixed ADB operation.
    pub async fn collect_android_bugreport(
        &self,
        serial: &str,
        output_path: &Path,
    ) -> Result<u64, AdbError> {
        validate_serial(serial)?;
        let file = hd_platform::open_owner_only_rw(output_path)?;
        file.set_len(0).map_err(|source| AdbError::BugreportIo {
            path: output_path.to_owned(),
            source,
        })?;
        drop(file);
        let args = vec![
            "-s".to_owned(),
            serial.to_owned(),
            "bugreport".to_owned(),
            output_path.to_string_lossy().into_owned(),
        ];
        run_bugreport_bounded(&self.executable, args, output_path).await?;
        drop(hd_platform::open_owner_only_rw(output_path)?);
        validate_android_bugreport_zip(output_path)
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

    /// Requests Android power-off through its Cuttlefish shell policy. Android permits this
    /// fixed command without changing the adbd privilege state.
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

    /// Requests Microdroid power-off through AOSP's Full-debug adbd control path.
    ///
    /// Microdroid's Full debug policy intentionally permits `adb root`, but its default shell
    /// domain cannot write `sys.powerctl`. Restarting adbd as root before the dedicated reboot
    /// operation follows the AOSP Full-debug contract without weakening the guest `SELinux` policy.
    /// The fixed sequence remains deliberately narrower than exposing a general shell surface to
    /// Host callers.
    pub async fn power_off_debuggable(&self, serial: &str) -> Result<(), AdbError> {
        validate_serial(serial)?;

        let mut identity = self
            .shell_with_timeout(serial, &["id"], CONNECT_STATE_TIMEOUT)
            .await
            .unwrap_or_default();
        if !identity.contains("uid=0(root)") {
            let root_output = self
                .run(
                    vec!["-s".to_owned(), serial.to_owned(), "root".to_owned()],
                    POWEROFF_TIMEOUT,
                )
                .await
                .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())?;
            let deadline = Instant::now() + POWEROFF_TIMEOUT;
            loop {
                match self
                    .shell_with_timeout(serial, &["id"], CONNECT_STATE_TIMEOUT)
                    .await
                {
                    Ok(output) => {
                        identity = output;
                        if identity.contains("uid=0(root)") {
                            break;
                        }
                    }
                    Err(error) => identity = error.to_string(),
                }
                if Instant::now() >= deadline {
                    return Err(AdbError::PowerOffPrivilege {
                        serial: serial.to_owned(),
                        detail: format!(
                            "adb root output: {root_output}; last identity: {identity}"
                        ),
                    });
                }
                tokio::time::sleep(POWEROFF_ROOT_RETRY_DELAY).await;
            }
        }

        self.run(
            vec![
                "-s".to_owned(),
                serial.to_owned(),
                "reboot".to_owned(),
                "-p".to_owned(),
            ],
            POWEROFF_TIMEOUT,
        )
        .await?;
        tracing::info!(
            event = "adb.debuggable_power_off.requested",
            %serial,
            "requested debuggable Guest power off"
        );
        Ok(())
    }

    pub async fn disconnect(&self, serial: &str) -> Result<(), AdbError> {
        validate_serial(serial)?;
        self.run(
            vec!["disconnect".to_owned(), serial.to_owned()],
            CONNECT_STATE_TIMEOUT,
        )
        .await?;
        tracing::info!(
            event = "adb.disconnect.succeeded",
            %serial,
            "removed the stopped instance transport from the ADB server"
        );
        Ok(())
    }

    async fn reset_offline_transport(&self, serial: &str) {
        tracing::warn!(
            event = "adb.connect.offline_transport_reset",
            %serial,
            "ADB transport is offline; resetting only the instance transport before retry"
        );
        if let Err(error) = self.disconnect(serial).await {
            tracing::debug!(
                event = "adb.connect.offline_disconnect_ignored",
                %serial,
                %error,
                "ignoring ADB disconnect failure while resetting offline transport"
            );
        }
    }

    async fn package_manager_background_handler_idle(
        &self,
        serial: &str,
        elapsed: Duration,
    ) -> bool {
        let result = self
            .shell_with_timeout(
                serial,
                &[
                    "cmd",
                    "package",
                    "wait-for-background-handler",
                    "--timeout",
                    READY_PACKAGE_HANDLER_TIMEOUT_MILLIS,
                ],
                READY_PACKAGE_HANDLER_TIMEOUT,
            )
            .await;
        match result {
            Ok(output) if output.trim().eq_ignore_ascii_case("success") => {
                tracing::info!(
                    event = "adb.readiness.package_background_handler_idle",
                    %serial,
                    elapsed_ms = elapsed.as_millis(),
                    "PackageManager background handler is idle"
                );
                true
            }
            Ok(output) => {
                tracing::debug!(
                    event = "adb.readiness.package_background_handler_busy",
                    %serial,
                    %output,
                    "PackageManager background handler has not drained yet"
                );
                false
            }
            Err(error) => {
                tracing::debug!(
                    event = "adb.readiness.package_background_handler_unavailable",
                    %serial,
                    %error,
                    "PackageManager background-handler drain probe will retry"
                );
                false
            }
        }
    }

    /// Ready is a stable conjunction: Android boot completed, boot animation exited, the primary
    /// user is unlocked, package manager responds, and the graphics and interactive services are
    /// registered. The complete conjunction must hold for consecutive samples; no transient state
    /// is reported as Ready. Global broadcast-idle is deliberately not required because Android
    /// background receivers may keep that queue busy long after the device is interactive.
    pub async fn wait_ready(&self, serial: &str) -> Result<(), AdbError> {
        validate_serial(serial)?;
        let started = Instant::now();
        let mut stable_samples = 0_u8;
        tracing::info!(event = "adb.readiness.started", %serial, "waiting for guest readiness");
        loop {
            let boot_completed = self.getprop(serial, "sys.boot_completed").await;
            let boot_animation_service = self.getprop(serial, "init.svc.bootanim").await;
            let boot_animation_exit = self.getprop(serial, "service.bootanim.exit").await;
            let boot_animation_process = self.boot_animation_process_stopped(serial).await;
            let package_manager = self
                .shell(serial, &["cmd", "package", "path", "android"])
                .await;
            let surface_flinger = self
                .shell(serial, &["service", "check", "SurfaceFlinger"])
                .await;
            let input_manager = self.shell(serial, &["service", "check", "input"]).await;
            let window_manager = self.shell(serial, &["service", "check", "window"]).await;
            let user_unlocked = self
                .shell(serial, &["cmd", "user", "is-user-unlocked", "0"])
                .await;
            let sample_ready = boot_completed.as_ref().is_ok_and(|value| value == "1")
                && boot_animation_service
                    .as_ref()
                    .is_ok_and(|value| value == "stopped")
                && boot_animation_exit.as_ref().is_ok_and(|value| value == "1")
                && boot_animation_process
                    .as_ref()
                    .is_ok_and(|stopped| *stopped)
                && package_manager
                    .as_ref()
                    .is_ok_and(|value| value.contains("package:"))
                && surface_flinger
                    .as_ref()
                    .is_ok_and(|value| value.contains("found"))
                && input_manager
                    .as_ref()
                    .is_ok_and(|value| value.contains("found"))
                && window_manager
                    .as_ref()
                    .is_ok_and(|value| value.contains("found"))
                && user_unlocked
                    .as_ref()
                    .is_ok_and(|value| value.trim() == "true");
            if sample_ready {
                stable_samples = stable_samples.saturating_add(1);
                if stable_samples >= READY_STABLE_SAMPLES {
                    if self
                        .package_manager_background_handler_idle(serial, started.elapsed())
                        .await
                    {
                        tracing::info!(
                            event = "adb.readiness.succeeded",
                            %serial,
                            elapsed_ms = started.elapsed().as_millis(),
                            stable_samples,
                            "stable interactive guest readiness confirmed"
                        );
                        return Ok(());
                    }
                    stable_samples = 0;
                }
            } else {
                stable_samples = 0;
            }
            if started.elapsed() >= READY_TIMEOUT {
                tracing::error!(
                    event = "adb.readiness.failed",
                    error_code = "adb_readiness_timeout",
                    %serial,
                    "guest readiness timed out"
                );
                let detail = format!(
                    "boot={:?}, animation_service={:?}, animation_exit={:?}, animation_process={:?}, package={:?}, surface={:?}, input={:?}, window={:?}, user_unlocked={:?}",
                    boot_completed.ok(),
                    boot_animation_service.ok(),
                    boot_animation_exit.ok(),
                    boot_animation_process.ok(),
                    package_manager.ok(),
                    surface_flinger.ok(),
                    input_manager.ok(),
                    window_manager.ok(),
                    user_unlocked.ok(),
                );
                return Err(AdbError::ReadinessTimeout {
                    serial: serial.to_owned(),
                    detail: detail.into_boxed_str(),
                });
            }
            tokio::time::sleep(readiness_retry_delay(sample_ready)).await;
        }
    }

    pub async fn install_and_verify(&self, serial: &str, apk: &Path) -> Result<String, AdbError> {
        validate_serial(serial)?;
        if !apk.is_file() {
            return Err(AdbError::InvalidApk(apk.to_owned()));
        }
        let package_name = if self.aapt2.is_some() {
            Some(self.package_name(apk).await?)
        } else {
            None
        };
        let install_label = package_name.clone().unwrap_or_else(|| {
            apk.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("APK")
                .to_owned()
        });
        tracing::info!(
            event = "adb.install.started",
            %serial,
            package = %install_label,
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
        if let Some(package_name) = &package_name {
            let installed = self
                .shell(serial, &["cmd", "package", "path", package_name])
                .await?;
            if !installed.lines().any(|line| line.starts_with("package:")) {
                return Err(AdbError::InstallVerification(package_name.clone()));
            }
        }
        tracing::info!(
            event = "adb.install.succeeded",
            %serial,
            package = %install_label,
            metadata_verified = package_name.is_some(),
            "APK installation completed"
        );
        Ok(install_label)
    }

    pub async fn set_display_configuration(
        &self,
        serial: &str,
        display: &DisplayConfigV2,
    ) -> Result<(), AdbError> {
        // The primary density comes from the instance boot profile. Android applies that same
        // logical density to every internal HWC display even when a secondary EDID advertises a
        // different physical DPI, so converge the per-display overrides before publishing ADB
        // readiness. Gfxstream reserves even HWC display IDs for scanouts (0, 2, 4, 6).
        for (secondary_index, secondary) in display.secondary_displays.iter().enumerate() {
            let android_display_id = (u32::try_from(secondary_index)
                .expect("secondary display product limit fits in u32")
                + 1)
                * 2;
            self.set_display_density(serial, android_display_id, secondary.dpi)
                .await?;
        }

        self.set_display_configuration_orientation(serial, display)
            .await
    }

    pub async fn set_display_configuration_orientation(
        &self,
        serial: &str,
        display: &DisplayConfigV2,
    ) -> Result<(), AdbError> {
        #[cfg(windows)]
        let expected_rotation = self
            .prepare_windows_display_geometry(serial, display)
            .await?;
        #[cfg(not(windows))]
        let expected_rotation = display.orientation.android_rotation();

        self.set_display_rotation(serial, expected_rotation).await
    }

    async fn set_display_rotation(
        &self,
        serial: &str,
        expected_rotation: u8,
    ) -> Result<(), AdbError> {
        // Rotation must not rewrite primary `wm density`: that changes resources and can block
        // behind screenrecord display teardown.
        let expected = expected_rotation.to_string();

        // Launcher and other foreground apps can request SCREEN_ORIENTATION_NOSENSOR. Android 15
        // records a user rotation lock in that state but leaves the display at ROTATION_0 unless
        // fixed-to-user-rotation is enabled. Register the request once, then wait for the actual
        // DisplayRotation state: `wm user-rotation` only proves that the lock was accepted and can
        // lead the asynchronous display transition. Android can occasionally retain the previous
        // stable rotation after accepting a new lock. Reassert it once within the same deadline.
        self.prepare_display_rotation(serial, &expected).await?;
        let started_at = Instant::now();
        let deadline = started_at + ORIENTATION_VERIFY_TIMEOUT;
        let reassert_at = started_at + ORIENTATION_REASSERT_DELAY;
        let mut reasserted = false;
        let mut actual_rotation: Option<u8> = None;
        let mut query_timed_out: bool;
        let mut query_attempt = 0_u32;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AdbError::OrientationVerification {
                    expected,
                    actual: actual_rotation.map_or_else(
                        || "WindowManager query deadline expired".to_owned(),
                        |rotation| rotation.to_string(),
                    ),
                });
            }
            query_attempt += 1;
            match self
                .shell_with_timeout(
                    serial,
                    &["dumpsys", "window", "displays"],
                    remaining.min(ORIENTATION_COMMAND_TIMEOUT),
                )
                .await
            {
                Ok(display_state) => {
                    query_timed_out = false;
                    actual_rotation = window_manager_rotation(&display_state);
                    if actual_rotation == Some(expected_rotation)
                        && window_manager_transition_complete(&display_state)
                    {
                        return Ok(());
                    }
                }
                Err(AdbError::Timeout { .. }) => {
                    // DisplayRotation holds the WindowManager display lock briefly while applying
                    // the new configuration. A read-only dumpsys probe can hit that lock even
                    // after the preceding rotation has fully completed.
                    query_timed_out = true;
                    tracing::warn!(
                        event = "adb.display.rotation_probe.timed_out",
                        %serial,
                        query_attempt,
                        remaining_ms = deadline
                            .saturating_duration_since(Instant::now())
                            .as_millis(),
                        "WindowManager rotation probe timed out"
                    );
                }
                Err(error) => return Err(error),
            }
            if !reasserted
                && Instant::now() >= reassert_at
                && actual_rotation != Some(expected_rotation)
            {
                self.reassert_display_rotation(
                    serial,
                    &expected,
                    expected_rotation,
                    actual_rotation,
                )
                .await?;
                reasserted = true;
            }
            if Instant::now() >= deadline {
                return Err(AdbError::OrientationVerification {
                    expected,
                    actual: actual_rotation.map_or_else(
                        || {
                            if query_timed_out {
                                "WindowManager query timed out during display transition".to_owned()
                            } else {
                                "unavailable".to_owned()
                            }
                        },
                        |rotation| rotation.to_string(),
                    ),
                });
            }
            tokio::time::sleep(ORIENTATION_RETRY_DELAY).await;
        }
    }

    #[cfg(windows)]
    async fn prepare_windows_display_geometry(
        &self,
        serial: &str,
        display: &DisplayConfigV2,
    ) -> Result<u8, AdbError> {
        // The Win32 backend creates crosvm's DRM mode in the instance's initial *oriented* size.
        // Treat that physical mode as Android's natural display instead of forcing a portrait
        // override on top of it. The old portrait override made SurfaceFlinger project a
        // 1080x1920 device into a 1920x1080 scanout: apps were letterboxed to a narrow central
        // frame while gfxstream stretched the result, so rendering and pointer coordinates could
        // never agree. Keeping WindowManager on the physical mode makes its logical, physical,
        // input, and zero-copy presentation rectangles identical.
        let initial_size = self
            .shell_with_timeout(
                serial,
                &["wm", "size", "-d", "0"],
                ORIENTATION_COMMAND_TIMEOUT,
            )
            .await?;
        let physical =
            physical_wm_size(&initial_size).ok_or_else(|| AdbError::DisplaySizeVerification {
                expected: "physical display size".to_owned(),
                actual: initial_size.trim().to_owned(),
            })?;
        let configured = (
            display.width.min(display.height),
            display.width.max(display.height),
        );
        let physical_sorted = (physical.0.min(physical.1), physical.0.max(physical.1));
        if physical_sorted != configured {
            return Err(AdbError::DisplaySizeVerification {
                expected: format!("{}x{} in either orientation", configured.0, configured.1),
                actual: format!("{}x{}", physical.0, physical.1),
            });
        }

        let expected = format!("{}x{}", physical.0, physical.1);
        let deadline = Instant::now() + ORIENTATION_VERIFY_TIMEOUT;
        loop {
            let observed = self
                .shell_with_timeout(
                    serial,
                    &["wm", "size", "-d", "0"],
                    ORIENTATION_COMMAND_TIMEOUT,
                )
                .await;
            let actual = match observed {
                Ok(output) if effective_wm_size(&output) == Some(physical) => break,
                Ok(_) => match self
                    .shell_with_timeout(
                        serial,
                        &["wm", "size", &expected, "-d", "0"],
                        ORIENTATION_COMMAND_TIMEOUT,
                    )
                    .await
                {
                    Ok(_) => "realigning WindowManager to the physical display".to_owned(),
                    Err(error) => error.to_string(),
                },
                Err(error) => error.to_string(),
            };
            if Instant::now() >= deadline {
                return Err(AdbError::DisplaySizeVerification {
                    expected: expected.clone(),
                    actual,
                });
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        loop {
            let actual = match self
                .shell_with_timeout(
                    serial,
                    &["cmd", "window", "get-ignore-orientation-request"],
                    ORIENTATION_COMMAND_TIMEOUT,
                )
                .await
            {
                Ok(output) if ignore_orientation_request_enabled(&output) => break,
                Ok(_) => match self
                    .shell_with_timeout(
                        serial,
                        &["wm", "set-ignore-orientation-request", "true", "-d", "0"],
                        ORIENTATION_COMMAND_TIMEOUT,
                    )
                    .await
                {
                    Ok(_) => "enabling display-owned orientation".to_owned(),
                    Err(error) => error.to_string(),
                },
                Err(error) => error.to_string(),
            };
            if Instant::now() >= deadline {
                return Err(AdbError::OrientationVerification {
                    expected: "ignoreOrientationRequest true".to_owned(),
                    actual,
                });
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        Ok(windows_relative_rotation(display.orientation, physical))
    }

    pub async fn set_display_density(
        &self,
        serial: &str,
        android_display_id: u32,
        density: u32,
    ) -> Result<(), AdbError> {
        let display_id = android_display_id.to_string();
        let expected_density = density.to_string();
        let deadline = Instant::now() + ORIENTATION_VERIFY_TIMEOUT;
        loop {
            let actual = match self
                .shell_with_timeout(
                    serial,
                    &["wm", "density", "-d", &display_id],
                    ORIENTATION_COMMAND_TIMEOUT,
                )
                .await
            {
                Ok(output) if physical_wm_density(&output).is_some() => break,
                Ok(output) => output.trim().to_owned(),
                Err(error) => error.to_string(),
            };
            if Instant::now() >= deadline {
                return Err(AdbError::DisplayDensityVerification {
                    display_id: android_display_id,
                    expected: density,
                    actual,
                });
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        loop {
            let actual = match self
                .shell_with_timeout(
                    serial,
                    &["wm", "density", &expected_density, "-d", &display_id],
                    ORIENTATION_COMMAND_TIMEOUT,
                )
                .await
            {
                Ok(_) => match self
                    .shell_with_timeout(
                        serial,
                        &["wm", "density", "-d", &display_id],
                        ORIENTATION_COMMAND_TIMEOUT,
                    )
                    .await
                {
                    Ok(output) if effective_wm_density(&output) == Some(density) => return Ok(()),
                    Ok(output) => output.trim().to_owned(),
                    Err(error) => error.to_string(),
                },
                Err(error) => error.to_string(),
            };
            if Instant::now() >= deadline {
                return Err(AdbError::DisplayDensityVerification {
                    display_id: android_display_id,
                    expected: density,
                    actual,
                });
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn prepare_display_rotation(&self, serial: &str, expected: &str) -> Result<(), AdbError> {
        self.shell_with_timeout(
            serial,
            &["settings", "put", "system", "accelerometer_rotation", "0"],
            ORIENTATION_COMMAND_TIMEOUT,
        )
        .await?;
        self.shell_with_timeout(
            serial,
            &["wm", "fixed-to-user-rotation", "-d", "0", "enabled"],
            ORIENTATION_COMMAND_TIMEOUT,
        )
        .await?;
        self.shell_with_timeout(
            serial,
            &["wm", "user-rotation", "-d", "0", "lock", expected],
            ORIENTATION_COMMAND_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    async fn reassert_display_rotation(
        &self,
        serial: &str,
        expected: &str,
        expected_rotation: u8,
        actual_rotation: Option<u8>,
    ) -> Result<(), AdbError> {
        // A physical-display hotplug or density transition can reload DisplayWindowSettings
        // after the first command pair. Reassert the fixed policy together with the user lock;
        // sending the lock alone leaves a NOSENSOR launcher free to retain ROTATION_0.
        self.shell_with_timeout(
            serial,
            &["wm", "fixed-to-user-rotation", "-d", "0", "enabled"],
            ORIENTATION_COMMAND_TIMEOUT,
        )
        .await?;
        self.shell_with_timeout(
            serial,
            &["wm", "user-rotation", "-d", "0", "lock", expected],
            ORIENTATION_COMMAND_TIMEOUT,
        )
        .await?;
        tracing::warn!(
            event = "adb.display.rotation_lock.reasserted",
            %serial,
            expected_rotation,
            actual_rotation = ?actual_rotation,
            "Android retained the previous stable rotation; reasserted the user lock once"
        );
        Ok(())
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

    /// Keeps the virtual phone display visible after boot. Android's default phone power policy
    /// can dim or turn off the virtual display shortly after Launcher appears, which is perceived
    /// as a native-rendering black screen even when the guest and ADB transport are healthy.
    pub async fn keep_display_awake(&self, serial: &str) -> Result<(), AdbError> {
        validate_serial(serial)?;
        for arguments in [
            [
                "settings",
                "put",
                "system",
                "screen_off_timeout",
                "2147483647",
            ],
            ["settings", "put", "secure", "screensaver_enabled", "0"],
            [
                "settings",
                "put",
                "global",
                "stay_on_while_plugged_in",
                "15",
            ],
        ] {
            self.shell(serial, &arguments).await?;
        }
        for arguments in [
            &["svc", "power", "stayon", "true"][..],
            &["input", "keyevent", "KEYCODE_WAKEUP"],
            &["wm", "dismiss-keyguard"],
            &["input", "keyevent", "KEYCODE_HOME"],
        ] {
            self.shell(serial, arguments).await?;
        }
        Ok(())
    }

    /// Reconciles Android services with the instance's fixed device inventory after boot.
    ///
    /// These commands are deliberately fixed rather than caller-provided. In particular, an
    /// Android 15 cuttlefish services must match the host-backed device inventory. Remove NFC for
    /// user 0 when its backend is absent; `install-existing` restores it when Casimir is enabled,
    /// then the HAL and framework radio are explicitly reconciled on every supported host.
    /// Thread Network is not part of the current HD device profile, so stop both its framework
    /// daemon and vendor HAL instead of allowing init to restart a missing `ThreadChip` backend.
    /// Also stop the boot-time full logcat stream: diagnostics collect a bounded log through ADB
    /// without continuously driving the virtio console or growing a per-run file.
    pub async fn apply_runtime_device_policy(
        &self,
        serial: &str,
        bluetooth_enabled: bool,
        nfc_enabled: bool,
        modem_enabled: bool,
    ) {
        if validate_serial(serial).is_err() {
            return;
        }

        let mut commands: Vec<(&str, Vec<&str>)> = Vec::new();
        if bluetooth_enabled {
            commands.push((
                "enable Bluetooth package",
                vec!["pm", "enable", "--user", "0", "com.android.bluetooth"],
            ));
            commands.push(("enable Bluetooth", vec!["svc", "bluetooth", "enable"]));
        } else {
            commands.push((
                "persist Bluetooth disabled",
                vec!["settings", "put", "global", "bluetooth_on", "0"],
            ));
            commands.push(("disable Bluetooth", vec!["svc", "bluetooth", "disable"]));
        }

        if nfc_enabled {
            commands.push((
                "restore NFC package",
                vec![
                    "cmd",
                    "package",
                    "install-existing",
                    "--user",
                    "0",
                    "com.android.nfc",
                ],
            ));
            commands.push((
                "enable NFC package",
                vec!["pm", "enable", "--user", "0", "com.android.nfc"],
            ));
            commands.push(("start NFC HAL", vec!["su", "0", "start", "nfc_hal_service"]));
            commands.push(("enable NFC", vec!["cmd", "nfc", "enable-nfc"]));
        } else {
            commands.push((
                "remove NFC package for user",
                vec!["pm", "uninstall", "--user", "0", "com.android.nfc"],
            ));
            commands.push(("stop NFC HAL", vec!["su", "0", "stop", "nfc_hal_service"]));
        }
        if cfg!(target_os = "macos") && !modem_enabled {
            commands.push((
                "stop RIL without a modem backend",
                vec!["su", "0", "stop", "vendor.ril-daemon"],
            ));
        }
        commands.push((
            "disable unsupported Thread radio",
            vec!["cmd", "thread_network", "disable"],
        ));
        commands.push((
            "stop Thread framework daemon",
            vec!["cmd", "thread_network", "force-stop-ot-daemon", "enabled"],
        ));
        commands.push((
            "stop missing Thread HAL backend",
            vec!["su", "0", "stop", "vendor.threadnetwork_hal"],
        ));
        commands.push((
            "stop unbounded boot log stream",
            vec!["su", "0", "stop", "seriallogging"],
        ));

        for (operation, arguments) in commands {
            if let Err(error) = self.shell(serial, &arguments).await {
                tracing::warn!(
                    event = "adb.runtime_device_policy.command_failed",
                    %serial,
                    %operation,
                    %error,
                    "guest runtime device policy command failed"
                );
            }
        }
        tracing::info!(
            event = "adb.runtime_device_policy.applied",
            %serial,
            bluetooth_enabled,
            nfc_enabled,
            modem_enabled,
            "guest runtime device policy reconciled"
        );
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
        let interface = self.primary_data_interface(serial).await?;
        let latency = format!("{}ms", condition.latency_ms);
        let loss = netem_loss(condition.loss_basis_points);
        let mut arguments = vec![
            "su".to_owned(),
            "0".to_owned(),
            "tc".to_owned(),
            "qdisc".to_owned(),
            "replace".to_owned(),
            "dev".to_owned(),
            interface.clone(),
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
            .shell(
                serial,
                &["su", "0", "tc", "qdisc", "show", "dev", &interface],
            )
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

    /// Reads Android `ConnectivityService`'s active default network instead of inferring Guest
    /// reachability from the presence of a host-side socket. `VALIDATED` is produced by Android's
    /// own DNS and HTTP/HTTPS probes, while DNS and route checks guard against incomplete DHCP.
    pub async fn network_health(&self, serial: &str) -> Result<AndroidNetworkHealth, AdbError> {
        let connectivity = self.shell(serial, &["dumpsys", "connectivity"]).await?;
        let mut health = parse_android_network_health(&connectivity);
        let kernel_routes = self
            // Android installs per-network routes in named policy-routing tables instead of the
            // legacy main table.  Inspecting only `ip route show default` therefore reports a
            // healthy validated Ethernet network as route-less.
            .shell(
                serial,
                &["ip", "-4", "route", "show", "table", "all", "default"],
            )
            .await?;
        health.has_default_route = health.has_default_route
            && health.interface.as_ref().is_some_and(|interface| {
                kernel_routes.lines().any(|line| {
                    line.split_whitespace().next() == Some("default")
                        && line
                            .split_whitespace()
                            .zip(line.split_whitespace().skip(1))
                            .any(|(field, value)| field == "dev" && value == interface)
                })
            });
        Ok(health)
    }

    /// Waits for Android's native validation on the stable dedicated Ethernet uplink. HD must not
    /// rename or flap Cuttlefish's Wi-Fi/mobile interfaces after boot: doing so creates a stream
    /// of replacement `NetworkAgents` and can exhaust `NetworkStack`'s broadcast-receiver limit.
    pub async fn refresh_network_validation(
        &self,
        serial: &str,
    ) -> Result<AndroidNetworkHealth, AdbError> {
        let initial = self.network_health(serial).await?;
        if initial.is_healthy() {
            return Ok(initial);
        }
        let started = Instant::now();
        loop {
            tokio::time::sleep(NETWORK_VALIDATE_RETRY_DELAY).await;
            let latest = self.network_health(serial).await?;
            if latest.is_healthy() || started.elapsed() >= NETWORK_VALIDATE_TIMEOUT {
                return Ok(latest);
            }
        }
    }

    async fn primary_data_interface(&self, serial: &str) -> Result<String, AdbError> {
        let health = self.network_health(serial).await?;
        if let Some(interface) = health
            .interface
            .filter(|interface| matches!(interface.as_str(), "eth0" | "eth1" | "eth2"))
        {
            return Ok(interface);
        }
        let links = self.shell(serial, &["ip", "-o", "link", "show"]).await?;
        Self::primary_data_interface_from_links(&links)
    }

    fn primary_data_interface_from_links(links: &str) -> Result<String, AdbError> {
        ["eth2", "eth1", "eth0"]
            .into_iter()
            .find(|interface| has_network_link(links, interface))
            .map(str::to_owned)
            .ok_or_else(|| AdbError::NetworkInterfaceUnavailable(links.to_owned()))
    }

    /// Probes the fixed Android 15 phone profile through framework/HAL readback. Passive software
    /// surfaces are reported separately from active radios, and host-controllable devices are
    /// explicitly identified instead of treating every installed package as usable.
    pub async fn device_runtime_health(
        &self,
        serial: &str,
    ) -> Result<Vec<AndroidDeviceRuntimeHealth>, AdbError> {
        let (
            packages,
            services,
            bluetooth,
            nfc_hal,
            uwb,
            telephony,
            sensors,
            audio,
            camera,
            battery,
            location,
            network,
        ) = tokio::try_join!(
            self.shell(serial, &["pm", "list", "packages", "-e"]),
            self.shell(serial, &["service", "list"]),
            self.shell(serial, &["dumpsys", "bluetooth_manager"]),
            self.getprop(serial, "init.svc.nfc_hal_service"),
            self.shell(serial, &["dumpsys", "uwb"]),
            self.shell(serial, &["dumpsys", "telephony.registry"]),
            self.shell(serial, &["dumpsys", "sensorservice"]),
            self.shell(serial, &["dumpsys", "media.audio_flinger"]),
            self.shell(serial, &["dumpsys", "media.camera"]),
            self.shell(serial, &["dumpsys", "battery"]),
            self.shell(serial, &["dumpsys", "location"]),
            self.network_health(serial),
        )?;
        let mut devices =
            radio_runtime_devices(&packages, &services, &bluetooth, &nfc_hal, &uwb, &telephony);
        devices.extend(interactive_runtime_devices(
            &services, &sensors, &battery, &location, &network,
        ));
        devices.extend(media_runtime_devices(&services, &audio, &camera));
        Ok(devices)
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

    pub async fn inject_sensor_pose(
        &self,
        serial: &str,
        frame: SensorMotionFrameV2,
    ) -> Result<(), AdbError> {
        const INJECTOR: &str = "/vendor/bin/cuttlefish_sensor_injection";

        validate_serial(serial)?;
        self.shell(serial, &["test", "-x", INJECTOR]).await?;
        let values = frame
            .accelerometer_microunits
            .into_iter()
            .chain(frame.magnetometer_microunits)
            .chain(frame.gyroscope_microunits)
            .map(format_sensor_microunits)
            .collect::<Vec<_>>();
        let mut arguments = vec!["su", "0", INJECTOR, "motion"];
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

    pub async fn start_screen_recording(
        &self,
        serial: &str,
        recording_id: uuid::Uuid,
        display_id: u32,
        max_duration_seconds: u16,
    ) -> Result<AdbScreenRecording, AdbError> {
        validate_serial(serial)?;
        if !(1..=180).contains(&max_duration_seconds) {
            return Err(AdbError::ScreenRecordingDuration(max_duration_seconds));
        }
        if let Some(pid) = self.screenrecord_pid(serial).await? {
            return Err(AdbError::ScreenRecordingGuestBusy(pid));
        }
        let remote_path = format!("/data/local/tmp/hd-screenrecord-{recording_id}.mp4");
        for attempt in 1..=SCREEN_RECORDING_START_ATTEMPTS {
            let mut command = Command::new(&self.executable);
            command
                .args([
                    "-s",
                    serial,
                    "shell",
                    "screenrecord",
                    "--verbose",
                    "--display-id",
                    &display_id.to_string(),
                    "--time-limit",
                    &max_duration_seconds.to_string(),
                    "--output-format=mp4",
                    &remote_path,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            hd_platform::configure_transient_command(command.as_std_mut()).map_err(|source| {
                AdbError::Spawn {
                    executable: self.executable.clone(),
                    source: std::io::Error::other(source),
                }
            })?;
            let mut child = command.spawn().map_err(|source| AdbError::Spawn {
                executable: self.executable.clone(),
                source,
            })?;
            let started = Instant::now();
            let mut exit_code = None;
            while started.elapsed() < SCREEN_RECORDING_START_TIMEOUT {
                if let Some(status) = child.try_wait().map_err(|source| AdbError::Io {
                    args: vec!["screenrecord".to_owned()],
                    source,
                })? {
                    exit_code = status.code();
                    break;
                }
                if let Some(remote_pid) = self.screenrecord_pid(serial).await?
                    && self
                        .screenrecord_output_initialized(serial, &remote_path)
                        .await?
                {
                    return Ok(AdbScreenRecording {
                        child,
                        remote_path,
                        remote_pid,
                    });
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if let Ok(Some(remote_pid)) = self.screenrecord_pid(serial).await {
                let _ = self
                    .shell(serial, &["kill", "-s", "KILL", &remote_pid.to_string()])
                    .await;
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = self
                .shell(serial, &["rm", "-f", remote_path.as_str()])
                .await;
            if attempt == SCREEN_RECORDING_START_ATTEMPTS {
                return match exit_code {
                    Some(code) => Err(AdbError::ScreenRecordingExited(Some(code))),
                    None => Err(AdbError::ScreenRecordingStartTimeout),
                };
            }
            tracing::warn!(
                event = "adb.screen_recording.start.retry",
                %serial,
                attempt,
                "screenrecord did not initialize an MP4; retrying with a fresh encoder"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        unreachable!("bounded screenrecord attempts always return")
    }

    pub async fn finish_screen_recording(
        &self,
        serial: &str,
        recording: &mut AdbScreenRecording,
        output_path: &Path,
    ) -> Result<(), AdbError> {
        validate_serial(serial)?;
        let child_running = recording
            .child
            .try_wait()
            .map_err(|source| AdbError::Io {
                args: vec!["screenrecord".to_owned()],
                source,
            })?
            .is_none();
        let stop_started = Instant::now();
        if self.screenrecord_pid(serial).await? == Some(recording.remote_pid) {
            // AOSP screenrecord handles SIGHUP as the host-launched `adb shell` stop path: the
            // local adb client normally receives Ctrl-C and the Guest process receives SIGHUP when
            // its transport closes. Signal that authoritative Guest process directly, then allow
            // its codec, muxer and SurfaceFlinger virtual display to unwind. The handler restores
            // the default disposition, so a second SIGHUP is the bounded force-stop fallback.
            self.shell(
                serial,
                &["kill", "-s", "HUP", &recording.remote_pid.to_string()],
            )
            .await?;
            while stop_started.elapsed() < SCREEN_RECORDING_GRACEFUL_STOP_TIMEOUT {
                if self.screenrecord_pid(serial).await? != Some(recording.remote_pid) {
                    break;
                }
                tokio::time::sleep(SCREEN_RECORDING_STOP_RETRY_DELAY).await;
            }
            if self.screenrecord_pid(serial).await? == Some(recording.remote_pid) {
                self.shell(
                    serial,
                    &["kill", "-s", "HUP", &recording.remote_pid.to_string()],
                )
                .await?;
            }
        }
        while stop_started.elapsed() < SCREEN_RECORDING_STOP_TIMEOUT {
            if self.screenrecord_pid(serial).await? != Some(recording.remote_pid) {
                break;
            }
            tokio::time::sleep(SCREEN_RECORDING_STOP_RETRY_DELAY).await;
        }
        if self.screenrecord_pid(serial).await? == Some(recording.remote_pid) {
            let _ = recording.child.start_kill();
            let _ = recording.child.wait().await;
            return Err(AdbError::ScreenRecordingStopTimeout);
        }

        // Some adb builds keep the local `adb shell` client alive after the Guest process has
        // handled SIGHUP, flushed the MP4 and exited. The Guest PID is the recording lifecycle
        // authority; once it is gone, bound and reap only that stale local transport process.
        let client_transport_forced_closed = child_running
            && tokio::time::timeout(SCREEN_RECORDING_CLIENT_EXIT_TIMEOUT, recording.child.wait())
                .await
                .is_err();
        if client_transport_forced_closed {
            let _ = recording.child.start_kill();
            let _ = recording.child.wait().await;
        }
        let pull = self
            .run(
                vec![
                    "-s".to_owned(),
                    serial.to_owned(),
                    "pull".to_owned(),
                    recording.remote_path.clone(),
                    output_path.to_string_lossy().into_owned(),
                ],
                Duration::from_mins(2),
            )
            .await;
        let cleanup = self
            .shell(serial, &["rm", "-f", recording.remote_path.as_str()])
            .await;
        pull?;
        cleanup?;
        if client_transport_forced_closed {
            // AOSP destroys ScreenRecorder's virtual display before the Guest process exits. If
            // the local long-lived `adb shell` client remains after that point, the stale host
            // transport is the unfinished boundary. Close only this instance's loopback transport
            // and reconnect it before publishing recording completion; otherwise a later mutating
            // WindowManager shell can inherit the draining service stream and block.
            tracing::warn!(
                event = "adb.screen_recording.transport_recovery.started",
                %serial,
                "recovering ADB transport after forcing a stale screenrecord client closed"
            );
            self.disconnect(serial).await?;
            tokio::time::timeout(
                SCREEN_RECORDING_TRANSPORT_RECOVERY_TIMEOUT,
                self.connect(serial),
            )
            .await
            .map_err(|_| AdbError::ScreenRecordingTransportRecovery)??;
            tracing::info!(
                event = "adb.screen_recording.transport_recovery.succeeded",
                %serial,
                "ADB transport recovered after screen recording"
            );
        }
        Ok(())
    }

    async fn screenrecord_pid(&self, serial: &str) -> Result<Option<u32>, AdbError> {
        match self.shell(serial, &["pidof", "screenrecord"]).await {
            Ok(value) => {
                let mut values = value.split_whitespace();
                let pid = values
                    .next()
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|pid| *pid > 0)
                    .ok_or_else(|| AdbError::ScreenRecordingPid(value.clone()))?;
                if values.next().is_some() {
                    return Err(AdbError::ScreenRecordingPid(value));
                }
                Ok(Some(pid))
            }
            Err(AdbError::Command {
                code: Some(1),
                ref output,
                ..
            }) if output.trim().is_empty() => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn screenrecord_output_initialized(
        &self,
        serial: &str,
        remote_path: &str,
    ) -> Result<bool, AdbError> {
        match self.shell(serial, &["test", "-s", remote_path]).await {
            Ok(_) => Ok(true),
            Err(AdbError::Command { code: Some(1), .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn getprop(&self, serial: &str, property: &str) -> Result<String, AdbError> {
        self.shell(serial, &["getprop", property])
            .await
            .map(|value| value.trim().to_owned())
    }

    async fn boot_animation_process_stopped(&self, serial: &str) -> Result<bool, AdbError> {
        match self.shell(serial, &["pidof", "bootanimation"]).await {
            Ok(_) => Ok(false),
            Err(AdbError::Command {
                code: Some(1),
                ref output,
                ..
            }) if output.trim().is_empty() => Ok(true),
            Err(error) => Err(error),
        }
    }

    async fn shell(&self, serial: &str, arguments: &[&str]) -> Result<String, AdbError> {
        self.shell_with_timeout(serial, arguments, COMMAND_TIMEOUT)
            .await
    }

    async fn shell_with_timeout(
        &self,
        serial: &str,
        arguments: &[&str],
        timeout: Duration,
    ) -> Result<String, AdbError> {
        validate_serial(serial)?;
        let mut args = vec!["-s".to_owned(), serial.to_owned(), "shell".to_owned()];
        args.extend(arguments.iter().map(|value| (*value).to_owned()));
        self.run(args, timeout)
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

fn format_sensor_microunits(value: i64) -> String {
    let magnitude = value.unsigned_abs();
    let whole = magnitude / 1_000_000;
    let fraction = magnitude % 1_000_000;
    if value.is_negative() {
        format!("-{whole}.{fraction:06}")
    } else {
        format!("{whole}.{fraction:06}")
    }
}

fn window_manager_rotation(output: &str) -> Option<u8> {
    let mut in_default_display = false;
    let mut saw_display_header = false;
    for line in output.lines() {
        let trimmed = line.trim_start();
        if let Some(display) = trimmed.strip_prefix("Display: mDisplayId=") {
            saw_display_header = true;
            in_default_display = display.split_whitespace().next() == Some("0");
            continue;
        }
        if in_default_display && let Some(rotation) = numeric_window_rotation(line) {
            return Some(rotation);
        }
    }
    (!saw_display_header)
        .then(|| output.lines().find_map(numeric_window_rotation))
        .flatten()
}

fn numeric_window_rotation(line: &str) -> Option<u8> {
    line.split_whitespace().find_map(|field| {
        field
            .strip_prefix("mRotation=")
            .and_then(|value| value.parse::<u8>().ok())
    })
}

fn window_manager_transition_complete(output: &str) -> bool {
    output.contains("mLayoutNeeded=false") && output.contains("no ScreenRotationAnimation")
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

async fn run_bugreport_bounded(
    executable: &Path,
    args: Vec<String>,
    output_path: &Path,
) -> Result<(), AdbError> {
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
    let monitor = async {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(metadata) = std::fs::symlink_metadata(output_path)
                && metadata.len() > MAX_ANDROID_BUGREPORT_BYTES
            {
                return metadata.len();
            }
        }
    };
    tokio::pin!(operation);
    tokio::pin!(monitor);
    let result = tokio::select! {
        result = &mut operation => result.map_err(|source| AdbError::Io {
            args: args.clone(),
            source,
        })?,
        size = &mut monitor => return Err(AdbError::BugreportSize(size)),
        () = tokio::time::sleep(BUGREPORT_TIMEOUT) => {
            return Err(AdbError::Timeout { args });
        }
    };
    let (status, stdout, stderr) = result;
    if status.success() {
        return Ok(());
    }
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

fn validate_android_bugreport_zip(path: &Path) -> Result<u64, AdbError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| AdbError::BugreportIo {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(AdbError::BugreportFormat(
            "output is not a regular non-symlink file".to_owned(),
        ));
    }
    let size = metadata.len();
    if !(22..=MAX_ANDROID_BUGREPORT_BYTES).contains(&size) {
        return Err(AdbError::BugreportSize(size));
    }
    let mut file = std::fs::File::open(path).map_err(|source| AdbError::BugreportIo {
        path: path.to_owned(),
        source,
    })?;
    let mut local_header = [0_u8; 4];
    file.read_exact(&mut local_header)
        .map_err(|source| AdbError::BugreportIo {
            path: path.to_owned(),
            source,
        })?;
    if local_header != *b"PK\x03\x04" {
        return Err(AdbError::BugreportFormat(
            "archive has no ZIP local-file header".to_owned(),
        ));
    }
    let tail_size = size.min(65_557);
    let tail_size_i64 = i64::try_from(tail_size)
        .map_err(|_| AdbError::BugreportFormat("archive tail length overflow".to_owned()))?;
    file.seek(SeekFrom::End(-tail_size_i64))
        .map_err(|source| AdbError::BugreportIo {
            path: path.to_owned(),
            source,
        })?;
    let mut tail = vec![0_u8; usize::try_from(tail_size).unwrap_or(65_557)];
    file.read_exact(&mut tail)
        .map_err(|source| AdbError::BugreportIo {
            path: path.to_owned(),
            source,
        })?;
    if !tail.windows(4).any(|window| window == b"PK\x05\x06") {
        return Err(AdbError::BugreportFormat(
            "archive has no ZIP end-of-central-directory record".to_owned(),
        ));
    }
    Ok(size)
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

fn parse_android_network_health(output: &str) -> AndroidNetworkHealth {
    let active_network = output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Active default network:")
            .map(str::trim)
            .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("none"))
            .map(str::to_owned)
    });
    let active_line = active_network.as_ref().and_then(|network| {
        let marker = format!("NetworkAgentInfo{{network{{{network}}}");
        output
            .lines()
            .skip_while(|line| line.trim() != "Current Networks:")
            .take_while(|line| line.trim() != "Status for known UIDs:")
            .find(|line| line.contains(&marker))
    });
    let capabilities = active_line
        .and_then(|line| line.split_once(" nc{"))
        .map_or("", |(_, value)| value);
    let validated = capabilities
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|token| token == "VALIDATED");
    let has_dns = active_line.is_some_and(|line| {
        line.split_once("DnsAddresses: [")
            .and_then(|(_, value)| value.split_once(']'))
            .is_some_and(|(addresses, _)| !addresses.trim().is_empty())
    });
    let has_default_route =
        active_line.is_some_and(|line| line.contains("0.0.0.0/0 ->") || line.contains("::/0 ->"));
    let interface = active_line.and_then(|line| {
        line.split_once("InterfaceName:")
            .and_then(|(_, value)| value.split_whitespace().next())
            .map(|value| value.trim_matches(|character| character == ',' || character == '}'))
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    });
    AndroidNetworkHealth {
        active_network,
        interface,
        validated,
        has_dns,
        has_default_route,
    }
}

#[derive(Debug, Clone, Copy)]
struct RuntimeDeviceFlags {
    installed: bool,
    configured: bool,
    running: bool,
    controllable: bool,
    verified: bool,
}

fn has_network_link(output: &str, interface: &str) -> bool {
    output.lines().any(|line| {
        line.split(':')
            .nth(1)
            .map(str::trim)
            .and_then(|value| value.split('@').next())
            == Some(interface)
    })
}

fn radio_runtime_devices(
    packages: &str,
    services: &str,
    bluetooth: &str,
    nfc_hal: &str,
    uwb: &str,
    telephony: &str,
) -> Vec<AndroidDeviceRuntimeHealth> {
    let bluetooth_installed = packages
        .lines()
        .any(|line| line.trim() == "package:com.android.bluetooth");
    let bluetooth_configured = services
        .contains("android.hardware.bluetooth.IBluetoothHci/default")
        && services.contains("bluetooth_manager:");
    let bluetooth_offline = bluetooth.contains("state: OFF")
        && bluetooth.contains("enabled: false")
        && bluetooth.contains("Bluetooth crashed 0 times");
    let nfc_installed = packages
        .lines()
        .any(|line| line.trim() == "package:com.android.nfc");
    let nfc_offline = nfc_hal == "stopped";
    let uwb_configured =
        services.contains("android.hardware.uwb.IUwb/default") && services.contains("\tuwb:");
    let uwb_baseline =
        uwb.contains("Device state = OFF") && uwb.contains("mCountryCode:") && uwb.contains("US");
    let modem_configured =
        services.contains("telephony.registry:") || services.contains("\tphone:");
    let modem_formal = telephony.contains("00101")
        && (telephony.contains("IN_SERVICE")
            || telephony.contains("mVoiceRegState=0")
            || telephony.contains("mDataRegState=0"));
    let modem_offline =
        telephony.contains("OUT_OF_SERVICE") && telephony.contains("mSignalStrength=null");
    vec![
        runtime_device(
            "bluetooth",
            RuntimeDeviceFlags {
                installed: bluetooth_installed,
                configured: bluetooth_configured,
                running: bluetooth_configured,
                controllable: false,
                verified: bluetooth_installed && bluetooth_configured && bluetooth_offline,
            },
            if bluetooth_offline {
                "framework and AIDL HCI are healthy; software radio is intentionally OFF and host peer injection is unavailable"
            } else {
                "Bluetooth framework/HCI baseline did not match the expected offline profile"
            },
        ),
        runtime_device(
            "nfc",
            RuntimeDeviceFlags {
                installed: nfc_installed,
                configured: nfc_installed,
                running: nfc_hal == "running",
                controllable: false,
                verified: nfc_installed && nfc_offline,
            },
            if nfc_offline {
                "framework package is enabled; NFC HAL is intentionally stopped and host tag injection is unavailable"
            } else {
                "NFC package or expected offline HAL state is missing"
            },
        ),
        runtime_device(
            "uwb",
            RuntimeDeviceFlags {
                installed: uwb_configured,
                configured: uwb_configured,
                running: uwb_configured,
                controllable: false,
                verified: uwb_configured && uwb_baseline,
            },
            "framework and AIDL HAL baseline are present; adapter is OFF and host ranging control is unavailable",
        ),
        runtime_device(
            "modem",
            RuntimeDeviceFlags {
                installed: modem_configured,
                configured: modem_configured,
                running: modem_configured,
                controllable: false,
                verified: modem_configured && (modem_formal || modem_offline),
            },
            if modem_formal {
                "telephony framework is attached to the formal host-vsock AT modem and deterministic test operator 00101; calls, SMS, IMS and data attach are unavailable"
            } else if modem_offline {
                "telephony framework is present with the deterministic no-RIL fallback; enable the formal modem component for AT-backed operator and registration queries"
            } else {
                "telephony framework did not match the formal test-operator or explicit no-RIL profile"
            },
        ),
    ]
}

fn interactive_runtime_devices(
    services: &str,
    sensors: &str,
    battery: &str,
    location: &str,
    network: &AndroidNetworkHealth,
) -> Vec<AndroidDeviceRuntimeHealth> {
    // `dumpsys location` is the authoritative LocationManager view. During Android boot,
    // `service list` can briefly lag behind an already registered GPS provider and caused the UI
    // to report GNSS as unsupported even though injection was ready.
    let gnss_configured = location.contains("gps provider:");
    let gnss_running = location_provider_enabled(location, "gps");
    let required_sensors = [
        "Accel Sensor",
        "Gyro Sensor",
        "Magnetic Field Sensor",
        "Light Sensor",
        "Proximity Sensor",
    ];
    let sensors_configured = services.contains("android.hardware.sensors.ISensors/default")
        && required_sensors.iter().all(|name| sensors.contains(name));
    let sensors_running = sensors_configured && sensors.contains("Recent Sensor events:");
    let power_running = battery.contains("present: true")
        && battery.contains("level:")
        && battery.contains("temperature:");
    vec![
        runtime_device(
            "gnss",
            RuntimeDeviceFlags {
                installed: gnss_configured,
                configured: gnss_configured,
                running: gnss_running,
                controllable: true,
                verified: gnss_running,
            },
            if gnss_running {
                "LocationManager gps provider is enabled and typed coordinate injection is available"
            } else {
                "LocationManager gps provider has not reached its enabled state"
            },
        ),
        runtime_device(
            "sensors",
            RuntimeDeviceFlags {
                installed: sensors_configured,
                configured: sensors_configured,
                running: sensors_running,
                controllable: true,
                verified: sensors_running,
            },
            "required accelerometer, gyroscope, magnetometer, light and proximity sensors are registered with live events",
        ),
        runtime_device(
            "network",
            RuntimeDeviceFlags {
                installed: network.active_network.is_some(),
                configured: network.has_dns && network.has_default_route,
                running: network.active_network.is_some(),
                controllable: true,
                verified: network.is_healthy(),
            },
            &network.detail(),
        ),
        runtime_device(
            "power",
            RuntimeDeviceFlags {
                installed: power_running,
                configured: power_running,
                running: power_running,
                controllable: true,
                verified: power_running,
            },
            "BatteryService is running and typed battery state injection/readback is available",
        ),
    ]
}

fn location_provider_enabled(output: &str, provider: &str) -> bool {
    let heading = format!("{provider} provider:");
    output
        .lines()
        .skip_while(|line| line.trim() != heading)
        .take(4)
        .any(|line| line.trim() == "enabled=true")
}

fn media_runtime_devices(
    services: &str,
    audio: &str,
    camera: &str,
) -> Vec<AndroidDeviceRuntimeHealth> {
    let audio_configured = services.contains("android.hardware.audio.core.IModule/default")
        && services.contains("media.audio_flinger:");
    let audio_running = audio_configured
        && audio.contains("Output thread")
        && audio.contains("AUDIO_DEVICE_OUT_SPEAKER");
    let camera_configured = services
        .contains("android.hardware.camera.provider.ICameraProvider/internal/0")
        && services.contains("media.camera:");
    let camera_running = camera_configured
        && camera.contains("Number of normal camera devices: 2")
        && camera.contains("Camera Provider HAL");
    vec![
        runtime_device(
            "audio",
            RuntimeDeviceFlags {
                installed: audio_configured,
                configured: audio_configured,
                running: audio_running,
                controllable: false,
                verified: audio_running,
            },
            "AudioFlinger and AIDL modules expose the Guest audio path; Host playback and capture boundaries are reported separately by the platform capability",
        ),
        runtime_device(
            "camera",
            RuntimeDeviceFlags {
                installed: camera_configured,
                configured: camera_configured,
                running: camera_running,
                controllable: false,
                verified: camera_running,
            },
            "camera provider exposes two public deterministic virtual cameras; physical host camera routing is unavailable",
        ),
    ]
}

fn runtime_device(id: &str, flags: RuntimeDeviceFlags, detail: &str) -> AndroidDeviceRuntimeHealth {
    AndroidDeviceRuntimeHealth {
        id: id.to_owned(),
        installed: flags.installed,
        configured: flags.configured,
        running: flags.running,
        controllable: flags.controllable,
        verified: flags.verified,
        detail: detail.to_owned(),
    }
}

fn rate_token_kbps(token: &str) -> Option<u64> {
    let normalized = token.to_ascii_lowercase();
    let (number, scale) = if let Some(number) = normalized.strip_suffix("kbit") {
        (number, 1_u64)
    } else if let Some(number) = normalized.strip_suffix("mbit") {
        (number, 1_000_u64)
    } else {
        let number = normalized.strip_suffix("gbit")?;
        (number, 1_000_000_u64)
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

fn effective_wm_density(output: &str) -> Option<u32> {
    let mut physical = None;
    let mut override_density = None;
    for line in output.lines() {
        let line = line.trim();
        let value = line
            .split_once(':')
            .and_then(|(_, value)| value.trim().parse::<u32>().ok());
        if line.starts_with("Physical density:") {
            physical = value;
        } else if line.starts_with("Override density:") {
            override_density = value;
        }
    }
    override_density.or(physical)
}

fn effective_wm_size(output: &str) -> Option<(u32, u32)> {
    let mut physical = None;
    let mut override_size = None;
    for line in output.lines() {
        let line = line.trim();
        let value = line
            .split_once(':')
            .and_then(|(_, value)| parse_wm_size(value.trim()));
        if line.starts_with("Physical size:") {
            physical = value;
        } else if line.starts_with("Override size:") {
            override_size = value;
        }
    }
    override_size.or(physical)
}

fn physical_wm_size(output: &str) -> Option<(u32, u32)> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Physical size:")
            .and_then(|value| parse_wm_size(value.trim()))
    })
}

fn ignore_orientation_request_enabled(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim().contains("ignoreOrientationRequest true"))
}

fn windows_relative_rotation(orientation: OrientationV2, physical_size: (u32, u32)) -> u8 {
    let physical_orientation = u8::from(physical_size.0 > physical_size.1);
    (orientation.android_rotation() + 4 - physical_orientation) & 3
}

fn parse_wm_size(value: &str) -> Option<(u32, u32)> {
    let (width, height) = value.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn physical_wm_density(output: &str) -> Option<u32> {
    output.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("Physical density:")
            .and_then(|value| value.trim().parse::<u32>().ok())
    })
}

#[derive(Debug, Error)]
pub enum AdbError {
    #[error(transparent)]
    BugreportPlatform(#[from] hd_platform::PlatformError),
    #[error("Android bugreport file I/O failed at {path}: {source}")]
    BugreportIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Android bugreport archive size must be 22 bytes to 256 MiB, got {0}")]
    BugreportSize(u64),
    #[error("Android bugreport archive is invalid: {0}")]
    BugreportFormat(String),
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
    #[error("ADB transport for {serial} did not become privileged for power-off: {detail}")]
    PowerOffPrivilege { serial: String, detail: String },
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
    #[error("display size verification failed: expected {expected}, actual {actual}")]
    DisplaySizeVerification { expected: String, actual: String },
    #[error(
        "display {display_id} density verification failed: expected {expected}, actual {actual}"
    )]
    DisplayDensityVerification {
        display_id: u32,
        expected: u32,
        actual: String,
    },
    #[error("battery verification failed: expected {expected}, actual {actual}")]
    BatteryVerification { expected: String, actual: String },
    #[error("network condition verification failed: expected {expected}, actual {actual}")]
    NetworkVerification { expected: String, actual: String },
    #[error("Android has no eth0, eth1 or eth2 data interface: {0}")]
    NetworkInterfaceUnavailable(String),
    #[error("the verified host bundle has no Android sensor injector")]
    SensorInjectorUnavailable,
    #[error("Android sensor injector is not a regular file: {0}")]
    SensorInjectorInvalid(PathBuf),
    #[error("sensor injection cleanup verification failed: {0}")]
    SensorVerification(String),
    #[error("guest screenshot did not return a PNG image")]
    ScreenshotFormat,
    #[error("screen recording duration must be between 1 and 180 seconds, got {0}")]
    ScreenRecordingDuration(u16),
    #[error("Android already has an active screenrecord process with pid {0}")]
    ScreenRecordingGuestBusy(u32),
    #[error("Android screenrecord exited before becoming active (code {0:?})")]
    ScreenRecordingExited(Option<i32>),
    #[error("Android screenrecord did not publish a single valid process id: {0}")]
    ScreenRecordingPid(String),
    #[error("Android screenrecord did not become active within the product deadline")]
    ScreenRecordingStartTimeout,
    #[error("Android screenrecord did not stop within the product deadline")]
    ScreenRecordingStopTimeout,
    #[error("Android ADB transport did not recover after screen recording")]
    ScreenRecordingTransportRecovery,
    #[error("guest readiness timed out for {serial}; {detail}")]
    ReadinessTimeout { serial: String, detail: Box<str> },
}

impl AdbError {
    /// Returns true only when ADB proves that the command could not reach the Guest transport.
    /// Callers may safely use a native transport after these failures without delivering the same
    /// input twice. Timeouts and generic command failures remain ambiguous and must not fall back.
    #[must_use]
    pub fn is_definitively_unavailable(&self) -> bool {
        match self {
            Self::Spawn { .. } | Self::ConnectTimeout { .. } => true,
            Self::Command { output, .. } => {
                let output = output.to_ascii_lowercase();
                output.contains("device offline")
                    || output.contains("device not found")
                    || (output.contains("device '") && output.contains("' not found"))
                    || output.contains("no devices/emulators found")
            }
            _ => false,
        }
    }
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

    #[test]
    fn window_rotation_is_read_from_the_default_display() {
        let dumpsys = r"
WINDOW MANAGER DISPLAY CONTENTS (dumpsys window displays)
  Display: mDisplayId=2 (organized)
    mRotation=0 mDeferredRotationPauseCount=0
  Display: mDisplayId=0 (organized)
    overrideConfig={mRotation=ROTATION_90}
    mRotation=1 mDeferredRotationPauseCount=0
";
        assert_eq!(window_manager_rotation(dumpsys), Some(1));
    }

    #[test]
    fn legacy_single_display_rotation_remains_supported() {
        assert_eq!(
            window_manager_rotation("mRotation=3 mDeferredRotationPauseCount=0"),
            Some(3)
        );
    }

    #[test]
    fn effective_window_manager_size_prefers_the_natural_override() {
        assert_eq!(
            effective_wm_size("Physical size: 1920x1080\nOverride size: 1080x1920\n"),
            Some((1080, 1920))
        );
        assert_eq!(
            effective_wm_size("Physical size: 720x1280\n"),
            Some((720, 1280))
        );
    }

    #[test]
    fn physical_window_manager_size_ignores_a_stale_override() {
        assert_eq!(
            physical_wm_size("Physical size: 1920x1080\nOverride size: 1080x1920\n"),
            Some((1920, 1080))
        );
    }

    #[test]
    fn windows_rotation_is_relative_to_the_crosvm_physical_mode() {
        let portrait_physical = (1080, 1920);
        assert_eq!(
            windows_relative_rotation(OrientationV2::Portrait, portrait_physical),
            0
        );
        assert_eq!(
            windows_relative_rotation(OrientationV2::Landscape, portrait_physical),
            1
        );
        assert_eq!(
            windows_relative_rotation(OrientationV2::ReversePortrait, portrait_physical),
            2
        );
        assert_eq!(
            windows_relative_rotation(OrientationV2::ReverseLandscape, portrait_physical),
            3
        );

        let landscape_physical = (1920, 1080);
        assert_eq!(
            windows_relative_rotation(OrientationV2::Portrait, landscape_physical),
            3
        );
        assert_eq!(
            windows_relative_rotation(OrientationV2::Landscape, landscape_physical),
            0
        );
        assert_eq!(
            windows_relative_rotation(OrientationV2::ReversePortrait, landscape_physical),
            1
        );
        assert_eq!(
            windows_relative_rotation(OrientationV2::ReverseLandscape, landscape_physical),
            2
        );
        assert!(ignore_orientation_request_enabled(
            "ignoreOrientationRequest true for displayId=0"
        ));
    }
}
