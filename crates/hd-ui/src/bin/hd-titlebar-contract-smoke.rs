use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use hd_core::FrameMetricsV2;
use hd_ui::ui_contract::{TITLEBAR_ACTION_CONTRACTS, aspect_size_from_preferred_area};
use serde_json::{Value, json};

const WEB_SOURCE: &str = include_str!("../../../../web/src/main.tsx");
const WEB_STYLE_SOURCE: &str = include_str!("../../../../web/src/style.css");
const WINDOWS_REAL_GUEST_SOURCE: &str = include_str!("../../../../scripts/windows-real-guest.ps1");
const XTASK_SOURCE: &str = include_str!("../../../../xtask/src/main.rs");
const ROOT_BUILD_SOURCE: &str = include_str!("../../../../../build_all.bat");
const SHELL_SOURCE: &str = include_str!("../web_shell.rs");
const UI_CONTRACT_SOURCE: &str = include_str!("../ui_contract.rs");
const WINDOWS_TITLEBAR_SOURCE: &str = include_str!("../../../hd-platform/src/windows_titlebar.rs");
const WINDOWS_PLATFORM_SOURCE: &str = include_str!("../../../hd-platform/src/windows.rs");
const HOST_SOURCE: &str = include_str!("../../../hd-runtime/src/host.rs");
const WORKER_SOURCE: &str = include_str!("../../../hd-runtime/src/worker.rs");
const BACKEND_SOURCE: &str = include_str!("../../../hd-runtime/src/backend.rs");
const DEV_SOURCE: &str = include_str!("../../../hd-runtime/src/dev.rs");
const ADB_SOURCE: &str = include_str!("../../../hd-runtime/src/adb.rs");
const NATIVE_DISPLAY_HOST_SOURCE: &str =
    include_str!("../../../hd-platform/src/native_display_host.rs");
const WINDOWS_NATIVE_SUBWINDOW_SOURCE: &str =
    include_str!("../../../../../hardware/google/gfxstream/host/NativeSubWindow_win32.cpp");
const GFXSTREAM_FRAMEBUFFER_SOURCE: &str =
    include_str!("../../../../../hardware/google/gfxstream/host/FrameBuffer.cpp");
const GFXSTREAM_COLOR_BUFFER_SOURCE: &str =
    include_str!("../../../../../hardware/google/gfxstream/host/ColorBuffer.cpp");
const GFXSTREAM_DISPLAY_SURFACE_SOURCE: &str =
    include_str!("../../../../../hardware/google/gfxstream/host/DisplaySurface.cpp");
const GFXSTREAM_POST_WORKER_SOURCE: &str =
    include_str!("../../../../../hardware/google/gfxstream/host/PostWorker.cpp");
const GFXSTREAM_DISPLAY_VK_HEADER_SOURCE: &str =
    include_str!("../../../../../hardware/google/gfxstream/host/vulkan/DisplayVk.h");
const GFXSTREAM_DISPLAY_VK_SOURCE: &str =
    include_str!("../../../../../hardware/google/gfxstream/host/vulkan/DisplayVk.cpp");
const GFXSTREAM_TELEMETRY_SOURCE: &str =
    include_str!("../../../../../hardware/google/gfxstream/host/vulkan/HdDisplayTelemetry.cpp");
const GFXSTREAM_RENDERER_SOURCE: &str =
    include_str!("../../../../../hardware/google/gfxstream/host/virtio-gpu-gfxstream-renderer.cpp");
const GFXSTREAM_DISPLAY_SELECTION_SOURCE: &str = concat!(
    include_str!("../../../../../hardware/google/gfxstream/host/HdDisplaySelection.h"),
    include_str!("../../../../../hardware/google/gfxstream/host/HdDisplaySelection.cpp")
);
const GFXSTREAM_HOST_RECORDER_WINDOWS_SOURCE: &str =
    include_str!("../../../../../hardware/google/gfxstream/host/HdHostRecorderWin.cpp");
const CROSVM_WINDOWS_DISPLAY_SOURCE: &str =
    include_str!("../../../../../external/crosvm/gpu_display/src/gpu_display_win/mod.rs");
const CROSVM_WINDOWS_SURFACE_SOURCE: &str =
    include_str!("../../../../../external/crosvm/gpu_display/src/gpu_display_win/surface.rs");
const CROSVM_WINDOWS_MESSAGE_SOURCE: &str = include_str!(
    "../../../../../external/crosvm/gpu_display/src/gpu_display_win/window_message_processor.rs"
);
const CROSVM_WINDOWS_MOUSE_SOURCE: &str = include_str!(
    "../../../../../external/crosvm/gpu_display/src/gpu_display_win/mouse_input_manager.rs"
);
const CROSVM_WINDOWS_VIRTUAL_DISPLAY_SOURCE: &str = include_str!(
    "../../../../../external/crosvm/gpu_display/src/gpu_display_win/virtual_display_manager.rs"
);
const CROSVM_VIRTIO_GPU_SOURCE: &str =
    include_str!("../../../../../external/crosvm/devices/src/virtio/gpu/virtio_gpu.rs");
const CROSVM_AUDIO_SOURCE: &str = include_str!(
    "../../../../../external/crosvm/devices/src/virtio/snd/common_backend/async_funcs.rs"
);
const CROSVM_WINDOWS_BROKER_SOURCE: &str =
    include_str!("../../../../../external/crosvm/src/crosvm/sys/windows/broker.rs");
const CROSVM_WINDOWS_GPU_MODE_SOURCE: &str =
    include_str!("../../../../../external/crosvm/vm_control/src/sys/windows/gpu.rs");
const CROSVM_WINDOWS_VSOCK_SOURCE: &str =
    include_str!("../../../../../external/crosvm/devices/src/virtio/vsock/sys/windows/vsock.rs");
const CROSVM_WIN_AUDIO_BUILD_SOURCE: &str =
    include_str!("../../../../../external/crosvm/win_audio/build.rs");
const RUNTIME_ARTIFACTS_SOURCE: &str = include_str!("../../../hd-runtime/src/artifacts.rs");
const MACOS_BRIDGE_SOURCE: &str =
    include_str!("../../../hd-platform/src/native_display_host_macos.m");

fn main() -> Result<()> {
    verify_portable_commands()?;
    verify_android_readiness_contract()?;
    verify_frame_metrics_contract()?;
    let windows_binding_count = verify_windows_webview_contract()?;
    #[cfg(target_os = "macos")]
    verify_native_macos_contracts()?;
    #[cfg(not(target_os = "macos"))]
    verify_native_macos_contracts();

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "ok",
            "portable_titlebar_actions": TITLEBAR_ACTION_CONTRACTS.len(),
            "global_actions": ["sidebar"],
            "android_player_actions": TITLEBAR_ACTION_CONTRACTS.len() - 1,
            "window_actions": ["minimize", "maximize", "close"],
            "shared_player_state": true,
            "exact_windows_webview_bindings": windows_binding_count,
            "windows_webview_contract_checked": true,
            "native_macos_contract_checked": cfg!(target_os = "macos"),
            "webview_titlebar": true,
            "webview_titlebar_portal_free": true,
            "native_display_reattach_siblings": true,
            "native_pointer_forwarding": true,
            "native_pointer_motion_async": true,
            "recorded_source_black_frame_detection": true,
            "dwm_composition_black_frame_detection": true,
            "dwm_composition_freeze_detection": true,
            "source_cadence_sample_accounting": true,
            "native_readback_accounting": true,
            "native_zero_copy_cpu_fallback_forbidden": true,
            "windows_release_copy_fallback_forbidden": true,
            "windows_physical_display_orientation": true,
            "windows_rotation_aware_pointer_projection": true,
            "windows_exact_guest_resolution": true,
            "windows_explicit_virtio_snd": true,
            "windows_vsock_read_failure_reset": true,
            "windows_gnu_static_r8brain": true,
            "windows_debug_msvc_runtime_forbidden": true,
            "windows_swiftshader_package_forbidden": true,
            "windows_root_build_gnu_closure": true,
            "selected_scanout_repost": true,
            "native_power_lifecycle": true,
            "native_system_icons": false,
            "duplicate_display_expansion": false,
            "default_profile_single_instance": true,
            "android_targeted_readiness": true,
        }))?
    );
    Ok(())
}

fn verify_frame_metrics_contract() -> Result<()> {
    let expected_intervals = 37;
    let metrics = FrameMetricsV2 {
        cadence_probe_source_intervals: expected_intervals,
        ..FrameMetricsV2::default()
    };
    let encoded = serde_json::to_vec(&metrics)?;
    let decoded: FrameMetricsV2 = serde_json::from_slice(&encoded)?;
    ensure!(
        decoded.cadence_probe_source_intervals == expected_intervals,
        "FrameMetricsV2 must preserve the independent source cadence interval count"
    );
    Ok(())
}

fn verify_android_readiness_contract() -> Result<()> {
    ensure!(
        ADB_SOURCE.contains("cmd\", \"user\", \"is-user-unlocked\", \"0")
            && ADB_SOURCE.contains("\"wait-for-background-handler\"")
            && ADB_SOURCE.contains("READY_PACKAGE_HANDLER_TIMEOUT_MILLIS")
            && !ADB_SOURCE.contains("wait-for-broadcast-idle"),
        "Android readiness must drain bounded PackageManager background work without waiting for the global broadcast queue"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE.contains("targeted_android_readiness")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("boot-user-unlock-and-bounded-package-manager-background-handler")
            && WINDOWS_REAL_GUEST_SOURCE.contains("wait-for-background-handler")
            && WINDOWS_REAL_GUEST_SOURCE.contains("global_broadcast_idle_required = $false"),
        "Windows real-guest evidence must independently verify targeted Android readiness"
    );
    Ok(())
}

fn verify_portable_commands() -> Result<()> {
    let mut commands = BTreeSet::new();
    for (action_id, message) in TITLEBAR_ACTION_CONTRACTS {
        let command: Value = serde_json::from_str(message)?;
        ensure!(
            command.get("command").is_some(),
            "{action_id} has no command"
        );
        ensure!(
            commands.insert(message),
            "duplicate command for {action_id}"
        );
    }
    let preferred_area = 1280.0 * 720.0;
    let portrait =
        aspect_size_from_preferred_area(preferred_area, 1080, 1920, 480.0, 640.0, 1300.0, 900.0);
    ensure!(
        (portrait.0 - 506.25).abs() < 0.01 && (portrait.1 - 900.0).abs() < 0.01,
        "portrait rotation must fit the monitor without changing the preferred area: {portrait:?}"
    );
    let landscape =
        aspect_size_from_preferred_area(preferred_area, 1920, 1080, 720.0, 360.0, 1300.0, 900.0);
    ensure!(
        (landscape.0 - 1280.0).abs() < 0.01 && (landscape.1 - 720.0).abs() < 0.01,
        "rotation round trip must restore the preferred landscape scale: {landscape:?}"
    );
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn verify_windows_webview_contract() -> Result<usize> {
    ensure!(
        WEB_SOURCE.contains("function TopSurface")
            && WEB_SOURCE.contains("function PlayerTitlebarTools")
            && WEB_SOURCE.contains("if (surface === 'top') return <TopSurface />")
            && WEB_SOURCE.contains("title={label}")
            && !WEB_SOURCE.contains("Tooltip.Portal")
            && !WEB_SOURCE.contains("Tooltip.Provider")
            && !WEB_STYLE_SOURCE.contains(".tooltip"),
        "the Windows React bundle must implement the dedicated top titlebar surface"
    );
    ensure!(
        WEB_SOURCE.contains("runtimeDisplays.length > 1")
            && WEB_SOURCE.contains("className=\"display-switcher\"")
            && WEB_SOURCE.contains("command: 'select_display'")
            && WEB_SOURCE.contains("aria-pressed={selected}")
            && WEB_STYLE_SOURCE.contains(".display-switcher-options")
            && WEB_SOURCE.find("className=\"display-switcher\"")
                < WEB_SOURCE.find("<nav className=\"sidebar-nav\""),
        "multi-display selection must live in the Player sidebar instead of the native titlebar"
    );
    ensure!(
        UI_CONTRACT_SOURCE.contains("pub const TOP_BAR_LOGICAL: f64 = 30.0;")
            && !UI_CONTRACT_SOURCE.contains("TitlebarDisplayState")
            && !UI_CONTRACT_SOURCE.contains("pub displays:")
            && WEB_SOURCE.contains("<span className=\"tool-divider\" />")
            && WEB_STYLE_SOURCE.contains("height: 30px"),
        "the compact WebView titlebar must exclude display selection and separate Android navigation from window controls"
    );
    ensure!(
        SHELL_SOURCE.contains("#[cfg(target_os = \"macos\")]\n        let top = None;")
            && SHELL_SOURCE.contains("#[cfg(not(target_os = \"macos\"))]")
            && SHELL_SOURCE.contains("install_windows_root_window_controller")
            && !SHELL_SOURCE.contains("install_windows_titlebar_controls")
            && !SHELL_SOURCE.contains("hide_windows_native_titlebar")
            && !SHELL_SOURCE.contains("set_windows_titlebar_layout")
            && !WINDOWS_TITLEBAR_SOURCE.contains("pub fn install_windows_titlebar_controls")
            && !WINDOWS_TITLEBAR_SOURCE.contains("pub fn hide_windows_native_titlebar")
            && !WINDOWS_TITLEBAR_SOURCE.contains("pub fn set_windows_titlebar_layout")
            && !WINDOWS_TITLEBAR_SOURCE.contains("WindowsTitlebarControlContract")
            && SHELL_SOURCE.contains("set_windows_window_content_aspect_ratio"),
        "Windows must use a top WebView and expose only the root lifecycle/aspect controller"
    );
    ensure!(
        SHELL_SOURCE.contains("let layout_before = shell.ipc_layout_signature();")
            && SHELL_SOURCE.contains("if shell.ipc_layout_signature() != layout_before")
            && SHELL_SOURCE.contains("fn ipc_layout_signature(&self)")
            && SHELL_SOURCE.contains("display_id: DisplayIdV2")
            && SHELL_SOURCE.contains("display_id: self.selected_display_id()"),
        "ordinary WebView titlebar clicks must not re-enter native layout, while same-size display selection must commit its new scanout"
    );
    let key_dispatch = SHELL_SOURCE
        .split("fn key(&mut self, key: &str)")
        .nth(1)
        .and_then(|source| source.split("fn trackpad").next())
        .unwrap_or_default();
    ensure!(
        key_dispatch.contains("UserEvent::InputFinished")
            && key_dispatch.contains("if result.is_err()")
            && !key_dispatch.contains("self.command_in_flight = true")
            && !key_dispatch.contains("self.ui_dirty = true"),
        "successful Android titlebar input must not disable or redraw the WebView controls"
    );
    ensure!(
        SHELL_SOURCE.contains("fn send_top(&self, message: &Value)")
            && SHELL_SOURCE.contains("fn send_body(&self, message: &Value)")
            && SHELL_SOURCE.contains("command_in_flight: false")
            && SHELL_SOURCE.contains("self.last_web_titlebar.as_ref() != Some(&web_titlebar)")
            && SHELL_SOURCE.contains("self.last_web_top_layout.as_ref() != Some(&top_layout)")
            && SHELL_SOURCE.contains("web_titlebar.sidebar_visible = false;")
            && SHELL_SOURCE.contains("one sidebar click to one React update")
            && SHELL_SOURCE.contains("The top surface consumes only these two layout fields")
            && WEB_SOURCE.contains("message.payload as Partial<LayoutState>")
            && SHELL_SOURCE.contains("shell.last_web_titlebar = None;")
            && SHELL_SOURCE.contains("shell.last_web_top_layout = None;")
            && WEB_SOURCE.contains("if (message.type === 'titlebar')"),
        "the top WebView must receive only deduplicated stable titlebar/layout state"
    );
    let occlusion_branch = SHELL_SOURCE
        .split("WindowEvent::Occluded(occluded)")
        .nth(1)
        .and_then(|source| source.split("WindowEvent::Focused(focused)").next())
        .context("desktop shell has no isolated native occlusion branch")?;
    ensure!(
        occlusion_branch.contains("if occluded && !shell.sidebar_collapsed")
            && occlusion_branch.contains("shell.sidebar_collapsed = true")
            && occlusion_branch.contains("apply_window_layout(window, webviews, shell, false)"),
        "occlusion must synchronously hide the sidebar overlay before publishing its collapsed titlebar state"
    );
    let focus_branch = SHELL_SOURCE
        .split("WindowEvent::Focused(focused)")
        .nth(1)
        .and_then(|source| source.split("_ => {}").next())
        .context("desktop shell has no isolated native focus branch")?;
    ensure!(
        focus_branch.contains("if !focused && !shell.sidebar_collapsed")
            && focus_branch.contains("shell.sidebar_collapsed = true")
            && focus_branch.contains("apply_window_layout(window, webviews, shell, false)")
            && focus_branch.contains("ui.window.focus.changed"),
        "focus loss must synchronously hide the sidebar overlay before publishing its collapsed titlebar state"
    );
    ensure!(
        UI_CONTRACT_SOURCE.contains("let host_fps_milli = if show_host_fps")
            && UI_CONTRACT_SOURCE.contains("an invisible value must not participate")
            && UI_CONTRACT_SOURCE.contains("host_fps_milli,"),
        "hidden host FPS samples must not invalidate and recompose the Windows WebView2 titlebar"
    );
    ensure!(
        SHELL_SOURCE.contains("if self.sidebar_visible")
            && SHELL_SOURCE.contains("if self.content_visible")
            && SHELL_SOURCE.contains("A hidden WebView2 surface still executes scripts"),
        "hidden body WebViews must not receive background snapshot work beside the native Vulkan Player"
    );
    ensure!(
        SHELL_SOURCE.contains("let top_geometry_changed =")
            && SHELL_SOURCE.contains("let sidebar_geometry_changed =")
            && SHELL_SOURCE.contains("let content_geometry_changed =")
            && SHELL_SOURCE.contains("Hide an overlay before parking it at 1x1")
            && !SHELL_SOURCE.contains("self.sidebar.set_bounds(if sidebar_visible")
            && !SHELL_SOURCE.contains("self.content.set_bounds(if content_visible"),
        "overlay visibility changes must not relayout the stable titlebar, and a visible WebView2 child must be hidden before it is parked"
    );
    ensure!(
        SHELL_SOURCE.contains("if layout.page == Page::Player")
            && SHELL_SOURCE.contains("Entering Player must expose the retained native frame")
            && SHELL_SOURCE.contains("When leaving Player, cover the Android child"),
        "page transitions must commit the destination child surface before withdrawing the source surface"
    );
    ensure!(
        WEB_SOURCE.contains("function keepGuestFocusOnTitlebarPress")
            && WEB_SOURCE.contains("event.preventDefault();")
            && WEB_SOURCE.contains("onPointerDown={keepGuestFocusOnTitlebarPress}")
            && WEB_SOURCE
                .matches("onPointerDown={keepGuestFocusOnTitlebarPress}")
                .count()
                >= 5
            && WEB_SOURCE.contains("onMouseDown={keepGuestFocusOnTitlebarPress}")
            && WEB_SOURCE
                .matches("onMouseDown={keepGuestFocusOnTitlebarPress}")
                .count()
                >= 5,
        "WebView titlebar pointerdown must prevent focus before the later mouse event reaches WebView2"
    );
    ensure!(
        NATIVE_DISPLAY_HOST_SOURCE.contains("GetGUIThreadInfo(0, &raw mut gui)")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("gui.hwndFocus == target")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("SetFocus(target)"),
        "Windows must avoid redundant cross-process focus changes after WebView titlebar clicks"
    );
    ensure!(
        GFXSTREAM_DISPLAY_SURFACE_SOURCE.contains("bool changed = false;")
            && GFXSTREAM_DISPLAY_SURFACE_SOURCE.contains("if (!changed)")
            && GFXSTREAM_DISPLAY_SURFACE_SOURCE.contains("users->surfaceUpdated(this);"),
        "same-size host notifications must not rebuild the gfxstream swapchain"
    );
    ensure!(
        GFXSTREAM_FRAMEBUFFER_SOURCE.contains("bool surfaceSizeChanged = false;")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("bool alreadyCommitted = false;")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("if (alreadyCommitted) return true;")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("alreadyCommitted ? \"noop\" : \"publish\"")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("if (surfaceSizeChanged && m_displayVk)")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains(
                "m_displaySurface->getWidth() != width || m_displaySurface->getHeight() != height"
            ),
        "the HD commit path must publish only real DisplaySurface size mutations"
    );
    let telemetry_record_path = GFXSTREAM_TELEMETRY_SOURCE
        .split("void record(const char* presentMode,")
        .nth(1)
        .and_then(|source| source.split("private:").next())
        .unwrap_or_default();
    ensure!(
        GFXSTREAM_TELEMETRY_SOURCE.contains("std::condition_variable mCondition;")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("std::thread mWriter;")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("void writerLoop()")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("Keep only the newest interval")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("host_present_latency_ns_max")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("host_present_over_33ms")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("cadence_probe_interval_ns_max")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("cadence_probe_swapchain_recreate_count")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("cadence_probe_aspect_mismatch_count")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("cadence_probe_source_interval_ns_max")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("cadence_probe_post_worker_queue_delay_ns_max")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("cadence_probe_post_worker_work_ns_max")
            && GFXSTREAM_TELEMETRY_SOURCE
                .matches("\\\"present_mode\\\":\\\"%s\\\"")
                .count()
                >= 2
            && WINDOWS_REAL_GUEST_SOURCE.contains("present_mode = $presentMode")
            && WINDOWS_REAL_GUEST_SOURCE.contains("recognized Vulkan present mode")
            && GFXSTREAM_DISPLAY_VK_SOURCE.contains("const auto hdPresentStarted")
            && GFXSTREAM_DISPLAY_VK_SOURCE.contains("hostPresentLatencyNs")
            && !telemetry_record_path.contains("writePayload(")
            && !telemetry_record_path.contains("publishMetrics("),
        "successful Vulkan present telemetry must never perform pipe or filesystem I/O on the present thread"
    );
    verify_native_display_z_order()?;
    ensure!(
        WINDOWS_TITLEBAR_SOURCE.contains("pub fn install_windows_root_window_controller")
            && WINDOWS_TITLEBAR_SOURCE.contains("install_windows_root_lifecycle")
            && WINDOWS_TITLEBAR_SOURCE.contains("no hidden native toolbar participates")
            && !SHELL_SOURCE.contains("windows_titlebar_control_contracts"),
        "Windows must not create or lay out a hidden native titlebar behind the WebView"
    );

    let controls = TITLEBAR_ACTION_CONTRACTS
        .iter()
        .map(|(action_id, message)| (*action_id, *message))
        .collect::<Vec<_>>();

    ensure!(
        controls.len() == TITLEBAR_ACTION_CONTRACTS.len(),
        "Windows WebView titlebar action count drifted from the portable contract"
    );
    for (action_id, message) in &controls {
        ensure!(
            WEB_SOURCE.contains(
                message
                    .trim_matches('{')
                    .trim_matches('}')
                    .split(',')
                    .next()
                    .unwrap_or(message)
            ) || WEB_SOURCE.contains(action_id),
            "Windows WebView titlebar action {action_id} is not represented in the UI source"
        );
    }

    ensure!(
        WEB_SOURCE.contains("action: 'minimize'")
            && WEB_SOURCE.contains("action: 'maximize'")
            && WEB_SOURCE.contains("action: 'close'")
            && WEB_SOURCE
                .contains("onDoubleClick={() => post({ command: 'window', action: 'maximize' })}")
            && !WEB_SOURCE.contains("maximize_display"),
        "Windows WebView minimize/maximize/close semantics are incomplete"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE.contains("FindVisibleTopWebView")
            && WINDOWS_REAL_GUEST_SOURCE.contains("HD_NATIVE_TITLEBAR_V1")
            && WINDOWS_REAL_GUEST_SOURCE.contains("removed native titlebar layer")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("compact-webview-titlebar-and-real-pointer-input")
            && WINDOWS_REAL_GUEST_SOURCE.contains("webview_close_method = \"Win32-SendInput\"")
            && WINDOWS_REAL_GUEST_SOURCE.contains("ClickClientPoint(")
            && !WINDOWS_REAL_GUEST_SOURCE.contains("ClickTitlebarButton("),
        "the real Guest runner must locate and physically click the WebView titlebar without accepting the removed Win32 button layer"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE.contains("titlebar-window-tree.txt")
            && WINDOWS_REAL_GUEST_SOURCE.contains("visible_webview_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("hidden_body_webviews_excluded")
            && WINDOWS_REAL_GUEST_SOURCE.contains("only the titlebar WebView"),
        "Windows UI evidence must retain the visible WebView/window tree before an interactive-desktop input gate"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE.contains("ExerciseInteractiveResize")
            && WINDOWS_REAL_GUEST_SOURCE.contains("wmEnterSizeMove = 0x0231")
            && WINDOWS_REAL_GUEST_SOURCE.contains("wmExitSizeMove = 0x0232")
            && WINDOWS_REAL_GUEST_SOURCE.contains("interactive_resize_elapsed_ms")
            && WINDOWS_REAL_GUEST_SOURCE.contains("interactive_resize_final_geometry_aligned")
            && WINDOWS_REAL_GUEST_SOURCE.contains("interactive_resize_swapchain_recreate_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("HD_GFXSTREAM_RENDER_CHILD_V1")
            && WINDOWS_REAL_GUEST_SOURCE.contains("native_resize_cached_render_window")
            && WINDOWS_REAL_GUEST_SOURCE.contains("native_swapchain_final_extent_verified")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("gfxstream applied viewport never reached the final native extent")
            && WINDOWS_REAL_GUEST_SOURCE.contains("intermediateSwapchainSizes")
            && WINDOWS_REAL_GUEST_SOURCE.contains("Read-SharedTextFile")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("NativeDisplayHost, gfxstream render and crosvm input geometry split"),
        "Windows real-guest UI gate must stress interactive resize and prove the final native geometry transaction"
    );
    let windows_live_resize_hot_path = WINDOWS_TITLEBAR_SOURCE
        .split("unsafe fn validated_cached_display_child")
        .nth(1)
        .and_then(|source| {
            source
                .split("pub fn set_windows_window_content_aspect_ratio")
                .next()
        })
        .unwrap_or_default();
    let windows_platform_projection_hot_path = WINDOWS_PLATFORM_SOURCE
        .split("fn notify_crosvm_viewport_impl")
        .nth(1)
        .and_then(|source| {
            source
                .split("pub(crate) fn set_native_display_visibility")
                .next()
        })
        .unwrap_or_default();
    ensure!(
        WINDOWS_TITLEBAR_SOURCE.contains("fn native_display_children")
            && WINDOWS_TITLEBAR_SOURCE.contains("WM_SIZE can run at pointer cadence")
            && WINDOWS_TITLEBAR_SOURCE.contains("const fn wide_ascii_z")
            && WINDOWS_PLATFORM_SOURCE.contains("const fn wide_ascii_z")
            && !windows_live_resize_hot_path.contains("wide(")
            && !windows_platform_projection_hot_path.contains("wide(")
            && WINDOWS_TITLEBAR_SOURCE.contains("HD_CROSVM_INPUT_CHILD_V1")
            && WINDOWS_TITLEBAR_SOURCE.contains("HD_GFXSTREAM_RENDER_CHILD_V1")
            && WINDOWS_TITLEBAR_SOURCE.contains("HD_CROSVM_INPUT_CHILD_COOKIE_V1")
            && WINDOWS_TITLEBAR_SOURCE.contains("HD_GFXSTREAM_RENDER_CHILD_COOKIE_V1")
            && WINDOWS_TITLEBAR_SOURCE.contains("HD_ROOT_DISPLAY_HOST_COOKIE_V1")
            && WINDOWS_TITLEBAR_SOURCE.contains("validated_cached_display_child")
            && WINDOWS_TITLEBAR_SOURCE.contains("validated_cached_display_host")
            && WINDOWS_TITLEBAR_SOURCE.matches("FindWindowExW(").count() == 1
            && NATIVE_DISPLAY_HOST_SOURCE.contains("cache_windows_native_display_host")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("uncache_windows_native_display_host")
            && WINDOWS_TITLEBAR_SOURCE.contains("GetParent(child) } == display_host")
            && WINDOWS_TITLEBAR_SOURCE
                .contains("GetPropW(child, cookie.as_ptr()) } == display_host")
            && WINDOWS_TITLEBAR_SOURCE.contains(
                "both process-owned children and the Vulkan swapchain completely stationary"
            )
            && WINDOWS_TITLEBAR_SOURCE
                .contains("The previous implementation posted one cross-thread")
            && !WINDOWS_TITLEBAR_SOURCE.contains("SWP_ASYNCWINDOWPOS")
            && WINDOWS_TITLEBAR_SOURCE.contains("Falling through used to send a")
            && WINDOWS_TITLEBAR_SOURCE
                .contains("commit_current_root_resize(hwnd, reference_data, false)")
            && WINDOWS_TITLEBAR_SOURCE
                .contains("Mouse-up already performed the single forced resize")
            && CROSVM_WINDOWS_SURFACE_SOURCE.contains("if !force_commit")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$resizeSwapchainSizes.Count -gt 1")
            && !WINDOWS_TITLEBAR_SOURCE.contains("String::from_utf16_lossy(&class_name"),
        "Windows live resize must keep cached native HWNDs validated, avoid hot-path allocation and child-tree scans, keep child HWNDs stationary and force exactly one final swapchain transaction"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE.contains("retained-session-frame-without-swapchain-rebuild")
            && WINDOWS_REAL_GUEST_SOURCE.contains("ShowWindow($rootWindow, 6)")
            && WINDOWS_REAL_GUEST_SOURCE.contains("IsIconic($rootWindow)")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$minimizeRestoreProbe.Samples -lt 30")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$minimizeRestoreProbe.NearBlackFrames -ne 0")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("$minimizeSwapchainCountAfter -ne $minimizeSwapchainCountBefore")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("GetParent($renderWindow) -eq $displayHostWindow")
            && WINDOWS_REAL_GUEST_SOURCE.contains("GetParent($inputWindow) -eq $displayHostWindow"),
        "Windows real-guest UI gate must prove minimize/restore retains both native children and the presented frame without rebuilding the swapchain"
    );
    ensure!(
        !WINDOWS_REAL_GUEST_SOURCE.contains("PostClientDrag")
            && WINDOWS_REAL_GUEST_SOURCE.contains("DragClientPoint(")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$pointerSampleIntervalMillis = 8")
            && WINDOWS_REAL_GUEST_SOURCE.contains("android.settings.SETTINGS")
            && WINDOWS_REAL_GUEST_SOURCE.contains("pointer_animation_stress")
            && WINDOWS_REAL_GUEST_SOURCE.contains("warmup_drag_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("guest_gfxinfo_reset_after_warmup")
            && WINDOWS_REAL_GUEST_SOURCE.contains("gfxinfo\", \"com.android.settings\", \"reset")
            && WINDOWS_REAL_GUEST_SOURCE.contains("presented_frames_delta")
            && WINDOWS_REAL_GUEST_SOURCE.contains("host_present_over_16ms_delta")
            && WINDOWS_REAL_GUEST_SOURCE.contains("cadence_probe_over_50ms")
            && WINDOWS_REAL_GUEST_SOURCE.contains("cadence_probe_aspect_mismatch_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("source_cadence_average_interval_ns")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$maximumCadenceAverageNs = [long]20000000")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("$sourceCadenceAverageNs -gt $maximumCadenceAverageNs")
            && WINDOWS_REAL_GUEST_SOURCE.contains("post_worker_queue_delay_ns_max")
            && WINDOWS_REAL_GUEST_SOURCE.contains("post_worker_work_ns_max")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$maximumPostWorkerStageNs = [long]33333333")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("$postWorkerQueueDelayNsMax -gt $maximumPostWorkerStageNs")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("cadence_probe_post_worker_queue_over_33ms -ne 0")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("$postWorkerWorkNsMax -gt $maximumPostWorkerStageNs")
            && WINDOWS_REAL_GUEST_SOURCE.contains("cadence_probe_post_worker_work_over_33ms -ne 0")
            && WINDOWS_REAL_GUEST_SOURCE.contains("audio_underrun_error_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("audio_overrun_error_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("minimum_host_available_memory_bytes")
            && WINDOWS_REAL_GUEST_SOURCE.contains("maximum_host_memory_load_percent")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("host resource pressure invalidated the native present cadence sample")
            && WINDOWS_REAL_GUEST_SOURCE.contains("pointer-animation-gfxinfo.txt")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("dumpsys\", \"gfxinfo\", \"com.android.settings\", \"framestats")
            && WINDOWS_REAL_GUEST_SOURCE.contains("pointer-animation-logcat.txt")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("dropped, undersampled, blackened or stalled source frames",)
            && WINDOWS_REAL_GUEST_SOURCE.contains("guest_logcat_cleared_before_probe")
            && WINDOWS_REAL_GUEST_SOURCE.contains("guest_logcat_sha256")
            && WINDOWS_REAL_GUEST_SOURCE.contains("host_sample_interval_millis")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$pointerSampleIntervalMillis = 8")
            && WINDOWS_REAL_GUEST_SOURCE.contains("\"logcat\", \"-d\", \"-v\", \"threadtime\"")
            && WINDOWS_REAL_GUEST_SOURCE.contains("HD_GFXSTREAM_CADENCE_PROBE_V1")
            && WINDOWS_REAL_GUEST_SOURCE.contains("crosvm_input_visual_region_empty")
            && WINDOWS_REAL_GUEST_SOURCE.contains("DistinctFrames")
            && WINDOWS_REAL_GUEST_SOURCE.contains("FrameTransitions")
            && WINDOWS_REAL_GUEST_SOURCE.contains("MaxConsecutiveIdenticalFrames")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$dwmProbe.MaxConsecutiveIdenticalFrames -gt 8")
            && WINDOWS_REAL_GUEST_SOURCE.contains("black, frozen, or incomplete frame sequence")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("pointer-driven Android animation violated the native present budget"),
        "Windows UI real-guest gate must measure pointer-driven Android animation through the gfxstream input and present path"
    );
    ensure!(
        CROSVM_AUDIO_SOURCE.contains("Underrun. No new DescriptorChain while running")
            && CROSVM_AUDIO_SOURCE.contains("Overrun. No new DescriptorChain while running")
            && CROSVM_AUDIO_SOURCE.contains("debug!("),
        "recoverable crosvm audio under/overruns must not create a period-rate error log storm"
    );
    ensure!(
        GFXSTREAM_TELEMETRY_SOURCE.contains("cadence_probe_source_intervals")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("++mCadenceProbeSourceIntervals")
            && GFXSTREAM_TELEMETRY_SOURCE
                .contains("frameEnqueueTimestampNs > mCadenceProbeLastSourceEnqueueTimestampNs",)
            && WINDOWS_REAL_GUEST_SOURCE.contains("$sourceCadenceIntervals")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$sourceCadenceIntervals -ne $cadenceIntervals")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("cadence_probe_source_interval_ns_max -gt 50000000",)
            && WINDOWS_REAL_GUEST_SOURCE.contains("cadence_probe_source_over_33ms -gt 2")
            && WINDOWS_REAL_GUEST_SOURCE.contains("cadence_probe_source_over_50ms -ne 0")
            && WINDOWS_REAL_GUEST_SOURCE.contains("cadence_probe_source_over_100ms -ne 0"),
        "Windows source cadence must use complete enqueue samples and reject visible jitter spikes"
    );
    ensure!(
        !GFXSTREAM_TELEMETRY_SOURCE.contains(r#"\"cpu_readback_bytes\":0"#)
            && !GFXSTREAM_TELEMETRY_SOURCE.contains(r#"\"software_blit_count\":0"#)
            && GFXSTREAM_TELEMETRY_SOURCE.contains("sample.cpuReadbackBytes")
            && GFXSTREAM_TELEMETRY_SOURCE.contains("sample.softwareBlitCount")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("recordHdCpuReadbackBytes")
            && GFXSTREAM_COLOR_BUFFER_SOURCE.contains("recordHdSoftwareBlit")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$recordingReadbackCheckpoint")
            && WINDOWS_REAL_GUEST_SOURCE.contains("cpu_readback_bytes_post_recording_delta")
            && WINDOWS_REAL_GUEST_SOURCE.contains("software_blit_count_post_recording_delta"),
        "Windows zero-copy evidence must use real readback/blit counters and isolate recording"
    );
    ensure!(
        BACKEND_SOURCE.contains("HD_NATIVE_ZERO_COPY_REQUIRED")
            && DEV_SOURCE.contains("cfg!(debug_assertions)")
            && DEV_SOURCE.contains("HD_DEV_ALLOW_DISPLAY_COPY_FALLBACK")
            && CROSVM_WINDOWS_DISPLAY_SOURCE.contains("fn strict_zero_copy_required()")
            && CROSVM_WINDOWS_DISPLAY_SOURCE.contains("HD_NATIVE_ZERO_COPY_REQUIRED")
            && CROSVM_WINDOWS_DISPLAY_SOURCE
                .contains("let framebuffer_data = if strict_zero_copy_required()")
            && CROSVM_WINDOWS_DISPLAY_SOURCE.contains("Vec::new()")
            && WINDOWS_REAL_GUEST_SOURCE.contains("native_zero_copy_required")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$cpuFramebufferFlipCount")
            && WINDOWS_REAL_GUEST_SOURCE.contains("cpu_framebuffer_flip_attempt_count"),
        "Windows native zero-copy mode must not expose a CPU framebuffer fallback"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE.contains("$packagedAdb = Join-Path $hostToolsRoot \"adb.exe\"")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("$packagedAapt2 = Join-Path $hostToolsRoot \"aapt2.exe\"")
            && WINDOWS_REAL_GUEST_SOURCE.contains("packaged_tooling =")
            && WINDOWS_REAL_GUEST_SOURCE.contains("adb_sha256 =")
            && WINDOWS_REAL_GUEST_SOURCE.contains("aapt2_sha256 =")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("isolated Windows product regression forbids Guest settle workarounds")
            && WINDOWS_REAL_GUEST_SOURCE.contains(
                "isolated Windows product regression requires packaged adb.exe and aapt2.exe",
            ),
        "Windows real-guest runs must prefer the packaged ADB and aapt2 closure and record the resolved tool paths"
    );
    ensure!(
        CROSVM_WINDOWS_MOUSE_SOURCE.contains("next_primary_tracking_id")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("active_primary_tracking_id")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("tracking_id.wrapping_add(1) & i32::MAX")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("else if pressed")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("touch_move_interval")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("display_refresh_rate.clamp(30, 240)")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("last_primary_touch_move")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("Instant::now()")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("now.duration_since(last)")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("elapsed.as_nanos() / interval_nanos")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("Preserve the fractional cadence remainder")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("exact drag endpoint")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("multitouch_absolute_x(pos.x)")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("multitouch_absolute_y(pos.y)")
            && CROSVM_WINDOWS_MOUSE_SOURCE.contains("not repeat tracking ID or BTN_TOUCH"),
        "Windows mouse drag updates must preserve one type-B contact, retain fractional host-sample cadence, and coalesce motion without losing the endpoint"
    );
    ensure!(
        GFXSTREAM_HOST_RECORDER_WINDOWS_SOURCE.contains("isNearBlackFrame")
            && GFXSTREAM_HOST_RECORDER_WINDOWS_SOURCE.contains("near_black_frames")
            && GFXSTREAM_HOST_RECORDER_WINDOWS_SOURCE.contains("max_consecutive_near_black_frames")
            && GFXSTREAM_HOST_RECORDER_WINDOWS_SOURCE.contains("max_source_frame_gap_millis")
            && WINDOWS_REAL_GUEST_SOURCE.contains("pointer_bridge_exercised")
            && WINDOWS_REAL_GUEST_SOURCE.contains("pointerEncodedMatch")
            && WINDOWS_REAL_GUEST_SOURCE.contains("pointerDroppedMatch")
            && WINDOWS_REAL_GUEST_SOURCE.contains("pointerMp4.sample_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("-lt 12")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("pointer_input_path = if ($RunUiDisplayInput)")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("physical_input_prepared = [bool]$recordingPhysicalInputPrepared")
            && WINDOWS_REAL_GUEST_SOURCE.contains(
                "gfxstream dropped, mismatched, blackened or stalled frames during the recorded Android interaction"
            ),
        "Windows real-guest evidence must exercise physical pointer input and distinguish source ColorBuffer black frames from later Win32/DWM presentation flashes"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE.contains("class DesktopFrameProbe")
            && WINDOWS_REAL_GUEST_SOURCE.contains("StretchBlt")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("final-desktop-composition-not-source-colorbuffer")
            && WINDOWS_REAL_GUEST_SOURCE.contains("dwm_composition_probe")
            && WINDOWS_REAL_GUEST_SOURCE.contains("titlebar_dwm_composition_probe")
            && WINDOWS_REAL_GUEST_SOURCE.contains("rotation_geometry")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$uiRotationSampleDeadline")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$uiRotationProbe.Samples -lt 20")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("webview-titlebar-overlay-with-stable-android-composition")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$titlebarSwapchainRecreates.Count -ne 0")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$dwmProbe = $null")
            && WINDOWS_REAL_GUEST_SOURCE.contains("if ($null -ne $dwmProbe)")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$titlebarDwmProbe = $null")
            && WINDOWS_REAL_GUEST_SOURCE.contains("if ($null -ne $titlebarDwmProbe)")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("RestoreAfterPhysicalInput($titlebarWebView)")
            && WINDOWS_REAL_GUEST_SOURCE.contains(
                "WebView titlebar interaction flashed DWM, changed guest focus, or recreated the Android swapchain"
            )
            && WINDOWS_REAL_GUEST_SOURCE.contains("max_black_pixel_ratio_ppm")
            && WINDOWS_REAL_GUEST_SOURCE.contains(
                "Windows final DWM composition produced a black, frozen, or incomplete frame sequence during pointer interaction"
            ),
        "Windows real-guest evidence must independently sample the final DWM composition after the source-frame and cadence gates and release physical-input state on every probe exit"
    );
    ensure!(
        CROSVM_WINDOWS_BROKER_SOURCE.contains("inherit_broker_priority")
            && CROSVM_WINDOWS_BROKER_SOURCE.contains("GetPriorityClass(GetCurrentProcess())")
            && CROSVM_WINDOWS_BROKER_SOURCE.contains("SetPriorityClass(process, priority)"),
        "Windows crosvm subprocesses must inherit the latency-sensitive broker priority class"
    );
    ensure!(
        BACKEND_SOURCE.contains("windows_realtime_vcpu_arguments")
            && BACKEND_SOURCE.contains("\"--rt-cpus\"")
            && BACKEND_SOURCE.contains("Windows MMCSS")
            && BACKEND_SOURCE.contains("system-responsiveness reserve"),
        "Windows Android vCPU threads must use crosvm's bounded MMCSS realtime path without promoting the whole VM to the realtime process class"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE
            .contains("legacy frame producer is present in the native direct-display runtime tree")
            && WINDOWS_REAL_GUEST_SOURCE.contains("legacy_frame_producer_present = $false")
            && !WINDOWS_REAL_GUEST_SOURCE
                .contains("frame producer is missing from the live runtime process tree"),
        "Windows native direct-display stability must reject the legacy frame producer instead of requiring it"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE
            .contains("stability run has no successful native presents before quiescence")
            && WINDOWS_REAL_GUEST_SOURCE.contains("presented frame counter regressed")
            && WINDOWS_REAL_GUEST_SOURCE.contains("static_frame_quiescence_allowed = $true")
            && !WINDOWS_REAL_GUEST_SOURCE
                .contains("presented frames did not advance across the stability interval"),
        "Windows stability must allow a static Android compositor to quiesce while keeping the native present counter monotonic"
    );
    ensure!(
        WEB_SOURCE.contains("state.controls_visible")
            && WEB_SOURCE.contains("layout.sidebar_visible")
            && WEB_SOURCE.contains("state.power_enabled")
            && WEB_SOURCE.contains("state.actions_enabled")
            && WEB_SOURCE.contains("state.recording_enabled")
            && WEB_SOURCE.contains("state.show_host_fps")
            && WEB_SOURCE.contains("state.host_fps_milli")
            && SHELL_SOURCE.contains("self.page == Page::Player")
            && SHELL_SOURCE.contains("titlebar_player_state("),
        "Windows WebView titlebar must consume the shared Player state"
    );
    ensure!(
        SHELL_SOURCE.contains("paths.cache.join(\"webview2\")")
            && SHELL_SOURCE.contains("WebContext::new(Some(webview_data_directory.to_owned()))")
            && SHELL_SOURCE.contains("WebViewBuilder::new_with_web_context(context)")
            && SHELL_SOURCE.contains("_context: Option<WebContext>")
            && WINDOWS_REAL_GUEST_SOURCE.contains("legacy_executable_webview_profile_absent")
            && WINDOWS_REAL_GUEST_SOURCE.contains("\"msedgewebview2.exe\"")
            && WINDOWS_REAL_GUEST_SOURCE.contains("bounded natural")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("Windows Player did not isolate WebView2 state under the HD data root",),
        "Windows WebView2 state must be shared by the titlebar surfaces but isolated under each HD data root"
    );
    ensure!(
        WEB_SOURCE.contains("停止录屏")
            && WEB_SOURCE.contains("关闭侧边栏")
            && WEB_SOURCE.contains("最大化或还原")
            && MACOS_BRIDGE_SOURCE.contains("停止录屏并保存")
            && MACOS_BRIDGE_SOURCE.contains("sidebarButton.toolTip = sidebarVisible")
            && MACOS_BRIDGE_SOURCE.contains("recordingButton.toolTip = recordingTooltip"),
        "Windows and macOS titlebar tooltips must expose the same state-dependent intent"
    );
    ensure!(
        WEB_SOURCE.contains("command: 'choose_install_apk'")
            && !WEB_SOURCE.contains("titlebarApkPath"),
        "install APK must always open the platform file chooser"
    );
    ensure!(
        MACOS_BRIDGE_SOURCE.contains("if (index == 8)")
            && MACOS_BRIDGE_SOURCE.contains("hd_titlebar_separator()")
            && WEB_SOURCE.contains("<span className=\"tool-divider\" />")
            && WEB_SOURCE.contains("className=\"window-controls\""),
        "Windows and macOS must separate device actions from Android navigation controls"
    );
    ensure!(
        SHELL_SOURCE.contains("let expanded = !window.is_maximized();")
            && SHELL_SOURCE.contains("shell.window_expanded = expanded;")
            && SHELL_SOURCE.contains("if window.is_maximized() {")
            && SHELL_SOURCE.contains("aspect_fit_rect("),
        "Windows expansion must share the macOS aspect-fit state"
    );
    ensure!(
        SHELL_SOURCE.contains("resubmitting identical Win32/gfxstream bounds")
            && SHELL_SOURCE.contains("if layout_changed {")
            && SHELL_SOURCE.contains("self.native_display.set_bounds(bounds)")
            && SHELL_SOURCE.contains("`hide` invalidates the last committed visible bounds")
            && SHELL_SOURCE.contains("self.last_native_bounds = None;"),
        "Windows sidebar overlays must not recommit unchanged Android bounds, while a hidden display must commit once when shown again"
    );
    let minimize_display = SHELL_SOURCE
        .split("fn suspend_display_for_minimize")
        .nth(1)
        .and_then(|source| {
            source
                .split("fn suspend_display_for_native_lifecycle")
                .next()
        })
        .unwrap_or_default();
    let navigate = SHELL_SOURCE
        .split("fn navigate")
        .nth(1)
        .and_then(|source| source.split("fn sync_window_aspect").next())
        .unwrap_or_default();
    let poll = SHELL_SOURCE
        .split("fn poll")
        .nth(1)
        .and_then(|source| source.split("fn network_status_poll_mode").next())
        .unwrap_or_default();
    let windows_input_child_lookup = NATIVE_DISPLAY_HOST_SOURCE
        .split("unsafe fn windows_crosvm_input_child")
        .nth(1)
        .and_then(|source| {
            source
                .split("fn commit_windows_crosvm_viewport_if_needed")
                .next()
        })
        .unwrap_or_default();
    let windows_pointer_window_proc = NATIVE_DISPLAY_HOST_SOURCE
        .split("unsafe extern \"system\" fn windows_native_display_window_proc")
        .nth(1)
        .and_then(|source| source.split("impl WindowsNativeDisplayHost").next())
        .unwrap_or_default();
    ensure!(
        SHELL_SOURCE.contains("fn set_viewport_locally_hidden")
            && SHELL_SOURCE.contains("returning to Player reveals the retained frame")
            && SHELL_SOURCE.contains("fn suspend_display_for_minimize")
            && SHELL_SOURCE.contains("self.set_viewport_locally_hidden();")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("let visibility_only_restore =")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("if !visibility_only_restore")
            && NATIVE_DISPLAY_HOST_SOURCE
                .contains("Reveal without SetWindowPos or a forced viewport")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("self.last_bounds = Some(bounds);")
            && minimize_display.contains("self.set_viewport_locally_hidden();")
            && !minimize_display.contains("self.release_display()")
            && navigate.contains("A page change only changes HD's local composition")
            && !navigate.contains("self.release_display()")
            && !navigate.contains("self.last_session_viewport = None")
            && !poll.contains("self.last_session_viewport = None")
            && SHELL_SOURCE.contains("Do not turn that retained state into a new")
            && SHELL_SOURCE.contains("&& self.last_native_bounds.is_some()")
            && WINDOWS_PLATFORM_SOURCE.contains("fn window_has_visible_style")
            && WINDOWS_PLATFORM_SOURCE.contains("IsWindowVisible folds ancestor visibility")
            && WINDOWS_PLATFORM_SOURCE.contains("i32::from(window_has_visible_style(input))")
            && WINDOWS_PLATFORM_SOURCE.contains("i32::from(window_has_visible_style(render))")
            && !WINDOWS_PLATFORM_SOURCE.contains("IsWindowVisible(input) == expected_visible")
            && !WINDOWS_PLATFORM_SOURCE.contains("IsWindowVisible(render) == expected_visible")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("HD_CROSVM_INPUT_CHILD_V1")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("HD_CROSVM_INPUT_CHILD_COOKIE_V1")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("const fn wide_ascii_z")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("Pointer messages run at common 125 Hz")
            && !windows_input_child_lookup.contains("wide(")
            && !windows_pointer_window_proc.contains("wide(")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("GetParent(cached) } == parent")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("GetPropW(cached, cookie.as_ptr()) } == parent")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("windows_child_has_visible_style(cached)")
            && NATIVE_DISPLAY_HOST_SOURCE
                .contains("SetPropW(parent, property.as_ptr(), child.cast())")
            && !NATIVE_DISPLAY_HOST_SOURCE.contains("String::from_utf16_lossy(&class_name")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("HD_POINTER_FORWARD_FAILURES_V1")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("HD_POINTER_FORWARD_MAX_MICROS_V1")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("PostMessageW(input, message, wparam, lparam)")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("SendMessageTimeoutW(")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("if message == WM_MOUSEMOVE")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("if !forwarded")
            && WINDOWS_REAL_GUEST_SOURCE.contains("HD_CROSVM_INPUT_CHILD_V1")
            && WINDOWS_REAL_GUEST_SOURCE.contains("HD_POINTER_FORWARD_FAILURES_V1")
            && WINDOWS_REAL_GUEST_SOURCE.contains("HD_POINTER_FORWARD_MAX_MICROS_V1")
            && WINDOWS_REAL_GUEST_SOURCE.contains("expectedTitlebarHeight")
            && WINDOWS_REAL_GUEST_SOURCE.contains("root-window DPI")
            && WINDOWS_REAL_GUEST_SOURCE.contains("divergent HD UI entry points")
            && WINDOWS_REAL_GUEST_SOURCE.contains("selectedHdUiHash")
            && WINDOWS_REAL_GUEST_SOURCE.contains("windows-runtime-closure.json")
            && WINDOWS_REAL_GUEST_SOURCE.contains("runtimeClosureManifestSha256")
            && WINDOWS_REAL_GUEST_SOURCE.contains("runtime_closure = [ordered]@{")
            && WINDOWS_REAL_GUEST_SOURCE.contains("non-empty Android smoke evidence root")
            && WINDOWS_REAL_GUEST_SOURCE.contains("existingOutput.Count -ne 0")
            && WINDOWS_REAL_GUEST_SOURCE.contains("guest_settle_seconds = $GuestSettleSeconds")
            && WINDOWS_REAL_GUEST_SOURCE.contains("aggregate_guest_image_size_bytes")
            && WINDOWS_REAL_GUEST_SOURCE.contains("readiness_timeline = $readinessTimeline")
            && WINDOWS_REAL_GUEST_SOURCE.contains("stage = \"timed_out\"")
            && WINDOWS_REAL_GUEST_SOURCE.contains("stage = \"complete\"")
            && WINDOWS_REAL_GUEST_SOURCE.contains("deferred-adb-readiness-v1.json")
            && WINDOWS_REAL_GUEST_SOURCE.contains("worker_stage")
            && WINDOWS_REAL_GUEST_SOURCE.contains("DragClientPoint(")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("physical Win32 pointer input during DWM composition diagnosis")
            && WINDOWS_REAL_GUEST_SOURCE.contains("physical_input_prepared")
            && WINDOWS_REAL_GUEST_SOURCE.contains("Win32-SendInput-capture-to-crosvm-EventDevice")
            && WINDOWS_REAL_GUEST_SOURCE.contains("native_pointer_cached_input_window")
            && WINDOWS_REAL_GUEST_SOURCE.contains("native_pointer_forward_failure_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("reopened_native_pointer_cached_input_window")
            && WINDOWS_REAL_GUEST_SOURCE.contains("reopened_native_pointer_forward_failure_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("reopened_native_pointer_forward_max_micros")
            && WINDOWS_REAL_GUEST_SOURCE.contains("did not cache the reattached crosvm input HWND"),
        "temporary page/minimize hiding must retain the display session and reveal the retained native frame without a forced swapchain transaction"
    );
    ensure!(
        WORKER_SOURCE.contains("deferred-adb-readiness-v1.json")
            && WORKER_SOURCE.contains("device_and_network_policy")
            && WORKER_SOURCE.contains("interactive_policy")
            && WORKER_SOURCE.contains("worker.adb.deferred_readiness.marker_failed"),
        "deferred Windows ADB readiness must retain an exact per-run diagnostic stage without changing the readiness gate"
    );
    ensure!(
        MACOS_BRIDGE_SOURCE.contains("state[@\"controls_visible\"]")
            && MACOS_BRIDGE_SOURCE.contains("state[@\"sidebar_visible\"]")
            && MACOS_BRIDGE_SOURCE.contains("button.hidden = !controlsVisible")
            && !MACOS_BRIDGE_SOURCE.contains("HDTitlebarDisplayPickerKey")
            && !MACOS_BRIDGE_SOURCE.contains("displayPickerPressed:"),
        "macOS must consume the same Player visibility state"
    );
    ensure!(
        WINDOWS_PLATFORM_SOURCE.contains("SetParent(render, parking)")
            && WINDOWS_PLATFORM_SOURCE.contains("SetParent(child, parking)")
            && WINDOWS_PLATFORM_SOURCE.contains("SetParent(render, parent)")
            && WINDOWS_PLATFORM_SOURCE.contains("SetParent(input, parent)")
            && WINDOWS_PLATFORM_SOURCE.contains("HD crosvm display parking")
            && WINDOWS_PLATFORM_SOURCE.contains("snapshot_native_window(render)")
            && WINDOWS_PLATFORM_SOURCE.contains("snapshot_native_input_window(input)")
            && WINDOWS_PLATFORM_SOURCE.contains("input_visual_region_empty: Option<bool>")
            && WINDOWS_PLATFORM_SOURCE.contains("rollback native input full visual region failed")
            && WINDOWS_PLATFORM_SOURCE.contains("restore_native_display_siblings(snapshots)")
            && WINDOWS_PLATFORM_SOURCE.contains("Restore the exact pre-attach hierarchy")
            && WINDOWS_PLATFORM_SOURCE.contains("native display attach failed")
            && WINDOWS_PLATFORM_SOURCE.contains("SetParent is not atomic across sibling HWNDs")
            && WINDOWS_PLATFORM_SOURCE.contains("sibling rollback also failed"),
        "Windows must park and reattach both crosvm and gfxstream sibling HWNDs"
    );
    ensure!(
        !NATIVE_DISPLAY_HOST_SOURCE.contains("windows_pointer_hook")
            && !NATIVE_DISPLAY_HOST_SOURCE.contains("SetWindowsHookExW")
            && !NATIVE_DISPLAY_HOST_SOURCE.contains("WH_MOUSE_LL")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("windows_native_display_window_proc")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("windows_crosvm_input_child")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("PostMessageW(input, message, wparam, lparam)")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("SendMessageTimeoutW(")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("SMTO_ABORTIFHUNG | SMTO_BLOCK")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("focus_windows_native_guest(hwnd)")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("GetGUIThreadInfo(target_thread")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("target_gui.hwndFocus == target")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("return Ok(());")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("HD_POINTER_FOCUS_FAILURES_V1")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("HD_POINTER_FOCUS_MAX_MICROS_V1")
            && WINDOWS_NATIVE_SUBWINDOW_SOURCE.contains("getCrosvmInputSibling")
            && WINDOWS_NATIVE_SUBWINDOW_SOURCE.contains("PostMessage(input, uMsg, wParam, lParam)")
            && WINDOWS_NATIVE_SUBWINDOW_SOURCE.contains("EnableWindow(ret, TRUE)")
            && WINDOWS_NATIVE_SUBWINDOW_SOURCE.contains("user_data->external_presentation")
            && WINDOWS_NATIVE_SUBWINDOW_SOURCE.contains("BeginPaint(hwnd, &paint)")
            && WINDOWS_NATIVE_SUBWINDOW_SOURCE
                .contains("GetParent(p_sub_window) == p_parent_window ? TRUE : FALSE")
            && WINDOWS_NATIVE_SUBWINDOW_SOURCE.contains("if (renderParent == p_window)")
            && WINDOWS_NATIVE_SUBWINDOW_SOURCE.contains("wc.style |= CS_HREDRAW | CS_VREDRAW")
            && !WINDOWS_NATIVE_SUBWINDOW_SOURCE.contains("SendMessage(")
            && !WINDOWS_PLATFORM_SOURCE.contains("bind_gfxstream_input_window(render, input)"),
        "Windows Player must asynchronously queue coalescible motion, keep lossless pointer messages bounded, avoid hooks and keep HD WM_PAINT out of the Vulkan post path"
    );
    ensure!(
        NATIVE_DISPLAY_HOST_SOURCE.contains("AttachThreadInput")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("GetWindowThreadProcessId(target")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("GetGUIThreadInfo(target_thread")
            && NATIVE_DISPLAY_HOST_SOURCE
                .contains("AttachThreadInput(current_thread, target_thread, 0)")
            && SHELL_SOURCE.contains("ui.display.focus.failed"),
        "Windows titlebar actions must restore the cross-process crosvm input focus through a bounded attached input queue and detach it immediately"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE.contains("native_pointer_focus_failure_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("reopened_native_pointer_focus_failure_count")
            && WINDOWS_REAL_GUEST_SOURCE.contains("native_pointer_focus_max_micros")
            && WINDOWS_REAL_GUEST_SOURCE.contains("native_pointer_focus_budget_micros = 20000")
            && WINDOWS_REAL_GUEST_SOURCE.contains("HD_POINTER_FOCUS_FAILURES_V1"),
        "Windows real-guest input regression must prove native pointer focus restoration before and after Player reattachment"
    );
    ensure!(
        BACKEND_SOURCE.contains("display.oriented_size()")
            && ADB_SOURCE.contains("prepare_windows_display_geometry")
            && ADB_SOURCE.contains("physical_wm_size(&initial_size)")
            && ADB_SOURCE.contains("windows_relative_rotation(display.orientation, physical)")
            && ADB_SOURCE.contains("set-ignore-orientation-request")
            && !ADB_SOURCE.contains("set_display_natural_size"),
        "Windows Android rotation must be relative to crosvm's physical mode without a portrait wm-size override"
    );
    ensure!(
        WINDOWS_PLATFORM_SOURCE.contains("HD_LATEST_ROTATION_V1")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("HD_APPLIED_ROTATION_V1")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("NATIVE_DISPLAY_ROTATION_PROPERTY")
            && CROSVM_WINDOWS_MESSAGE_SOURCE.contains("LATEST_ROTATION_PROPERTY")
            && CROSVM_WINDOWS_MESSAGE_SOURCE.contains("APPLIED_ROTATION_PROPERTY")
            && CROSVM_WINDOWS_SURFACE_SOURCE.contains("update_host_guest_transforms_with_rotation")
            && CROSVM_WINDOWS_VIRTUAL_DISPLAY_SOURCE.contains("relative_rotation")
            && CROSVM_WINDOWS_VIRTUAL_DISPLAY_SOURCE
                .contains("host logical (x, y) -> physical (1 - y, x)")
            && CROSVM_WINDOWS_VIRTUAL_DISPLAY_SOURCE
                .contains("host logical (x, y) -> physical (y, 1 - x)")
            && WINDOWS_REAL_GUEST_SOURCE.contains("rotation-pointer-")
            && WINDOWS_REAL_GUEST_SOURCE.contains("HD_FORCED_VIEWPORT_V1")
            && WINDOWS_REAL_GUEST_SOURCE.contains("HD_FORCED_ROTATION_V1")
            && WINDOWS_REAL_GUEST_SOURCE.contains("uiRotationSwapchainRecreates.Count -gt 1")
            && WINDOWS_REAL_GUEST_SOURCE.contains("Win32-SendInput-to-crosvm-to-Android-getevent")
            && WINDOWS_REAL_GUEST_SOURCE.contains("HD Android Display 0")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$primaryTouchDevicePath")
            && WINDOWS_REAL_GUEST_SOURCE.contains("primary_touch_device")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("primary touchscreen physical range did not match")
            && WINDOWS_REAL_GUEST_SOURCE.contains("coordinate_state_primed")
            && WINDOWS_REAL_GUEST_SOURCE.contains("artifact_sha256")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$uiDisplayInputFinalEvidence.GetEnumerator()")
            && WINDOWS_REAL_GUEST_SOURCE.contains("Linux evdev suppresses unchanged ABS axes")
            && WINDOWS_REAL_GUEST_SOURCE.contains("guest_physical_point")
            && WINDOWS_REAL_GUEST_SOURCE.contains("quadrant_matches")
            && WINDOWS_REAL_GUEST_SOURCE.contains("@(\"landscape\", \"reverse-landscape\")")
            && WINDOWS_REAL_GUEST_SOURCE.contains("aspect_cross_error")
            && WINDOWS_REAL_GUEST_SOURCE.contains("maximum_aspect_cross_error")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("-not $uiRotationPointerEvidence.quadrant_matches"),
        "Windows viewport updates must carry Android-aware pointer rotation and prove every runtime quadrant through Guest getevent"
    );
    ensure!(
        !WINDOWS_PLATFORM_SOURCE.contains("BeginDeferWindowPos(2)")
            && WINDOWS_PLATFORM_SOURCE.contains("SendMessageTimeoutW(")
            && WINDOWS_PLATFORM_SOURCE
                .contains("Crosvm's window thread owns the geometry transaction")
            && !WINDOWS_PLATFORM_SOURCE.contains("RDW_UPDATENOW")
            && !WINDOWS_PLATFORM_SOURCE.contains("WM_PAINT"),
        "Windows native Player resizing must have one crosvm-owned transaction without GDI repaint"
    );
    ensure!(
        BACKEND_SOURCE.contains("environment.insert(\"HD_DISPLAY_SELECTION\"")
            && CROSVM_WINDOWS_DISPLAY_SOURCE.contains("gfxstream_backend_configure_display")
            && CROSVM_WINDOWS_DISPLAY_SOURCE.contains("gfxstream_backend_set_scanout_resource")
            && CROSVM_WINDOWS_DISPLAY_SOURCE
                .contains("fn set_scanout_resource(&mut self, scanout_id: u32, resource_id: u32)"),
        "Windows gfxstream must present only the selected crosvm scanout with explicit resource mapping"
    );
    ensure!(
        SHELL_SOURCE.contains("fn reconcile_selected_display_target(&mut self)")
            && SHELL_SOURCE.contains("selected_displays\n            .entry(instance_id)")
            && SHELL_SOURCE.contains("reset_display_attachment_for_scanout(0)")
            && SHELL_SOURCE.contains("self.display_target.scanout_id() != scanout_id")
            && !SHELL_SOURCE.contains(
                "self.selected_displays\n            .insert(instance_id, DisplayIdV2::Primary);\n        let owner"
            ),
        "multi-display selection must stay scoped to its instance and restore the matching scanout after refresh"
    );
    ensure!(
        CROSVM_WINDOWS_DISPLAY_SOURCE.contains("gfxstream_backend_set_scanout_resource")
            && CROSVM_VIRTIO_GPU_SOURCE
                .contains("let was_enabled = scanout.resource_id.is_some();")
            && CROSVM_VIRTIO_GPU_SOURCE
                .contains("if was_enabled && scanout_type == SurfaceType::Scanout"),
        "Windows crosvm must bridge authoritative scanout resources without clearing an initial disabled scanout"
    );
    ensure!(
        WINDOWS_PLATFORM_SOURCE.contains("WM_USER_SELECT_DISPLAY_INTERNAL: u32 = WM_USER + 3")
            && WINDOWS_PLATFORM_SOURCE.contains("SendMessageTimeoutW(")
            && WINDOWS_PLATFORM_SOURCE.contains("select_crosvm_display(input)?")
            && WINDOWS_PLATFORM_SOURCE.contains("message_result != 1 && message_result != 2")
            && CROSVM_WINDOWS_MESSAGE_SOURCE.contains("WM_USER_SELECT_DISPLAY_INTERNAL")
            && CROSVM_WINDOWS_MESSAGE_SOURCE.contains("SelectDisplay")
            && CROSVM_WINDOWS_SURFACE_SOURCE.contains("gfxstream_backend_select_display(")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("FrameBuffer::selectDisplayForHd")
            && GFXSTREAM_FRAMEBUFFER_SOURCE
                .contains("hdSetScanoutResource(p2->displayId, p2->targetHandle);")
            && GFXSTREAM_FRAMEBUFFER_SOURCE
                .contains("hdDisplaySelectionEnabled() && p2->targetHandle != 0")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("p2->displayId != 0 && p2->targetHandle != 0")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("if (composeDeviceV2->targetHandle == 0)")
            && GFXSTREAM_FRAMEBUFFER_SOURCE
                .contains("Treat it\n            // as a protocol-level no-op")
            && GFXSTREAM_POST_WORKER_SOURCE.contains(
                "if (guestLayer.cbHandle == 0) {\n                // Cursor and DEVICE transitions"
            )
            && GFXSTREAM_POST_WORKER_SOURCE.contains(
                "Keep gfxstream's normal per-layer behavior: rejecting the whole request leaves"
            )
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("hdLatestScanoutResource(scanoutId)")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("hdScanoutForDisplayBuffer(")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("colorBuffer = m_lastPostedColorBuffer;")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("postImplSync(colorBuffer, true, true)")
            && GFXSTREAM_FRAMEBUFFER_SOURCE.contains("return posted ? 2 : 0;")
            && GFXSTREAM_RENDERER_SOURCE
                .contains("return fb != nullptr ? fb->selectDisplayForHd(scanout_id) : 0;")
            && GFXSTREAM_DISPLAY_SELECTION_SOURCE.contains("sLatestResources")
            && GFXSTREAM_DISPLAY_SELECTION_SOURCE.contains("hdConfigureScanoutSize")
            && GFXSTREAM_DISPLAY_SELECTION_SOURCE.contains("configuredSizeMatches")
            && GFXSTREAM_DISPLAY_SELECTION_SOURCE.contains("actual - configured <= 64")
            && GFXSTREAM_DISPLAY_SELECTION_SOURCE.contains("hdScanoutForDisplayBuffer")
            && GFXSTREAM_RENDERER_SOURCE.contains("hdSetScanoutResource(scanoutId, res_handle);")
            && GFXSTREAM_DISPLAY_SELECTION_SOURCE.contains("return scanoutId;")
            && GFXSTREAM_DISPLAY_SELECTION_SOURCE
                .contains("SET_SCANOUT(0) disables the guest's active scanout association")
            && GFXSTREAM_DISPLAY_SELECTION_SOURCE
                .contains("sLatestResources[scanoutId].store(resourceId"),
        "selecting a Windows scanout must synchronously reach gfxstream and repost its retained static frame"
    );
    ensure!(
        CROSVM_WINDOWS_SURFACE_SOURCE.contains("gfxstream_backend_setup_window_for_display(")
            && CROSVM_WINDOWS_SURFACE_SOURCE.contains("window.scanout_id(),")
            && GFXSTREAM_RENDERER_SOURCE.contains("scanout_id != gfxstream::hdSelectedScanout()")
            && !CROSVM_WINDOWS_SURFACE_SOURCE
                .contains("#[cfg(feature = \"gfxstream\")]\n        return;"),
        "embedded viewport changes must resize both crosvm VulkanDisplay and gfxstream presentation"
    );
    ensure!(
        GFXSTREAM_FRAMEBUFFER_SOURCE.contains(
            "if (m_lastPostedColorBuffer) {\n                GL_LOG(\"setupSubwindow: draw last posted cb\");"
        ) && GFXSTREAM_FRAMEBUFFER_SOURCE.contains(
            "otherwise a\n            // static Android screen remains black"
        ) && GFXSTREAM_FRAMEBUFFER_SOURCE.contains(
            "postImpl(m_lastPostedColorBuffer,"
        ) && GFXSTREAM_FRAMEBUFFER_SOURCE.contains(
            "Waiting synchronously here creates a lock"
        ),
        "Vulkan viewport resizing must asynchronously repost the selected scanout without a lock cycle"
    );
    ensure!(
        GFXSTREAM_DISPLAY_VK_HEADER_SOURCE.contains("m_retiredSwapChainStateVk")
            && GFXSTREAM_DISPLAY_VK_SOURCE.contains("swapChainCi->mCreateInfo.oldSwapchain")
            && GFXSTREAM_DISPLAY_VK_SOURCE.contains("auto previousSwapchain")
            && GFXSTREAM_DISPLAY_VK_SOURCE.contains("auto replacementSwapchain")
            && GFXSTREAM_DISPLAY_VK_SOURCE
                .contains("m_retiredSwapChainStateVk = std::move(previousSwapchain)"),
        "Win32 Vulkan resize must create and prime the replacement before retiring the displayed swapchain"
    );
    ensure!(
        WINDOWS_PLATFORM_SOURCE.contains("pub(crate) fn ensure_native_display(")
            && WINDOWS_PLATFORM_SOURCE.contains("GetClientRect(render")
            && WINDOWS_PLATFORM_SOURCE.contains("make_native_input_visual_region_empty(input)?")
            && WINDOWS_PLATFORM_SOURCE.contains("SetWindowRgn(input, region, 0)")
            && WINDOWS_PLATFORM_SOURCE.contains("native_input_visual_region_is_empty(input)")
            && WINDOWS_PLATFORM_SOURCE.contains("if in_sync")
            && HOST_SOURCE.contains("request.viewport.revision < active.viewport.revision")
            && HOST_SOURCE.contains("WorkerCommandV2::ResizeDisplay")
            && WORKER_SOURCE.contains("viewport.revision == session.viewport.revision")
            && WORKER_SOURCE.contains("hd_platform::ensure_native_display"),
        "display heartbeats must repair gfxstream child-size or input-only-region drift without resizing a healthy Vulkan surface"
    );
    ensure!(
        SHELL_SOURCE.contains("layout.height.saturating_sub(layout.top_height)")
            && SHELL_SOURCE.contains("display_origin_y.saturating_add(fitted.y)")
            && SHELL_SOURCE.contains("current_height - titlebar_height")
            && SHELL_SOURCE.contains("aspect_fit_rect(available_width, available_height")
            && SHELL_SOURCE.contains("target_content_height_px")
            && SHELL_SOURCE.contains("current.width != target_width_px")
            && SHELL_SOURCE.contains("if !aspect_changed")
            && SHELL_SOURCE.contains("preferred_window_content_area")
            && SHELL_SOURCE.contains("programmatic_window_inner_size")
            && SHELL_SOURCE.contains("aspect_size_from_preferred_area(")
            && SHELL_SOURCE.contains("apply_window_layout(window, webviews, shell, true)")
            && WINDOWS_TITLEBAR_SOURCE.contains("WM_SIZING")
            && WINDOWS_TITLEBAR_SOURCE.contains("constrain_sizing_rect")
            && WINDOWS_TITLEBAR_SOURCE.contains("titlebar_height")
            && WINDOWS_TITLEBAR_SOURCE.contains("GetPropW")
            && !WINDOWS_TITLEBAR_SOURCE.contains("GetWindowSubclass("),
        "Windows must preserve Android aspect and user scale across native resize and rotation"
    );
    ensure!(
        SHELL_SOURCE.contains("platform_window_aspect: Option<(u32, u32, u32, u8, u32)>")
            && SHELL_SOURCE.contains("Some((guest_width, guest_height, rotation_quarters))")
            && SHELL_SOURCE.contains("maximized. Keep `window_aspect` unchanged")
            && SHELL_SOURCE.contains("physical_dimension(96.0 * window.scale_factor(), 960)")
            && WINDOWS_TITLEBAR_SOURCE.contains("message == WM_DPICHANGED")
            && WINDOWS_TITLEBAR_SOURCE.contains("aspect.titlebar_height")
            && WINDOWS_TITLEBAR_SOURCE.contains("aspect.dpi = new_dpi")
            && SHELL_SOURCE
                .find("Some((guest_width, guest_height, rotation_quarters))")
                .zip(SHELL_SOURCE.find("if window.is_maximized()"))
                .is_some_and(|(sync, expanded)| sync < expanded),
        "maximized rotation must update the native DPI-aware aspect controller before returning while preserving the restore-window sizing aspect"
    );
    ensure!(
        WINDOWS_REAL_GUEST_SOURCE.contains("maximized_rotation_restore")
            && WINDOWS_REAL_GUEST_SOURCE.contains("$clickWebViewWindowMaximize")
            && WINDOWS_REAL_GUEST_SOURCE.contains("Win32-SendInput-to-WebView-window-control")
            && WINDOWS_REAL_GUEST_SOURCE.contains("maximize_control = $maximizeClickEvidence")
            && WINDOWS_REAL_GUEST_SOURCE.contains("restore_control = $restoreClickEvidence")
            && !WINDOWS_REAL_GUEST_SOURCE.contains("ShowWindow($rootWindow, 3)")
            && WINDOWS_REAL_GUEST_SOURCE.contains("maximizedSwapchainRecreates.Count -gt 4")
            && WINDOWS_REAL_GUEST_SOURCE.contains("SendCurrentSize($rootWindow, 2)")
            && WINDOWS_REAL_GUEST_SOURCE.contains("maximized_after_native_wm_size")
            && WINDOWS_REAL_GUEST_SOURCE.contains("restored Windows Player did not adopt")
            && WINDOWS_REAL_GUEST_SOURCE.contains("no_letterbox")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("$maximizedRotationProbe.DistinctFrames -lt 4")
            && WINDOWS_REAL_GUEST_SOURCE
                .contains("$maximizedRotationProbe.FrameTransitions -lt 4")
            && WINDOWS_REAL_GUEST_SOURCE.contains(
                "maximized rotate/restore produced an incomplete, black, or frozen DWM frame sequence"
            ),
        "Windows real-guest regression must physically click the WebView maximize control, rotate, replay native WM_SIZE, physically restore without letterboxing, and reject black or frozen DWM frames"
    );
    ensure!(
        WINDOWS_TITLEBAR_SOURCE.contains("message == WM_SIZE")
            && WINDOWS_TITLEBAR_SOURCE.contains("synchronize_native_children_during_root_resize")
            && WINDOWS_TITLEBAR_SOURCE.contains("WM_ENTERSIZEMOVE")
            && WINDOWS_TITLEBAR_SOURCE.contains("WM_EXITSIZEMOVE")
            && WINDOWS_TITLEBAR_SOURCE.contains("ROOT_RESIZE_SETTLE_TIMER_ID")
            && WINDOWS_TITLEBAR_SOURCE.contains("ROOT_RESIZE_SETTLE_MS")
            && WINDOWS_TITLEBAR_SOURCE.contains("message == WM_TIMER")
            && WINDOWS_TITLEBAR_SOURCE.contains("force_commit_current_root_resize")
            && WINDOWS_TITLEBAR_SOURCE.contains("commit_live_display_host_viewport")
            && WINDOWS_TITLEBAR_SOURCE.contains("commit_current_root_resize")
            && WINDOWS_TITLEBAR_SOURCE.contains("commit_settled_root_resize")
            && WINDOWS_TITLEBAR_SOURCE.contains("ROOT_INTERACTIVE_RESIZE_PROPERTY")
            && WINDOWS_TITLEBAR_SOURCE.contains("DISPLAY_HOST_CLASS")
            && WINDOWS_TITLEBAR_SOURCE.contains("GFXSTREAM_SUBWINDOW_CLASS_UTF16")
            && !WINDOWS_TITLEBAR_SOURCE.contains("BeginDeferWindowPos(count)")
            && WINDOWS_TITLEBAR_SOURCE.contains("WM_USER_HOST_VIEWPORT_CHANGE_INTERNAL")
            && WINDOWS_TITLEBAR_SOURCE.contains("FORCE_VIEWPORT_COMMIT_WPARAM")
            && WINDOWS_TITLEBAR_SOURCE.contains("FORCE_VIEWPORT_PENDING_PROPERTY")
            && WINDOWS_TITLEBAR_SOURCE.contains("parked dispatcher or timeout")
            && WINDOWS_TITLEBAR_SOURCE.contains("LATEST_VIEWPORT_PROPERTY")
            && WINDOWS_TITLEBAR_SOURCE.contains("LATEST_ROTATION_PROPERTY")
            && WINDOWS_TITLEBAR_SOURCE.contains("aspect.rotation_quarters")
            && WINDOWS_TITLEBAR_SOURCE.contains("encoded_rotation")
            && !WINDOWS_TITLEBAR_SOURCE.contains("SWP_ASYNCWINDOWPOS")
            && WINDOWS_TITLEBAR_SOURCE.contains("Vulkan swapchain completely stationary")
            && WINDOWS_TITLEBAR_SOURCE.contains("MoveWindow(")
            && WINDOWS_TITLEBAR_SOURCE
                .contains("commit_current_root_resize(hwnd, reference_data, false)")
            && !WINDOWS_TITLEBAR_SOURCE
                .contains("PostMessageW(input, WM_USER_HOST_VIEWPORT_CHANGE_INTERNAL")
            && WINDOWS_PLATFORM_SOURCE.contains("SetPropW(child, property.as_ptr()")
            && WINDOWS_PLATFORM_SOURCE.contains("force_notify_crosvm_viewport")
            && WINDOWS_PLATFORM_SOURCE.contains("FORCED_VIEWPORT_PROPERTY")
            && WINDOWS_PLATFORM_SOURCE.contains("if message_result == 1")
            && WINDOWS_PLATFORM_SOURCE.contains("ordinary UI rotation")
            && WINDOWS_PLATFORM_SOURCE.contains("FORCE_VIEWPORT_PENDING_PROPERTY")
            && WINDOWS_PLATFORM_SOURCE.contains("forced_projection_verified")
            && WINDOWS_PLATFORM_SOURCE.contains("GFXSTREAM_APPLIED_VIEWPORT_PROPERTY")
            && WINDOWS_PLATFORM_SOURCE.contains("gfxstream_projection_verified")
            && WINDOWS_PLATFORM_SOURCE.contains("ROOT_INTERACTIVE_RESIZE_PROPERTY")
            && WINDOWS_PLATFORM_SOURCE
                .contains("!gfxstream_projection_verified || geometry_changed")
            && GFXSTREAM_DISPLAY_VK_SOURCE.contains("HD_GFXSTREAM_APPLIED_VIEWPORT_V1")
            && GFXSTREAM_DISPLAY_VK_SOURCE.contains("publishHdAppliedSwapchainViewport(surfaceVk")
            && CROSVM_WINDOWS_MESSAGE_SOURCE.contains("GetPropW(")
            && CROSVM_WINDOWS_MESSAGE_SOURCE.contains("LATEST_VIEWPORT_PROPERTY")
            && CROSVM_WINDOWS_MESSAGE_SOURCE.contains("latest as LPARAM")
            && CROSVM_WINDOWS_MESSAGE_SOURCE.contains("FORCE_VIEWPORT_COMMIT_WPARAM")
            && CROSVM_WINDOWS_MESSAGE_SOURCE.contains("FORCE_VIEWPORT_PENDING_PROPERTY")
            && CROSVM_WINDOWS_SURFACE_SOURCE.contains("if !force_commit")
            && CROSVM_WINDOWS_SURFACE_SOURCE.contains("RemovePropW")
            && WINDOWS_TITLEBAR_SOURCE.contains("SendMessageTimeoutW(")
            && WINDOWS_TITLEBAR_SOURCE.contains("SMTO_ABORTIFHUNG | SMTO_BLOCK")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("if interactive_resize")
            && NATIVE_DISPLAY_HOST_SOURCE
                .contains("root subclass owns the stable clipping viewport")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("last_forced_bounds")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("force_commit_windows_crosvm_viewport")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("commit_windows_crosvm_viewport_if_needed")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("projection_only_rotation")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("Reverse portrait/landscape")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("fn position_if_needed(")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("ScreenToClient(self.parent_hwnd")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("if geometry_matches")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("HD_FORCED_ROTATION_V1")
            && NATIVE_DISPLAY_HOST_SOURCE
                .contains("shared marker as this host's local acknowledgement")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("windows_root_is_interactive_resize")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("same_orientation")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("current_windows_root_content_bounds")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("width != expected_width")
            && NATIVE_DISPLAY_HOST_SOURCE
                .contains("WM_EXITSIZEMOVE performs the final acknowledged commit"),
        "interactive Windows resize must center/crop a stationary retained frame without cross-thread child moves, then perform one forced final transaction and a deduplicated settle confirmation"
    );
    let position_failure_branch = NATIVE_DISPLAY_HOST_SOURCE
        .split_once("if positioned == 0 {")
        .and_then(|(_, suffix)| suffix.split_once("        Ok(())"))
        .map(|(branch, _)| branch)
        .context("Windows NativeDisplayHost position failure branch is missing")?;
    ensure!(
        position_failure_branch.contains("position NativeDisplayHost child HWND failed")
            && position_failure_branch.contains("leave the last successfully presented viewport")
            && !position_failure_branch.contains("ShowWindow(self.hwnd, SW_HIDE)"),
        "a transient Windows viewport positioning failure must preserve the last presented Android frame rather than explicitly hiding it"
    );
    ensure!(
        SHELL_SOURCE.contains("fn effective_root_window_focus(")
            && SHELL_SOURCE.contains("windows_root_has_foreground_focus(handle.as_raw())")
            && SHELL_SOURCE.contains("let focused = effective_root_window_focus(window, focused);")
            && SHELL_SOURCE
                .contains("Treat only a real foreground-window change as product focus loss")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("fn windows_root_has_foreground_focus(")
            && NATIVE_DISPLAY_HOST_SOURCE.contains("GetForegroundWindow() } == root"),
        "focus transfers between the Windows root, WebView titlebar and native Android child must not masquerade as application focus loss or collapse the sidebar"
    );
    let resized_branch = SHELL_SOURCE
        .split_once("WindowEvent::Resized(_size) => {")
        .and_then(|(_, suffix)| suffix.split_once("WindowEvent::ScaleFactorChanged"))
        .map(|(branch, _)| branch)
        .context("root resize event branch is missing")?;
    ensure!(
        resized_branch.contains("let minimized = shell.native_display.root_is_minimized();")
            && resized_branch.contains("shell.root_was_minimized = true;")
            && resized_branch.contains("shell.root_was_minimized = false;")
            && !resized_branch.contains("size.width == 0")
            && !resized_branch.contains("size.height == 0"),
        "root resize visibility must follow the current platform minimize state rather than a queued historical 0x0 event"
    );
    let ipc_layout_signature = SHELL_SOURCE
        .split_once("fn ipc_layout_signature(&self)")
        .and_then(|(_, suffix)| suffix.split_once("fn selected_display_id(&self)"))
        .map(|(branch, _)| branch)
        .context("IPC layout signature is missing")?;
    ensure!(
        !ipc_layout_signature.contains("window_expanded: self.window_expanded")
            && ipc_layout_signature.contains("window_expanded` is presentation state")
            && ipc_layout_signature.contains("WM_SIZE/Resized event applies maximized geometry"),
        "a maximize titlebar command must wait for the authoritative root resize instead of submitting an old-extent layout from presentation-only state"
    );
    let display_update = HOST_SOURCE
        .split_once("pub async fn update_display_session(")
        .and_then(|(_, suffix)| suffix.split_once("pub async fn release_display_session("))
        .map(|(branch, _)| branch)
        .context("Host display update function is missing")?;
    let registry_commit = display_update
        .rfind("let mut sessions = self.display_sessions.lock().await;")
        .context("display update post-IPC registry commit is missing")?;
    let worker_await = display_update
        .find(".call_worker(")
        .context("display update Worker IPC is missing")?;
    ensure!(
        display_update.contains("let worker_session = {")
            && display_update.contains("a stalled instance must not serialize")
            && worker_await < registry_commit
            && display_update.contains("active.session.id != worker_session.id")
            && display_update.contains("active.session.generation != worker_session.generation"),
        "display heartbeat IPC must run outside the global session registry lock and revalidate identity before committing"
    );
    ensure!(
        HOST_SOURCE.contains("display_operations: Mutex<HashMap<Uuid, Arc<Mutex<()>>>>")
            && HOST_SOURCE
                .matches("let mut operations = self.display_operations.lock().await;")
                .count()
                >= 4
            && HOST_SOURCE
                .matches("let _display_guard = display_operation.lock().await;")
                .count()
                >= 4
            && HOST_SOURCE.contains("Different Android instances keep")
            && HOST_SOURCE.contains("two Players racing to replace one target")
            && HOST_SOURCE.contains("Expiry races with Player replacement")
            && HOST_SOURCE
                .contains("newly acquired session is never removed by a stale expiry candidate"),
        "display acquire, heartbeat update, release and expiry must serialize per instance without restoring the global registry lock bottleneck"
    );
    ensure!(
        WORKER_SOURCE.contains("display_operation: Mutex<()>")
            && WORKER_SOURCE.contains("an old target cannot reappear")
            && WORKER_SOURCE
                .contains("let display_operation = worker.display_operation.lock().await;")
            && WORKER_SOURCE.contains("drop(display_operation);")
            && WORKER_SOURCE
                .matches("let _display_operation = self.display_operation.lock().await;")
                .count()
                >= 4
            && WORKER_SOURCE.contains("Stop owns the display pipeline until the child is gone"),
        "Worker native attach, resize, detach, shutdown and deferred startup attach must share one display operation lock without holding it across retry delays"
    );
    ensure!(
        SHELL_SOURCE.contains("display_request_epoch: u64")
            && SHELL_SOURCE
                .matches("self.display_request_epoch = self.display_request_epoch.wrapping_add(1);")
                .count()
                >= 4
            && SHELL_SOURCE.contains("request_epoch != self.display_request_epoch")
            && SHELL_SOURCE.contains("late success must never reinstall that session")
            && SHELL_SOURCE.contains("release_returned_display_session(session)"),
        "late display acquire/update completions must be invalidated by release, target replacement and close instead of reinstalling stale Player sessions"
    );
    ensure!(
        SHELL_SOURCE.contains("std::process::Command::new(\"wt.exe\")")
            && SHELL_SOURCE.contains("std::process::Command::new(\"powershell.exe\")")
            && SHELL_SOURCE.contains("const BUNDLED_ADB_FILENAME: &str = \"adb.exe\"")
            && SHELL_SOURCE.contains("parent.join(BUNDLED_ADB_FILENAME)")
            && SHELL_SOURCE.contains("已打开 Microdroid Shell"),
        "Windows and macOS must both open an interactive Microdroid shell"
    );
    ensure!(
        WINDOWS_TITLEBAR_SOURCE.contains("SetWindowSubclass")
            && WINDOWS_TITLEBAR_SOURCE.contains("WM_POWERBROADCAST")
            && WINDOWS_TITLEBAR_SOURCE.contains("PBT_APMSUSPEND")
            && WINDOWS_TITLEBAR_SOURCE.contains("PBT_APMRESUMEAUTOMATIC")
            && WINDOWS_TITLEBAR_SOURCE.contains("NATIVE_LIFECYCLE_SUSPENDED_MESSAGE")
            && WINDOWS_TITLEBAR_SOURCE.contains("NATIVE_LIFECYCLE_RESUMED_MESSAGE")
            && SHELL_SOURCE.contains("suspend_display_for_native_lifecycle")
            && SHELL_SOURCE.contains("resume_display_after_native_lifecycle"),
        "Windows sleep and wake notifications must drive the shared native display lifecycle"
    );
    ensure!(
        SHELL_SOURCE.contains("DISPLAY_RELEASE_ON_CLOSE_TIMEOUT")
            && SHELL_SOURCE.contains("tokio::time::timeout(")
            && SHELL_SOURCE.contains("ui.lifecycle.display_release.timed_out")
            && SHELL_SOURCE.contains("ShutdownFinished(Ok(()))"),
        "closing the UI must remain bounded when display-session release stalls"
    );
    ensure!(
        XTASK_SOURCE.contains("runtime_dir: PathBuf")
            && XTASK_SOURCE.contains("adb: PathBuf")
            && XTASK_SOURCE.contains("aapt2: PathBuf")
            && XTASK_SOURCE.contains("\"crosvm.exe\"")
            && XTASK_SOURCE.contains("\"vm.exe\"")
            && XTASK_SOURCE.contains("\"virtmgr.exe\"")
            && XTASK_SOURCE.contains("\"libbinder-rpc.dll\"")
            && XTASK_SOURCE.contains("\"libgfxstream_backend.dll\"")
            && XTASK_SOURCE.contains("\"AdbWinApi.dll\"")
            && XTASK_SOURCE.contains("\"AdbWinUsbApi.dll\"")
            && XTASK_SOURCE.contains("verify_packaged_windows_tool")
            && XTASK_SOURCE.contains(".env_clear()"),
        "Windows packaging must carry the same explicit VM, graphics, ADB and APK-tool closure as macOS"
    );
    ensure!(
        CROSVM_WINDOWS_GPU_MODE_SOURCE
            .contains("Self::Windowed(width, height) => (*width, *height)")
            && CROSVM_WINDOWS_GPU_MODE_SOURCE
                .contains("explicit_windowed_virtual_display_size_matches_request")
            && CROSVM_WINDOWS_BROKER_SOURCE.contains("!cfg.virtio_snds.is_empty()")
            && CROSVM_WINDOWS_BROKER_SOURCE.contains(".virtio_snds")
            && CROSVM_WINDOWS_BROKER_SOURCE.contains(".get(card_index)")
            && CROSVM_WINDOWS_BROKER_SOURCE.contains("CREATE_NO_WINDOW")
            && CROSVM_WINDOWS_BROKER_SOURCE.contains("proc.creation_flags(CREATE_NO_WINDOW)")
            && BACKEND_SOURCE.contains("HostAudioInputV2::DefaultMicrophone")
            && BACKEND_SOURCE.contains("capture=true,backend=winaudio")
            && BACKEND_SOURCE.contains("capture=false,backend=winaudio")
            && BACKEND_SOURCE.contains("\"--virtio-snd\".to_owned()")
            && CROSVM_WINDOWS_VSOCK_SOURCE.contains("Failed to read from pipe")
            && CROSVM_WINDOWS_VSOCK_SOURCE.contains("connections.remove(&port)")
            && CROSVM_WINDOWS_VSOCK_SOURCE.contains("VIRTIO_VSOCK_OP_RST")
            && CROSVM_WINDOWS_VSOCK_SOURCE.contains("reset_header.as_bytes()")
            && CROSVM_WIN_AUDIO_BUILD_SOURCE.contains(".compile(\"r8brain\")")
            && CROSVM_WIN_AUDIO_BUILD_SOURCE.contains("R8BSRC_DECL")
            && !CROSVM_WIN_AUDIO_BUILD_SOURCE.contains("download_prebuilts")
            && !CROSVM_WIN_AUDIO_BUILD_SOURCE.contains("ucrtbased.dll")
            && !CROSVM_WIN_AUDIO_BUILD_SOURCE.contains("vcruntime140d.dll")
            && !RUNTIME_ARTIFACTS_SOURCE.contains("r8Brain.dll")
            && !XTASK_SOURCE.contains("\"r8Brain.dll\"")
            && XTASK_SOURCE.contains("\"UCRTBASED.DLL\"")
            && XTASK_SOURCE.contains("executables and libraries")
            && XTASK_SOURCE.contains("r8brain-free-src-LICENSE.txt")
            && XTASK_SOURCE.contains("PACKAGED_WINDOWS_HELP_PROBES")
            && XTASK_SOURCE.contains("verify_windows_package_policy")
            && XTASK_SOURCE.contains("!name.contains(\"swiftshader\")")
            && XTASK_SOURCE.contains("\"hd-host.exe\"")
            && XTASK_SOURCE.contains("\"hd-worker.exe\"")
            && XTASK_SOURCE.contains("\"virtmgr.exe\"")
            && ROOT_BUILD_SOURCE
                .contains("RUSTUP_TOOLCHAIN=1.94.0-x86_64-pc-windows-gnu")
            && ROOT_BUILD_SOURCE
                .contains("host: x86_64-pc-windows-gnu")
            && ROOT_BUILD_SOURCE.contains("for %%F in (libslirp-0.dll) do")
            && ROOT_BUILD_SOURCE.contains("r8brain-free-src-LICENSE.txt")
            && ROOT_BUILD_SOURCE.contains(
                "for %%F in (r8Brain.dll ucrtbased.dll vcruntime140d.dll msvcp140d.dll concrt140d.dll)"
            )
            && !ROOT_BUILD_SOURCE.contains(
                "for %%F in (libslirp-0.dll r8Brain.dll ucrtbased.dll vcruntime140d.dll)"
            ),
        "Windows must preserve configured guest resolution/audio and terminate failed host-vsock pipes with a Guest-visible reset"
    );
    verify_default_single_instance_contract()?;
    Ok(controls.len())
}

fn verify_native_display_z_order() -> Result<()> {
    let display_host_position = SHELL_SOURCE
        .find("create_native_display_host(raw_parent)")
        .ok_or_else(|| anyhow::anyhow!("Windows native display host creation is missing"))?;
    let titlebar_position = SHELL_SOURCE
        .find("install_platform_window_controls(window.as_ref(), &proxy)")
        .ok_or_else(|| anyhow::anyhow!("platform window controller installation is missing"))?;
    ensure!(
        display_host_position < titlebar_position,
        "the platform window controller must be installed after the native Android host"
    );
    Ok(())
}

fn verify_default_single_instance_contract() -> Result<()> {
    let pre_artifact_source = SHELL_SOURCE
        .split("let bundled_signed")
        .next()
        .ok_or_else(|| anyhow::anyhow!("desktop shell has no artifact initialization boundary"))?;
    ensure!(
        pre_artifact_source.contains("cli.data_root.is_none()")
            && pre_artifact_source.contains("activate_existing_desktop_application")
            && WINDOWS_TITLEBAR_SOURCE.contains("HD_DEFAULT_UI_PROFILE_V1")
            && WINDOWS_TITLEBAR_SOURCE.contains("HD_DEFAULT_UI_OWNER_V1_")
            && WINDOWS_TITLEBAR_SOURCE.contains("CreateMutexW")
            && WINDOWS_TITLEBAR_SOURCE.contains("OpenMutexW")
            && WINDOWS_TITLEBAR_SOURCE.contains("GetWindowThreadProcessId")
            && WINDOWS_TITLEBAR_SOURCE.contains("SetForegroundWindow")
            && WINDOWS_TITLEBAR_SOURCE.contains("FlashWindow")
            && WINDOWS_TITLEBAR_SOURCE.contains("for _ in 0..320")
            && MACOS_BRIDGE_SOURCE
                .contains("runningApplicationsWithBundleIdentifier:bundleIdentifier")
            && MACOS_BRIDGE_SOURCE.contains("NSApplicationActivateAllWindows"),
        "default-profile launches must activate the existing native HD window before artifact or WebView initialization while explicit data-root profiles remain isolated"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_native_macos_contracts() -> Result<()> {
    let controls = hd_platform::macos_titlebar_control_contracts()?;
    ensure!(
        controls.len() == TITLEBAR_ACTION_CONTRACTS.len(),
        "macOS native titlebar action count drifted from the portable contract"
    );
    for (control, (action_id, message)) in controls.iter().zip(TITLEBAR_ACTION_CONTRACTS) {
        ensure!(
            control.message == message,
            "macOS native titlebar action {action_id} is not bound to its portable command"
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn verify_native_macos_contracts() {}
