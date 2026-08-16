use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand, ValueEnum};
use hd_core::{
    BatteryStateV2, BluetoothAdvertisementFrameV2, BluetoothPeerActionV2, CreateInstanceRequestV2,
    DiagnosticRequestV2, DisplayIdV2, InstanceActionV2, InstanceSpecV2, KeyActionV2, LocationV2,
    ModemStateV2, NetworkConditionV2, NfcTagActionV2, OperationKindV2, OrientationV2,
    PackagedArtifactChannelV2, SensorInjectionV2, SensorPoseV2, StopModeV2,
    UpdateInstanceRequestV2, UwbRangingV2, VsyncModeV2,
};
use hd_platform::DataPaths;
use hd_runtime::{
    HostClientV2, prepare_direct_dev_artifact_store, verify_packaged_android_artifact_store,
};
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
    /// Check only CPU, memory and disk admission for one saved instance specification.
    ResourceAdmission {
        id: Uuid,
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
    /// Reset stopped Android userdata while retaining one owner-only recoverable backup.
    Powerwash {
        id: Uuid,
        #[arg(long)]
        expected_revision: Option<u64>,
        #[arg(long)]
        confirm_name: String,
        #[arg(long)]
        no_wait: bool,
    },
    /// Restore a powerwash backup; displaced current userdata becomes the new rollback backup.
    PowerwashRestore {
        id: Uuid,
        backup_id: Uuid,
        #[arg(long)]
        expected_revision: Option<u64>,
        #[arg(long)]
        confirm_name: String,
        #[arg(long)]
        no_wait: bool,
    },
    /// Permanently discard the retained powerwash backup.
    PowerwashDiscard {
        id: Uuid,
        backup_id: Uuid,
        #[arg(long)]
        expected_revision: Option<u64>,
        #[arg(long)]
        confirm_name: String,
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
    /// Start a bounded Android screen recording for one Ready instance.
    ScreenRecordStart {
        id: Uuid,
        /// `primary` or the stable UUID of a configured secondary display.
        #[arg(long, default_value = "primary", value_parser = parse_display_target)]
        display: DisplayIdV2,
        #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u16).range(1..=180))]
        max_duration_seconds: u16,
    },
    /// Stop and finalize the active Android screen recording as an MP4 artifact.
    ScreenRecordStop {
        id: Uuid,
    },
    /// Generate a bounded full AOSP bugreport for one Ready Android instance.
    Bugreport {
        id: Uuid,
    },
    Install {
        id: Uuid,
        apk: PathBuf,
        #[arg(long)]
        no_wait: bool,
    },
    Upload {
        apk: PathBuf,
        #[arg(long)]
        microdroid_payload: bool,
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
    VerifyAndroidArtifactStore {
        #[arg(long)]
        store_root: PathBuf,
        #[arg(long)]
        trust_store: PathBuf,
        #[arg(long, value_enum)]
        channel: ArtifactChannelArg,
    },
    /// Materialize deterministic manifest-only metadata for explicitly enabled direct images.
    #[command(hide = true)]
    PrepareDirectDevArtifacts,
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
    RouteStart {
        route: PathBuf,
        #[arg(long, default_value_t = 1_000, value_parser = clap::value_parser!(u32).range(250..=60_000))]
        interval_ms: u32,
        #[arg(long)]
        repeat: bool,
    },
    RoutePause,
    RouteResume,
    RouteStop,
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
    SensorPose {
        #[arg(value_parser = clap::value_parser!(i32).range(-180_000..=180_000))]
        x_millidegrees: i32,
        #[arg(value_parser = clap::value_parser!(i32).range(-180_000..=180_000))]
        y_millidegrees: i32,
        #[arg(value_parser = clap::value_parser!(i32).range(-180_000..=180_000))]
        z_millidegrees: i32,
        #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u32).range(200..=10_000))]
        transition_ms: u32,
    },
    BluetoothCreate {
        peer_id: Uuid,
        name: String,
    },
    BluetoothBeacon {
        peer_id: Uuid,
        name: String,
        advertising_data_hex: String,
    },
    BluetoothScriptedBeacon {
        peer_id: Uuid,
        name: String,
        #[arg(long)]
        frames_json: String,
        #[arg(long)]
        repeat: bool,
    },
    BluetoothHidKeyboard {
        peer_id: Uuid,
        name: String,
    },
    BluetoothHidKeyboardReport {
        peer_id: Uuid,
        #[arg(long, default_value_t = 0)]
        modifiers: u8,
        #[arg(long, value_delimiter = ',', num_args = 0..=6, value_parser = clap::value_parser!(u8).range(4..=231))]
        keys: Vec<u8>,
    },
    BluetoothHciCapture {
        #[arg(long, default_value_t = 5_000, value_parser = clap::value_parser!(u32).range(1_000..=30_000))]
        duration_ms: u32,
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
    UwbRanging {
        #[arg(value_parser = clap::value_parser!(u16).range(1..=65_535))]
        distance_cm: u16,
    },
    ModemState {
        operator_numeric: String,
        #[arg(value_parser = clap::value_parser!(u8).range(0..=31))]
        signal_strength: u8,
        #[arg(long, default_value = "HD Mobile")]
        operator_long_name: String,
        #[arg(long, default_value = "HD")]
        operator_short_name: String,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        registered: bool,
    },
    MicrodroidConsoleChallenge {
        #[arg(long)]
        challenge_id: Option<Uuid>,
        #[arg(long)]
        confirm: bool,
    },
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ArtifactChannelArg {
    Development,
    Release,
}

impl From<ArtifactChannelArg> for PackagedArtifactChannelV2 {
    fn from(value: ArtifactChannelArg) -> Self {
        match value {
            ArtifactChannelArg::Development => Self::Development,
            ArtifactChannelArg::Release => Self::Release,
        }
    }
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::VerifyAndroidArtifactStore {
        store_root,
        trust_store,
        channel,
    } = &cli.command
    {
        let verified =
            verify_packaged_android_artifact_store(store_root, trust_store, (*channel).into())
                .context("verify packaged Android artifact store")?;
        let rootfs = verified
            .bundles
            .guest_manifest
            .files
            .iter()
            .find(|file| file.role == "rootfs")
            .context("verified Android Guest bundle omitted rootfs")?;
        print_json(&serde_json::json!({
            "schema_version": verified.index.schema_version,
            "channel": verified.index.channel,
            "android_version": verified.index.android_version,
            "data_profile": verified.index.data_profile,
            "guest_bundle_digest": verified.index.guest_bundle_digest,
            "host_bundle_digest": verified.index.host_bundle_digest,
            "guest_file_count": verified.bundles.guest_manifest.files.len(),
            "host_file_count": verified.bundles.host_manifest.files.len(),
            "rootfs_relative_path": rootfs.relative_path,
            "rootfs_sha256": rootfs.sha256,
            "rootfs_size_bytes": rootfs.size_bytes,
            "exact_closure": true,
            "signature_verified": true
        }))?;
        return Ok(());
    }
    let paths = match cli.data_root {
        Some(root) => DataPaths::resolve(root).context("resolve HD data directory")?,
        None => DataPaths::discover().context("discover HD data directory")?,
    };
    paths.ensure().context("create HD data directories")?;
    if matches!(cli.command, Command::PrepareDirectDevArtifacts) {
        let selection = prepare_direct_dev_artifact_store(&paths)
            .context("prepare direct Android development artifacts")?;
        print_json(&selection)?;
        return Ok(());
    }
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
        Command::ResourceAdmission { id } => {
            print_json(&client.resource_admission(id).await?)?;
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
        Command::Powerwash {
            id,
            expected_revision,
            confirm_name,
            no_wait,
        } => {
            let revision = match expected_revision {
                Some(revision) => revision,
                None => client.get_instance(id).await?.status.revision,
            };
            run_operation(
                &client,
                id,
                OperationKindV2::Powerwash {
                    expected_revision: revision,
                    confirmation_name: confirm_name,
                },
                no_wait,
                Duration::from_mins(10),
            )
            .await?;
        }
        Command::PowerwashRestore {
            id,
            backup_id,
            expected_revision,
            confirm_name,
            no_wait,
        } => {
            let revision = match expected_revision {
                Some(revision) => revision,
                None => client.get_instance(id).await?.status.revision,
            };
            run_operation(
                &client,
                id,
                OperationKindV2::RestorePowerwash {
                    backup_id,
                    expected_revision: revision,
                    confirmation_name: confirm_name,
                },
                no_wait,
                Duration::from_mins(10),
            )
            .await?;
        }
        Command::PowerwashDiscard {
            id,
            backup_id,
            expected_revision,
            confirm_name,
            no_wait,
        } => {
            let revision = match expected_revision {
                Some(revision) => revision,
                None => client.get_instance(id).await?.status.revision,
            };
            run_operation(
                &client,
                id,
                OperationKindV2::DiscardPowerwashBackup {
                    backup_id,
                    expected_revision: revision,
                    confirmation_name: confirm_name,
                },
                no_wait,
                Duration::from_mins(10),
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
            print_json(&client.action(id, into_instance_action(action)?).await?)?;
        }
        Command::ScreenRecordStart {
            id,
            display,
            max_duration_seconds,
        } => print_json(
            &client
                .start_display_screen_recording(id, display, max_duration_seconds)
                .await?,
        )?,
        Command::ScreenRecordStop { id } => {
            print_json(&client.stop_screen_recording(id).await?)?;
        }
        Command::Bugreport { id } => {
            print_json(&client.collect_android_bugreport(id).await?)?;
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
        Command::Upload {
            apk,
            microdroid_payload,
        } => {
            if microdroid_payload {
                hd_runtime::validate_microdroid_payload_apk(&apk)
                    .await
                    .context("validate Microdroid Payload APK")?;
            }
            print_json(&client.upload_apk(&apk).await?)?;
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
        Command::Shutdown { stop_all } => client.shutdown_and_wait(stop_all).await?,
        Command::VerifyAndroidArtifactStore { .. } => {
            unreachable!("artifact store verification returns before Host connection")
        }
        Command::PrepareDirectDevArtifacts => {
            unreachable!("direct development preparation returns before Host connection")
        }
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

fn parse_display_target(value: &str) -> Result<DisplayIdV2, String> {
    if value.eq_ignore_ascii_case("primary") {
        return Ok(DisplayIdV2::Primary);
    }
    Uuid::parse_str(value)
        .map(|id| DisplayIdV2::Secondary { id })
        .map_err(|_| "display must be `primary` or a configured secondary display UUID".to_owned())
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

fn into_instance_action(value: ActionCommand) -> Result<InstanceActionV2> {
    Ok(match value {
        ActionCommand::Key { key } => InstanceActionV2::Key { key: key.into() },
        ActionCommand::Rotate { orientation } => InstanceActionV2::Rotate {
            orientation: orientation.into(),
        },
        ActionCommand::Location {
            latitude_e7,
            longitude_e7,
            altitude_mm,
            accuracy_mm,
        } => InstanceActionV2::SetLocation {
            location: LocationV2 {
                latitude_e7,
                longitude_e7,
                altitude_mm,
                accuracy_mm,
            },
        },
        ActionCommand::RouteStart {
            route,
            interval_ms,
            repeat,
        } => InstanceActionV2::StartLocationRoute {
            route: hd_runtime::load_location_route(&route, interval_ms, repeat)
                .with_context(|| format!("import location route {}", route.display()))?,
        },
        ActionCommand::RoutePause => InstanceActionV2::PauseLocationRoute,
        ActionCommand::RouteResume => InstanceActionV2::ResumeLocationRoute,
        ActionCommand::RouteStop => InstanceActionV2::StopLocationRoute,
        ActionCommand::Battery {
            level_percent,
            charging,
            temperature_deci_celsius,
        } => InstanceActionV2::SetBattery {
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
        } => InstanceActionV2::SetNetworkCondition {
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
        } => InstanceActionV2::InjectSensor {
            injection: SensorInjectionV2 {
                sensor,
                values_microunits,
                duration_ms,
            },
        },
        ActionCommand::SensorPose {
            x_millidegrees,
            y_millidegrees,
            z_millidegrees,
            transition_ms,
        } => sensor_pose_instance_action(
            x_millidegrees,
            y_millidegrees,
            z_millidegrees,
            transition_ms,
        ),
        value @ (ActionCommand::BluetoothCreate { .. }
        | ActionCommand::BluetoothBeacon { .. }
        | ActionCommand::BluetoothScriptedBeacon { .. }
        | ActionCommand::BluetoothHidKeyboard { .. }
        | ActionCommand::BluetoothHidKeyboardReport { .. }
        | ActionCommand::BluetoothHciCapture { .. }
        | ActionCommand::BluetoothRemove { .. }
        | ActionCommand::BluetoothAdvertise { .. }) => bluetooth_instance_action(value)?,
        ActionCommand::NfcType2 { ndef_hex } => InstanceActionV2::NfcTag {
            action: NfcTagActionV2::PresentType2 { ndef_hex },
        },
        ActionCommand::NfcType4 { ndef_hex } => InstanceActionV2::NfcTag {
            action: NfcTagActionV2::PresentType4 { ndef_hex },
        },
        ActionCommand::NfcRemove => InstanceActionV2::NfcTag {
            action: NfcTagActionV2::Remove,
        },
        ActionCommand::UwbRanging { distance_cm } => InstanceActionV2::SetUwbRanging {
            ranging: UwbRangingV2 { distance_cm },
        },
        value @ ActionCommand::ModemState { .. } => modem_instance_action(value),
        ActionCommand::MicrodroidConsoleChallenge {
            challenge_id,
            confirm,
        } => microdroid_instance_action(challenge_id, confirm),
    })
}

fn sensor_pose_instance_action(
    x_millidegrees: i32,
    y_millidegrees: i32,
    z_millidegrees: i32,
    transition_ms: u32,
) -> InstanceActionV2 {
    InstanceActionV2::SetSensorPose {
        pose: SensorPoseV2 {
            x_millidegrees,
            y_millidegrees,
            z_millidegrees,
            transition_ms,
        },
    }
}

fn modem_instance_action(value: ActionCommand) -> InstanceActionV2 {
    let ActionCommand::ModemState {
        operator_numeric,
        signal_strength,
        operator_long_name,
        operator_short_name,
        registered,
    } = value
    else {
        unreachable!("modem action helper received a non-modem command")
    };
    InstanceActionV2::SetModemState {
        modem: ModemStateV2 {
            operator_numeric,
            operator_long_name,
            operator_short_name,
            signal_strength,
            registered,
        },
    }
}

fn microdroid_instance_action(challenge_id: Option<Uuid>, confirm: bool) -> InstanceActionV2 {
    InstanceActionV2::MicrodroidConsoleChallenge {
        challenge_id: challenge_id.unwrap_or_else(Uuid::new_v4),
        confirmed: confirm,
    }
}

fn bluetooth_instance_action(value: ActionCommand) -> Result<InstanceActionV2> {
    let action = match value {
        ActionCommand::BluetoothCreate { peer_id, name } => {
            BluetoothPeerActionV2::CreateGattPeer { peer_id, name }
        }
        ActionCommand::BluetoothBeacon {
            peer_id,
            name,
            advertising_data_hex,
        } => BluetoothPeerActionV2::CreateBeacon {
            peer_id,
            name,
            advertising_data_hex,
        },
        ActionCommand::BluetoothScriptedBeacon {
            peer_id,
            name,
            frames_json,
            repeat,
        } => BluetoothPeerActionV2::CreateScriptedBeacon {
            peer_id,
            name,
            frames: serde_json::from_str::<Vec<BluetoothAdvertisementFrameV2>>(&frames_json)
                .context("parse --frames-json as a Bluetooth advertisement frame array")?,
            repeat,
        },
        ActionCommand::BluetoothHidKeyboard { peer_id, name } => {
            BluetoothPeerActionV2::CreateHidKeyboard { peer_id, name }
        }
        ActionCommand::BluetoothHidKeyboardReport {
            peer_id,
            modifiers,
            keys,
        } => BluetoothPeerActionV2::SendHidKeyboardReport {
            peer_id,
            modifiers,
            keys,
        },
        ActionCommand::BluetoothHciCapture { duration_ms } => BluetoothPeerActionV2::CaptureHci {
            capture_id: Uuid::new_v4(),
            duration_ms,
        },
        ActionCommand::BluetoothRemove { peer_id } => BluetoothPeerActionV2::RemovePeer { peer_id },
        ActionCommand::BluetoothAdvertise { peer_id, enabled } => {
            BluetoothPeerActionV2::SetAdvertising { peer_id, enabled }
        }
        _ => unreachable!("Bluetooth action helper received a non-Bluetooth command"),
    };
    Ok(InstanceActionV2::BluetoothPeer { action })
}
