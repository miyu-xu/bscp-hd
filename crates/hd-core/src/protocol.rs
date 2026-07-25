use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AdbConfigV2, DesiredStateV2, DisplayConfigV2, InstanceSpecV2, InstanceStatusV2,
    ObservedStateV2, OrientationV2,
};

pub const CONTROL_PROTOCOL_VERSION: u32 = 2;
pub const WORKER_PROTOCOL_VERSION: u32 = 2;
pub const FRAME_PROTOCOL_VERSION: u32 = 2;
pub const ARTIFACT_INDEX_VERSION: u32 = 2;
pub const DIAGNOSTIC_MANIFEST_VERSION: u32 = 2;
pub const HOST_CERTIFICATION_VERSION: u32 = 2;
pub const COMPONENT_PROTOCOL_VERSION: u32 = 2;
pub const DEVICE_GUEST_ENDPOINT_ROLES_V2: [&str; 9] = [
    "bluetooth",
    "gnss",
    "location",
    "uwb",
    "nfc",
    "sensors",
    "mcu-control",
    "mcu-uart",
    "modem",
];

pub fn device_component_guest_roles_v2(component: &str) -> &'static [&'static str] {
    match component {
        "hd-device-sim" => &["gnss", "location", "sensors", "mcu-control", "mcu-uart"],
        "rootcanal-adapter" => &["bluetooth"],
        "casimir-adapter" => &["nfc"],
        "uwb-adapter" => &["uwb"],
        // Modem uses host-vsock, network shaping uses typed ADB, and audio/camera
        // are native Guest virtio/HAL devices. Unknown components likewise receive
        // no private serial control channel.
        _ => &[],
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DeviceControlTokenV2(String);

impl DeviceControlTokenV2 {
    pub fn from_hex(value: String) -> Option<Self> {
        (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then_some(Self(value))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for DeviceControlTokenV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl<'de> Deserialize<'de> for DeviceControlTokenV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_hex(value).ok_or_else(|| {
            serde::de::Error::custom("device control token must be 64 hexadecimal characters")
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyActionV2 {
    Home,
    Recent,
    Back,
    Power,
    VolumeUp,
    VolumeDown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocationV2 {
    pub latitude_e7: i32,
    pub longitude_e7: i32,
    pub altitude_mm: i32,
    pub accuracy_mm: u32,
}

impl LocationV2 {
    pub fn is_valid(&self) -> bool {
        (-900_000_000..=900_000_000).contains(&self.latitude_e7)
            && (-1_800_000_000..=1_800_000_000).contains(&self.longitude_e7)
            && self.accuracy_mm <= 1_000_000
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatteryStateV2 {
    pub level_percent: u8,
    pub charging: bool,
    pub temperature_deci_celsius: i16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConditionV2 {
    pub latency_ms: u32,
    pub loss_basis_points: u16,
    pub bandwidth_kbps: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorInjectionV2 {
    pub sensor: String,
    pub values_microunits: Vec<i64>,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum BluetoothPeerActionV2 {
    CreateGattPeer { peer_id: Uuid, name: String },
    RemovePeer { peer_id: Uuid },
    SetAdvertising { peer_id: Uuid, enabled: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum NfcTagActionV2 {
    PresentType2 { ndef_hex: String },
    PresentType4 { ndef_hex: String },
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "parameters", rename_all = "snake_case")]
pub enum InstanceActionV2 {
    Key { key: KeyActionV2 },
    Rotate { orientation: OrientationV2 },
    SetLocation { location: LocationV2 },
    SetBattery { battery: BatteryStateV2 },
    SetNetworkCondition { condition: NetworkConditionV2 },
    InjectSensor { injection: SensorInjectionV2 },
    BluetoothPeer { action: BluetoothPeerActionV2 },
    NfcTag { action: NfcTagActionV2 },
}

impl InstanceActionV2 {
    pub fn validate(&self) -> Result<(), ActionValidationError> {
        match self {
            Self::Key { .. }
            | Self::Rotate { .. }
            | Self::NfcTag {
                action: NfcTagActionV2::Remove,
            } => Ok(()),
            Self::SetLocation { location } if location.is_valid() => Ok(()),
            Self::SetLocation { .. } => Err(ActionValidationError::Location),
            Self::SetBattery { battery }
                if battery.level_percent <= 100
                    && (-500..=1_500).contains(&battery.temperature_deci_celsius) =>
            {
                Ok(())
            }
            Self::SetBattery { .. } => Err(ActionValidationError::Battery),
            Self::SetNetworkCondition { condition }
                if condition.latency_ms <= 60_000
                    && condition.loss_basis_points <= 10_000
                    && condition.bandwidth_kbps != Some(0) =>
            {
                Ok(())
            }
            Self::SetNetworkCondition { .. } => Err(ActionValidationError::Network),
            Self::InjectSensor { injection } => validate_sensor_injection(injection),
            Self::BluetoothPeer { action } => validate_bluetooth_action(action),
            Self::NfcTag {
                action:
                    NfcTagActionV2::PresentType2 { ndef_hex }
                    | NfcTagActionV2::PresentType4 { ndef_hex },
            } => validate_ndef_hex(ndef_hex),
        }
    }
}

fn validate_sensor_injection(injection: &SensorInjectionV2) -> Result<(), ActionValidationError> {
    let expected_values = match injection.sensor.as_str() {
        "accelerometer" | "gyroscope" | "magnetometer" => 3,
        "light" | "proximity" => 1,
        _ => return Err(ActionValidationError::Sensor),
    };
    if injection.values_microunits.len() == expected_values
        && injection.duration_ms <= 3_600_000
        && injection
            .values_microunits
            .iter()
            .all(|value| value.unsigned_abs() <= 1_000_000_000_000)
    {
        Ok(())
    } else {
        Err(ActionValidationError::Sensor)
    }
}

fn validate_bluetooth_action(action: &BluetoothPeerActionV2) -> Result<(), ActionValidationError> {
    let valid = match action {
        BluetoothPeerActionV2::CreateGattPeer { peer_id, name } => {
            !peer_id.is_nil() && !name.trim().is_empty() && name.len() <= 128
        }
        BluetoothPeerActionV2::RemovePeer { peer_id }
        | BluetoothPeerActionV2::SetAdvertising { peer_id, .. } => !peer_id.is_nil(),
    };
    if valid {
        Ok(())
    } else {
        Err(ActionValidationError::Bluetooth)
    }
}

fn validate_ndef_hex(ndef_hex: &str) -> Result<(), ActionValidationError> {
    if !ndef_hex.is_empty()
        && ndef_hex.len() <= 16 * 1024
        && ndef_hex.len().is_multiple_of(2)
        && ndef_hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(ActionValidationError::Nfc)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ActionValidationError {
    #[error("location is outside the supported geodetic range")]
    Location,
    #[error("battery state is outside the supported range")]
    Battery,
    #[error("network condition is outside the supported range")]
    Network,
    #[error("sensor injection is outside the supported range")]
    Sensor,
    #[error("Bluetooth peer action is outside the supported contract")]
    Bluetooth,
    #[error("NFC NDEF payload is outside the supported contract")]
    Nfc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopModeV2 {
    Graceful,
    Force,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "parameters", rename_all = "snake_case")]
pub enum OperationKindV2 {
    Start,
    Stop {
        mode: StopModeV2,
        graceful_timeout_ms: u32,
    },
    Restart,
    Pause,
    Resume,
    Reconfigure {
        display: DisplayConfigV2,
        adb: AdbConfigV2,
    },
    InstallApk {
        upload_id: Uuid,
        sha256: String,
    },
    CollectDiagnostics {
        include_guest_logs: bool,
    },
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStateV2 {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRecordV2 {
    pub id: Uuid,
    pub instance_id: Option<Uuid>,
    pub kind: OperationKindV2,
    pub state: OperationStateV2,
    pub progress_per_mille: u16,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub error: Option<ApiErrorV2>,
    pub result: BTreeMap<String, String>,
}

impl OperationRecordV2 {
    pub fn queued(instance_id: Option<Uuid>, kind: OperationKindV2) -> Self {
        Self {
            id: Uuid::new_v4(),
            instance_id,
            kind,
            state: OperationStateV2::Queued,
            progress_per_mille: 0,
            created_at: OffsetDateTime::now_utc(),
            started_at: None,
            finished_at: None,
            error: None,
            result: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerIdentityV2 {
    pub pid: u32,
    pub process_start_marker: String,
    pub nonce: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceRecordV2 {
    pub spec: InstanceSpecV2,
    pub status: InstanceStatusV2,
    pub active_run_id: Option<Uuid>,
    pub worker: Option<WorkerIdentityV2>,
    pub adb_serial: Option<String>,
    pub host_fps_milli: Option<u32>,
    pub frame_generation: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl InstanceRecordV2 {
    pub fn new(spec: InstanceSpecV2) -> Self {
        Self {
            spec,
            status: InstanceStatusV2::default(),
            active_run_id: None,
            worker: None,
            adb_serial: None,
            host_fps_milli: None,
            frame_generation: 0,
            created_at: OffsetDateTime::now_utc(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSummaryV2 {
    pub id: Uuid,
    pub name: String,
    pub status: InstanceStatusV2,
    pub active_run_id: Option<Uuid>,
    pub worker: Option<WorkerIdentityV2>,
    pub adb_serial: Option<String>,
    pub host_fps_milli: Option<u32>,
    pub frame_generation: u64,
}

impl From<&InstanceRecordV2> for InstanceSummaryV2 {
    fn from(record: &InstanceRecordV2) -> Self {
        Self {
            id: record.spec.id,
            name: record.spec.name.clone(),
            status: record.status.clone(),
            active_run_id: record.active_run_id,
            worker: record.worker.clone(),
            adb_serial: record.adb_serial.clone(),
            host_fps_milli: record.host_fps_milli,
            frame_generation: record.frame_generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInstanceRequestV2 {
    pub spec: InstanceSpecV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateInstanceRequestV2 {
    pub expected_revision: u64,
    pub spec: InstanceSpecV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateOperationRequestV2 {
    pub operation: OperationKindV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequestV2 {
    pub action: InstanceActionV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiErrorV2 {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: BTreeMap<String, String>,
}

impl ApiErrorV2 {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            details: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    #[must_use]
    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponseV2 {
    pub protocol_version: u32,
    pub service: String,
    pub pid: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRuntimeDescriptorV2 {
    pub protocol_version: u32,
    pub pid: u32,
    pub process_start_marker: String,
    pub origin: String,
    pub bearer_token: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
}

impl std::fmt::Debug for HostRuntimeDescriptorV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostRuntimeDescriptorV2")
            .field("protocol_version", &self.protocol_version)
            .field("pid", &self.pid)
            .field("process_start_marker", &self.process_start_marker)
            .field("origin", &self.origin)
            .field("bearer_token", &"[REDACTED]")
            .field("started_at", &self.started_at)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatusV2 {
    Supported,
    Blocked,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityProbeV2 {
    pub id: String,
    pub status: CapabilityStatusV2,
    pub required: bool,
    pub detail: String,
    pub properties: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceBackendKindV2 {
    OfficialComponent,
    Simulated,
    SoftwareBacked,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapabilityV2 {
    pub id: String,
    pub backend: DeviceBackendKindV2,
    pub available: bool,
    pub boundary: String,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapabilitiesV2 {
    pub profile: String,
    pub devices: Vec<DeviceCapabilityV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormalComponentProbeV2 {
    pub protocol_version: u32,
    pub component: String,
    pub formal: bool,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "parameters", rename_all = "snake_case")]
pub enum FormalComponentConfigurationV2 {
    FrameBroker {
        broker_endpoint: String,
        frame_ready_marker: PathBuf,
        generation: u64,
        transport: FrameTransportKindV2,
        display: DisplayConfigV2,
    },
    AdbBridge {
        listen_address: String,
        listen_port: u16,
        guest_cid: u32,
        guest_port: u32,
        vm_control_endpoint: String,
        crosvm_executable: PathBuf,
    },
    DeviceAdapter {
        control_endpoint: String,
        control_token: DeviceControlTokenV2,
        guest_cid: u32,
        vm_control_endpoint: String,
        guest_endpoints: BTreeMap<String, DeviceSerialEndpointV2>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormalComponentLaunchV2 {
    pub protocol_version: u32,
    pub component: String,
    pub instance_id: Uuid,
    pub run_id: Uuid,
    pub component_ready_marker: PathBuf,
    pub configuration: FormalComponentConfigurationV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormalComponentReadyV2 {
    pub protocol_version: u32,
    pub component: String,
    pub instance_id: Uuid,
    pub run_id: Uuid,
    pub launch_sha256: String,
    pub pid: u32,
    pub process_start_marker: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "parameters", rename_all = "snake_case")]
pub enum DeviceControlCommandV2 {
    Ping,
    Action { action: InstanceActionV2 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceControlRequestV2 {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub instance_id: Uuid,
    pub run_id: Uuid,
    pub bearer_token: DeviceControlTokenV2,
    pub command: DeviceControlCommandV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceControlResponseV2 {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub instance_id: Uuid,
    pub run_id: Uuid,
    pub ok: bool,
    pub error: Option<ApiErrorV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapabilitiesV2 {
    pub schema_version: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub platform: String,
    pub architecture: String,
    pub fingerprint: String,
    pub certified: bool,
    pub probes: Vec<CapabilityProbeV2>,
    pub devices: DeviceCapabilitiesV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCertificationV2 {
    pub schema_version: u32,
    pub certification_id: Uuid,
    pub platform: String,
    pub architecture: String,
    pub capability_fingerprint: String,
    pub guest_bundle_digest: String,
    pub host_bundle_digest: String,
    pub device_profile: String,
    pub control_protocol_version: u32,
    pub frame_protocol_version: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub evidence_sha256: BTreeMap<String, String>,
    pub signer_key_id: String,
    pub signature_ed25519: String,
}

impl HostCapabilitiesV2 {
    pub fn can_start(&self) -> bool {
        self.certified
            && self.probes.iter().all(|probe| {
                !probe.required || matches!(probe.status, CapabilityStatusV2::Supported)
            })
            && self
                .devices
                .devices
                .iter()
                .filter(|device| !matches!(device.backend, DeviceBackendKindV2::Unsupported))
                .all(|device| device.available)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseKindV2 {
    CpuCapacity,
    MemoryBytes,
    GuestCid,
    AdbPort,
    DiskOverlay,
    GpuSlot,
    WorkerEndpoint,
    DeviceEndpoint,
    FrameGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseOwnerV2 {
    pub instance_id: Uuid,
    pub worker_nonce: Option<Uuid>,
    pub pid: Option<u32>,
    pub process_start_marker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseV2 {
    pub id: Uuid,
    pub kind: LeaseKindV2,
    pub resource: String,
    pub owner: LeaseOwnerV2,
    pub generation: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub acquired_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_verified_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameTransportKindV2 {
    VulkanWin32,
    VulkanDmaBuf,
    MetalIoSurface,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameFormatV2 {
    Bgra8Srgb,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameDescriptorV2 {
    pub protocol_version: u32,
    pub instance_id: Uuid,
    pub generation: u64,
    pub buffer_id: u8,
    pub transport: FrameTransportKindV2,
    pub format: FrameFormatV2,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub acquire_value: u64,
    pub release_value: u64,
    pub out_of_band_handle_count: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMetricsV2 {
    pub generation: u64,
    pub produced_frames: u64,
    pub imported_frames: u64,
    pub presented_frames: u64,
    pub dropped_frames: u64,
    pub cpu_readback_bytes: u64,
    pub software_blit_count: u64,
    pub fps_milli: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameReadyMarkerV2 {
    pub protocol_version: u32,
    pub instance_id: Uuid,
    pub run_id: Uuid,
    pub producer_pid: u32,
    pub producer_process_start_marker: String,
    pub generation: u64,
    pub transport: FrameTransportKindV2,
    pub buffer_count: u8,
    pub memory_export: bool,
    pub explicit_sync: bool,
    pub same_adapter: bool,
    pub validation_clean: bool,
    pub cpu_readback_bytes: u64,
    pub software_blit_count: u64,
}

impl FrameReadyMarkerV2 {
    pub fn strict_zero_copy(&self) -> bool {
        self.protocol_version == FRAME_PROTOCOL_VERSION
            && self.generation > 0
            && self.buffer_count >= 3
            && self.memory_export
            && self.explicit_sync
            && self.same_adapter
            && self.validation_clean
            && self.cpu_readback_bytes == 0
            && self.software_blit_count == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactBundleKindV2 {
    Guest,
    HostTools,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactFileV2 {
    pub role: String,
    pub relative_path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactBundleV2 {
    pub schema_version: u32,
    pub digest: String,
    pub kind: ArtifactBundleKindV2,
    pub platform: String,
    pub architecture: String,
    pub source_manifest_digest: String,
    pub files: Vec<ArtifactFileV2>,
    pub capabilities: Vec<String>,
    pub signer_key_id: String,
    pub signature_ed25519: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReadyMarkerV2 {
    pub schema_version: u32,
    pub bundle_digest: String,
    pub manifest_sha256: String,
    #[serde(with = "time::serde::rfc3339")]
    pub published_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedGuestArtifactsV2 {
    pub guest_bundle_digest: String,
    pub host_bundle_digest: String,
    pub guest_bundle_root: PathBuf,
    pub host_bundle_root: PathBuf,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub rootfs: PathBuf,
    pub android_fstab: PathBuf,
    pub sensor_injector: PathBuf,
    pub system_image: Option<PathBuf>,
    pub vendor_image: Option<PathBuf>,
    pub host_tools: BTreeMap<String, PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStatusV2 {
    Pass,
    Warn,
    Fail,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticCheckV2 {
    pub id: String,
    pub status: DiagnosticStatusV2,
    pub detail: String,
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticFileV2 {
    pub relative_path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticManifestV2 {
    pub schema_version: u32,
    pub bundle_id: Uuid,
    pub instance_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub redaction_policy: String,
    pub max_bytes: u64,
    pub files: Vec<DiagnosticFileV2>,
    pub omitted: Vec<String>,
    pub checks: Vec<DiagnosticCheckV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticRequestV2 {
    pub instance_id: Option<Uuid>,
    pub include_guest_logs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticBundleResponseV2 {
    pub bundle_id: Uuid,
    pub path: PathBuf,
    pub manifest_sha256: String,
    pub archive_sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunManifestV2 {
    pub schema_version: u32,
    pub run_id: Uuid,
    pub instance: InstanceSpecV2,
    pub artifact_bundles: Vec<ArtifactBundleV2>,
    pub capabilities_fingerprint: String,
    pub launch: Option<LaunchPlanV2>,
    pub toolchain: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEventV2 {
    pub schema_version: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub sequence: u64,
    pub trace_id: Uuid,
    pub instance_id: Uuid,
    pub run_id: Uuid,
    pub category: String,
    pub name: String,
    pub state: Option<ObservedStateV2>,
    pub error_code: Option<String>,
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResultV2 {
    pub schema_version: u32,
    pub run_id: Uuid,
    pub instance_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub final_state: ObservedStateV2,
    pub exit_code: Option<i32>,
    pub error_code: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPlanV2 {
    pub schema_version: u32,
    pub instance_id: Uuid,
    pub run_id: Uuid,
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: PathBuf,
    pub control_endpoint: String,
    pub frame_endpoint: String,
    /// Raw virtio-input event endpoint. This is an ephemeral launch contract and is never
    /// persisted in instance configuration.
    pub keyboard_endpoint: String,
    /// Ephemeral host/guest serial bridges keyed by the fixed Android device role.
    pub device_endpoints: BTreeMap<String, DeviceSerialEndpointV2>,
    /// Authenticated local control endpoints keyed by signed formal device component id.
    pub device_control_endpoints: BTreeMap<String, String>,
    pub adb_serial: Option<String>,
    pub guest_cid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSerialEndpointV2 {
    /// Bytes emitted by the guest are read from this endpoint by the host component.
    pub guest_output: String,
    /// Bytes written here by the host component are delivered to the guest.
    pub guest_input: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerDescriptorV2 {
    pub protocol_version: u32,
    pub instance_id: Uuid,
    pub identity: WorkerIdentityV2,
    pub endpoint: String,
    pub secret_path: PathBuf,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRequestV2 {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub instance_id: Uuid,
    pub bearer_token: String,
    pub command: WorkerCommandV2,
}

impl std::fmt::Debug for WorkerRequestV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerRequestV2")
            .field("protocol_version", &self.protocol_version)
            .field("request_id", &self.request_id)
            .field("instance_id", &self.instance_id)
            .field("bearer_token", &"[REDACTED]")
            .field("command", &self.command)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "parameters", rename_all = "snake_case")]
pub enum WorkerCommandV2 {
    Ping,
    Status,
    Start {
        spec: Box<InstanceSpecV2>,
        run_id: Uuid,
        leases: Vec<LeaseV2>,
        capabilities_fingerprint: String,
    },
    Stop {
        mode: StopModeV2,
        graceful_timeout_ms: u32,
    },
    Pause,
    Resume,
    Reconfigure {
        display: DisplayConfigV2,
        adb: AdbConfigV2,
    },
    Action {
        action: InstanceActionV2,
    },
    InstallApk {
        upload_path: PathBuf,
        sha256: String,
    },
    AttachDisplay {
        session_id: Uuid,
        generation: u64,
        target: NativeDisplayTargetV2,
        viewport: DisplayViewportV2,
    },
    ResizeDisplay {
        session_id: Uuid,
        generation: u64,
        viewport: DisplayViewportV2,
    },
    DetachDisplay {
        session_id: Uuid,
        generation: u64,
    },
    CaptureScreenshot {
        output_path: PathBuf,
    },
    CollectGuestLogs,
    Diagnose,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerStatusV2 {
    pub identity: WorkerIdentityV2,
    pub observed: ObservedStateV2,
    pub run_id: Option<Uuid>,
    pub child_pid: Option<u32>,
    #[serde(default)]
    pub cleanup_pending: bool,
    pub adb_serial: Option<String>,
    pub frame_generation: u64,
    pub frame_metrics: FrameMetricsV2,
    pub last_error: Option<ApiErrorV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum WorkerPayloadV2 {
    Empty,
    Pong(WorkerStatusV2),
    Status(WorkerStatusV2),
    Diagnostics(Vec<DiagnosticCheckV2>),
    GuestLog(DiagnosticFileV2),
    Screenshot(ScreenshotRecordV2),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResponseV2 {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub ok: bool,
    pub payload: Option<WorkerPayloadV2>,
    pub error: Option<ApiErrorV2>,
}

impl WorkerResponseV2 {
    pub fn success(request_id: Uuid, payload: WorkerPayloadV2) -> Self {
        Self {
            protocol_version: WORKER_PROTOCOL_VERSION,
            request_id,
            ok: true,
            payload: Some(payload),
            error: None,
        }
    }

    pub fn failure(request_id: Uuid, error: ApiErrorV2) -> Self {
        Self {
            protocol_version: WORKER_PROTOCOL_VERSION,
            request_id,
            ok: false,
            payload: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
pub enum HostEventKindV2 {
    Instance(InstanceSummaryV2),
    Operation(OperationRecordV2),
    Capabilities(HostCapabilitiesV2),
    Lease(LeaseV2),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEventV2 {
    pub sequence: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub trace_id: Uuid,
    pub kind: HostEventKindV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownHostRequestV2 {
    pub stop_all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplaySessionV2 {
    pub id: Uuid,
    pub instance_id: Uuid,
    pub worker_endpoint: String,
    pub session_token: String,
    pub generation: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum NativeDisplayTargetV2 {
    WindowsHwnd { hwnd: u64, owner: WorkerIdentityV2 },
}

impl NativeDisplayTargetV2 {
    pub const fn owner(&self) -> &WorkerIdentityV2 {
        match self {
            Self::WindowsHwnd { owner, .. } => owner,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayViewportV2 {
    pub width_px: u32,
    pub height_px: u32,
    pub dpi: u32,
    pub visible: bool,
    pub revision: u64,
}

impl DisplayViewportV2 {
    pub const fn is_valid(&self) -> bool {
        self.width_px > 0
            && self.height_px > 0
            && self.width_px <= 16_384
            && self.height_px <= 16_384
            && self.dpi >= 72
            && self.dpi <= 960
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquireDisplaySessionRequestV2 {
    pub target: NativeDisplayTargetV2,
    pub viewport: DisplayViewportV2,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateDisplaySessionRequestV2 {
    pub session_token: String,
    pub viewport: DisplayViewportV2,
}

impl std::fmt::Debug for UpdateDisplaySessionRequestV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpdateDisplaySessionRequestV2")
            .field("session_token", &"[REDACTED]")
            .field("viewport", &self.viewport)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseDisplaySessionRequestV2 {
    pub session_token: String,
}

impl std::fmt::Debug for ReleaseDisplaySessionRequestV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReleaseDisplaySessionRequestV2")
            .field("session_token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotRecordV2 {
    pub instance_id: Uuid,
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRecordV2 {
    pub id: Uuid,
    pub file_name: String,
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRecordV2 {
    pub instance_id: Uuid,
    pub source_version: u32,
    pub target_version: u32,
    pub backup_path: PathBuf,
    pub notes: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub migrated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileReportV2 {
    pub desired: DesiredStateV2,
    pub observed_before: ObservedStateV2,
    pub observed_after: ObservedStateV2,
    pub worker_reconnected: bool,
    pub actions: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_wire_format_is_version_independent_data() {
        let operation = OperationRecordV2::queued(Some(Uuid::nil()), OperationKindV2::Start);
        let bytes = serde_json::to_vec(&operation).expect("encode");
        let decoded: OperationRecordV2 = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(operation, decoded);
    }

    #[test]
    fn required_blocked_probe_prevents_start() {
        let capabilities = HostCapabilitiesV2 {
            schema_version: 2,
            generated_at: OffsetDateTime::UNIX_EPOCH,
            platform: "test".to_owned(),
            architecture: "test".to_owned(),
            fingerprint: "fingerprint".to_owned(),
            certified: false,
            probes: vec![CapabilityProbeV2 {
                id: "zero_copy".to_owned(),
                status: CapabilityStatusV2::Blocked,
                required: true,
                detail: "missing".to_owned(),
                properties: BTreeMap::new(),
            }],
            devices: DeviceCapabilitiesV2 {
                profile: "phone".to_owned(),
                devices: Vec::new(),
            },
        };
        assert!(!capabilities.can_start());
    }
}
