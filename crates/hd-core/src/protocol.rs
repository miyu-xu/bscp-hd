use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AdbConfigV2, DesiredStateV2, DisplayConfigV2, GuestKindV2, InstanceSpecV2, InstanceStatusV2,
    ObservedStateV2, OrientationV2,
};

pub const CONTROL_PROTOCOL_VERSION: u32 = 2;
pub const WORKER_PROTOCOL_VERSION: u32 = 2;
pub const FRAME_PROTOCOL_VERSION: u32 = 2;
pub const ARTIFACT_INDEX_VERSION: u32 = 2;
pub const DIAGNOSTIC_MANIFEST_VERSION: u32 = 2;
pub const HOST_CERTIFICATION_VERSION: u32 = 2;
pub const COMPONENT_PROTOCOL_VERSION: u32 = 2;
pub const MIN_TIMED_SENSOR_INJECTION_MS: u32 = 200;
pub const MAX_SENSOR_INJECTION_MS: u32 = 3_600_000;
pub const MIN_SENSOR_POSE_TRANSITION_MS: u32 = 200;
pub const MAX_SENSOR_POSE_TRANSITION_MS: u32 = 10_000;
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

pub const MAX_LOCATION_ROUTE_POINTS_V2: usize = 2_048;
pub const MIN_LOCATION_ROUTE_INTERVAL_MS_V2: u32 = 250;
pub const MAX_LOCATION_ROUTE_INTERVAL_MS_V2: u32 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocationRouteV2 {
    pub id: Uuid,
    pub name: String,
    pub points: Vec<LocationV2>,
    pub interval_ms: u32,
    pub repeat: bool,
}

impl LocationRouteV2 {
    pub fn is_valid(&self) -> bool {
        !self.id.is_nil()
            && !self.name.trim().is_empty()
            && self.name.len() <= 128
            && (2..=MAX_LOCATION_ROUTE_POINTS_V2).contains(&self.points.len())
            && (MIN_LOCATION_ROUTE_INTERVAL_MS_V2..=MAX_LOCATION_ROUTE_INTERVAL_MS_V2)
                .contains(&self.interval_ms)
            && self.points.iter().all(LocationV2::is_valid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationRoutePlaybackStateV2 {
    Playing,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocationRouteStatusV2 {
    pub id: Uuid,
    pub name: String,
    pub point_count: u32,
    pub current_point: u32,
    pub interval_ms: u32,
    pub repeat: bool,
    pub state: LocationRoutePlaybackStateV2,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationRouteFinishReasonV2 {
    Completed,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocationRouteRecordV2 {
    pub id: Uuid,
    pub name: String,
    pub point_count: u32,
    pub applied_points: u32,
    pub repeat: bool,
    pub reason: LocationRouteFinishReasonV2,
    pub error_code: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorPoseV2 {
    pub x_millidegrees: i32,
    pub y_millidegrees: i32,
    pub z_millidegrees: i32,
    pub transition_ms: u32,
}

impl Default for SensorPoseV2 {
    fn default() -> Self {
        Self {
            x_millidegrees: 0,
            y_millidegrees: 0,
            z_millidegrees: 0,
            transition_ms: MIN_SENSOR_POSE_TRANSITION_MS,
        }
    }
}

impl SensorPoseV2 {
    pub fn is_valid(self) -> bool {
        [
            self.x_millidegrees,
            self.y_millidegrees,
            self.z_millidegrees,
        ]
        .into_iter()
        .all(|angle| (-180_000..=180_000).contains(&angle))
            && (MIN_SENSOR_POSE_TRANSITION_MS..=MAX_SENSOR_POSE_TRANSITION_MS)
                .contains(&self.transition_ms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorMotionFrameV2 {
    pub accelerometer_microunits: [i64; 3],
    pub magnetometer_microunits: [i64; 3],
    pub gyroscope_microunits: [i64; 3],
}

pub fn sensor_motion_frame(previous: SensorPoseV2, current: SensorPoseV2) -> SensorMotionFrameV2 {
    let previous_rotation = sensor_rotation_matrix(previous);
    let current_rotation = sensor_rotation_matrix(current);
    let acceleration = matrix_vector_product(current_rotation, [0.0, 9.806_65, 0.0]);
    let magnetic = matrix_vector_product(current_rotation, [0.0, 5.9, -48.4]);
    let transition = matrix_product(previous_rotation, matrix_transpose(current_rotation));
    let rotation_vector = rotation_vector(transition);
    let seconds = f64::from(current.transition_ms) / 1_000.0;
    let gyroscope = rotation_vector.map(|value| value / seconds);
    SensorMotionFrameV2 {
        accelerometer_microunits: acceleration.map(to_microunits),
        magnetometer_microunits: magnetic.map(to_microunits),
        gyroscope_microunits: gyroscope.map(to_microunits),
    }
}

fn sensor_rotation_matrix(pose: SensorPoseV2) -> [[f64; 3]; 3] {
    let radians = |millidegrees: i32| -f64::from(millidegrees) * std::f64::consts::PI / 180_000.0;
    let (sin_x, cos_x) = radians(pose.x_millidegrees).sin_cos();
    let (sin_y, cos_y) = radians(pose.y_millidegrees).sin_cos();
    let (sin_z, cos_z) = radians(pose.z_millidegrees).sin_cos();
    let rotation_x = [[1.0, 0.0, 0.0], [0.0, cos_x, -sin_x], [0.0, sin_x, cos_x]];
    let rotation_y = [[cos_y, 0.0, sin_y], [0.0, 1.0, 0.0], [-sin_y, 0.0, cos_y]];
    let rotation_z = [[cos_z, -sin_z, 0.0], [sin_z, cos_z, 0.0], [0.0, 0.0, 1.0]];
    matrix_product(rotation_z, matrix_product(rotation_y, rotation_x))
}

fn matrix_product(left: [[f64; 3]; 3], right: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    std::array::from_fn(|row| {
        std::array::from_fn(|column| {
            (0..3)
                .map(|index| left[row][index] * right[index][column])
                .sum()
        })
    })
}

fn matrix_vector_product(matrix: [[f64; 3]; 3], vector: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|row| {
        (0..3)
            .map(|column| matrix[row][column] * vector[column])
            .sum()
    })
}

fn matrix_transpose(matrix: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    std::array::from_fn(|row| std::array::from_fn(|column| matrix[column][row]))
}

fn rotation_vector(matrix: [[f64; 3]; 3]) -> [f64; 3] {
    let trace = matrix[0][0] + matrix[1][1] + matrix[2][2];
    let mut quaternion = if trace > 0.0 {
        let scale = (trace + 1.0).sqrt() * 2.0;
        [
            0.25 * scale,
            (matrix[2][1] - matrix[1][2]) / scale,
            (matrix[0][2] - matrix[2][0]) / scale,
            (matrix[1][0] - matrix[0][1]) / scale,
        ]
    } else if matrix[0][0] > matrix[1][1] && matrix[0][0] > matrix[2][2] {
        let scale = (1.0 + matrix[0][0] - matrix[1][1] - matrix[2][2]).sqrt() * 2.0;
        [
            (matrix[2][1] - matrix[1][2]) / scale,
            0.25 * scale,
            (matrix[0][1] + matrix[1][0]) / scale,
            (matrix[0][2] + matrix[2][0]) / scale,
        ]
    } else if matrix[1][1] > matrix[2][2] {
        let scale = (1.0 + matrix[1][1] - matrix[0][0] - matrix[2][2]).sqrt() * 2.0;
        [
            (matrix[0][2] - matrix[2][0]) / scale,
            (matrix[0][1] + matrix[1][0]) / scale,
            0.25 * scale,
            (matrix[1][2] + matrix[2][1]) / scale,
        ]
    } else {
        let scale = (1.0 + matrix[2][2] - matrix[0][0] - matrix[1][1]).sqrt() * 2.0;
        [
            (matrix[1][0] - matrix[0][1]) / scale,
            (matrix[0][2] + matrix[2][0]) / scale,
            (matrix[1][2] + matrix[2][1]) / scale,
            0.25 * scale,
        ]
    };
    let norm = quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    for value in &mut quaternion {
        *value /= norm;
    }
    let should_flip = quaternion[0] < 0.0
        || (quaternion[0].abs() <= f64::EPSILON
            && quaternion[1..]
                .iter()
                .find(|value| value.abs() > f64::EPSILON)
                .is_some_and(|value| *value < 0.0));
    if should_flip {
        for value in &mut quaternion {
            *value = -*value;
        }
    }
    let vector_norm = quaternion[1..]
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    if vector_norm <= 1.0e-12 {
        return [0.0; 3];
    }
    let angle = 2.0 * vector_norm.atan2(quaternion[0].clamp(-1.0, 1.0));
    std::array::from_fn(|index| quaternion[index + 1] * angle / vector_norm)
}

#[allow(clippy::cast_possible_truncation)]
fn to_microunits(value: f64) -> i64 {
    (value * 1_000_000.0).round() as i64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UwbRangingV2 {
    pub distance_cm: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModemStateV2 {
    pub operator_numeric: String,
    pub operator_long_name: String,
    pub operator_short_name: String,
    pub signal_strength: u8,
    pub registered: bool,
}

impl Default for ModemStateV2 {
    fn default() -> Self {
        Self {
            operator_numeric: "00101".to_owned(),
            operator_long_name: "HD Mobile".to_owned(),
            operator_short_name: "HD".to_owned(),
            signal_strength: 20,
            registered: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BluetoothAdvertisementFrameV2 {
    pub advertising_data_hex: String,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum BluetoothPeerActionV2 {
    CreateGattPeer {
        peer_id: Uuid,
        name: String,
    },
    CreateBeacon {
        peer_id: Uuid,
        name: String,
        advertising_data_hex: String,
    },
    CreateScriptedBeacon {
        peer_id: Uuid,
        name: String,
        frames: Vec<BluetoothAdvertisementFrameV2>,
        repeat: bool,
    },
    CreateHidKeyboard {
        peer_id: Uuid,
        name: String,
    },
    SendHidKeyboardReport {
        peer_id: Uuid,
        modifiers: u8,
        keys: Vec<u8>,
    },
    RemovePeer {
        peer_id: Uuid,
    },
    SetAdvertising {
        peer_id: Uuid,
        enabled: bool,
    },
    CaptureHci {
        capture_id: Uuid,
        duration_ms: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BluetoothPeerKindV2 {
    Gatt,
    Beacon,
    ScriptedBeacon,
    HidKeyboard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BluetoothPeerStateV2 {
    pub peer_id: Uuid,
    pub name: String,
    pub kind: BluetoothPeerKindV2,
    pub advertising: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripted_frame_count: Option<u16>,
    #[serde(default)]
    pub repeat: bool,
    #[serde(default)]
    pub keyboard_reports_sent: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BluetoothHciCaptureRecordV2 {
    pub capture_id: Uuid,
    pub file_name: String,
    pub requested_duration_ms: u32,
    pub packets_captured: u64,
    pub packets_dropped: u64,
    pub output_size_bytes: u64,
    pub truncated: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum NfcTagActionV2 {
    PresentType2 { ndef_hex: String },
    PresentType4 { ndef_hex: String },
    Remove,
}

pub const TRACKPAD_AXIS_MAX: u16 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackpadPhaseV2 {
    Down,
    Move,
    Up,
}

/// One normalized event from HD's independent indirect pointing surface.
///
/// Coordinates use a fixed range so the UI layout and the Guest's configured trackpad dimensions
/// remain independent. The Worker maps this range to the virtio-input absolute axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackpadEventV2 {
    pub phase: TrackpadPhaseV2,
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "parameters", rename_all = "snake_case")]
pub enum InstanceActionV2 {
    Key { key: KeyActionV2 },
    Rotate { orientation: OrientationV2 },
    SetLocation { location: LocationV2 },
    StartLocationRoute { route: LocationRouteV2 },
    PauseLocationRoute,
    ResumeLocationRoute,
    StopLocationRoute,
    SetBattery { battery: BatteryStateV2 },
    SetNetworkCondition { condition: NetworkConditionV2 },
    InjectSensor { injection: SensorInjectionV2 },
    SetSensorPose { pose: SensorPoseV2 },
    BluetoothPeer { action: BluetoothPeerActionV2 },
    NfcTag { action: NfcTagActionV2 },
    SetUwbRanging { ranging: UwbRangingV2 },
    SetModemState { modem: ModemStateV2 },
    Trackpad { event: TrackpadEventV2 },
    MicrodroidConsoleChallenge { challenge_id: Uuid, confirmed: bool },
}

impl InstanceActionV2 {
    pub fn validate(&self) -> Result<(), ActionValidationError> {
        match self {
            Self::Key { .. }
            | Self::Rotate { .. }
            | Self::PauseLocationRoute
            | Self::ResumeLocationRoute
            | Self::StopLocationRoute
            | Self::NfcTag {
                action: NfcTagActionV2::Remove,
            } => Ok(()),
            Self::SetLocation { location } if location.is_valid() => Ok(()),
            Self::SetLocation { .. } => Err(ActionValidationError::Location),
            Self::StartLocationRoute { route } if route.is_valid() => Ok(()),
            Self::StartLocationRoute { .. } => Err(ActionValidationError::LocationRoute),
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
            Self::SetSensorPose { pose } if pose.is_valid() => Ok(()),
            Self::SetSensorPose { .. } => Err(ActionValidationError::SensorPose),
            Self::BluetoothPeer { action } => validate_bluetooth_action(action),
            Self::SetUwbRanging { ranging } if ranging.distance_cm > 0 => Ok(()),
            Self::SetUwbRanging { .. } => Err(ActionValidationError::Uwb),
            Self::SetModemState { modem } if modem_state_is_valid(modem) => Ok(()),
            Self::SetModemState { .. } => Err(ActionValidationError::Modem),
            Self::Trackpad { event }
                if event.x <= TRACKPAD_AXIS_MAX && event.y <= TRACKPAD_AXIS_MAX =>
            {
                Ok(())
            }
            Self::Trackpad { .. } => Err(ActionValidationError::Trackpad),
            Self::MicrodroidConsoleChallenge {
                challenge_id,
                confirmed: true,
            } if !challenge_id.is_nil() => Ok(()),
            Self::MicrodroidConsoleChallenge { .. } => {
                Err(ActionValidationError::MicrodroidConsoleChallenge)
            }
            Self::NfcTag {
                action:
                    NfcTagActionV2::PresentType2 { ndef_hex }
                    | NfcTagActionV2::PresentType4 { ndef_hex },
            } => validate_ndef_hex(ndef_hex),
        }
    }
}

fn modem_state_is_valid(modem: &ModemStateV2) -> bool {
    let valid_name = |name: &str, max_len: usize| {
        !name.trim().is_empty()
            && name.len() <= max_len
            && !name
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '"'))
    };
    matches!(modem.operator_numeric.len(), 5 | 6)
        && modem
            .operator_numeric
            .bytes()
            .all(|byte| byte.is_ascii_digit())
        && valid_name(&modem.operator_long_name, 32)
        && valid_name(&modem.operator_short_name, 16)
        && modem.signal_strength <= 31
}

fn validate_sensor_injection(injection: &SensorInjectionV2) -> Result<(), ActionValidationError> {
    let expected_values = match injection.sensor.as_str() {
        "accelerometer" | "gyroscope" | "magnetometer" => 3,
        "light" | "proximity" => 1,
        _ => return Err(ActionValidationError::Sensor),
    };
    let duration_is_valid = injection.duration_ms == 0
        || (MIN_TIMED_SENSOR_INJECTION_MS..=MAX_SENSOR_INJECTION_MS)
            .contains(&injection.duration_ms);
    if injection.values_microunits.len() == expected_values
        && duration_is_valid
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
        BluetoothPeerActionV2::CreateGattPeer { peer_id, name }
        | BluetoothPeerActionV2::CreateHidKeyboard { peer_id, name } => {
            !peer_id.is_nil() && !name.trim().is_empty() && name.len() <= 128
        }
        BluetoothPeerActionV2::CreateBeacon {
            peer_id,
            name,
            advertising_data_hex,
        } => {
            !peer_id.is_nil()
                && !name.trim().is_empty()
                && name.len() <= 128
                && valid_ble_advertising_data(advertising_data_hex)
        }
        BluetoothPeerActionV2::CreateScriptedBeacon {
            peer_id,
            name,
            frames,
            ..
        } => {
            !peer_id.is_nil()
                && !name.trim().is_empty()
                && name.len() <= 128
                && !frames.is_empty()
                && frames.len() <= 64
                && frames.iter().all(|frame| {
                    (20..=60_000).contains(&frame.duration_ms)
                        && valid_ble_advertising_data(&frame.advertising_data_hex)
                })
                && frames
                    .iter()
                    .map(|frame| u64::from(frame.duration_ms))
                    .sum::<u64>()
                    <= 600_000
        }
        BluetoothPeerActionV2::SendHidKeyboardReport { peer_id, keys, .. } => {
            !peer_id.is_nil()
                && keys.len() <= 6
                && keys
                    .iter()
                    .enumerate()
                    .all(|(index, key)| (4..=231).contains(key) && !keys[..index].contains(key))
        }
        BluetoothPeerActionV2::RemovePeer { peer_id }
        | BluetoothPeerActionV2::SetAdvertising { peer_id, .. } => !peer_id.is_nil(),
        BluetoothPeerActionV2::CaptureHci {
            capture_id,
            duration_ms,
        } => !capture_id.is_nil() && (1_000..=30_000).contains(duration_ms),
    };
    if valid {
        Ok(())
    } else {
        Err(ActionValidationError::Bluetooth)
    }
}

fn valid_ble_advertising_data(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 62
        || !value.len().is_multiple_of(2)
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    let Some(bytes) = value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            Some((digit(pair[0])? << 4) | digit(pair[1])?)
        })
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let mut offset = 0;
    while offset < bytes.len() {
        let field_len = usize::from(bytes[offset]);
        if field_len == 0 || offset + field_len + 1 > bytes.len() {
            return false;
        }
        offset += field_len + 1;
    }
    offset == bytes.len()
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
    #[error("location route is outside the supported playback contract")]
    LocationRoute,
    #[error("battery state is outside the supported range")]
    Battery,
    #[error("network condition is outside the supported range")]
    Network,
    #[error("trackpad coordinates are outside the normalized input range")]
    Trackpad,
    #[error("sensor injection is outside the supported range")]
    Sensor,
    #[error("sensor pose is outside the supported angle or transition range")]
    SensorPose,
    #[error("Bluetooth peer action is outside the supported contract")]
    Bluetooth,
    #[error("NFC NDEF payload is outside the supported contract")]
    Nfc,
    #[error("UWB ranging distance is outside the supported contract")]
    Uwb,
    #[error("modem state is outside the supported contract")]
    Modem,
    #[error("Microdroid console challenge requires explicit confirmation and a non-nil id")]
    MicrodroidConsoleChallenge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopModeV2 {
    Graceful,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerwashBackupReasonV2 {
    Powerwash,
    RestoreRollback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerwashBackupV2 {
    pub id: Uuid,
    pub reason: PowerwashBackupReasonV2,
    pub source_revision: u64,
    pub size_bytes: u64,
    pub sha256: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transaction", content = "parameters", rename_all = "snake_case")]
pub enum InstanceStorageTransactionV2 {
    Powerwash {
        operation_id: Uuid,
        backup: PowerwashBackupV2,
    },
    RestorePowerwash {
        operation_id: Uuid,
        source: PowerwashBackupV2,
        rollback: Option<PowerwashBackupV2>,
    },
    DiscardPowerwashBackup {
        operation_id: Uuid,
        backup: PowerwashBackupV2,
    },
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
    Powerwash {
        expected_revision: u64,
        confirmation_name: String,
    },
    RestorePowerwash {
        backup_id: Uuid,
        expected_revision: u64,
        confirmation_name: String,
    },
    DiscardPowerwashBackup {
        backup_id: Uuid,
        expected_revision: u64,
        confirmation_name: String,
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
    #[serde(default)]
    pub adb_ready: bool,
    pub host_fps_milli: Option<u32>,
    pub frame_generation: u64,
    #[serde(default)]
    pub runtime_displays: Vec<RuntimeDisplayV2>,
    #[serde(default)]
    pub screen_recording: Option<ScreenRecordingStatusV2>,
    #[serde(default)]
    pub last_screen_recording: Option<ScreenRecordingRecordV2>,
    #[serde(default)]
    pub location_route: Option<LocationRouteStatusV2>,
    #[serde(default)]
    pub last_location_route: Option<LocationRouteRecordV2>,
    #[serde(default)]
    pub uwb_ranging: Option<UwbRangingV2>,
    #[serde(default)]
    pub modem_state: Option<ModemStateV2>,
    #[serde(default)]
    pub sensor_pose: Option<SensorPoseV2>,
    #[serde(default)]
    pub powerwash_backup: Option<PowerwashBackupV2>,
    #[serde(default)]
    pub storage_transaction: Option<InstanceStorageTransactionV2>,
    #[serde(default)]
    pub bluetooth_peers: Vec<BluetoothPeerStateV2>,
    #[serde(default)]
    pub last_bluetooth_hci_capture: Option<BluetoothHciCaptureRecordV2>,
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
            adb_ready: false,
            host_fps_milli: None,
            frame_generation: 0,
            runtime_displays: Vec::new(),
            screen_recording: None,
            last_screen_recording: None,
            location_route: None,
            last_location_route: None,
            uwb_ranging: None,
            modem_state: None,
            sensor_pose: None,
            powerwash_backup: None,
            storage_transaction: None,
            bluetooth_peers: Vec::new(),
            last_bluetooth_hci_capture: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSummaryV2 {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub guest_kind: GuestKindV2,
    pub status: InstanceStatusV2,
    pub active_run_id: Option<Uuid>,
    pub worker: Option<WorkerIdentityV2>,
    pub adb_serial: Option<String>,
    #[serde(default)]
    pub adb_ready: bool,
    pub host_fps_milli: Option<u32>,
    pub frame_generation: u64,
    #[serde(default)]
    pub screen_recording: Option<ScreenRecordingStatusV2>,
    #[serde(default)]
    pub last_screen_recording: Option<ScreenRecordingRecordV2>,
}

impl From<&InstanceRecordV2> for InstanceSummaryV2 {
    fn from(record: &InstanceRecordV2) -> Self {
        Self {
            id: record.spec.id,
            name: record.spec.name.clone(),
            guest_kind: record.spec.guest_kind,
            status: record.status.clone(),
            active_run_id: record.active_run_id,
            worker: record.worker.clone(),
            adb_serial: record.adb_serial.clone(),
            adb_ready: record.adb_ready,
            host_fps_milli: record.host_fps_milli,
            frame_generation: record.frame_generation,
            screen_recording: record.screen_recording.clone(),
            last_screen_recording: record.last_screen_recording.clone(),
        }
    }
}

/// One internally consistent UI read model produced from a single persistent-store enumeration.
///
/// Keeping the selected record beside the summaries avoids the torn list/detail combinations that
/// are possible when presentation clients issue two independent requests while lifecycle state is
/// changing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSnapshotV2 {
    pub summaries: Vec<InstanceSummaryV2>,
    pub selected: Option<InstanceRecordV2>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_sha256: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_sha256: Option<String>,
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
            .field("executable_sha256", &self.executable_sha256)
            .field("started_at", &self.started_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostStartupFailureV1 {
    pub schema_version: u32,
    pub startup_attempt_id: Uuid,
    pub code: String,
    pub message: String,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
}

impl HostStartupFailureV1 {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err("unsupported Host startup failure schema".to_owned());
        }
        if self.startup_attempt_id.is_nil() {
            return Err("Host startup failure has a nil attempt id".to_owned());
        }
        if self.code.is_empty()
            || self.code.len() > 64
            || !self
                .code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err("Host startup failure has an invalid code".to_owned());
        }
        if self.message.trim().is_empty() || self.message.len() > 4096 {
            return Err("Host startup failure has an invalid message".to_owned());
        }
        Ok(())
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
pub struct DeviceRuntimeStateV2 {
    #[serde(default)]
    pub probed: bool,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub configured: bool,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub controllable: bool,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub detail: String,
}

impl Default for DeviceRuntimeStateV2 {
    fn default() -> Self {
        Self {
            probed: false,
            installed: false,
            configured: false,
            running: false,
            controllable: false,
            verified: false,
            detail: "runtime state has not been probed".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapabilityV2 {
    pub id: String,
    pub backend: DeviceBackendKindV2,
    pub available: bool,
    pub boundary: String,
    pub features: Vec<String>,
    #[serde(default)]
    pub runtime: DeviceRuntimeStateV2,
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
    #[serde(default)]
    pub verified: bool,
    pub certified: bool,
    #[serde(default)]
    pub development_bypass: bool,
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
    /// Absent from legacy Android certificates so their signed payload remains byte-for-byte
    /// compatible. Microdroid certificates serialize this discriminator explicitly.
    #[serde(default, skip_serializing_if = "GuestKindV2::is_android")]
    pub guest_kind: GuestKindV2,
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
        (self.certified || self.development_bypass)
            && self.probes.iter().all(|probe| {
                !probe.required || matches!(probe.status, CapabilityStatusV2::Supported)
            })
        // Device availability is authorized by the required, instance-aware `device.profile`
        // probe.  The device list intentionally still describes disabled or unavailable optional
        // devices for the UI, but it has no copy of the selected instance switches and therefore
        // cannot safely act as a second launch gate.
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
    #[serde(default)]
    pub host_present_latency_ns_total: u64,
    #[serde(default)]
    pub host_present_latency_ns_max: u64,
    #[serde(default)]
    pub host_present_over_16ms: u64,
    #[serde(default)]
    pub host_present_over_33ms: u64,
    #[serde(default)]
    pub cadence_probe_epoch: u64,
    #[serde(default)]
    pub cadence_probe_frames: u64,
    #[serde(default)]
    pub cadence_probe_interval_ns_total: u64,
    #[serde(default)]
    pub cadence_probe_interval_ns_max: u64,
    #[serde(default)]
    pub cadence_probe_over_33ms: u64,
    #[serde(default)]
    pub cadence_probe_over_50ms: u64,
    #[serde(default)]
    pub cadence_probe_over_100ms: u64,
    #[serde(default)]
    pub cadence_probe_host_present_latency_ns_max: u64,
    #[serde(default)]
    pub cadence_probe_source_intervals: u64,
    #[serde(default)]
    pub cadence_probe_source_interval_ns_total: u64,
    #[serde(default)]
    pub cadence_probe_source_interval_ns_max: u64,
    #[serde(default)]
    pub cadence_probe_source_over_33ms: u64,
    #[serde(default)]
    pub cadence_probe_source_over_50ms: u64,
    #[serde(default)]
    pub cadence_probe_source_over_100ms: u64,
    #[serde(default)]
    pub cadence_probe_post_worker_queue_delay_ns_max: u64,
    #[serde(default)]
    pub cadence_probe_post_worker_queue_over_16ms: u64,
    #[serde(default)]
    pub cadence_probe_post_worker_queue_over_33ms: u64,
    #[serde(default)]
    pub cadence_probe_post_worker_work_ns_max: u64,
    #[serde(default)]
    pub cadence_probe_post_worker_work_over_16ms: u64,
    #[serde(default)]
    pub cadence_probe_post_worker_work_over_33ms: u64,
    #[serde(default)]
    pub cadence_probe_swapchain_recreate_count: u64,
    #[serde(default)]
    pub cadence_probe_swapchain_failure_count: u64,
    #[serde(default)]
    pub cadence_probe_swapchain_out_of_date_count: u64,
    #[serde(default)]
    pub cadence_probe_aspect_mismatch_count: u64,
    #[serde(default)]
    pub cadence_probe_source_extent_change_count: u64,
    #[serde(default)]
    pub source_width: u32,
    #[serde(default)]
    pub source_height: u32,
    #[serde(default)]
    pub swapchain_width: u32,
    #[serde(default)]
    pub swapchain_height: u32,
    #[serde(default)]
    pub host_available_memory_bytes: u64,
    #[serde(default)]
    pub host_memory_load_percent: u32,
    #[serde(default)]
    pub swapchain_recreate_count: u64,
    #[serde(default)]
    pub swapchain_recreate_failure_count: u64,
    #[serde(default)]
    pub swapchain_out_of_date_count: u64,
    #[serde(default)]
    pub present_mode: String,
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

pub const PACKAGED_ANDROID_ARTIFACT_STORE_VERSION: u32 = 2;
pub const PACKAGED_ANDROID_ARTIFACT_INDEX_FILE: &str = "index-v2.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackagedArtifactChannelV2 {
    Development,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackagedAndroidArtifactStoreV2 {
    pub schema_version: u32,
    pub channel: PackagedArtifactChannelV2,
    pub guest_kind: GuestKindV2,
    pub android_version: String,
    pub data_profile: String,
    pub guest_bundle_digest: String,
    pub host_bundle_digest: String,
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
    /// Optional raw virtio-input endpoint for the independent indirect pointing surface.
    #[serde(default)]
    pub trackpad_endpoint: Option<String>,
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
        #[serde(default)]
        initial_display: Option<PreparedNativeDisplayV2>,
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
        display_id: DisplayIdV2,
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
    CollectAndroidBugreport {
        bugreport_id: Uuid,
        output_path: PathBuf,
    },
    StartScreenRecording {
        recording_id: Uuid,
        #[serde(default)]
        display_id: DisplayIdV2,
        output_path: PathBuf,
        max_duration_seconds: u16,
    },
    StopScreenRecording {
        recording_id: Uuid,
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
    #[serde(default)]
    pub adb_ready: bool,
    pub frame_generation: u64,
    pub frame_metrics: FrameMetricsV2,
    #[serde(default)]
    pub runtime_displays: Vec<RuntimeDisplayV2>,
    #[serde(default)]
    pub screen_recording: Option<ScreenRecordingStatusV2>,
    #[serde(default)]
    pub last_screen_recording: Option<ScreenRecordingRecordV2>,
    #[serde(default)]
    pub location_route: Option<LocationRouteStatusV2>,
    #[serde(default)]
    pub last_location_route: Option<LocationRouteRecordV2>,
    #[serde(default)]
    pub uwb_ranging: Option<UwbRangingV2>,
    #[serde(default)]
    pub modem_state: Option<ModemStateV2>,
    #[serde(default)]
    pub sensor_pose: Option<SensorPoseV2>,
    #[serde(default)]
    pub bluetooth_peers: Vec<BluetoothPeerStateV2>,
    #[serde(default)]
    pub last_bluetooth_hci_capture: Option<BluetoothHciCaptureRecordV2>,
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
    AndroidBugreport(AndroidBugreportRecordV2),
    Screenshot(ScreenshotRecordV2),
    ScreenRecordingStatus(ScreenRecordingStatusV2),
    ScreenRecording(ScreenRecordingRecordV2),
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
    #[serde(default)]
    pub display_id: DisplayIdV2,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum NativeDisplayTargetV2 {
    WindowsHwnd {
        hwnd: u64,
        #[serde(default)]
        scanout_id: u32,
        owner: WorkerIdentityV2,
    },
    MacCaContext {
        endpoint: String,
        #[serde(default)]
        scanout_id: u32,
        owner: WorkerIdentityV2,
    },
}

impl NativeDisplayTargetV2 {
    pub const fn owner(&self) -> &WorkerIdentityV2 {
        match self {
            Self::WindowsHwnd { owner, .. } | Self::MacCaContext { owner, .. } => owner,
        }
    }

    pub const fn scanout_id(&self) -> u32 {
        match self {
            Self::WindowsHwnd { scanout_id, .. } | Self::MacCaContext { scanout_id, .. } => {
                *scanout_id
            }
        }
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DisplayIdV2 {
    #[default]
    Primary,
    Secondary {
        id: Uuid,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDisplayV2 {
    pub display_id: DisplayIdV2,
    pub scanout_id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub refresh_rate_hz: u16,
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedNativeDisplayV2 {
    pub session_id: Uuid,
    #[serde(default)]
    pub display_id: DisplayIdV2,
    pub target: NativeDisplayTargetV2,
    pub viewport: DisplayViewportV2,
}

impl std::fmt::Debug for PreparedNativeDisplayV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedNativeDisplayV2")
            .field("session_id", &self.session_id)
            .field("display_id", &self.display_id)
            .field("target", &"[NON_PERSISTENT_NATIVE_HANDLE]")
            .field("viewport", &self.viewport)
            .finish()
    }
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
    #[serde(default)]
    pub display_id: DisplayIdV2,
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
pub struct AndroidBugreportRecordV2 {
    pub id: Uuid,
    pub instance_id: Uuid,
    pub run_id: Uuid,
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub contains_sensitive_data: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartScreenRecordingRequestV2 {
    #[serde(default)]
    pub display_id: DisplayIdV2,
    pub max_duration_seconds: u16,
}

impl StartScreenRecordingRequestV2 {
    pub const MIN_DURATION_SECONDS: u16 = 1;
    pub const MAX_DURATION_SECONDS: u16 = 180;

    pub const fn is_valid(&self) -> bool {
        self.max_duration_seconds >= Self::MIN_DURATION_SECONDS
            && self.max_duration_seconds <= Self::MAX_DURATION_SECONDS
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRecordingStatusV2 {
    pub id: Uuid,
    pub instance_id: Uuid,
    #[serde(default)]
    pub display_id: DisplayIdV2,
    pub max_duration_seconds: u16,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRecordingRecordV2 {
    pub id: Uuid,
    pub instance_id: Uuid,
    #[serde(default)]
    pub display_id: DisplayIdV2,
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    pub duration_millis: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
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
    fn host_startup_failure_requires_attempt_scoped_safe_data() {
        let mut failure = HostStartupFailureV1 {
            schema_version: HostStartupFailureV1::SCHEMA_VERSION,
            startup_attempt_id: Uuid::new_v4(),
            code: "release_materials_invalid".to_owned(),
            message: "trust root does not match".to_owned(),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(failure.validate(), Ok(()));

        failure.code = "Invalid-Code".to_owned();
        assert!(failure.validate().is_err());
        failure.code = "release_materials_invalid".to_owned();
        failure.startup_attempt_id = Uuid::nil();
        assert!(failure.validate().is_err());
    }

    #[test]
    fn required_blocked_probe_prevents_start() {
        let capabilities = HostCapabilitiesV2 {
            schema_version: 2,
            generated_at: OffsetDateTime::UNIX_EPOCH,
            platform: "test".to_owned(),
            architecture: "test".to_owned(),
            fingerprint: "fingerprint".to_owned(),
            verified: false,
            certified: false,
            development_bypass: false,
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

    #[test]
    fn development_bypass_allows_start_without_claiming_certification() {
        let capabilities = HostCapabilitiesV2 {
            schema_version: 2,
            generated_at: OffsetDateTime::UNIX_EPOCH,
            platform: "test".to_owned(),
            architecture: "test".to_owned(),
            fingerprint: "fingerprint".to_owned(),
            verified: false,
            certified: false,
            development_bypass: true,
            probes: vec![
                CapabilityProbeV2 {
                    id: "runtime".to_owned(),
                    status: CapabilityStatusV2::Supported,
                    required: true,
                    detail: "available".to_owned(),
                    properties: BTreeMap::new(),
                },
                CapabilityProbeV2 {
                    id: "release.certification".to_owned(),
                    status: CapabilityStatusV2::Blocked,
                    required: false,
                    detail: "development bypass".to_owned(),
                    properties: BTreeMap::new(),
                },
            ],
            devices: DeviceCapabilitiesV2 {
                profile: "phone".to_owned(),
                devices: Vec::new(),
            },
        };
        assert!(capabilities.can_start());
        assert!(!capabilities.certified);
        assert!(!capabilities.verified);
    }

    #[test]
    fn certification_guest_kind_preserves_legacy_android_signatures() {
        let certification = HostCertificationV2 {
            schema_version: HOST_CERTIFICATION_VERSION,
            certification_id: Uuid::nil(),
            platform: "macos".to_owned(),
            architecture: "aarch64".to_owned(),
            guest_kind: GuestKindV2::Android,
            capability_fingerprint: "00".repeat(32),
            guest_bundle_digest: "11".repeat(32),
            host_bundle_digest: "22".repeat(32),
            device_profile: "hd-phone-android15-v2".to_owned(),
            control_protocol_version: CONTROL_PROTOCOL_VERSION,
            frame_protocol_version: FRAME_PROTOCOL_VERSION,
            issued_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: OffsetDateTime::UNIX_EPOCH + time::Duration::days(1),
            evidence_sha256: BTreeMap::new(),
            signer_key_id: "release".to_owned(),
            signature_ed25519: String::new(),
        };
        let android = serde_json::to_value(&certification).expect("encode Android certification");
        assert!(android.get("guest_kind").is_none());
        let decoded: HostCertificationV2 =
            serde_json::from_value(android).expect("decode legacy Android certification");
        assert_eq!(decoded.guest_kind, GuestKindV2::Android);

        let mut microdroid = certification;
        microdroid.guest_kind = GuestKindV2::Microdroid;
        let microdroid =
            serde_json::to_value(&microdroid).expect("encode Microdroid certification");
        assert_eq!(microdroid["guest_kind"], "microdroid");
    }
}
