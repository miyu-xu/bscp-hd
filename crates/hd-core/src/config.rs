use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const INSTANCE_CONFIG_VERSION: u32 = 2;
pub const DEFAULT_CPU_COUNT: u16 = 4;
pub const DEFAULT_MEMORY_MIB: u32 = 4096;
pub const DEFAULT_WIDTH: u32 = 1080;
pub const DEFAULT_HEIGHT: u32 = 1920;
pub const DEFAULT_DPI: u32 = 420;
pub const DEFAULT_REFRESH_RATE_HZ: u16 = 60;
pub const MAX_SECONDARY_DISPLAYS: usize = 3;
pub const MAX_MICRODROID_EXTRA_APKS: usize = 8;

const ALLOWED_REFRESH_RATES: [u16; 4] = [30, 60, 90, 120];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrientationV2 {
    Portrait,
    Landscape,
    ReversePortrait,
    ReverseLandscape,
}

impl OrientationV2 {
    pub const fn is_portrait(self) -> bool {
        matches!(self, Self::Portrait | Self::ReversePortrait)
    }

    pub const fn android_rotation(self) -> u8 {
        match self {
            Self::Portrait => 0,
            Self::Landscape => 1,
            Self::ReversePortrait => 2,
            Self::ReverseLandscape => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VsyncModeV2 {
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayConfigV2 {
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub refresh_rate_hz: u16,
    pub orientation: OrientationV2,
    pub vsync: VsyncModeV2,
    pub show_host_fps: bool,
    /// Additional Android displays in stable product order. The vector position is never exposed
    /// as product identity: crosvm scanout ids are rebound for every run from each entry's UUID.
    #[serde(default)]
    pub secondary_displays: Vec<SecondaryDisplayConfigV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecondaryDisplayConfigV2 {
    pub id: Uuid,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub refresh_rate_hz: u16,
}

impl Default for DisplayConfigV2 {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            dpi: DEFAULT_DPI,
            refresh_rate_hz: DEFAULT_REFRESH_RATE_HZ,
            orientation: OrientationV2::Portrait,
            vsync: VsyncModeV2::On,
            show_host_fps: false,
            secondary_displays: Vec::new(),
        }
    }
}

impl DisplayConfigV2 {
    pub fn oriented_size(&self) -> (u32, u32) {
        if self.orientation.is_portrait() {
            (self.width.min(self.height), self.width.max(self.height))
        } else {
            (self.width.max(self.height), self.width.min(self.height))
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let (width, height) = self.oriented_size();
        if !(320..=8192).contains(&width) || !(320..=8192).contains(&height) {
            return Err(ConfigError::invalid(
                "display dimensions must each be in 320..=8192",
            ));
        }
        if !(72..=960).contains(&self.dpi) {
            return Err(ConfigError::invalid("display dpi must be in 72..=960"));
        }
        if !ALLOWED_REFRESH_RATES.contains(&self.refresh_rate_hz) {
            return Err(ConfigError::invalid(
                "display refresh_rate_hz must be one of 30, 60, 90, 120",
            ));
        }
        if self.secondary_displays.len() > MAX_SECONDARY_DISPLAYS {
            return Err(ConfigError::invalid(format!(
                "at most {MAX_SECONDARY_DISPLAYS} secondary displays are supported"
            )));
        }
        let mut ids = std::collections::BTreeSet::new();
        for secondary in &self.secondary_displays {
            secondary.validate()?;
            if !ids.insert(secondary.id) {
                return Err(ConfigError::invalid(
                    "secondary display ids must be unique within an instance",
                ));
            }
        }
        Ok(())
    }
}

impl SecondaryDisplayConfigV2 {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.id.is_nil() {
            return Err(ConfigError::invalid("secondary display id must not be nil"));
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 40 {
            return Err(ConfigError::invalid(
                "secondary display name must contain 1..=40 characters",
            ));
        }
        if !(320..=8192).contains(&self.width) || !(320..=8192).contains(&self.height) {
            return Err(ConfigError::invalid(
                "secondary display dimensions must each be in 320..=8192",
            ));
        }
        if !(72..=960).contains(&self.dpi) {
            return Err(ConfigError::invalid(
                "secondary display dpi must be in 72..=960",
            ));
        }
        if !ALLOWED_REFRESH_RATES.contains(&self.refresh_rate_hz) {
            return Err(ConfigError::invalid(
                "secondary display refresh_rate_hz must be one of 30, 60, 90, 120",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdbModeV2 {
    Disabled,
    Loopback,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestKindV2 {
    #[default]
    Android,
    Microdroid,
}

impl GuestKindV2 {
    pub const fn is_android(&self) -> bool {
        matches!(self, Self::Android)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicrodroidDebugLevelV2 {
    None,
    #[default]
    Full,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicrodroidCpuTopologyV2 {
    #[default]
    OneCpu,
    MatchHost,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MicrodroidPayloadV2 {
    #[default]
    Empty,
    Uploaded {
        upload_id: Uuid,
        sha256: String,
        config_path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MicrodroidExtraApkV2 {
    pub upload_id: Uuid,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MicrodroidConfigV2 {
    pub debug_level: MicrodroidDebugLevelV2,
    pub cpu_topology: MicrodroidCpuTopologyV2,
    pub payload: MicrodroidPayloadV2,
    /// Number of extra APKs declared by the uploaded Payload config. `None` is retained only for
    /// instances created before Host-side config inspection was introduced.
    pub payload_extra_apk_count: Option<u8>,
    pub extra_apks: Vec<MicrodroidExtraApkV2>,
    pub encrypted_storage_mib: Option<u32>,
}

impl Default for MicrodroidConfigV2 {
    fn default() -> Self {
        Self {
            debug_level: MicrodroidDebugLevelV2::Full,
            cpu_topology: MicrodroidCpuTopologyV2::OneCpu,
            payload: MicrodroidPayloadV2::Empty,
            payload_extra_apk_count: None,
            extra_apks: Vec::new(),
            encrypted_storage_mib: Some(64),
        }
    }
}

impl MicrodroidConfigV2 {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(size) = self.encrypted_storage_mib
            && !(10..=4096).contains(&size)
        {
            return Err(ConfigError::invalid(
                "microdroid encrypted_storage_mib must be in 10..=4096",
            ));
        }
        if let MicrodroidPayloadV2::Uploaded {
            upload_id,
            sha256,
            config_path,
        } = &self.payload
        {
            if upload_id.is_nil() {
                return Err(ConfigError::invalid(
                    "microdroid payload upload_id cannot be nil",
                ));
            }
            validate_sha256("microdroid payload sha256", sha256)?;
            if config_path != "assets/vm_config.json" {
                return Err(ConfigError::invalid(
                    "microdroid payload config_path must be assets/vm_config.json",
                ));
            }
        }
        if matches!(self.payload, MicrodroidPayloadV2::Empty) {
            if self.payload_extra_apk_count.is_some() {
                return Err(ConfigError::invalid(
                    "microdroid EmptyPayload cannot retain a payload_extra_apk_count",
                ));
            }
            if !self.extra_apks.is_empty() {
                return Err(ConfigError::invalid(
                    "microdroid extra_apks require an uploaded main Payload APK",
                ));
            }
        }
        if self
            .payload_extra_apk_count
            .is_some_and(|count| usize::from(count) > MAX_MICRODROID_EXTRA_APKS)
        {
            return Err(ConfigError::invalid(format!(
                "microdroid Payload cannot declare more than {MAX_MICRODROID_EXTRA_APKS} extra APKs"
            )));
        }
        if self.extra_apks.len() > MAX_MICRODROID_EXTRA_APKS {
            return Err(ConfigError::invalid(format!(
                "microdroid extra_apks cannot exceed {MAX_MICRODROID_EXTRA_APKS}"
            )));
        }
        let mut uploads = BTreeSet::new();
        for extra in &self.extra_apks {
            if extra.upload_id.is_nil() {
                return Err(ConfigError::invalid(
                    "microdroid extra APK upload_id cannot be nil",
                ));
            }
            if !uploads.insert(extra.upload_id) {
                return Err(ConfigError::invalid(
                    "microdroid extra APK upload_id cannot be repeated",
                ));
            }
            validate_sha256("microdroid extra APK sha256", &extra.sha256)?;
        }
        if self
            .payload_extra_apk_count
            .is_some_and(|count| self.extra_apks.len() > usize::from(count))
        {
            return Err(ConfigError::invalid(
                "microdroid selected extra APKs exceed the count declared by the Payload",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdbConfigV2 {
    pub mode: AdbModeV2,
    pub host_port: Option<u16>,
    pub executable: Option<PathBuf>,
}

impl Default for AdbConfigV2 {
    fn default() -> Self {
        Self {
            mode: AdbModeV2::Loopback,
            host_port: None,
            executable: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSelectionV2 {
    pub store_root: PathBuf,
    pub guest_bundle_digest: String,
    pub host_bundle_digest: String,
}

impl ArtifactSelectionV2 {
    fn validate(&self) -> Result<(), ConfigError> {
        if !self.store_root.is_absolute()
            || self
                .store_root
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(ConfigError::invalid(
                "artifact store_root must be an absolute normalized path",
            ));
        }
        validate_sha256("guest_bundle_digest", &self.guest_bundle_digest)?;
        validate_sha256("host_bundle_digest", &self.host_bundle_digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BootConfigV2 {
    pub kernel_log_level: u8,
    pub panic_timeout_seconds: u16,
    pub boot_animation: bool,
}

impl Default for BootConfigV2 {
    fn default() -> Self {
        Self {
            kernel_log_level: 4,
            panic_timeout_seconds: 5,
            boot_animation: true,
        }
    }
}

impl BootConfigV2 {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.kernel_log_level > 7 {
            return Err(ConfigError::invalid("kernel_log_level must be in 0..=7"));
        }
        if self.panic_timeout_seconds > 300 {
            return Err(ConfigError::invalid(
                "panic_timeout_seconds must be in 0..=300",
            ));
        }
        Ok(())
    }

    pub fn kernel_arguments(&self) -> Vec<String> {
        let mut arguments = vec![
            format!("loglevel={}", self.kernel_log_level),
            format!("panic={}", self.panic_timeout_seconds),
            format!(
                "printk.devkmsg={}",
                if self.kernel_log_level >= 7 {
                    "on"
                } else {
                    "off"
                }
            ),
        ];
        if !self.boot_animation {
            arguments.push("androidboot.debug.sf.nobootanimation=1".to_owned());
        }
        arguments
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceConfigV2 {
    pub bluetooth: bool,
    pub nfc: bool,
    pub uwb: bool,
    pub modem: bool,
    pub gnss: bool,
    pub sensors: bool,
    pub network: bool,
    pub audio: bool,
    pub camera: bool,
    pub power: bool,
    /// Independent indirect pointing surface, separate from the Android display touchscreen.
    pub touchpad: bool,
}

impl Default for DeviceConfigV2 {
    fn default() -> Self {
        Self {
            bluetooth: true,
            nfc: true,
            uwb: true,
            modem: true,
            gnss: true,
            sensors: true,
            network: true,
            audio: true,
            camera: true,
            power: true,
            touchpad: false,
        }
    }
}

impl DeviceConfigV2 {
    pub const fn all_disabled(&self) -> bool {
        !self.bluetooth
            && !self.nfc
            && !self.uwb
            && !self.modem
            && !self.gnss
            && !self.sensors
            && !self.network
            && !self.audio
            && !self.camera
            && !self.power
            && !self.touchpad
    }

    pub const fn disabled() -> Self {
        Self {
            bluetooth: false,
            nfc: false,
            uwb: false,
            modem: false,
            gnss: false,
            sensors: false,
            network: false,
            audio: false,
            camera: false,
            power: false,
            touchpad: false,
        }
    }
}

/// Explicit Host audio-input routing for one Android instance.
///
/// The virtual capture endpoint can exist without reading a physical microphone.  Host capture
/// therefore stays opt-in and is represented separately from `devices.audio` so enabling the
/// Guest sound card never grants microphone access implicitly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostAudioInputV2 {
    #[default]
    Disabled,
    DefaultMicrophone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicyV2 {
    Never,
    OnFailure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstanceSpecV2 {
    pub schema_version: u32,
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub guest_kind: GuestKindV2,
    #[serde(default)]
    pub microdroid: Option<MicrodroidConfigV2>,
    pub cpu_count: u16,
    pub memory_mib: u32,
    pub display: DisplayConfigV2,
    pub adb: AdbConfigV2,
    pub artifacts: Option<ArtifactSelectionV2>,
    pub boot: BootConfigV2,
    pub devices: DeviceConfigV2,
    #[serde(default)]
    pub host_audio_input: HostAudioInputV2,
    pub restart_policy: RestartPolicyV2,
    pub labels: BTreeMap<String, String>,
}

impl Default for InstanceSpecV2 {
    fn default() -> Self {
        Self {
            schema_version: INSTANCE_CONFIG_VERSION,
            id: Uuid::new_v4(),
            name: "Android".to_owned(),
            guest_kind: GuestKindV2::Android,
            microdroid: None,
            cpu_count: DEFAULT_CPU_COUNT,
            memory_mib: DEFAULT_MEMORY_MIB,
            display: DisplayConfigV2::default(),
            adb: AdbConfigV2::default(),
            artifacts: None,
            boot: BootConfigV2::default(),
            devices: DeviceConfigV2::default(),
            host_audio_input: HostAudioInputV2::Disabled,
            restart_policy: RestartPolicyV2::OnFailure,
            labels: BTreeMap::new(),
        }
    }
}

impl InstanceSpecV2 {
    pub fn microdroid(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            guest_kind: GuestKindV2::Microdroid,
            microdroid: Some(MicrodroidConfigV2::default()),
            cpu_count: 1,
            memory_mib: 512,
            display: DisplayConfigV2::default(),
            adb: AdbConfigV2 {
                mode: AdbModeV2::Disabled,
                host_port: None,
                executable: None,
            },
            devices: DeviceConfigV2::disabled(),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != INSTANCE_CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.schema_version));
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 80 {
            return Err(ConfigError::invalid(
                "instance name must contain 1..=80 characters",
            ));
        }
        if !(1..=256).contains(&self.cpu_count) {
            return Err(ConfigError::invalid("cpu_count must be in 1..=256"));
        }
        let minimum_memory = match self.guest_kind {
            GuestKindV2::Android => 2048,
            GuestKindV2::Microdroid => 256,
        };
        if !(minimum_memory..=1_048_576).contains(&self.memory_mib) {
            return Err(ConfigError::invalid(format!(
                "memory_mib must be in {minimum_memory}..=1048576 for this guest kind"
            )));
        }
        match self.guest_kind {
            GuestKindV2::Android => {
                if self.microdroid.is_some() {
                    return Err(ConfigError::invalid(
                        "microdroid config must be absent for Android instances",
                    ));
                }
                self.display.validate()?;
                self.boot.validate()?;
                if !self.devices.audio
                    && !matches!(self.host_audio_input, HostAudioInputV2::Disabled)
                {
                    return Err(ConfigError::invalid(
                        "host_audio_input requires the Android audio device",
                    ));
                }
            }
            GuestKindV2::Microdroid => {
                let microdroid = self.microdroid.as_ref().ok_or_else(|| {
                    ConfigError::invalid("microdroid config is required for Microdroid instances")
                })?;
                microdroid.validate()?;
                if self.cpu_count != 1 {
                    return Err(ConfigError::invalid(
                        "Microdroid cpu_count must remain 1; microdroid.cpu_topology selects one_cpu or match_host",
                    ));
                }
                if !self.devices.all_disabled() {
                    return Err(ConfigError::invalid(
                        "Android device simulation must be disabled for Microdroid instances",
                    ));
                }
                if !matches!(self.host_audio_input, HostAudioInputV2::Disabled) {
                    return Err(ConfigError::invalid(
                        "host audio input is not valid for Microdroid instances",
                    ));
                }
                if self.artifacts.is_some() {
                    return Err(ConfigError::invalid(
                        "Android artifact bundle selection is not valid for Microdroid instances",
                    ));
                }
                if matches!(microdroid.debug_level, MicrodroidDebugLevelV2::None)
                    && !matches!(self.adb.mode, AdbModeV2::Disabled)
                {
                    return Err(ConfigError::invalid(
                        "ADB must be disabled when Microdroid debug_level is none",
                    ));
                }
            }
        }
        if matches!(self.adb.mode, AdbModeV2::Disabled) && self.adb.host_port.is_some() {
            return Err(ConfigError::invalid(
                "adb.host_port must be absent when adb is disabled",
            ));
        }
        if self.adb.host_port == Some(0) {
            return Err(ConfigError::invalid("adb.host_port cannot be zero"));
        }
        if let Some(artifacts) = &self.artifacts {
            artifacts.validate()?;
        }
        if self.labels.len() > 32 {
            return Err(ConfigError::invalid("at most 32 labels are allowed"));
        }
        for (key, value) in &self.labels {
            if key.is_empty()
                || key.len() > 64
                || value.len() > 256
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                return Err(ConfigError::invalid(
                    "labels must use 1..=64 ASCII key characters and values up to 256 bytes",
                ));
            }
        }
        Ok(())
    }

    pub fn change_impact(&self, next: &Self) -> ChangeImpactV2 {
        if self == next {
            return ChangeImpactV2::None;
        }
        if self.id != next.id
            || self.guest_kind != next.guest_kind
            || self.microdroid != next.microdroid
            || self.cpu_count != next.cpu_count
            || self.memory_mib != next.memory_mib
            || self.artifacts != next.artifacts
            || self.boot != next.boot
            || self.devices != next.devices
            || self.host_audio_input != next.host_audio_input
        {
            ChangeImpactV2::RestartRequired
        } else {
            ChangeImpactV2::Live
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeImpactV2 {
    None,
    Live,
    RestartRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationOutcomeV2 {
    pub spec: InstanceSpecV2,
    pub source_version: u32,
    pub notes: Vec<String>,
}

pub fn decode_or_migrate_instance(bytes: &[u8]) -> Result<MigrationOutcomeV2, ConfigError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(ConfigError::Decode)?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| ConfigError::invalid("schema_version is missing or invalid"))?;
    let version = u32::try_from(version)
        .map_err(|_| ConfigError::invalid("schema_version is out of range"))?;
    match version {
        INSTANCE_CONFIG_VERSION => {
            let spec: InstanceSpecV2 =
                serde_json::from_value(value).map_err(ConfigError::Decode)?;
            spec.validate()?;
            Ok(MigrationOutcomeV2 {
                spec,
                source_version: INSTANCE_CONFIG_VERSION,
                notes: Vec::new(),
            })
        }
        1 => migrate_v1(value),
        other => Err(ConfigError::UnsupportedVersion(other)),
    }
}

fn migrate_v1(value: serde_json::Value) -> Result<MigrationOutcomeV2, ConfigError> {
    let legacy: LegacyInstanceConfigV1 =
        serde_json::from_value(value).map_err(ConfigError::Decode)?;
    let orientation = match legacy.display.orientation {
        LegacyOrientation::Landscape => OrientationV2::Landscape,
        LegacyOrientation::Portrait => OrientationV2::Portrait,
    };
    let vsync = match legacy.display.vsync {
        LegacyVsyncMode::On => VsyncModeV2::On,
        LegacyVsyncMode::Off => VsyncModeV2::Off,
    };
    let adb_mode = if legacy.adb.enabled {
        AdbModeV2::Loopback
    } else {
        AdbModeV2::Disabled
    };
    let mut notes = vec![
        "legacy runtime state was reset to stopped".to_owned(),
        "legacy arbitrary kernel arguments were rejected by the V2 safety policy".to_owned(),
        "legacy direct artifact paths require selection of a signed V2 bundle".to_owned(),
    ];
    if !legacy.extra_kernel_args.is_empty() {
        notes.push(format!(
            "{} legacy kernel argument(s) were not migrated",
            legacy.extra_kernel_args.len()
        ));
    }
    if legacy.artifacts.has_any_path() {
        notes.push("legacy artifact paths remain only in the migration backup".to_owned());
    }
    let spec = InstanceSpecV2 {
        schema_version: INSTANCE_CONFIG_VERSION,
        id: legacy.id,
        name: legacy.name,
        guest_kind: GuestKindV2::Android,
        microdroid: None,
        cpu_count: legacy.cpu_count,
        memory_mib: legacy.memory_mib.max(2048),
        display: DisplayConfigV2 {
            width: legacy.display.width,
            height: legacy.display.height,
            dpi: legacy.display.dpi,
            refresh_rate_hz: u16::try_from(legacy.display.refresh_rate_hz)
                .ok()
                .filter(|rate| ALLOWED_REFRESH_RATES.contains(rate))
                .unwrap_or(DEFAULT_REFRESH_RATE_HZ),
            orientation,
            vsync,
            show_host_fps: legacy.display.show_host_fps,
            secondary_displays: Vec::new(),
        },
        adb: AdbConfigV2 {
            mode: adb_mode,
            host_port: if legacy.adb.enabled && !legacy.adb.auto_port {
                legacy.adb.host_port
            } else {
                None
            },
            executable: legacy.adb.adb_path,
        },
        artifacts: None,
        boot: BootConfigV2::default(),
        devices: DeviceConfigV2::default(),
        host_audio_input: HostAudioInputV2::Disabled,
        restart_policy: RestartPolicyV2::OnFailure,
        labels: BTreeMap::new(),
    };
    spec.validate()?;
    Ok(MigrationOutcomeV2 {
        spec,
        source_version: 1,
        notes,
    })
}

fn validate_sha256(label: &str, value: &str) -> Result<(), ConfigError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ConfigError::invalid(format!(
            "{label} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyInstanceConfigV1 {
    #[allow(dead_code)]
    schema_version: u32,
    id: Uuid,
    name: String,
    cpu_count: u16,
    memory_mib: u32,
    display: LegacyDisplayConfig,
    adb: LegacyAdbConfig,
    artifacts: LegacyArtifactConfig,
    #[serde(default)]
    extra_kernel_args: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyOrientation {
    Landscape,
    Portrait,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyVsyncMode {
    On,
    Off,
}

#[derive(Debug, Deserialize)]
struct LegacyDisplayConfig {
    width: u32,
    height: u32,
    dpi: u32,
    refresh_rate_hz: u32,
    orientation: LegacyOrientation,
    vsync: LegacyVsyncMode,
    show_host_fps: bool,
}

#[derive(Debug, Deserialize)]
struct LegacyAdbConfig {
    enabled: bool,
    auto_port: bool,
    host_port: Option<u16>,
    adb_path: Option<PathBuf>,
    #[allow(dead_code)]
    auto_root: bool,
}

#[derive(Debug, Deserialize)]
struct LegacyArtifactConfig {
    kernel: PathBuf,
    initrd: PathBuf,
    rootfs: PathBuf,
    android_fstab: PathBuf,
    system_image: Option<PathBuf>,
    vendor_image: Option<PathBuf>,
    #[allow(dead_code)]
    expected_sha256: BTreeMap<String, String>,
}

impl LegacyArtifactConfig {
    fn has_any_path(&self) -> bool {
        !self.kernel.as_os_str().is_empty()
            || !self.initrd.as_os_str().is_empty()
            || !self.rootfs.as_os_str().is_empty()
            || !self.android_fstab.as_os_str().is_empty()
            || self.system_image.is_some()
            || self.vendor_image.is_some()
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("unsupported config schema version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("failed to decode config: {0}")]
    Decode(serde_json::Error),
    #[error("failed to encode config: {0}")]
    Encode(serde_json::Error),
}

impl ConfigError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_v2_contract() {
        let spec = InstanceSpecV2::default();
        assert_eq!(spec.memory_mib, 4096);
        assert_eq!(spec.display.oriented_size(), (1080, 1920));
        assert_eq!(spec.display.dpi, 420);
        assert_eq!(spec.display.refresh_rate_hz, 60);
        assert_eq!(spec.display.vsync, VsyncModeV2::On);
        assert_eq!(spec.adb.mode, AdbModeV2::Loopback);
        assert_eq!(spec.boot.kernel_log_level, 4);
        assert!(
            spec.boot
                .kernel_arguments()
                .contains(&"printk.devkmsg=off".to_owned())
        );
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn unsafe_legacy_fields_are_not_migrated() {
        let bytes = br#"{
          "schema_version":1,
          "id":"00000000-0000-0000-0000-000000000001",
          "name":"legacy",
          "cpu_count":4,
          "memory_mib":4096,
          "display":{"width":1280,"height":720,"dpi":320,"refresh_rate_hz":60,"orientation":"landscape","vsync":"on","show_host_fps":false},
          "adb":{"enabled":true,"auto_port":true,"host_port":null,"adb_path":null,"auto_root":true},
          "artifacts":{"kernel":"kernel","initrd":"initrd","rootfs":"rootfs","android_fstab":"fstab","system_image":null,"vendor_image":null,"expected_sha256":{}},
          "extra_kernel_args":["androidboot.selinux=permissive"]
        }"#;
        let migrated = decode_or_migrate_instance(bytes).expect("migrate");
        assert_eq!(migrated.source_version, 1);
        assert!(migrated.spec.artifacts.is_none());
        assert!(
            migrated
                .notes
                .iter()
                .any(|note| note.contains("not migrated"))
        );
    }

    #[test]
    fn artifact_store_root_must_be_process_independent() {
        let selection = ArtifactSelectionV2 {
            store_root: PathBuf::from("relative-artifacts"),
            guest_bundle_digest: "a".repeat(64),
            host_bundle_digest: "b".repeat(64),
        };
        assert!(selection.validate().is_err());
    }

    #[test]
    fn microdroid_rejects_unrepresentable_cpu_and_android_artifacts() {
        let mut spec = InstanceSpecV2::microdroid("microdroid");
        spec.cpu_count = 2;
        assert!(spec.validate().is_err());

        spec.cpu_count = 1;
        spec.artifacts = Some(ArtifactSelectionV2 {
            store_root: PathBuf::from("/absolute/artifacts"),
            guest_bundle_digest: "a".repeat(64),
            host_bundle_digest: "b".repeat(64),
        });
        assert!(spec.validate().is_err());
    }

    #[test]
    fn microdroid_none_debug_requires_adb_disabled() {
        let mut spec = InstanceSpecV2::microdroid("microdroid");
        spec.microdroid
            .as_mut()
            .expect("microdroid config")
            .debug_level = MicrodroidDebugLevelV2::None;
        spec.adb.mode = AdbModeV2::Loopback;
        assert!(spec.validate().is_err());
        spec.adb.mode = AdbModeV2::Disabled;
        assert!(spec.validate().is_ok());
    }
}
