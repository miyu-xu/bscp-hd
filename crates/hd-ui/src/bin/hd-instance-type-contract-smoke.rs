use anyhow::{Result, ensure};
use hd_runtime::microdroid_platform_supported;
use serde_json::json;

const WEB_SOURCE: &str = include_str!("../../../../web/src/main.tsx");
const SHELL_SOURCE: &str = include_str!("../web_shell.rs");
const WINDOWS_MICRODROID_SMOKE_SOURCE: &str =
    include_str!("../../../../scripts/windows-microdroid-real-guest.ps1");

fn main() -> Result<()> {
    let expected_microdroid_support = cfg!(all(target_os = "macos", target_arch = "aarch64"))
        || cfg!(all(target_os = "windows", target_arch = "x86_64"));
    ensure!(
        microdroid_platform_supported() == expected_microdroid_support,
        "Microdroid platform capability drifted from the supported desktop matrix"
    );
    ensure!(
        WEB_SOURCE.contains(
            "snapshot.microdroid_supported && <option value=\"microdroid\">Microdroid</option>",
        ) && !WEB_SOURCE.contains("isMacOS && <option value=\"microdroid\">Microdroid</option>",),
        "the instance-type selector must consume the Host capability instead of the user agent"
    );
    ensure!(
        SHELL_SOURCE.contains("\"microdroid_supported\": microdroid_platform_supported()")
            && SHELL_SOURCE.contains(
                "guest_kind == GuestKindV2::Microdroid && !microdroid_platform_supported()",
            ),
        "the create gate and UI snapshot must share one Microdroid platform capability"
    );
    ensure!(
        WINDOWS_MICRODROID_SMOKE_SOURCE.contains("HD_MICRODROID_ARTIFACTS_ROOT")
            && WINDOWS_MICRODROID_SMOKE_SOURCE.contains("--no-start-host")
            && WINDOWS_MICRODROID_SMOKE_SOURCE.contains("payload verification successful")
            && WINDOWS_MICRODROID_SMOKE_SOURCE.contains("boot completed, time to run payload")
            && WINDOWS_MICRODROID_SMOKE_SOURCE.contains("Notified host payload ready successfully")
            && WINDOWS_MICRODROID_SMOKE_SOURCE.contains("remaining_process_count"),
        "Windows Microdroid support must retain a repeatable real-Guest lifecycle gate"
    );

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "ok",
            "platform": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "microdroid_supported": microdroid_platform_supported(),
            "selector_source": "host_capability",
            "windows_real_guest_gate": true,
        }))?
    );
    Ok(())
}
