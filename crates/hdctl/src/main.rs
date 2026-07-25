use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand, ValueEnum};
use hd_core::{
    BatteryStateV2, BluetoothPeerActionV2, CreateInstanceRequestV2, DiagnosticRequestV2,
    InstanceActionV2, InstanceSpecV2, KeyActionV2, LocationV2, NetworkConditionV2, NfcTagActionV2,
    OperationKindV2, OrientationV2, SensorInjectionV2, StopModeV2, UpdateInstanceRequestV2,
    VsyncModeV2,
};
use hd_platform::DataPaths;
use hd_runtime::HostClientV2;
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "hdctl", version, about = "Control the local HD Android host")]
struct Cli {
    #[arg(long, global = true)]
    data_root: Option<PathBuf>,
    #[arg(long, global = true)]
    no_start_host: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Health,
    Capabilities {
        instance_id: Option<Uuid>,
    },
    List,
    Show {
        id: Uuid,
    },
    Create {
        #[arg(long)]
        spec: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
    },
    Update {
        id: Uuid,
        spec: PathBuf,
        #[arg(long)]
        expected_revision: Option<u64>,
    },
    Start {
        id: Uuid,
        #[arg(long)]
        no_wait: bool,
    },
    Stop {
        id: Uuid,
        #[arg(long)]
        force: bool,
        #[arg(long, default_value_t = 20_000)]
        graceful_timeout_ms: u32,
        #[arg(long)]
        no_wait: bool,
    },
    Restart {
        id: Uuid,
        #[arg(long)]
        no_wait: bool,
    },
    Pause {
        id: Uuid,
        #[arg(long)]
        no_wait: bool,
    },
    Resume {
        id: Uuid,
        #[arg(long)]
        no_wait: bool,
    },
    Delete {
        id: Uuid,
        #[arg(long)]
        no_wait: bool,
    },
    Display {
        id: Uuid,
        #[arg(long)]
        width: u32,
        #[arg(long)]
        height: u32,
        #[arg(long)]
        dpi: u32,
        #[arg(long, value_parser = parse_refresh_rate)]
        refresh_rate: u16,
        #[arg(long, value_enum)]
        orientation: OrientationArg,
        #[arg(long, value_enum)]
        vsync: VsyncArg,
        #[arg(long)]
        show_fps: bool,
        #[arg(long)]
        no_wait: bool,
    },
    Action {
        id: Uuid,
        #[command(subcommand)]
        action: ActionCommand,
    },
    Install {
        id: Uuid,
        apk: PathBuf,
        #[arg(long)]
        no_wait: bool,
    },
    Diagnostics {
        #[arg(long)]
        instance_id: Option<Uuid>,
        #[arg(long)]
        include_guest_logs: bool,
    },
    Operations,
    Operation {
        id: Uuid,
        #[arg(long)]
        wait: bool,
    },
    Shutdown {
        #[arg(long)]
        stop_all: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ActionCommand {
    Key {
        key: KeyArg,
    },
    Rotate {
        orientation: OrientationArg,
    },
    Location {
        latitude_e7: i32,
        longitude_e7: i32,
        #[arg(long, default_value_t = 0)]
        altitude_mm: i32,
        #[arg(long, default_value_t = 5_000)]
        accuracy_mm: u32,
    },
    Battery {
        level_percent: u8,
        #[arg(long)]
        charging: bool,
        #[arg(long, default_value_t = 250)]
        temperature_deci_celsius: i16,
    },
    Network {
        latency_ms: u32,
        loss_basis_points: u16,
        #[arg(long)]
        bandwidth_kbps: Option<u32>,
    },
    Sensor {
        sensor: String,
        #[arg(required = true, num_args = 1..)]
        values_microunits: Vec<i64>,
        #[arg(long, default_value_t = 0)]
        duration_ms: u32,
    },
    BluetoothCreate {
        peer_id: Uuid,
        name: String,
    },
    BluetoothRemove {
        peer_id: Uuid,
    },
    BluetoothAdvertise {
        peer_id: Uuid,
        #[arg(action = clap::ArgAction::Set)]
        enabled: bool,
    },
    NfcType2 {
        ndef_hex: String,
    },
    NfcType4 {
        ndef_hex: String,
    },
    NfcRemove,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum KeyArg {
    Home,
    Recent,
    Back,
    Power,
    VolumeUp,
    VolumeDown,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OrientationArg {
    Portrait,
    Landscape,
    ReversePortrait,
    ReverseLandscape,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum VsyncArg {
    On,
    Off,
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = match cli.data_root {
        Some(root) => DataPaths::resolve(root).context("resolve HD data directory")?,
        None => DataPaths::discover().context("discover HD data directory")?,
    };
    paths.ensure().context("create HD data directories")?;
    let client = if cli.no_start_host {
        HostClientV2::connect(paths).await
    } else {
        HostClientV2::connect_or_start(paths).await
    }
    .context("connect to HD host")?;

    match cli.command {
        Command::Health => print_json(&client.health().await?)?,
        Command::Capabilities { instance_id } => {
            print_json(&client.capabilities(instance_id).await?)?;
        }
        Command::List => print_json(&client.list_instances().await?)?,
        Command::Show { id } => print_json(&client.get_instance(id).await?)?,
        Command::Create { spec, name } => {
            let mut spec =
                spec.map_or_else(|| Ok(InstanceSpecV2::default()), |path| load_spec(&path))?;
            if let Some(name) = name {
                spec.name = name;
            }
            spec.validate().context("validate instance specification")?;
            print_json(
                &client
                    .create_instance(&CreateInstanceRequestV2 { spec })
                    .await?,
            )?;
        }
        Command::Update {
            id,
            spec,
            expected_revision,
        } => {
            let spec = load_spec(&spec)?;
            if spec.id != id {
                anyhow::bail!("specification id does not match command id");
            }
            spec.validate().context("validate instance specification")?;
            let revision = match expected_revision {
                Some(revision) => revision,
                None => client.get_instance(id).await?.status.revision,
            };
            print_json(
                &client
                    .update_instance(
                        id,
                        &UpdateInstanceRequestV2 {
                            expected_revision: revision,
                            spec,
                        },
                    )
                    .await?,
            )?;
        }
        Command::Start { id, no_wait } => {
            run_operation(
                &client,
                id,
                OperationKindV2::Start,
                no_wait,
                Duration::from_mins(10),
            )
            .await?;
        }
        Command::Stop {
            id,
            force,
            graceful_timeout_ms,
            no_wait,
        } => {
            run_operation(
                &client,
                id,
                OperationKindV2::Stop {
                    mode: if force {
                        StopModeV2::Force
                    } else {
                        StopModeV2::Graceful
                    },
                    graceful_timeout_ms,
                },
                no_wait,
                Duration::from_secs(90),
            )
            .await?;
        }
        Command::Restart { id, no_wait } => {
            run_operation(
                &client,
                id,
                OperationKindV2::Restart,
                no_wait,
                Duration::from_mins(11),
            )
            .await?;
        }
        Command::Pause { id, no_wait } => {
            run_operation(
                &client,
                id,
                OperationKindV2::Pause,
                no_wait,
                Duration::from_secs(30),
            )
            .await?;
        }
        Command::Resume { id, no_wait } => {
            run_operation(
                &client,
                id,
                OperationKindV2::Resume,
                no_wait,
                Duration::from_secs(30),
            )
            .await?;
        }
        Command::Delete { id, no_wait } => {
            run_operation(
                &client,
                id,
                OperationKindV2::Delete,
                no_wait,
                Duration::from_mins(1),
            )
            .await?;
        }
        Command::Display {
            id,
            width,
            height,
            dpi,
            refresh_rate,
            orientation,
            vsync,
            show_fps,
            no_wait,
        } => {
            let record = client.get_instance(id).await?;
            let mut display = record.spec.display;
            display.width = width;
            display.height = height;
            display.dpi = dpi;
            display.refresh_rate_hz = refresh_rate;
            display.orientation = orientation.into();
            display.vsync = vsync.into();
            display.show_host_fps = show_fps;
            run_operation(
                &client,
                id,
                OperationKindV2::Reconfigure {
                    display,
                    adb: record.spec.adb,
                },
                no_wait,
                Duration::from_secs(45),
            )
            .await?;
        }
        Command::Action { id, action } => {
            print_json(&client.action(id, action.into()).await?)?;
        }
        Command::Install { id, apk, no_wait } => {
            let upload = client.upload_apk(&apk).await?;
            run_operation(
                &client,
                id,
                OperationKindV2::InstallApk {
                    upload_id: upload.id,
                    sha256: upload.sha256,
                },
                no_wait,
                Duration::from_mins(10),
            )
            .await?;
        }
        Command::Diagnostics {
            instance_id,
            include_guest_logs,
        } => print_json(
            &client
                .collect_diagnostics(&DiagnosticRequestV2 {
                    instance_id,
                    include_guest_logs,
                })
                .await?,
        )?,
        Command::Operations => print_json(&client.list_operations().await?)?,
        Command::Operation { id, wait } => {
            if wait {
                print_json(&client.wait_operation(id, Duration::from_mins(11)).await?)?;
            } else {
                print_json(&client.operation(id).await?)?;
            }
        }
        Command::Shutdown { stop_all } => client.shutdown(stop_all).await?,
    }
    Ok(())
}

async fn run_operation(
    client: &HostClientV2,
    id: Uuid,
    kind: OperationKindV2,
    no_wait: bool,
    timeout: Duration,
) -> Result<()> {
    let key = format!("hdctl-{}", Uuid::new_v4());
    let operation = client.create_operation(id, kind, &key).await?;
    if no_wait {
        print_json(&operation)?;
    } else {
        print_json(&client.wait_operation(operation.id, timeout).await?)?;
    }
    Ok(())
}

fn parse_refresh_rate(value: &str) -> Result<u16, String> {
    match value {
        "30" => Ok(30),
        "60" => Ok(60),
        "90" => Ok(90),
        "120" => Ok(120),
        _ => Err("refresh rate must be one of 30, 60, 90, or 120".to_owned()),
    }
}

fn load_spec(path: &Path) -> Result<InstanceSpecV2> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if bytes.len() > 1024 * 1024 {
        anyhow::bail!("instance specification exceeds 1 MiB");
    }
    serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

impl From<KeyArg> for KeyActionV2 {
    fn from(value: KeyArg) -> Self {
        match value {
            KeyArg::Home => Self::Home,
            KeyArg::Recent => Self::Recent,
            KeyArg::Back => Self::Back,
            KeyArg::Power => Self::Power,
            KeyArg::VolumeUp => Self::VolumeUp,
            KeyArg::VolumeDown => Self::VolumeDown,
        }
    }
}

impl From<OrientationArg> for OrientationV2 {
    fn from(value: OrientationArg) -> Self {
        match value {
            OrientationArg::Portrait => Self::Portrait,
            OrientationArg::Landscape => Self::Landscape,
            OrientationArg::ReversePortrait => Self::ReversePortrait,
            OrientationArg::ReverseLandscape => Self::ReverseLandscape,
        }
    }
}

impl From<VsyncArg> for VsyncModeV2 {
    fn from(value: VsyncArg) -> Self {
        match value {
            VsyncArg::On => Self::On,
            VsyncArg::Off => Self::Off,
        }
    }
}

impl From<ActionCommand> for InstanceActionV2 {
    fn from(value: ActionCommand) -> Self {
        match value {
            ActionCommand::Key { key } => Self::Key { key: key.into() },
            ActionCommand::Rotate { orientation } => Self::Rotate {
                orientation: orientation.into(),
            },
            ActionCommand::Location {
                latitude_e7,
                longitude_e7,
                altitude_mm,
                accuracy_mm,
            } => Self::SetLocation {
                location: LocationV2 {
                    latitude_e7,
                    longitude_e7,
                    altitude_mm,
                    accuracy_mm,
                },
            },
            ActionCommand::Battery {
                level_percent,
                charging,
                temperature_deci_celsius,
            } => Self::SetBattery {
                battery: BatteryStateV2 {
                    level_percent,
                    charging,
                    temperature_deci_celsius,
                },
            },
            ActionCommand::Network {
                latency_ms,
                loss_basis_points,
                bandwidth_kbps,
            } => Self::SetNetworkCondition {
                condition: NetworkConditionV2 {
                    latency_ms,
                    loss_basis_points,
                    bandwidth_kbps,
                },
            },
            ActionCommand::Sensor {
                sensor,
                values_microunits,
                duration_ms,
            } => Self::InjectSensor {
                injection: SensorInjectionV2 {
                    sensor,
                    values_microunits,
                    duration_ms,
                },
            },
            ActionCommand::BluetoothCreate { peer_id, name } => Self::BluetoothPeer {
                action: BluetoothPeerActionV2::CreateGattPeer { peer_id, name },
            },
            ActionCommand::BluetoothRemove { peer_id } => Self::BluetoothPeer {
                action: BluetoothPeerActionV2::RemovePeer { peer_id },
            },
            ActionCommand::BluetoothAdvertise { peer_id, enabled } => Self::BluetoothPeer {
                action: BluetoothPeerActionV2::SetAdvertising { peer_id, enabled },
            },
            ActionCommand::NfcType2 { ndef_hex } => Self::NfcTag {
                action: NfcTagActionV2::PresentType2 { ndef_hex },
            },
            ActionCommand::NfcType4 { ndef_hex } => Self::NfcTag {
                action: NfcTagActionV2::PresentType4 { ndef_hex },
            },
            ActionCommand::NfcRemove => Self::NfcTag {
                action: NfcTagActionV2::Remove,
            },
        }
    }
}
