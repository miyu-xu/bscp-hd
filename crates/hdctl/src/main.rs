use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use hd_core::{
    ControlCommandV1, ControlRequestV1, ControlResponseV1, DisplayConfig, InstanceAction,
    InstanceConfigV1, Orientation, VsyncMode,
};
use hd_runtime::{control_endpoint, send_request};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "hdctl",
    version,
    about = "Control the local HD Android supervisor"
)]
struct Cli {
    /// Override the platform-local control endpoint.
    #[arg(long, global = true)]
    endpoint: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Ping,
    List,
    Show {
        id: Uuid,
    },
    Create {
        #[arg(long, default_value = "Android")]
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    Update {
        config: PathBuf,
    },
    Start {
        id: Uuid,
        /// Use the deterministic phase-0 backend without Android artifacts.
        #[arg(long)]
        mock: bool,
    },
    Stop {
        id: Uuid,
    },
    Delete {
        id: Uuid,
    },
    Action {
        id: Uuid,
        action: ActionArg,
    },
    Install {
        id: Uuid,
        apk: PathBuf,
    },
    Display {
        id: Uuid,
        #[arg(long)]
        width: u32,
        #[arg(long)]
        height: u32,
        #[arg(long, default_value_t = 320)]
        dpi: u32,
        #[arg(long, default_value_t = 60)]
        refresh_rate: u32,
        #[arg(long, value_enum, default_value_t = OrientationArg::Landscape)]
        orientation: OrientationArg,
        #[arg(long, value_enum, default_value_t = VsyncArg::On)]
        vsync: VsyncArg,
        #[arg(long)]
        show_fps: bool,
    },
    Diagnose {
        id: Uuid,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ActionArg {
    Home,
    Recent,
    Back,
    Power,
    VolumeUp,
    VolumeDown,
    Rotate,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OrientationArg {
    Landscape,
    Portrait,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum VsyncArg {
    On,
    Off,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let endpoint = cli.endpoint.map_or_else(control_endpoint, Ok)?;
    let command = into_protocol(cli.command)?;
    let response = send_request(&endpoint, &ControlRequestV1::new(command))
        .await
        .with_context(|| format!("connect to HD supervisor at {endpoint}"))?;
    print_response(&response)?;
    Ok(())
}

fn into_protocol(command: Command) -> Result<ControlCommandV1> {
    Ok(match command {
        Command::Ping => ControlCommandV1::Ping,
        Command::List => ControlCommandV1::List,
        Command::Show { id } => ControlCommandV1::Show { id },
        Command::Create { name, config } => {
            let mut config = if let Some(path) = config {
                InstanceConfigV1::load(&path)
                    .with_context(|| format!("load config {}", path.display()))?
            } else {
                InstanceConfigV1::default()
            };
            config.name = name;
            ControlCommandV1::Create { config }
        }
        Command::Update { config } => ControlCommandV1::Update {
            config: InstanceConfigV1::load(&config)
                .with_context(|| format!("load config {}", config.display()))?,
        },
        Command::Start { id, mock } => ControlCommandV1::Start { id, mock },
        Command::Stop { id } => ControlCommandV1::Stop { id },
        Command::Delete { id } => ControlCommandV1::Delete { id },
        Command::Action { id, action } => ControlCommandV1::Action {
            id,
            action: match action {
                ActionArg::Home => InstanceAction::Home,
                ActionArg::Recent => InstanceAction::Recent,
                ActionArg::Back => InstanceAction::Back,
                ActionArg::Power => InstanceAction::Power,
                ActionArg::VolumeUp => InstanceAction::VolumeUp,
                ActionArg::VolumeDown => InstanceAction::VolumeDown,
                ActionArg::Rotate => InstanceAction::Rotate,
            },
        },
        Command::Install { id, apk } => ControlCommandV1::InstallApk { id, path: apk },
        Command::Display {
            id,
            width,
            height,
            dpi,
            refresh_rate,
            orientation,
            vsync,
            show_fps,
        } => ControlCommandV1::ApplyDisplay {
            id,
            display: DisplayConfig {
                width,
                height,
                dpi,
                refresh_rate_hz: refresh_rate,
                orientation: match orientation {
                    OrientationArg::Landscape => Orientation::Landscape,
                    OrientationArg::Portrait => Orientation::Portrait,
                },
                vsync: match vsync {
                    VsyncArg::On => VsyncMode::On,
                    VsyncArg::Off => VsyncMode::Off,
                },
                show_host_fps: show_fps,
            },
        },
        Command::Diagnose { id } => ControlCommandV1::Diagnose { id },
        Command::Shutdown => ControlCommandV1::Shutdown,
    })
}

fn print_response(response: &ControlResponseV1) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(response).context("encode response")?
    );
    if !response.ok {
        let message = response
            .error
            .as_ref()
            .map_or("unknown supervisor error", |error| error.message.as_str());
        bail!(message.to_owned());
    }
    Ok(())
}
