//! Deterministic host-side device simulation state used by the signed HD device component.
//!
//! Bluetooth and NFC are intentionally outside this component: production routes those protocols
//! to signed `RootCanal` and `Casimir` binaries. This component owns the typed location, battery,
//! network and sensor control plane and never reports capabilities it does not implement.

use std::collections::BTreeMap;

use hd_core::{
    BatteryStateV2, COMPONENT_PROTOCOL_VERSION, FormalComponentProbeV2, LocationV2,
    NetworkConditionV2, SensorInjectionV2,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

pub const DEVICE_SIM_PROTOCOL_VERSION: u32 = COMPONENT_PROTOCOL_VERSION;
pub const MAX_DEVICE_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "parameters", rename_all = "snake_case")]
pub enum DeviceCommandV2 {
    Probe,
    Status,
    SetLocation { location: LocationV2 },
    SetBattery { battery: BatteryStateV2 },
    SetNetworkCondition { condition: NetworkConditionV2 },
    InjectSensor { injection: SensorInjectionV2 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRequestV2 {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub command: DeviceCommandV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStateV2 {
    pub revision: u64,
    pub location: LocationV2,
    pub battery: BatteryStateV2,
    pub network: NetworkConditionV2,
    pub sensors: BTreeMap<String, SensorInjectionV2>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl Default for DeviceStateV2 {
    fn default() -> Self {
        Self {
            revision: 0,
            location: LocationV2 {
                latitude_e7: 374_219_999,
                longitude_e7: -1_220_840_577,
                altitude_mm: 5_000,
                accuracy_mm: 5_000,
            },
            battery: BatteryStateV2 {
                level_percent: 100,
                charging: true,
                temperature_deci_celsius: 250,
            },
            network: NetworkConditionV2 {
                latency_ms: 0,
                loss_basis_points: 0,
                bandwidth_kbps: None,
            },
            sensors: BTreeMap::new(),
            updated_at: OffsetDateTime::now_utc(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum DevicePayloadV2 {
    Probe(FormalComponentProbeV2),
    State(DeviceStateV2),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceResponseV2 {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub ok: bool,
    pub payload: Option<DevicePayloadV2>,
    pub error_code: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceSimulatorV2 {
    state: DeviceStateV2,
}

impl DeviceSimulatorV2 {
    pub fn state(&self) -> &DeviceStateV2 {
        &self.state
    }

    pub fn handle(&mut self, request: DeviceRequestV2) -> DeviceResponseV2 {
        if request.protocol_version != DEVICE_SIM_PROTOCOL_VERSION {
            return failure(
                request.request_id,
                "device_protocol_version",
                format!("protocol {} is unsupported", request.protocol_version),
            );
        }
        match self.apply(request.command) {
            Ok(payload) => DeviceResponseV2 {
                protocol_version: DEVICE_SIM_PROTOCOL_VERSION,
                request_id: request.request_id,
                ok: true,
                payload: Some(payload),
                error_code: None,
                message: None,
            },
            Err(error) => failure(request.request_id, error.code(), error.to_string()),
        }
    }

    fn apply(&mut self, command: DeviceCommandV2) -> Result<DevicePayloadV2, DeviceSimError> {
        match command {
            DeviceCommandV2::Probe => Ok(DevicePayloadV2::Probe(probe())),
            DeviceCommandV2::Status => Ok(DevicePayloadV2::State(self.state.clone())),
            DeviceCommandV2::SetLocation { location } => {
                if !location.is_valid() {
                    return Err(DeviceSimError::InvalidLocation);
                }
                self.state.location = location;
                self.changed();
                Ok(DevicePayloadV2::State(self.state.clone()))
            }
            DeviceCommandV2::SetBattery { battery } => {
                if battery.level_percent > 100
                    || !(-500..=1_500).contains(&battery.temperature_deci_celsius)
                {
                    return Err(DeviceSimError::InvalidBattery);
                }
                self.state.battery = battery;
                self.changed();
                Ok(DevicePayloadV2::State(self.state.clone()))
            }
            DeviceCommandV2::SetNetworkCondition { condition } => {
                if condition.latency_ms > 60_000
                    || condition.loss_basis_points > 10_000
                    || condition.bandwidth_kbps == Some(0)
                {
                    return Err(DeviceSimError::InvalidNetwork);
                }
                self.state.network = condition;
                self.changed();
                Ok(DevicePayloadV2::State(self.state.clone()))
            }
            DeviceCommandV2::InjectSensor { injection } => {
                validate_sensor(&injection)?;
                self.state
                    .sensors
                    .insert(injection.sensor.clone(), injection);
                self.changed();
                Ok(DevicePayloadV2::State(self.state.clone()))
            }
        }
    }

    fn changed(&mut self) {
        self.state.revision = self.state.revision.saturating_add(1);
        self.state.updated_at = OffsetDateTime::now_utc();
    }
}

pub fn probe() -> FormalComponentProbeV2 {
    FormalComponentProbeV2 {
        protocol_version: DEVICE_SIM_PROTOCOL_VERSION,
        component: "hd-device-sim".to_owned(),
        formal: true,
        features: vec![
            "battery-v2".to_owned(),
            "location-v2".to_owned(),
            "network-condition-v2".to_owned(),
            "sensor-injection-v2".to_owned(),
        ],
    }
}

fn validate_sensor(injection: &SensorInjectionV2) -> Result<(), DeviceSimError> {
    const ALLOWED: [(&str, usize); 5] = [
        ("accelerometer", 3),
        ("gyroscope", 3),
        ("magnetometer", 3),
        ("light", 1),
        ("proximity", 1),
    ];
    let Some((_, values)) = ALLOWED.iter().find(|(name, _)| *name == injection.sensor) else {
        return Err(DeviceSimError::UnsupportedSensor(injection.sensor.clone()));
    };
    if injection.values_microunits.len() != *values
        || injection.duration_ms > 3_600_000
        || injection
            .values_microunits
            .iter()
            .any(|value| value.unsigned_abs() > 1_000_000_000_000)
    {
        return Err(DeviceSimError::InvalidSensor);
    }
    Ok(())
}

fn failure(request_id: Uuid, code: &str, message: String) -> DeviceResponseV2 {
    DeviceResponseV2 {
        protocol_version: DEVICE_SIM_PROTOCOL_VERSION,
        request_id,
        ok: false,
        payload: None,
        error_code: Some(code.to_owned()),
        message: Some(message),
    }
}

#[derive(Debug, Error)]
enum DeviceSimError {
    #[error("location is outside the supported geodetic range")]
    InvalidLocation,
    #[error("battery state is outside the supported range")]
    InvalidBattery,
    #[error("network condition is outside the supported range")]
    InvalidNetwork,
    #[error("sensor injection is outside the supported range")]
    InvalidSensor,
    #[error("sensor {0} is unsupported")]
    UnsupportedSensor(String),
}

impl DeviceSimError {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidLocation => "device_location_invalid",
            Self::InvalidBattery => "device_battery_invalid",
            Self::InvalidNetwork => "device_network_invalid",
            Self::InvalidSensor => "device_sensor_invalid",
            Self::UnsupportedSensor(_) => "device_sensor_unsupported",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_rejects_unversioned_requests() {
        let mut simulator = DeviceSimulatorV2::default();
        let response = simulator.handle(DeviceRequestV2 {
            protocol_version: 1,
            request_id: Uuid::nil(),
            command: DeviceCommandV2::Status,
        });
        assert!(!response.ok);
    }
}
