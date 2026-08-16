pub(crate) fn env_flag_enabled(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| {
        !value.is_empty() && value != "0" && !value.to_string_lossy().eq_ignore_ascii_case("false")
    })
}

pub(crate) fn fast_artifacts_enabled() -> bool {
    env_flag_enabled("HD_DEV_FAST_ARTIFACTS")
}

pub(crate) fn fast_capabilities_enabled() -> bool {
    fast_artifacts_enabled() || env_flag_enabled("HD_DEV_FAST_CAPABILITIES")
}

pub(crate) fn allow_display_copy_fallback_enabled() -> bool {
    // A release Player must never silently leave the native host-GPU path. Diagnostic recording
    // has its own explicitly measured readback path; this escape hatch is only for debug builds.
    cfg!(debug_assertions) && env_flag_enabled("HD_DEV_ALLOW_DISPLAY_COPY_FALLBACK")
}

pub(crate) fn allow_adb_offline_boot_ready_enabled() -> bool {
    env_flag_enabled("HD_DEV_ALLOW_ADB_OFFLINE_BOOT_READY")
}

pub(crate) const fn native_display_direct_enabled() -> bool {
    // macOS presents gfxstream's CAMetalLayer through CAContext/CALayerHost; Windows presents the
    // same gfxstream-owned surface as a NativeDisplayHost child HWND. Both are native host-GPU
    // paths and publish presentation metrics from the actual GL/Vulkan present operation. The
    // separate Windows external-memory broker is not a Player consumer and must not be used as a
    // second, boot-buffer-bound display readiness path.
    cfg!(any(windows, target_os = "macos"))
}
