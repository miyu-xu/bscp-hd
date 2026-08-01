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
    use std::path::PathBuf;

    use anyhow::{Context as _, Result, ensure};
    use hd_core::{InstanceSpecV2, OrientationV2};
    use hd_platform::{macos_titlebar_control_contracts, map_macos_pointer};
    use hd_ui::ui_contract::{Page, SurfaceLayout, oriented_guest_dimensions};
    use serde_json::json;
    use winit::dpi::PhysicalSize;

    const WEB_SOURCE: &str = include_str!("../../../../web/src/main.tsx");
    const SHELL_SOURCE: &str = include_str!("../web_shell.rs");

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
            controls.len() == 9,
            "native titlebar must expose exactly nine HD controls"
        );
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
            ("install_apk", "'choose_install_apk'"),
            ("rotate", "action('rotate')"),
            ("screenshot", "action('screenshot')"),
            ("diagnostics", "command: 'diagnostics'"),
            ("selected_instance_no_restart", "selectedId !== instanceId"),
            (
                "device_runtime_control",
                "features.includes('runtime_control')",
            ),
            ("start", "operation('start')"),
            ("pause_resume", "observed === 'paused' ? 'resume' : 'pause'"),
            ("restart", "operation('restart')"),
            ("stop", "operation('stop')"),
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
            "\"rotate\" =>",
            "\"screenshot\" =>",
            "\"diagnostics\" =>",
            "\"operation\" =>",
            "\"key\" =>",
            "\"window\" =>",
        ] {
            ensure!(
                SHELL_SOURCE.contains(handler),
                "missing shell handler {handler}"
            );
        }

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
            },
            "pointer_matrix": pointer_results,
            "native_titlebar": controls,
            "web_contract_count": required_web_contracts.len(),
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
