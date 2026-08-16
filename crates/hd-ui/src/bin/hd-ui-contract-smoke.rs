#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("macOS UI interaction contract smoke requires macOS")
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    macos::run()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, Result, ensure};
    use hd_core::{InstanceSpecV2, OrientationV2};
    use hd_platform::{macos_titlebar_control_contracts, map_macos_pointer};
    use hd_ui::macos_network::{
        NETWORK_STATUS_TIMEOUT, NetworkSetupAction, administrator_install_apple_script,
        run_command_with_timeout, run_status_script, script_matches_embedded, staged_shell_command,
    };
    use hd_ui::ui_contract::{
        FIRST_PAINT_TIMEOUT, NETWORK_STATUS_REFRESH_INTERVAL, NetworkStatusPollMode, Page,
        SnapshotPollMode, SnapshotRefreshGate, SurfaceLayout, TITLEBAR_ACTION_CONTRACTS,
        aspect_fit_rect, first_paint_timed_out, network_status_poll_mode,
        oriented_guest_dimensions, snapshot_poll_mode,
    };
    use serde_json::json;
    use winit::dpi::PhysicalSize;

    const WEB_SOURCE: &str = include_str!("../../../../web/src/main.tsx");
    const WEB_TYPES_SOURCE: &str = include_str!("../../../../web/src/types.ts");
    const CONFIG_SOURCE: &str = include_str!("../../../hd-core/src/config.rs");
    const PROTOCOL_SOURCE: &str = include_str!("../../../hd-core/src/protocol.rs");
    const SHELL_SOURCE: &str = include_str!("../web_shell.rs");
    const TRACKPAD_QUEUE_SOURCE: &str = include_str!("../trackpad_queue.rs");
    const HOST_SOURCE: &str = include_str!("../../../hd-runtime/src/host.rs");
    const ADB_SOURCE: &str = include_str!("../../../hd-runtime/src/adb.rs");
    const WORKER_SOURCE: &str = include_str!("../../../hd-runtime/src/worker.rs");
    const MICRODROID_CONSOLE_SOURCE: &str =
        include_str!("../../../hd-runtime/src/microdroid_console.rs");
    const MICRODROID_EXIT_SOURCE: &str = include_str!("../../../hd-runtime/src/microdroid_exit.rs");
    const BACKEND_SOURCE: &str = include_str!("../../../hd-runtime/src/backend.rs");
    const HOST_RECORDER_SOURCE: &str = include_str!("../../../hd-runtime/src/host_recorder.rs");
    const WINDOWS_RECORDER_SOURCE: &str =
        include_str!("../../../../../hardware/google/gfxstream/host/HdHostRecorderWin.cpp");
    const PLATFORM_SOURCE: &str = include_str!("../../../hd-platform/src/lib.rs");
    const NATIVE_DISPLAY_SOURCE: &str =
        include_str!("../../../hd-platform/src/native_display_host.rs");
    const WINDOWS_TITLEBAR_SOURCE: &str =
        include_str!("../../../hd-platform/src/windows_titlebar.rs");
    const RETENTION_SOURCE: &str = include_str!("../../../hd-runtime/src/retention.rs");
    const UPLOADS_SOURCE: &str = include_str!("../../../hd-runtime/src/uploads.rs");
    const ROUTE_SOURCE: &str = include_str!("../../../hd-runtime/src/routes.rs");
    const POWERWASH_SOURCE: &str = include_str!("../../../hd-runtime/src/powerwash.rs");
    const CLIENT_SOURCE: &str = include_str!("../../../hd-runtime/src/client.rs");
    const HDCTL_SOURCE: &str = include_str!("../../../hdctl/src/main.rs");
    const CAPABILITIES_SOURCE: &str = include_str!("../../../hd-runtime/src/capabilities.rs");
    const PERIPHERAL_ADAPTER_SOURCE: &str =
        include_str!("../../../hd-peripheral-adapters/src/lib.rs");
    const ROOTCANAL_ADAPTER_SOURCE: &str =
        include_str!("../../../hd-rootcanal-adapter/src/main.rs");
    const HTTP_SOURCE: &str = include_str!("../../../hd-runtime/src/http.rs");
    const MACOS_BRIDGE_SOURCE: &str =
        include_str!("../../../hd-platform/src/native_display_host_macos.m");
    const NETWORK_SETUP_SOURCE: &str = include_str!("../../../../scripts/macos-network-setup.sh");
    const MACOS_PACKAGE_SOURCE: &str = include_str!("../../../../scripts/package-macos.sh");
    const NATIVE_LIFECYCLE_SMOKE_SOURCE: &str =
        include_str!("../../../../scripts/macos-ui-native-lifecycle-smoke.sh");
    const ANDROID_READINESS_SOAK_SOURCE: &str =
        include_str!("../../../../scripts/macos-android-readiness-soak-smoke.sh");

    #[allow(clippy::too_many_lines)]
    pub fn run() -> Result<()> {
        let output = output_path()?;
        let window = PhysicalSize::new(1080, 1920);
        let collapsed = SurfaceLayout::from_window(window, 2.0, Page::Player, true, false);
        let expanded = SurfaceLayout::from_window(window, 2.0, Page::Player, false, false);
        let maximized = SurfaceLayout::from_window(window, 2.0, Page::Player, false, true);
        ensure!(
            collapsed.top_height == 0,
            "macOS must not reserve a WebView title row"
        );
        ensure!(!collapsed.sidebar_visible(), "sidebar must default closed");
        ensure!(
            expanded.sidebar_visible(),
            "sidebar toggle must reveal the overlay"
        );
        ensure!(
            collapsed.android_bounds() == expanded.android_bounds()
                && expanded.android_bounds() == maximized.android_bounds(),
            "sidebar and display maximize must not resize the Android child"
        );
        ensure!(
            maximized.android_focused && !maximized.sidebar_visible(),
            "display maximize must focus Android and hide the sidebar overlay"
        );

        let mut refresh_gate = SnapshotRefreshGate::default();
        let initial_refresh = refresh_gate
            .begin()
            .context("initial snapshot refresh did not start")?;
        refresh_gate.invalidate();
        ensure!(
            !refresh_gate.complete(initial_refresh),
            "a pre-selection snapshot must not overwrite the new selection"
        );
        let selected_refresh = refresh_gate
            .begin()
            .context("replacement snapshot refresh did not start")?;
        ensure!(
            refresh_gate.complete(selected_refresh),
            "the replacement snapshot must be accepted"
        );
        ensure!(
            !refresh_gate.complete(selected_refresh),
            "a duplicate snapshot completion must be rejected"
        );
        let next_refresh = refresh_gate
            .begin()
            .context("next snapshot refresh did not start")?;
        ensure!(
            !refresh_gate.complete(selected_refresh),
            "an older duplicate must not complete a newer request"
        );
        ensure!(
            refresh_gate.complete(next_refresh),
            "the newer request must remain active after an older duplicate"
        );
        let mutation_refresh = refresh_gate
            .begin()
            .context("pre-mutation snapshot refresh did not start")?;
        refresh_gate.invalidate();
        refresh_gate.invalidate();
        ensure!(
            !refresh_gate.complete(mutation_refresh),
            "a snapshot spanning multiple local mutations must be rejected"
        );
        let current_refresh = refresh_gate
            .begin()
            .context("current snapshot refresh did not start")?;
        ensure!(
            refresh_gate.complete(current_refresh),
            "the current post-mutation snapshot must be accepted"
        );

        let poll_modes = [
            (
                "lifecycle_transition",
                snapshot_poll_mode(Page::Player, false, true, true),
                Duration::from_millis(250),
            ),
            (
                "foreground_player",
                snapshot_poll_mode(Page::Player, true, false, false),
                Duration::from_secs(1),
            ),
            (
                "foreground_auxiliary",
                snapshot_poll_mode(Page::Settings, true, false, false),
                Duration::from_secs(2),
            ),
            (
                "background",
                snapshot_poll_mode(Page::Player, false, false, false),
                Duration::from_secs(2),
            ),
            (
                "minimized",
                snapshot_poll_mode(Page::Player, false, true, false),
                Duration::from_secs(5),
            ),
            (
                "occluded",
                snapshot_poll_mode(Page::Player, false, true, false),
                Duration::from_secs(5),
            ),
        ];
        for (name, mode, expected) in poll_modes {
            ensure!(
                mode.interval() == expected,
                "snapshot poll mode {name} has an invalid interval"
            );
        }
        ensure!(
            snapshot_poll_mode(Page::Player, false, true, true)
                == SnapshotPollMode::LifecycleTransition,
            "lifecycle responsiveness must take precedence over minimized throttling"
        );
        ensure!(
            !first_paint_timed_out(false, false, Duration::from_millis(14_999))
                && first_paint_timed_out(false, false, FIRST_PAINT_TIMEOUT)
                && !first_paint_timed_out(true, false, FIRST_PAINT_TIMEOUT)
                && !first_paint_timed_out(false, true, FIRST_PAINT_TIMEOUT),
            "first-paint timeout must fail only an unrevealed, non-closing root at the deadline"
        );
        let network_poll_modes = [
            (
                "foreground_devices",
                network_status_poll_mode(Page::Devices, true, false),
                Some(NETWORK_STATUS_REFRESH_INTERVAL),
            ),
            (
                "player",
                network_status_poll_mode(Page::Player, true, false),
                None,
            ),
            (
                "background_devices",
                network_status_poll_mode(Page::Devices, false, false),
                None,
            ),
            (
                "minimized_devices",
                network_status_poll_mode(Page::Devices, true, true),
                None,
            ),
        ];
        for (name, mode, expected) in network_poll_modes {
            ensure!(
                mode.interval() == expected,
                "network status poll mode {name} has an invalid interval"
            );
        }
        ensure!(
            network_status_poll_mode(Page::Devices, true, false)
                == NetworkStatusPollMode::ForegroundDevices
                && network_status_poll_mode(Page::Settings, true, false)
                    == NetworkStatusPollMode::Suspended,
            "network status probes must be scoped to the visible Devices page"
        );

        let mut spec = InstanceSpecV2::default();
        spec.display.width = 1080;
        spec.display.height = 1920;
        spec.display.orientation = OrientationV2::Portrait;
        let portrait = oriented_guest_dimensions(&spec.display);
        spec.display.orientation = OrientationV2::Landscape;
        let landscape = oriented_guest_dimensions(&spec.display);
        ensure!(portrait == (1080, 1920), "portrait aspect contract changed");
        ensure!(
            landscape == (1920, 1080),
            "landscape aspect contract changed"
        );
        let portrait_fullscreen = aspect_fit_rect(1680, 1050, portrait.0, portrait.1);
        let landscape_fullscreen = aspect_fit_rect(1680, 1050, landscape.0, landscape.1);
        ensure!(
            portrait_fullscreen.x == 544
                && portrait_fullscreen.y == 0
                && portrait_fullscreen.width == 591
                && portrait_fullscreen.height == 1050,
            "portrait fullscreen must be centered with side bars: {portrait_fullscreen:?}"
        );
        ensure!(
            landscape_fullscreen.x == 0
                && landscape_fullscreen.y == 52
                && landscape_fullscreen.width == 1680
                && landscape_fullscreen.height == 945,
            "landscape fullscreen must be centered with top and bottom bars: {landscape_fullscreen:?}"
        );

        let pointer_cases = [
            (0_u8, (0.25, 0.75), (270, 1440)),
            (1_u8, (0.25, 0.75), (270, 480)),
            (2_u8, (0.25, 0.75), (810, 480)),
            (3_u8, (0.25, 0.75), (810, 1440)),
        ];
        let pointer_results = pointer_cases
            .into_iter()
            .map(|(rotation, point, expected)| {
                let actual = map_macos_pointer(point.0, point.1, rotation, 1080, 1920);
                ensure!(
                    actual == expected,
                    "rotation {rotation} pointer map returned {actual:?}, expected {expected:?}"
                );
                Ok(json!({
                    "rotation_quarters": rotation,
                    "input": [point.0, point.1],
                    "guest": [actual.0, actual.1],
                }))
            })
            .collect::<Result<Vec<_>>>()?;

        let controls = macos_titlebar_control_contracts()?;
        ensure!(
            controls.len() == TITLEBAR_ACTION_CONTRACTS.len(),
            "native titlebar must expose exactly eleven HD controls"
        );
        for (control, (action_id, expected_message)) in
            controls.iter().zip(TITLEBAR_ACTION_CONTRACTS)
        {
            ensure!(
                control.message == expected_message,
                "native titlebar action {action_id} drifted from the portable command contract"
            );
        }
        ensure!(
            controls.first().is_some_and(|control| {
                control.placement == "left" && control.message.contains("toggle_sidebar")
            }),
            "sidebar control must be the sole left HD titlebar accessory"
        );
        ensure!(
            controls.get(1).is_some_and(|control| {
                control.placement == "right" && control.message.contains("\"power\"")
            }),
            "power must be the first right-side HD titlebar control"
        );
        ensure!(
            controls.get(6).is_some_and(|control| {
                control.placement == "right"
                    && control.message.contains("\"start_screen_recording\"")
            }),
            "screen recording must be directly available from the titlebar"
        );
        ensure!(
            controls.get(7).is_some_and(|control| {
                control.placement == "right" && control.message.contains("\"choose_install_apk\"")
            }),
            "install APK must be directly available from the titlebar"
        );
        ensure!(
            !MACOS_BRIDGE_SOURCE.contains("displayPickerPressed:")
                && !MACOS_BRIDGE_SOURCE.contains("HDTitlebarDisplayPickerKey")
                && WEB_SOURCE.contains("className=\"display-switcher\"")
                && SHELL_SOURCE.contains("set_macos_titlebar_player_state"),
            "multi-display selection must not occupy either platform titlebar"
        );
        ensure!(
            WEB_SOURCE.contains("state.controls_visible")
                && WEB_SOURCE.contains("function PlayerTitlebarTools")
                && SHELL_SOURCE.contains("self.page == Page::Player")
                && MACOS_BRIDGE_SOURCE.contains("state[@\"controls_visible\"]")
                && MACOS_BRIDGE_SOURCE.contains("button.hidden = !controlsVisible")
                && SHELL_SOURCE.contains("fn send_top(&self, message: &Value)"),
            "Windows WebView and macOS native titlebars must share player-page visibility"
        );
        ensure!(
            HOST_SOURCE.contains(
                "stop the instance before changing restart-sensitive specification fields"
            ) && !WEB_SOURCE.contains("secondary_displays: []")
                && WEB_SOURCE.contains("运行中修改会受控重启该实例")
                && WORKER_SOURCE.contains("runtime.scanout_id * 2")
                && ADB_SOURCE.contains("physical_wm_density(&output).is_some()"),
            "secondary display changes must remain per-instance but restart-gated until Android HWC runtime hotplug is release-safe"
        );
        ensure!(
            CONFIG_SOURCE.contains("host_audio_input: HostAudioInputV2::Disabled")
                && WEB_SOURCE.contains("关闭（默认）")
                && WEB_SOURCE.contains("if (key === 'audio' && !event.target.checked) next.host_audio_input = 'disabled'")
                && BACKEND_SOURCE.contains("capture=false,backend=winaudio,num_output_devices=1,num_input_devices=0")
                && BACKEND_SOURCE.contains("capture=true,backend=winaudio,num_output_devices=1,num_input_devices=1")
                && BACKEND_SOURCE.contains("capture=false,backend=coreaudio,num_output_devices=1,num_input_devices=0")
                && BACKEND_SOURCE.contains("capture=true,backend=coreaudio,num_output_devices=1,num_input_devices=1")
                && CAPABILITIES_SOURCE.contains("audio.host_default_microphone")
                && CAPABILITIES_SOURCE.contains("&[\"virtio_snd\", \"host_playback\"]")
                && MACOS_PACKAGE_SOURCE.contains("NSMicrophoneUsageDescription")
                && !MACOS_PACKAGE_SOURCE.contains("crosvm-runtime-audio-disabled"),
            "Host microphone routing must remain per-instance, explicit, default-off and capability-gated"
        );
        ensure!(
            SHELL_SOURCE.contains("Arc<EventLoopProxy<UserEvent>>")
                && SHELL_SOURCE.contains("Arc::new(event_loop.create_proxy())")
                && SHELL_SOURCE.contains("Arc::clone(proxy)"),
            "desktop async work must share one winit proxy instead of registering one macOS run-loop source per poll"
        );
        let mut messages = BTreeSet::new();
        for control in &controls {
            let value: serde_json::Value = serde_json::from_str(&control.message)
                .with_context(|| format!("decode titlebar message for {}", control.tooltip))?;
            ensure!(
                value.get("command").is_some(),
                "titlebar message has no command"
            );
            ensure!(
                messages.insert(control.message.clone()),
                "duplicate native titlebar action {}",
                control.message
            );
            ensure!(
                SHELL_SOURCE.contains(&format!("\"{}\"", value["command"].as_str().unwrap_or(""))),
                "native command has no shell handler: {}",
                control.message
            );
        }

        let required_web_contracts = [
            ("cancel_create", "aria-label=\"取消新建\""),
            ("sidebar_blur_close", "post({ command: 'close_sidebar' })"),
            (
                "settings_install_apk",
                "onInstallApk={() => post({ command: 'install_apk'",
            ),
            (
                "hot_settings_save",
                "requiresInstanceRestart(record.spec, draft)",
            ),
            ("diagnostics", "command: 'diagnostics'"),
            (
                "powerwash_exact_name_confirmation",
                "requiredText={record?.spec.name ?? ''}",
            ),
            (
                "powerwash_revision_binding",
                "expected_revision: record.status.revision",
            ),
            ("powerwash_restore", "kind: 'restore_powerwash'"),
            ("powerwash_discard", "kind: 'discard_powerwash_backup'"),
            (
                "host_only_diagnostics",
                "未选择实例，将生成 Host-only 诊断包",
            ),
            ("diagnostic_artifact", "snapshot.diagnostic_artifact"),
            ("reveal_diagnostics", "command: 'reveal_diagnostics'"),
            ("network_setup_status", "snapshot.network_setup"),
            ("network_setup_refresh", "command: 'network_setup_refresh'"),
            ("network_setup_install", "command: 'network_setup_install'"),
            ("resource_admission", "snapshot.resource_capability"),
            (
                "resource_admission_refresh",
                "command: 'resource_admission_refresh'",
            ),
            (
                "device_capabilities_on_demand",
                "command: 'device_capabilities_refresh'",
            ),
            ("resource_admission_disk", "required_disk_bytes"),
            ("resource_admission_memory", "available_memory_bytes"),
            ("runtime_upgrade_state", "snapshot.host_runtime_current"),
            (
                "runtime_upgrade_guidance",
                "旧版 HD 运行时仍在承载实例。停止所有实例并重新打开 HD 即可完成升级。",
            ),
            ("selected_instance_no_restart", "selectedId !== instanceId"),
            (
                "instance_operation_target_binding",
                "command: 'operation', kind, instance_id: instanceId",
            ),
            (
                "device_runtime_control",
                "features.includes('runtime_control')",
            ),
            ("device_form_instance_isolation", "}, [record?.spec.id]);"),
            (
                "location_accuracy_control",
                "accuracy_mm: Math.round(accuracyMeters * 1000)",
            ),
            ("location_route_import", "command: 'choose_location_route'"),
            ("location_route_start", "command: 'start_location_route'"),
            ("location_route_pause", "'pause_location_route'"),
            ("location_route_resume", "'resume_location_route'"),
            ("location_route_stop", "'stop_location_route'"),
            (
                "battery_temperature_control",
                "temperature_deci_celsius: Math.round(temperatureCelsius * 10)",
            ),
            ("network_bandwidth_control", "bandwidth_kbps: bandwidth"),
            ("sensor_duration_control", "duration_ms: sensorDurationMs"),
            (
                "sensor_duration_semantics",
                "持续时间为 0（持续覆盖）或 200–600000 ms",
            ),
            ("sensor_pose_action", "action: 'set_sensor_pose'"),
            (
                "sensor_pose_angles",
                "x_millidegrees: Math.round(poseXDegrees * 1000)",
            ),
            ("sensor_pose_transition", "transition_ms: poseTransitionMs"),
            ("sensor_pose_model", "AOSP `Rz × Ry × Rx` 模型"),
            ("bluetooth_stop_advertising", "enabled: false"),
            ("bluetooth_remove_peer", "command: 'remove_peer'"),
            ("bluetooth_beacon", "command: 'create_beacon'"),
            ("bluetooth_instance_state", "record.bluetooth_peers ?? []"),
            ("nfc_type4", "presentNfc('present_type4')"),
            ("uwb_distance_control", "action: 'set_uwb_ranging'"),
            ("uwb_instance_state", "record?.uwb_ranging?.distance_cm"),
            ("modem_state_control", "action: 'set_modem_state'"),
            (
                "modem_instance_state",
                "record?.modem_state?.operator_numeric",
            ),
            (
                "microdroid_create_kind",
                "snapshot.microdroid_supported && <option value=\"microdroid\">Microdroid</option>",
            ),
            (
                "microdroid_settings_sections",
                "const microdroidSettingSections",
            ),
            (
                "microdroid_graphics_boundary",
                "Microdroid 是无图形工作负载，不提供显示、旋转、FPS 或设备模拟设置。",
            ),
            (
                "microdroid_devices_hidden",
                "snapshot.selected?.spec.guest_kind !== 'microdroid' && <button",
            ),
            ("microdroid_workload", "<h1>Microdroid 工作负载</h1>"),
            (
                "microdroid_full_debug_adb_truth",
                "Full 调试模式按 AOSP 契约挂载 adbd APEX，并为 EmptyPayload 与上传 Payload 提供实例独立的回环 ADB",
            ),
            (
                "microdroid_uploaded_payload_config_truth",
                "配置：APK 内 assets/vm_config.json",
            ),
            (
                "microdroid_builtin_payload_config_truth",
                "配置：系统内置 Microdroid EmptyPayload",
            ),
            (
                "microdroid_payload_import",
                "command: 'choose_microdroid_payload'",
            ),
            (
                "microdroid_payload_declared_extra_count",
                "payload_extra_apk_count",
            ),
            (
                "microdroid_payload_exact_extra_count_feedback",
                "主 Payload 声明 ${spec.microdroid.payload_extra_apk_count} 个额外 APK",
            ),
            ("microdroid_shell", "command: 'microdroid_shell'"),
            ("start", "operation('start')"),
            ("pause_resume", "observed === 'paused' ? 'resume' : 'pause'"),
            ("restart", "operation('restart')"),
            ("stop", "force_stop' : 'stop'"),
        ];
        for (name, token) in required_web_contracts {
            ensure!(
                WEB_SOURCE.contains(token),
                "missing UI contract {name}: {token}"
            );
        }
        for handler in [
            "\"toggle_sidebar\" =>",
            "\"close_sidebar\" =>",
            "\"choose_install_apk\" =>",
            "\"choose_location_route\" =>",
            "\"start_location_route\" =>",
            "\"rotate\" =>",
            "\"screenshot\" =>",
            "\"start_screen_recording\" =>",
            "\"stop_screen_recording\" =>",
            "\"diagnostics\" =>",
            "\"reveal_diagnostics\" =>",
            "\"android_bugreport\" =>",
            "\"reveal_android_bugreport\" =>",
            "\"network_setup_refresh\" =>",
            "\"network_setup_install\" =>",
            "\"resource_admission_refresh\" | \"capabilities_refresh\" =>",
            "\"device_capabilities_refresh\" =>",
            "\"choose_microdroid_payload\" =>",
            "\"microdroid_shell\" =>",
            "\"operation\" =>",
            "\"key\" =>",
            "\"trackpad\" =>",
            "\"window\" =>",
        ] {
            ensure!(
                SHELL_SOURCE.contains(handler),
                "missing shell handler {handler}"
            );
        }
        ensure!(
            SHELL_SOURCE.contains("\"resource_capability\"")
                && SHELL_SOURCE.contains("request_resource_admission")
                && SHELL_SOURCE.contains("ui.resource_admission.stale_rejected")
                && HOST_SOURCE.contains("blocked_capability_detail")
                && HOST_SOURCE.contains("required_disk_bytes")
                && HOST_SOURCE.contains("disk_requirement_mode"),
            "resource admission must reach the instance UI and preserve actionable Host failure detail"
        );
        ensure!(
            HTTP_SOURCE.contains("/v2/instances/{id}/screen-recordings")
                && CLIENT_SOURCE.contains("start_screen_recording")
                && CLIENT_SOURCE.contains("start_display_screen_recording")
                && CLIENT_SOURCE.contains("stop_screen_recording")
                && HOST_SOURCE.contains("WorkerCommandV2::StartScreenRecording")
                && HOST_SOURCE.contains("WorkerCommandV2::StopScreenRecording")
                && !HOST_SOURCE.contains("zero-copy host IOSurface recorder")
                && WORKER_SOURCE.contains("HD_HOST_RECORDER_ENDPOINT")
                && WORKER_SOURCE.contains("HostRecording::start")
                && HOST_RECORDER_SOURCE.contains("UnixStream::connect")
                && HOST_RECORDER_SOURCE.contains("ClientOptions::new().open")
                && HOST_RECORDER_SOURCE.contains("REQUEST_MAGIC")
                && HOST_RECORDER_SOURCE.contains("PROTOCOL_VERSION: u16 = 2")
                && HOST_RECORDER_SOURCE.contains("display_id.to_le_bytes()")
                && WORKER_SOURCE.contains("scanout_id")
                && PROTOCOL_SOURCE.contains("pub display_id: DisplayIdV2")
                && SHELL_SOURCE.contains("start_display_screen_recording")
                && SHELL_SOURCE.contains("\"select_display\"")
                && SHELL_SOURCE.contains("\"screen_recording_supported\": true")
                && SHELL_SOURCE.contains("ui.async_result.selection_rejected"),
            "screen recording must use the bounded cross-platform gfxstream host-recorder protocol and selection-safe UI completion"
        );
        ensure!(
            WINDOWS_RECORDER_SOURCE.contains("MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS")
                && WINDOWS_RECORDER_SOURCE.contains("selectedEncoderIsHardware")
                && WINDOWS_RECORDER_SOURCE.contains("MFT_ENUM_HARDWARE_URL_Attribute")
                && WINDOWS_RECORDER_SOURCE.contains("kFramesPerSecond = 30")
                && WINDOWS_RECORDER_SOURCE.contains("mPendingFrame = std::move(frame)")
                && WINDOWS_RECORDER_SOURCE.contains("lifecycleLock(mLifecycleMutex)")
                && WINDOWS_RECORDER_SOURCE.contains("setPostCallback(nullptr")
                && WINDOWS_RECORDER_SOURCE.contains("PIPE_REJECT_REMOTE_CLIENTS"),
            "Windows recording must use bounded gfxstream readback, verified hardware H.264, serialized lifecycle, owner-only local control and immediate callback removal"
        );
        ensure!(
            CONFIG_SOURCE.contains("pub touchpad: bool")
                && WEB_SOURCE.contains("spec.devices.touchpad = false")
                && WEB_SOURCE.contains("key !== 'touchpad'")
                && !WEB_SOURCE.contains("TrackpadSurface")
                && !WEB_SOURCE.contains("command: 'trackpad'")
                && !CAPABILITIES_SOURCE
                    .contains("independent crosvm virtio-input indirect pointing surface"),
            "the release UI must expose one pointer model through the Android render surface; the legacy touchpad field remains read-compatible but hidden and forced off"
        );
        ensure!(
            PROTOCOL_SOURCE.contains("pub probed: bool")
                && HOST_SOURCE.contains("device.runtime.probed = true")
                && HOST_SOURCE.contains("device.runtime.probed = false")
                && WEB_TYPES_SOURCE.contains("probed: boolean")
                && WEB_SOURCE.contains("运行态未验证"),
            "device status must distinguish an unavailable runtime probe from a verified stopped device"
        );
        ensure!(
            CAPABILITIES_SOURCE.contains("fn unique_features(features: &[&str])")
                && CAPABILITIES_SOURCE.contains("device.features = unique_features(features)")
                && CAPABILITIES_SOURCE.contains("features: unique_features(features)"),
            "platform capability overlays and base device capabilities must publish stable unique feature lists"
        );
        ensure!(
            HOST_SOURCE.contains("host.capabilities.device_runtime.unavailable")
                && HOST_SOURCE.contains("mark_device_runtime_probe_unavailable")
                && HOST_SOURCE
                    .contains("Worker diagnostics did not include a result for this device",)
                && HOST_SOURCE.contains("device.runtime.configured = configured")
                && HOST_SOURCE.contains("device.runtime.controllable = false")
                && HOST_SOURCE.contains("实例已 Ready，但设备运行态探测失败"),
            "Ready-instance capability refresh must not silently present failed device diagnostics as an unsupported or healthy runtime"
        );
        ensure!(
            TRACKPAD_QUEUE_SOURCE.contains("TRACKPAD_QUEUE_CAPACITY")
                && TRACKPAD_QUEUE_SOURCE.contains("GestureAlreadyActive")
                && TRACKPAD_QUEUE_SOURCE.contains("active_instance")
                && TRACKPAD_QUEUE_SOURCE.contains("queued.event = event")
                && SHELL_SOURCE.contains("TrackpadDispatchSender")
                && !SHELL_SOURCE.contains("mpsc::unbounded_channel"),
            "trackpad UI dispatch must be bounded, coalesce MOVE and pin release to the DOWN instance"
        );
        ensure!(
            PROTOCOL_SOURCE.contains("pub struct AndroidBugreportRecordV2")
                && PROTOCOL_SOURCE.contains("CollectAndroidBugreport")
                && HTTP_SOURCE.contains("/v2/instances/{id}/android-bugreports")
                && CLIENT_SOURCE.contains("collect_android_bugreport")
                && HOST_SOURCE.contains("retain_android_bugreports")
                && WORKER_SOURCE.contains("collect_android_bugreport")
                && WEB_SOURCE.contains("可能包含账号、网络、应用和设备敏感信息")
                && WEB_SOURCE.contains("只保留当前实例最近 3 份")
                && HDCTL_SOURCE.contains("Command::Bugreport"),
            "Android bugreport must be explicit, typed, bounded, instance-owned and retention-limited without caller paths or shell commands"
        );
        ensure!(
            ROUTE_SOURCE.contains("MAX_LOCATION_ROUTE_BYTES")
                && ROUTE_SOURCE.contains("Event::DocType")
                && WORKER_SOURCE.contains("worker.location_route.started")
                && WORKER_SOURCE.contains("stop_location_route(false)")
                && HOST_SOURCE.contains("record.location_route.clone_from"),
            "location route import/playback must be bounded, instance-owned, observable and lifecycle-safe"
        );
        ensure!(
            PROTOCOL_SOURCE.contains("pub struct PowerwashBackupV2")
                && PROTOCOL_SOURCE.contains("RestorePowerwash {")
                && PROTOCOL_SOURCE.contains("DiscardPowerwashBackup {")
                && POWERWASH_SOURCE.contains("validate_storage_confirmation")
                && POWERWASH_SOURCE.contains("move_regular_file")
                && POWERWASH_SOURCE.contains("recover_storage_transactions")
                && POWERWASH_SOURCE.contains("verify_backup")
                && POWERWASH_SOURCE.contains("sync_all()")
                && HOST_SOURCE.contains("detach_worker_for_storage_mutation")
                && HOST_SOURCE.contains("delete instance powerwash backups")
                && HTTP_SOURCE.contains("restore_powerwash")
                && HDCTL_SOURCE.contains("Command::PowerwashRestore")
                && WEB_SOURCE.contains("当前 userdata 会先成为新的回滚备份"),
            "powerwash must remain Android-only, stopped, revision/name-confirmed, recoverable, integrity-checked and crash-reconciled"
        );
        ensure!(
            WORKER_SOURCE.contains("InstanceActionV2::SetUwbRanging")
                && WORKER_SOURCE.contains("status.uwb_ranging = Some(ranging)")
                && HOST_SOURCE.contains("record.uwb_ranging.clone_from")
                && HDCTL_SOURCE.contains("ActionCommand::UwbRanging")
                && HDCTL_SOURCE.contains("UwbRangingV2 { distance_cm }")
                && CAPABILITIES_SOURCE.contains("deterministic_ranging")
                && PERIPHERAL_ADAPTER_SOURCE.contains("uwb_distance_updates.changed()")
                && PERIPHERAL_ADAPTER_SOURCE
                    .contains("uwb_short_range_notification(&mut uwb_session, distance_cm)",),
            "UWB runtime ranging must be authenticated, instance-owned, observable and delivered to active FiRa sessions"
        );
        ensure!(
            PROTOCOL_SOURCE.contains("pub struct ModemStateV2")
                && PROTOCOL_SOURCE.contains("SetModemState { modem: ModemStateV2 }")
                && WORKER_SOURCE.contains("InstanceActionV2::SetModemState")
                && WORKER_SOURCE.contains("status.modem_state = Some(modem)")
                && HOST_SOURCE.contains("record.modem_state.clone_from")
                && HDCTL_SOURCE.contains("ActionCommand::ModemState")
                && HDCTL_SOURCE.contains("ModemStateV2 {")
                && HTTP_SOURCE.contains("set_modem_state")
                && CAPABILITIES_SOURCE.contains("runtime_control")
                && PERIPHERAL_ADAPTER_SOURCE.contains("runtime-modem-state-v2")
                && PERIPHERAL_ADAPTER_SOURCE.contains("runtime-modem-unsolicited-v2")
                && PERIPHERAL_ADAPTER_SOURCE.contains("context.modem_state.send_replace(modem)")
                && PERIPHERAL_ADAPTER_SOURCE.contains("modem.signal_strength")
                && PERIPHERAL_ADAPTER_SOURCE.contains("u8::from(modem.registered)"),
            "modem runtime state must be validated, authenticated, instance-owned, observable and reflected by Guest AT queries"
        );
        ensure!(
            PERIPHERAL_ADAPTER_SOURCE
                .contains("matches!(profile.component, \"uwb-adapter\" | \"modem-adapter\")")
                && PERIPHERAL_ADAPTER_SOURCE.contains("has no formal Guest data plane")
                && !PERIPHERAL_ADAPTER_SOURCE.contains("let formal = cfg!(windows)"),
            "reserved network, audio and camera adapter profiles must not advertise a formal Guest data plane"
        );
        ensure!(
            PROTOCOL_SOURCE.contains("CreateBeacon {")
                && PROTOCOL_SOURCE.contains("pub struct BluetoothPeerStateV2")
                && WORKER_SOURCE.contains("BluetoothPeerKindV2::Beacon")
                && WORKER_SOURCE.contains("status.bluetooth_peers")
                && HOST_SOURCE.contains("record.bluetooth_peers.clone_from")
                && HDCTL_SOURCE.contains("ActionCommand::BluetoothBeacon")
                && CAPABILITIES_SOURCE.contains("beacon-peer-control-v2")
                && ROOTCANAL_ADAPTER_SOURCE.contains("beacon-peer-control-v2")
                && ROOTCANAL_ADAPTER_SOURCE.contains("if peer.connectable { 0x00 } else { 0x03 }")
                && !ROOTCANAL_ADAPTER_SOURCE.contains("remote console"),
            "Bluetooth beacon control must be typed, instance-owned and observable without exposing an arbitrary RootCanal console"
        );
        ensure!(
            PROTOCOL_SOURCE.contains("CreateScriptedBeacon {")
                && PROTOCOL_SOURCE.contains("frames.len() <= 64")
                && PROTOCOL_SOURCE.contains("<= 600_000")
                && WORKER_SOURCE.contains("BluetoothPeerKindV2::ScriptedBeacon")
                && WEB_SOURCE.contains("command: 'create_scripted_beacon'")
                && WEB_SOURCE.contains("广播序列最多 64 帧/10 分钟")
                && HDCTL_SOURCE.contains("ActionCommand::BluetoothScriptedBeacon")
                && CAPABILITIES_SOURCE.contains("scripted-beacon-control-v2")
                && ROOTCANAL_ADAPTER_SOURCE.contains("tick_scripted_peer")
                && ROOTCANAL_ADAPTER_SOURCE.contains("AdvertisingPlan::Scripted")
                && !ROOTCANAL_ADAPTER_SOURCE.contains("config_file"),
            "scripted Beacon playback must use a bounded typed timeline without file paths or arbitrary RootCanal commands"
        );
        ensure!(
            PROTOCOL_SOURCE.contains("CreateHidKeyboard {")
                && PROTOCOL_SOURCE.contains("SendHidKeyboardReport {")
                && PROTOCOL_SOURCE.contains("keys.len() <= 6")
                && WORKER_SOURCE.contains("BluetoothPeerKindV2::HidKeyboard")
                && WEB_SOURCE.contains("command: 'create_hid_keyboard'")
                && WEB_SOURCE.contains("command: 'send_hid_keyboard_report'")
                && HDCTL_SOURCE.contains("ActionCommand::BluetoothHidKeyboardReport")
                && CAPABILITIES_SOURCE.contains("hogp-keyboard-control-v2")
                && ROOTCANAL_ADAPTER_SOURCE.contains("HID_REPORT_MAP")
                && ROOTCANAL_ADAPTER_SOURCE.contains("handle_smp")
                && ROOTCANAL_ADAPTER_SOURCE.contains("input_notifications")
                && ROOTCANAL_ADAPTER_SOURCE.contains("peer.encrypted")
                && !ROOTCANAL_ADAPTER_SOURCE.contains("keyboard.cc"),
            "Bluetooth HID keyboard control must use bounded HOGP reports, SMP encryption and subscribed notifications instead of the removed legacy fake device"
        );
        ensure!(
            PROTOCOL_SOURCE.contains("CaptureHci {")
                && PROTOCOL_SOURCE.contains("(1_000..=30_000).contains(duration_ms)")
                && PROTOCOL_SOURCE.contains("pub struct BluetoothHciCaptureRecordV2")
                && WORKER_SOURCE.contains("read_bluetooth_hci_capture_record")
                && WORKER_SOURCE.contains("last_bluetooth_hci_capture")
                && HOST_SOURCE.contains("last_bluetooth_hci_capture")
                && WEB_SOURCE.contains("command: 'capture_hci'")
                && WEB_SOURCE.contains("最多 4 MiB")
                && HDCTL_SOURCE.contains("ActionCommand::BluetoothHciCapture")
                && CAPABILITIES_SOURCE.contains("bounded-hci-capture-v2")
                && ROOTCANAL_ADAPTER_SOURCE.contains("btsnoop\\0")
                && ROOTCANAL_ADAPTER_SOURCE.contains("MAX_HCI_CAPTURE_BYTES")
                && !WEB_SOURCE.contains("rootcanal console"),
            "Bluetooth HCI capture must be typed, time and size bounded, instance-owned and delivered through diagnostics without arbitrary paths or console access"
        );
        ensure!(
            HOST_SOURCE.contains("instance guest type is immutable")
                && SHELL_SOURCE.contains("microdroid_platform_supported()")
                && SHELL_SOURCE
                    .contains("\"microdroid_supported\": microdroid_platform_supported()")
                && CAPABILITIES_SOURCE
                    .contains("target_os = \"windows\", target_arch = \"x86_64\"")
                && HOST_SOURCE
                    .contains("Microdroid requires macOS Apple Silicon or Windows x86_64")
                && HOST_SOURCE.contains("instance.settings.revision_conflict")
                && HOST_SOURCE.contains("Android device actions are not available for Microdroid")
                && HOST_SOURCE.contains("Microdroid is headless and has no display session")
                && HOST_SOURCE.contains("Microdroid is headless and does not support screenshots")
                && WORKER_SOURCE.contains("spawn_microdroid_deferred_adb_readiness")
                && WORKER_SOURCE.contains("Microdroid Payload is Ready but did not expose adbd")
                && HOST_SOURCE.contains("matches!(guest_kind, GuestKindV2::Android)")
                && HOST_SOURCE.contains("runtime_restart_waits_for_player(record)"),
            "Microdroid must remain an immutable, instance-scoped headless guest with no Android display controls"
        );
        ensure!(
            MICRODROID_EXIT_SOURCE.contains("payload finished with exit code ")
                && MICRODROID_EXIT_SOURCE.contains("VM ended: Shutdown")
                && MICRODROID_EXIT_SOURCE.contains("process_exit_code != Some(0)")
                && WORKER_SOURCE.contains("stop_after_microdroid_completion")
                && WORKER_SOURCE.contains("microdroid_payload_failed")
                && WORKER_SOURCE.contains("event = \"microdroid.payload.finished\"")
                && HOST_SOURCE.contains("read_completed_microdroid_payload_result")
                && HOST_SOURCE.contains("instance.microdroid.payload.completed")
                && !MICRODROID_EXIT_SOURCE.contains("microdroid-guest.log"),
            "finite Microdroid payloads must trust only the host vm launcher, retain the payload exit code and reuse exact stop cleanup"
        );
        ensure!(
            CONFIG_SOURCE.contains("payload_extra_apk_count: Option<u8>")
                && UPLOADS_SOURCE.contains("MAX_MICRODROID_CONFIG_BYTES: u64 = 64 * 1024")
                && UPLOADS_SOURCE.contains("inspect_microdroid_payload_apk")
                && CAPABILITIES_SOURCE.contains("extra_apk_selection_complete")
                && WORKER_SOURCE.contains("MicrodroidExtraApkCountMismatch")
                && SHELL_SOURCE.contains("inspection.declared_extra_apk_count"),
            "Microdroid extra APK selection must be bounded, instance-scoped and rejected before VM launch when it differs from the signed Payload declaration"
        );
        ensure!(
            PROTOCOL_SOURCE.contains("MicrodroidConsoleChallenge {")
                && PROTOCOL_SOURCE.contains("confirmed: true")
                && WORKER_SOURCE.contains("microdroid-console-in.fifo")
                && WORKER_SOURCE.contains("MicrodroidDebugLevelV2::Full")
                && HOST_SOURCE
                    .contains("Microdroid console challenge is not available for Android",)
                && MICRODROID_CONSOLE_SOURCE.contains("HD_CONSOLE_CHALLENGE_V1")
                && MICRODROID_CONSOLE_SOURCE.contains("HD_CONSOLE_RESPONSE_V1")
                && MICRODROID_CONSOLE_SOURCE
                    .contains("only one Microdroid console challenge is allowed per run")
                && MICRODROID_CONSOLE_SOURCE.contains("Duration::from_secs(5)")
                && HDCTL_SOURCE.contains("MicrodroidConsoleChallenge {")
                && HDCTL_SOURCE.contains("confirm: bool")
                && CAPABILITIES_SOURCE.contains("typed-nonce-v1")
                && CAPABILITIES_SOURCE.contains("cfg!(windows)")
                && CAPABILITIES_SOURCE.contains("\"unavailable\"")
                && HOST_SOURCE.contains(
                    "Microdroid console challenge is unavailable on Windows until the console input path provides a live, verified named-pipe channel",
                )
                && !WEB_SOURCE.contains("microdroid_console_challenge"),
            "Microdroid console input must remain a Full-debug, explicitly confirmed, fixed one-shot nonce challenge on Unix, fail closed on Windows until a verified named-pipe channel exists, and stay hidden from the general UI until real Guest attestation"
        );
        ensure!(
            CLIENT_SOURCE.contains("/v2/instances/{instance_id}/resource-admission")
                && HTTP_SOURCE.contains("get(resource_admission)")
                && HOST_SOURCE.contains("scope = \"resource_only\"")
                && CAPABILITIES_SOURCE.contains("pub fn resource_admission")
                && CAPABILITIES_SOURCE
                    .contains("resource_probe(&self.paths, Some(spec), Some(active_leases))")
                && CAPABILITIES_SOURCE.contains("lifecycle start path must continue")
                && HOST_SOURCE.contains("self.discovery.discover(Some(&record.spec)).await"),
            "resource UI preflight must avoid artifact/device discovery while lifecycle start keeps strict discovery"
        );
        ensure!(
            SHELL_SOURCE.contains("let restarting = was_active && restart;")
                && !SHELL_SOURCE.contains("if restart {\n            self.release_display();")
                && SHELL_SOURCE.contains("refresh_display_after_command")
                && SHELL_SOURCE.contains("Resume and restart preserve the Host-owned session")
                && !SHELL_SOURCE.contains(
                    "if refresh_display && result.is_ok() {\n            self.release_display();"
                ),
            "settings and lifecycle restart/resume must preserve the Player-owned native display session"
        );
        ensure!(
            SHELL_SOURCE.contains("set_macos_titlebar_fps")
                && SHELL_SOURCE.contains("titlebar_player_state(")
                && WEB_SOURCE.contains("if (message.type === 'titlebar')")
                && WEB_SOURCE.contains("state.show_host_fps")
                && WEB_SOURCE.contains("state.host_fps_milli"),
            "Host FPS and titlebar readiness must share one portable state on Windows and macOS"
        );
        ensure!(
            !WEB_SOURCE.contains("navigator.userAgent")
                && WEB_SOURCE.contains("正在读取 Host 网络能力")
                && WEB_SOURCE.contains("系统默认麦克风 · Host PCM → virtio-snd"),
            "portable UI must derive platform capabilities from Host state rather than browser identity"
        );
        ensure!(
            WEB_SOURCE.contains("current.host_audio_input !== next.host_audio_input")
                && WEB_SOURCE.contains("show_host_fps: false")
                && HOST_SOURCE.contains("allowed.name.clone_from(&next.name)")
                && HOST_SOURCE.contains("allowed.restart_policy = next.restart_policy")
                && HOST_SOURCE.contains("allowed.labels.clone_from(&next.labels)")
                && HOST_SOURCE
                    .contains("allowed.display.show_host_fps = next.display.show_host_fps",),
            "UI restart prompts must match the Host boundary: microphone routing restarts while FPS and Host-only metadata stay live"
        );
        ensure!(
            WEB_SOURCE.contains("停止录屏")
                && WEB_SOURCE.contains("关闭侧边栏")
                && WEB_SOURCE.contains("最大化或还原")
                && MACOS_BRIDGE_SOURCE.contains("recordingButton.toolTip = recordingTooltip")
                && MACOS_BRIDGE_SOURCE.contains("sidebarButton.toolTip = sidebarVisible"),
            "platform titlebars must expose matching state-dependent sidebar, recording and expansion intent"
        );
        ensure!(
            WEB_SOURCE.contains("action: 'minimize'")
                && WEB_SOURCE.contains("action: 'maximize'")
                && WEB_SOURCE.contains("action: 'close'")
                && WEB_SOURCE.contains(
                    "onDoubleClick={() => post({ command: 'window', action: 'maximize' })}"
                )
                && !WEB_SOURCE.contains("maximize_display")
                && SHELL_SOURCE.contains("let expanded = !window.is_maximized();")
                && SHELL_SOURCE.contains("shell.window_expanded = expanded;"),
            "Windows WebView controls must match macOS minimize/expand/close semantics"
        );
        ensure!(
            SHELL_SOURCE.contains("\"window_expansion\" =>")
                && SHELL_SOURCE.contains("aspect_fit_rect(")
                && SHELL_SOURCE.contains("window_expanded"),
            "fullscreen must use the native expansion event and aspect-fit the Android child"
        );
        ensure!(
            MACOS_BRIDGE_SOURCE.contains("NSWindowDidEnterFullScreenNotification")
                && MACOS_BRIDGE_SOURCE.contains("NSWindowDidExitFullScreenNotification")
                && MACOS_BRIDGE_SOURCE.contains("\\\"command\\\":\\\"window_expansion\\\"")
                && MACOS_BRIDGE_SOURCE.contains("MIN(viewWidth / orientedWidth")
                && MACOS_BRIDGE_SOURCE.contains("NSColor.blackColor.CGColor"),
            "macOS bridge must report fullscreen state and render uniformly over black"
        );
        ensure!(
            !SHELL_SOURCE.contains("MaximizeOrientationFinished")
                && !SHELL_SOURCE.contains("maximize_restore_orientation"),
            "fullscreen must not rotate Android or maintain a rotation restore state"
        );
        ensure!(
            NETWORK_SETUP_SOURCE.contains("schema_version=%s")
                && NETWORK_SETUP_SOURCE.contains("service_action=%s")
                && NETWORK_SETUP_SOURCE.contains("network_usable=%s")
                && NETWORK_SETUP_SOURCE.contains("rollback_install")
                && NETWORK_SETUP_SOURCE.contains("restoring previous state")
                && !NETWORK_SETUP_SOURCE.contains("status() {\n    require_root"),
            "macOS network service must expose non-root status and transactional installation"
        );
        ensure!(
            SHELL_SOURCE.contains("administrator_install_apple_script")
                && SHELL_SOURCE.contains("script_matches_embedded"),
            "desktop shell must use the shared embedded network installer contract"
        );
        ensure!(
            SHELL_SOURCE.contains("NetworkSetupRefreshed {")
                && SHELL_SOURCE.contains("network_setup_generation")
                && SHELL_SOURCE.contains("generation != self.network_setup_generation")
                && SHELL_SOURCE.contains(
                    "self.network_setup_refresh_in_flight || self.command_in_flight || self.closing",
                )
                && SHELL_SOURCE.contains("request_network_setup_status(false)")
                && SHELL_SOURCE.contains("request_network_setup_status(true)")
                && SHELL_SOURCE.contains("ui.network_status.hidden_refresh_rejected")
                && SHELL_SOURCE.contains("ui.network_status.hidden_install_rejected")
                && SHELL_SOURCE.contains("ui.network_status.probe.started")
                && !SHELL_SOURCE
                    .split("event_loop.run")
                    .next()
                    .unwrap_or_default()
                    .contains("request_network_setup_status"),
            "network status refresh must be on-demand, reject stale generations and serialize with commands"
        );
        ensure!(
            SHELL_SOURCE.contains("SnapshotRefreshGate")
                && SHELL_SOURCE.contains("self.refresh_gate.invalidate()")
                && SHELL_SOURCE.contains("if !self.refresh_gate.complete(generation)")
                && SHELL_SOURCE.contains("目标实例仍在同步；为避免控制错误实例")
                && SHELL_SOURCE.contains("当前选择已切换，未覆盖当前实例界面")
                && SHELL_SOURCE.contains("ui.snapshot.stale_rejected")
                && SHELL_SOURCE.contains("ui.instance_operation.target_rejected")
                && SHELL_SOURCE.contains("ui.async_result.selection_changed"),
            "instance selection, targeted operations and async completions must reject stale UI state"
        );
        let pre_handler_source = SHELL_SOURCE
            .split("event_loop.run")
            .next()
            .context("desktop shell has no native event loop")?;
        ensure!(
            SHELL_SOURCE.contains("Event::Resumed")
                && SHELL_SOURCE.contains("struct PendingShell")
                && SHELL_SOURCE.contains("struct RunningShell")
                && SHELL_SOURCE.contains("start_running_shell(active, inputs)")
                && SHELL_SOURCE.contains(".create_window(window_attributes)")
                && SHELL_SOURCE.contains("ui.lifecycle.startup.failed")
                && NATIVE_DISPLAY_SOURCE.contains("bscp-hd-ca-{user_scope}-{}.sock")
                && NATIVE_DISPLAY_SOURCE.contains("std::process::id()")
                && !pre_handler_source.contains("create_window(window_attributes)")
                && !pre_handler_source.contains("WebSurfaces::new"),
            "native windows and WebViews must initialize exactly once after the event loop resumes"
        );
        let pre_artifact_source = SHELL_SOURCE
            .split("let bundled_signed")
            .next()
            .context("desktop shell has no bundled artifact initialization")?;
        ensure!(
            pre_artifact_source.contains("cli.data_root.is_none()")
                && pre_artifact_source.contains("activate_existing_desktop_application")
                && MACOS_BRIDGE_SOURCE
                    .contains("runningApplicationsWithBundleIdentifier:bundleIdentifier")
                && MACOS_BRIDGE_SOURCE.contains("NSApplicationActivateAllWindows")
                && WINDOWS_TITLEBAR_SOURCE.contains("HD_DEFAULT_UI_PROFILE_V1")
                && WINDOWS_TITLEBAR_SOURCE.contains("HD_DEFAULT_UI_OWNER_V1_")
                && WINDOWS_TITLEBAR_SOURCE.contains("GetWindowThreadProcessId")
                && WINDOWS_TITLEBAR_SOURCE.contains("SetForegroundWindow"),
            "default Windows and macOS launches must activate the existing HD window before artifact or WebView initialization while explicit data-root profiles remain isolated"
        );
        ensure!(
            SHELL_SOURCE.contains("first_paint_timed_out(")
                && SHELL_SOURCE.contains("window.is_visible() == Some(false)")
                && SHELL_SOURCE.contains("ui.webview.asset.response")
                && SHELL_SOURCE.contains("ui.webview.page_load")
                && SHELL_SOURCE.contains("ui.lifecycle.first_paint.ready")
                && SHELL_SOURCE.contains("ui.lifecycle.first_paint.visibility_rejected")
                && SHELL_SOURCE.contains("ui.lifecycle.first_paint.timed_out")
                && SHELL_SOURCE.contains("*startup_error_for_loop.borrow_mut() = Some(error)")
                && SHELL_SOURCE.contains("active.exit()"),
            "first paint must confirm native visibility or propagate a bounded startup failure"
        );
        ensure!(
            NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("open -n \"$APP\"")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("TOTAL_UI_COUNT")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("TOTAL_HOST_COUNT")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE
                    .contains("tried to run event handler, but no handler was set")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("ui.lifecycle.startup.failed")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("FIRST_PAINT_BUDGET_SECONDS=15")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("FIRST_PAINT_WAIT_ATTEMPTS")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("$STAGE/processes.txt")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("SECONDARY_DATA")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE
                    .contains("second launch created another UI process")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("secondary_data_root_created:false")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("shutdown --stop-all")
                && NATIVE_LIFECYCLE_SMOKE_SOURCE.contains("macos-ui-native-lifecycle"),
            "packaged Finder lifecycle smoke must reject pre-handler errors and prove shutdown cleanup"
        );
        ensure!(
            ADB_SOURCE.contains("getprop(serial, \"init.svc.bootanim\")")
                && ADB_SOURCE.contains("getprop(serial, \"service.bootanim.exit\")")
                && ADB_SOURCE.contains("shell(serial, &[\"pidof\", \"bootanimation\"])")
                && ADB_SOURCE.contains("service\", \"check\", \"input")
                && ADB_SOURCE.contains("service\", \"check\", \"window")
                && ADB_SOURCE.contains("cmd\", \"user\", \"is-user-unlocked\", \"0")
                && ADB_SOURCE.contains("\"wait-for-background-handler\"")
                && ADB_SOURCE.contains("READY_PACKAGE_HANDLER_TIMEOUT_MILLIS")
                && !ADB_SOURCE.contains("wait-for-broadcast-idle")
                && ADB_SOURCE.contains("READY_STABLE_SAMPLES")
                && WORKER_SOURCE.contains("worker.adb.deferred_policy_readiness.failed")
                && WORKER_SOURCE.contains("worker.adb.deferred_final_readiness.failed")
                && WORKER_SOURCE.contains("worker.adb.deferred_interactive_policy.retrying")
                && WORKER_SOURCE.contains("DEFERRED_INTERACTIVE_POLICY_TIMEOUT"),
            "Android ADB readiness must be stable, interactive, and revalidated after startup policy"
        );
        ensure!(
            PLATFORM_SOURCE.contains("spawn detached process reaper")
                && PLATFORM_SOURCE.contains(".name(format!(\"hd-reap-{pid}\"))")
                && PLATFORM_SOURCE.contains("child.wait()"),
            "Unix detached Host and Worker children must be actively reaped"
        );
        ensure!(
            RETENTION_SOURCE.contains("fn finished_run_ephemeral_artifact_names()")
                && RETENTION_SOURCE.contains("initrd-android-hd.img")
                && RETENTION_SOURCE.contains("MAX_MICRODROID_EXTRA_APKS")
                && RETENTION_SOURCE.contains("microdroid-extra-{index}.idsig")
                && RETENTION_SOURCE.contains("RunNotFinished")
                && RETENTION_SOURCE.contains(
                    "removed_ephemeral.extend(remove_finished_run_ephemeral_artifacts(&path)?)",
                )
                && WORKER_SOURCE.contains("runtime.run.ephemeral_artifact.removed")
                && WORKER_SOURCE.contains("maintenance = \"pre_start_retention\""),
            "finalized runs must drop reproducible launch artifacts without touching active runs"
        );
        ensure!(
            CLIENT_SOURCE.contains("/v2/ui-snapshot")
                && HTTP_SOURCE.contains("uiSnapshot")
                && HOST_SOURCE.contains("pub fn ui_snapshot")
                && HOST_SOURCE.contains("let records = self.store.list_instances()?")
                && SHELL_SOURCE.contains(".ui_snapshot(selected_id)")
                && SHELL_SOURCE.contains("Event::AboutToWait")
                && SHELL_SOURCE.contains("self.snapshot_poll_mode().interval()")
                && SHELL_SOURCE.contains("WindowEvent::Occluded(occluded)")
                && SHELL_SOURCE.contains("ui.window.occlusion.changed")
                && SHELL_SOURCE.contains("self.root_was_minimized || self.root_occluded")
                && !SHELL_SOURCE.contains("std::thread::sleep(Duration::from_millis(100))")
                && !SHELL_SOURCE.contains("client.list_instances().await")
                && !SHELL_SOURCE.contains("client.get_instance(id).await"),
            "UI polling must use one atomic Host snapshot, adaptive intervals and native event-loop timing"
        );
        let occlusion_branch = SHELL_SOURCE
            .split("WindowEvent::Occluded(occluded)")
            .nth(1)
            .and_then(|source| source.split("WindowEvent::Focused(focused)").next())
            .context("desktop shell has no isolated native occlusion branch")?;
        ensure!(
            occlusion_branch.contains("shell.root_occluded = occluded")
                && occlusion_branch.contains("ui.window.occlusion.changed")
                && !occlusion_branch.contains("release_display")
                && !occlusion_branch.contains("suspend_display_for_minimize")
                && !occlusion_branch.contains("native_display.hide"),
            "occlusion may throttle control-plane work but must preserve the Android display session"
        );
        ensure!(
            SHELL_SOURCE.contains("Event::Suspended")
                && SHELL_SOURCE.contains("Event::Resumed")
                && SHELL_SOURCE.contains("suspend_display_for_native_lifecycle")
                && SHELL_SOURCE.contains("resume_display_after_native_lifecycle")
                && SHELL_SOURCE.contains("ui.lifecycle.suspended")
                && SHELL_SOURCE.contains("ui.lifecycle.resumed")
                && SHELL_SOURCE.contains("self.native_suspended")
                && SHELL_SOURCE.contains("self.set_viewport_hidden()")
                && SHELL_SOURCE.contains("self.release_display()"),
            "native suspension must release the Android display and resume must schedule a fresh attachment"
        );
        ensure!(
            MACOS_BRIDGE_SOURCE.contains("NSWorkspaceWillSleepNotification")
                && MACOS_BRIDGE_SOURCE.contains("NSWorkspaceDidWakeNotification")
                && MACOS_BRIDGE_SOURCE.contains("HDWorkspaceLifecycleObserversKey")
                && MACOS_BRIDGE_SOURCE.contains(
                    "{\\\"command\\\":\\\"native_lifecycle\\\",\\\"state\\\":\\\"suspended\\\"}",
                )
                && MACOS_BRIDGE_SOURCE.contains(
                    "{\\\"command\\\":\\\"native_lifecycle\\\",\\\"state\\\":\\\"resumed\\\"}",
                )
                && SHELL_SOURCE.contains("\"native_lifecycle\" => match")
                && SHELL_SOURCE.contains("Some(\"suspended\")")
                && SHELL_SOURCE.contains("Some(\"resumed\")"),
            "macOS workspace sleep and wake notifications must drive the native display lifecycle"
        );
        ensure!(
            WINDOWS_TITLEBAR_SOURCE.contains("SetWindowSubclass")
                && WINDOWS_TITLEBAR_SOURCE.contains("WM_POWERBROADCAST")
                && WINDOWS_TITLEBAR_SOURCE.contains("PBT_APMSUSPEND")
                && WINDOWS_TITLEBAR_SOURCE.contains("PBT_APMRESUMEAUTOMATIC")
                && WINDOWS_TITLEBAR_SOURCE.contains("NATIVE_LIFECYCLE_SUSPENDED_MESSAGE",)
                && WINDOWS_TITLEBAR_SOURCE.contains("NATIVE_LIFECYCLE_RESUMED_MESSAGE"),
            "Windows power broadcasts must drive the same native display lifecycle as macOS"
        );
        ensure!(
            ANDROID_READINESS_SOAK_SOURCE.contains("signed-artifact-store-v2")
                && ANDROID_READINESS_SOAK_SOURCE.contains("direct-development-v1")
                && ANDROID_READINESS_SOAK_SOURCE.contains("exact_closure")
                && ANDROID_READINESS_SOAK_SOURCE.contains("MANIFEST_COUNT")
                && ANDROID_READINESS_SOAK_SOURCE
                    .contains("direct-development store must contain exactly two bundle manifests")
                && ANDROID_READINESS_SOAK_SOURCE.contains("artifact_distribution"),
            "Android resource soak must cover both mutually exclusive packaged artifact distributions"
        );

        let network_source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/macos-network-setup.sh");
        ensure!(
            script_matches_embedded(&network_source),
            "repository network resource differs from the binary-embedded snapshot"
        );
        let temporary = tempfile::tempdir().context("create network UI contract workspace")?;
        let tampered = temporary.path().join("tampered-network-setup.sh");
        let mut tampered_bytes = NETWORK_SETUP_SOURCE.as_bytes().to_vec();
        tampered_bytes.extend_from_slice(b"\n# tampered\n");
        fs::write(&tampered, tampered_bytes).context("write tampered network resource")?;
        ensure!(
            !script_matches_embedded(&tampered),
            "tampered network resource must be rejected"
        );

        let timeout_started = Instant::now();
        let timeout_child_pid_file = temporary.path().join("timeout-child.pid");
        let mut timeout_command = Command::new("/bin/sh");
        timeout_command
            .env("HD_TIMEOUT_CHILD_PID_FILE", &timeout_child_pid_file)
            .args([
                "-c",
                "/bin/sleep 30 & child=$!; /usr/bin/printf '%s\n' \"$child\" > \"$HD_TIMEOUT_CHILD_PID_FILE\"; wait \"$child\"",
            ]);
        let timeout_error = run_command_with_timeout(timeout_command, Duration::from_millis(250))
            .expect_err("bounded command must time out");
        ensure!(
            timeout_error.contains("超过 250 毫秒")
                && timeout_started.elapsed() < Duration::from_secs(2),
            "bounded network status command did not terminate promptly: {timeout_error}"
        );
        let timeout_child_pid = fs::read_to_string(&timeout_child_pid_file)
            .context("bounded command did not report its child PID")?
            .trim()
            .parse::<u32>()
            .context("bounded command reported an invalid child PID")?;
        let mut timeout_child_alive = true;
        for _ in 0..50 {
            timeout_child_alive = Command::new("/bin/kill")
                .args(["-0", &timeout_child_pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !timeout_child_alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        ensure!(
            !timeout_child_alive,
            "bounded command left child process {timeout_child_pid} alive"
        );

        let concurrent_status = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    let network_source = &network_source;
                    scope.spawn(move || run_status_script(network_source, NETWORK_STATUS_TIMEOUT))
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| "concurrent network status reader panicked".to_owned())?
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .map_err(anyhow::Error::msg)?;
        ensure!(
            concurrent_status.iter().all(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("schema_version=2\n")
                    && String::from_utf8_lossy(&output.stdout).contains("source_sha256=")
            }),
            "concurrent bounded network status readers returned inconsistent results"
        );

        let stages_before = network_staging_entries()?;
        let status_output = Command::new("/bin/sh")
            .args(["-c", &staged_shell_command(NetworkSetupAction::Status)])
            .output()
            .context("execute embedded network status through production staging command")?;
        ensure!(
            status_output.status.success(),
            "embedded network status failed: {}",
            String::from_utf8_lossy(&status_output.stderr)
        );
        let status_stdout =
            String::from_utf8(status_output.stdout).context("network status is not UTF-8")?;
        ensure!(
            status_stdout.contains("schema_version=2\n")
                && status_stdout.contains("network_usable=")
                && status_stdout.contains("source_sha256="),
            "staged embedded network status returned an invalid contract"
        );
        ensure!(
            network_staging_entries()? == stages_before,
            "embedded network staging directory leaked"
        );

        let compiled_apple_script = temporary.path().join("network-install.scpt");
        let compile_output = Command::new("/usr/bin/osacompile")
            .arg("-e")
            .arg(administrator_install_apple_script())
            .arg("-o")
            .arg(&compiled_apple_script)
            .output()
            .context("compile administrator network AppleScript")?;
        ensure!(
            compile_output.status.success(),
            "administrator network AppleScript did not compile: {}",
            String::from_utf8_lossy(&compile_output.stderr)
        );
        ensure!(
            compiled_apple_script.is_file(),
            "administrator AppleScript compiler produced no artifact"
        );

        let evidence = json!({
            "schema_version": 1,
            "gate": "macos-ui-interaction",
            "status": "pass",
            "layout": {
                "collapsed": collapsed,
                "expanded": expanded,
                "display_maximized": maximized,
                "android_bounds": collapsed.android_bounds(),
                "portrait": portrait,
                "landscape": landscape,
                "portrait_fullscreen": portrait_fullscreen,
                "landscape_fullscreen": landscape_fullscreen,
            },
            "pointer_matrix": pointer_results,
            "native_titlebar": controls,
            "native_titlebar_fps": true,
            "fullscreen_aspect_fit": true,
            "network_admin_snapshot": {
                "resource_binding": "pass",
                "tamper_rejected": true,
                "staged_status": "pass",
                "stage_cleanup": "pass",
                "applescript_compile": "pass",
                "bounded_timeout": "pass",
                "process_tree_cleanup": "pass",
                "concurrent_readers": concurrent_status.len(),
                "stale_generation_rejected": true,
            },
            "web_contract_count": required_web_contracts.len(),
            "microdroid_instance_ui": {
                "guest_kind_selectable": true,
                "instance_settings_revisioned": true,
                "guest_kind_immutable": true,
                "android_player_tools_hidden": true,
                "android_devices_hidden": true,
                "headless_workload_page": true,
                "payload_import": true,
                "adb_shell": true,
                "graphics_settings_hidden": true,
                "host_display_actions_rejected": true,
                "runtime_restart_without_display_session": true,
            },
            "snapshot_concurrency": {
                "stale_selection_rejected": true,
                "stale_mutation_rejected": true,
                "duplicate_completion_rejected": true,
                "operation_target_bound": true,
                "screen_recording_result_target_bound": true,
                "structured_events": [
                    "ui.snapshot.stale_rejected",
                    "ui.instance_operation.target_rejected",
                    "ui.async_result.selection_changed",
                ],
            },
            "ui_idle_poll_performance": {
                "host_requests_per_refresh": 1,
                "lifecycle_transition_ms": 250,
                "foreground_player_ms": 1000,
                "foreground_auxiliary_ms": 2000,
                "background_ms": 2000,
                "minimized_ms": 5000,
                "occluded_ms": 5000,
                "legacy_minimized_requests_per_minute": 480,
                "current_minimized_requests_per_minute": 12,
                "current_occluded_requests_per_minute": 12,
                "request_reduction_percent": 97.5,
                "native_event_loop_timer": true,
                "atomic_list_and_selected": true,
            },
            "network_status_background_work": {
                "startup_probe": false,
                "legacy_player_probes_per_hour": 120,
                "current_player_probes_per_hour": 0,
                "foreground_devices_interval_ms": 30000,
                "foreground_devices_max_probes_per_hour": 120,
                "background_suspended": true,
                "minimized_suspended": true,
                "occluded_suspended": true,
                "manual_refresh_preserved": true,
                "install_completion_refresh_preserved": true,
                "stale_page_result_rejected": true,
            },
            "native_event_lifecycle": {
                "window_created_after_resumed": true,
                "webviews_created_after_resumed": true,
                "single_running_shell": true,
                "native_suspend_releases_display": true,
                "native_resume_reattaches_display": true,
                "macos_workspace_sleep_observed": true,
                "macos_workspace_wake_observed": true,
                "windows_power_suspend_observed": true,
                "windows_power_resume_observed": true,
                "occlusion_preserves_display": true,
                "startup_error_propagated": true,
                "first_paint_timeout_ms": FIRST_PAINT_TIMEOUT.as_millis(),
                "first_paint_visibility_confirmed": true,
                "invisible_process_rejected": true,
                "finder_product_smoke": "scripts/macos-ui-native-lifecycle-smoke.sh",
            },
            "android_readiness_soak_distribution": {
                "signed_artifact_store": true,
                "signed_exact_closure_required": true,
                "direct_development": true,
                "direct_bundle_manifest_count": 2,
                "mixed_distribution_rejected": true,
            },
            "android_adb_readiness": {
                "boot_completed": "sys.boot_completed=1",
                "boot_animation_service": "init.svc.bootanim=stopped",
                "boot_animation_exit": "service.bootanim.exit=1",
                "boot_animation_process": "pidof bootanimation=absent",
                "primary_user_unlocked": "cmd user is-user-unlocked 0=true",
                "interactive_services": ["SurfaceFlinger", "input", "window", "package"],
                "package_manager_handlers": ["responsive foreground", "idle background"],
                "package_manager_handler_timeout_ms": 5000,
                "global_broadcast_idle_required": false,
                "stable_samples": 2,
                "post_policy_revalidated": true,
                "post_display_revalidated": true,
                "interactive_policy_bounded_retry": true,
                "exit_request_is_insufficient": true,
            },
            "detached_process_reaping": {
                "unix_reaper_thread": true,
                "worker_delete_zombie_free": true,
                "host_shutdown_zombie_free": true,
            },
            "runtime_storage_hygiene": {
                "finished_run_ephemeral_cleanup": true,
                "legacy_finished_run_upgrade_cleanup": true,
                "active_run_protected": true,
                "instance_storage_preserved": true,
            },
        });
        let bytes = serde_json::to_vec_pretty(&evidence)?;
        if let Some(path) = output {
            hd_platform::write_owner_only(&path, &bytes)
                .with_context(|| format!("write macOS UI evidence {}", path.display()))?;
        } else {
            println!("{}", String::from_utf8(bytes)?);
        }
        Ok(())
    }

    fn network_staging_entries() -> Result<BTreeSet<OsString>> {
        let entries = fs::read_dir(Path::new("/private/tmp"))
            .context("read /private/tmp for network staging audit")?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().starts_with("hd-network-ui."))
            .collect();
        Ok(entries)
    }

    fn output_path() -> Result<Option<PathBuf>> {
        let mut args = std::env::args_os().skip(1);
        let Some(argument) = args.next() else {
            return Ok(None);
        };
        ensure!(
            argument == "--output",
            "usage: hd-ui-contract-smoke [--output PATH]"
        );
        let path = args
            .next()
            .map(PathBuf::from)
            .context("--output requires a path")?;
        ensure!(args.next().is_none(), "unexpected extra arguments");
        Ok(Some(path))
    }
}
