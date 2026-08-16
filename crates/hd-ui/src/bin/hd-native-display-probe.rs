#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("the native display probe is available only on Windows")
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows::run()
}

#[cfg(windows)]
mod windows {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, Result};
    use clap::Parser;
    use hd_core::{DisplayViewportV2, OperationKindV2, StopModeV2};
    use hd_platform::{
        DataPaths, NativeDisplayBounds, create_native_display_host, current_process_identity,
    };
    use hd_runtime::HostClientV2;
    use raw_window_handle::HasWindowHandle as _;
    use uuid::Uuid;
    use winit::dpi::LogicalSize;
    use winit::event::{Event, WindowEvent};
    use winit::event_loop::{ControlFlow, EventLoop};
    use winit::window::Window;

    const HEARTBEAT: Duration = Duration::from_secs(4);

    #[derive(Debug, Parser)]
    #[command(
        name = "hd-native-display-probe",
        about = "Minimal native Android child-window probe"
    )]
    struct Cli {
        #[arg(long)]
        instance_id: Uuid,
        #[arg(long)]
        data_root: Option<PathBuf>,
    }

    #[derive(Debug)]
    enum ProbeEvent {
        Started(Result<(), String>),
    }

    #[allow(deprecated)]
    #[allow(clippy::too_many_lines)]
    pub fn run() -> Result<()> {
        let cli = Cli::parse();
        let paths = cli
            .data_root
            .map_or_else(DataPaths::discover, |root| Ok(DataPaths::from_root(root)))
            .context("discover HD data directory")?;
        paths.ensure().context("create HD data directories")?;
        let runtime = tokio::runtime::Runtime::new().context("create probe runtime")?;
        let client = runtime
            .block_on(HostClientV2::connect_or_start(paths))
            .context("connect to HD Host")?;
        let event_loop = EventLoop::<ProbeEvent>::with_user_event().build()?;
        let proxy = event_loop.create_proxy();
        let window = event_loop.create_window(
            Window::default_attributes()
                .with_title("HD native display probe")
                .with_inner_size(LogicalSize::new(540.0, 960.0))
                .with_min_inner_size(LogicalSize::new(270.0, 480.0)),
        )?;
        let raw_parent = window.window_handle()?.as_raw();
        let mut native_display = create_native_display_host(raw_parent)?;
        let physical = window.inner_size();
        native_display.set_bounds(NativeDisplayBounds {
            x_px: 0,
            y_px: 0,
            width_px: physical.width,
            height_px: physical.height,
            rotation_quarters: 0,
            visible: true,
        })?;
        let mut viewport = display_viewport(physical, window.scale_factor(), 1);
        let target = native_display.target(current_process_identity(Uuid::new_v4())?, 0);
        let mut session = runtime.block_on(client.acquire_display_session(
            cli.instance_id,
            hd_core::DisplayIdV2::Primary,
            target,
            viewport.clone(),
        ))?;
        let start_client = client.clone();
        let instance_id = cli.instance_id;
        runtime.spawn(async move {
            let result = async {
                let operation = start_client
                    .create_operation(
                        instance_id,
                        OperationKindV2::Start,
                        &format!("native-probe-start-{}", Uuid::new_v4()),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                start_client
                    .wait_operation(operation.id, Duration::from_mins(3))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            .await;
            let _ = proxy.send_event(ProbeEvent::Started(result));
        });
        let mut revision = 1_u64;
        let mut last_heartbeat = Instant::now();
        event_loop.run(move |event, active| {
            active.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(100),
            ));
            match event {
                Event::AboutToWait if last_heartbeat.elapsed() >= HEARTBEAT => {
                    if let Ok(updated) = runtime.block_on(client.update_display_session(
                        instance_id,
                        session.session_token.clone(),
                        viewport.clone(),
                    )) {
                        session = updated;
                    }
                    last_heartbeat = Instant::now();
                }
                Event::WindowEvent {
                    event: WindowEvent::Resized(size),
                    ..
                } if size.width > 0 && size.height > 0 => {
                    let _ = native_display.set_bounds(NativeDisplayBounds {
                        x_px: 0,
                        y_px: 0,
                        width_px: size.width,
                        height_px: size.height,
                        rotation_quarters: 0,
                        visible: true,
                    });
                    revision = revision.saturating_add(1);
                    viewport = display_viewport(size, window.scale_factor(), revision);
                    if let Ok(updated) = runtime.block_on(client.update_display_session(
                        instance_id,
                        session.session_token.clone(),
                        viewport.clone(),
                    )) {
                        session = updated;
                    }
                }
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    let _ = runtime.block_on(
                        client.release_display_session(instance_id, session.session_token.clone()),
                    );
                    let _ = runtime.block_on(client.create_operation(
                        instance_id,
                        OperationKindV2::Stop {
                            mode: StopModeV2::Graceful,
                            graceful_timeout_ms: 20_000,
                        },
                        &format!("native-probe-stop-{}", Uuid::new_v4()),
                    ));
                    active.exit();
                }
                Event::UserEvent(ProbeEvent::Started(Ok(()))) => {
                    window.set_title("HD native display probe — Android ready");
                }
                Event::UserEvent(ProbeEvent::Started(Err(error))) => {
                    window.set_title(&format!("HD native display probe — failed: {error}"));
                }
                _ => {}
            }
        })?;
        Ok(())
    }

    fn display_viewport(
        physical: winit::dpi::PhysicalSize<u32>,
        scale: f64,
        revision: u64,
    ) -> DisplayViewportV2 {
        DisplayViewportV2 {
            width_px: physical.width.max(1),
            height_px: physical.height.max(1),
            dpi: physical_dpi(scale),
            visible: true,
            revision,
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn physical_dpi(scale: f64) -> u32 {
        (96.0 * scale).round().clamp(72.0, 960.0) as u32
    }
}
