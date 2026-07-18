use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{DisplayConfig, InstanceConfigV1, InstanceState, StateSnapshot};

pub const CONTROL_PROTOCOL_VERSION: u32 = 1;
pub const GPU_STATS_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceAction {
    Home,
    Recent,
    Back,
    Power,
    VolumeUp,
    VolumeDown,
    Rotate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "arguments", rename_all = "snake_case")]
pub enum ControlCommandV1 {
    Ping,
    List,
    Show { id: Uuid },
    Create { config: InstanceConfigV1 },
    Update { config: InstanceConfigV1 },
    Start { id: Uuid, mock: bool },
    Stop { id: Uuid },
    Delete { id: Uuid },
    Action { id: Uuid, action: InstanceAction },
    InstallApk { id: Uuid, path: PathBuf },
    ApplyDisplay { id: Uuid, display: DisplayConfig },
    Diagnose { id: Uuid },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequestV1 {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub command: ControlCommandV1,
}

impl ControlRequestV1 {
    pub fn new(command: ControlCommandV1) -> Self {
        Self {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            command,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSummaryV1 {
    pub id: Uuid,
    pub name: String,
    pub state: StateSnapshot,
    pub adb_serial: Option<String>,
    pub host_fps_milli: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum ControlPayloadV1 {
    Pong,
    Empty,
    Instances(Vec<InstanceSummaryV1>),
    Instance(InstanceSummaryV1),
    Diagnosis(DiagnosisV1),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResponseV1 {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub ok: bool,
    pub payload: Option<ControlPayloadV1>,
    pub error: Option<ProtocolErrorV1>,
}

impl ControlResponseV1 {
    pub fn success(request_id: Uuid, payload: ControlPayloadV1) -> Self {
        Self {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            request_id,
            ok: true,
            payload: Some(payload),
            error: None,
        }
    }

    pub fn failure(request_id: Uuid, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            request_id,
            ok: false,
            payload: None,
            error: Some(ProtocolErrorV1 {
                code: code.into(),
                message: message.into(),
                retryable: false,
                details: BTreeMap::new(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolErrorV1 {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayLeaseV1 {
    pub lease_id: Uuid,
    pub platform: String,
    pub binding: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPlanV1 {
    pub schema_version: u32,
    pub instance_id: Uuid,
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: PathBuf,
    pub display_lease: Option<DisplayLeaseV1>,
    pub control_endpoint: String,
    pub gpu_stats_endpoint: String,
    pub adb_serial: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuStatsEventV1 {
    pub protocol_version: u32,
    pub instance_id: Uuid,
    pub monotonic_ns: u64,
    pub presented_frames: u64,
    pub interval_ns: u64,
    pub fps_milli: u32,
    pub present_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResultV1 {
    pub schema_version: u32,
    pub run_id: Uuid,
    pub instance_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub final_state: InstanceState,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosisV1 {
    pub instance_id: Uuid,
    pub checks: Vec<DiagnosticCheckV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticCheckV1 {
    pub name: String,
    pub status: DiagnosticStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStatus {
    Pass,
    Warn,
    Fail,
    Blocked,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_preserves_version_and_id() {
        let request = ControlRequestV1::new(ControlCommandV1::Ping);
        let bytes = serde_json::to_vec(&request).expect("encode");
        let decoded: ControlRequestV1 = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(request, decoded);
        assert_eq!(decoded.protocol_version, CONTROL_PROTOCOL_VERSION);
    }
}
