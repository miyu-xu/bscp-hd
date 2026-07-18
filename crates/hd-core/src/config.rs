use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const INSTANCE_CONFIG_VERSION: u32 = 1;
pub const DEFAULT_CPU_COUNT: u16 = 4;
pub const DEFAULT_MEMORY_MIB: u32 = 8192;
pub const DEFAULT_WIDTH: u32 = 1280;
pub const DEFAULT_HEIGHT: u32 = 720;
pub const DEFAULT_DPI: u32 = 320;
pub const DEFAULT_REFRESH_RATE_HZ: u32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    Landscape,
    Portrait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VsyncMode {
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayConfig {
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub refresh_rate_hz: u32,
    pub orientation: Orientation,
    pub vsync: VsyncMode,
    pub show_host_fps: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            dpi: DEFAULT_DPI,
            refresh_rate_hz: DEFAULT_REFRESH_RATE_HZ,
            orientation: Orientation::Landscape,
            vsync: VsyncMode::On,
            show_host_fps: false,
        }
    }
}

impl DisplayConfig {
    pub fn oriented_size(&self) -> (u32, u32) {
        match self.orientation {
            Orientation::Landscape => (self.width.max(self.height), self.width.min(self.height)),
            Orientation::Portrait => (self.width.min(self.height), self.width.max(self.height)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdbConfig {
    pub enabled: bool,
    pub auto_port: bool,
    pub host_port: Option<u16>,
    pub adb_path: Option<PathBuf>,
    pub auto_root: bool,
}

impl Default for AdbConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_port: true,
            host_port: None,
            adb_path: None,
            auto_root: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArtifactConfig {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub rootfs: PathBuf,
    pub android_fstab: PathBuf,
    pub system_image: Option<PathBuf>,
    pub vendor_image: Option<PathBuf>,
    pub expected_sha256: BTreeMap<String, String>,
}

impl Default for ArtifactConfig {
    fn default() -> Self {
        Self {
            kernel: PathBuf::from("artifacts/kernel"),
            initrd: PathBuf::from("artifacts/initrd.img"),
            rootfs: PathBuf::from("artifacts/rootfs.img"),
            android_fstab: PathBuf::from("artifacts/android_fstab.dt"),
            system_image: None,
            vendor_image: None,
            expected_sha256: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstanceConfigV1 {
    pub schema_version: u32,
    pub id: Uuid,
    pub name: String,
    pub cpu_count: u16,
    pub memory_mib: u32,
    pub display: DisplayConfig,
    pub adb: AdbConfig,
    pub artifacts: ArtifactConfig,
    pub extra_kernel_args: Vec<String>,
}

impl Default for InstanceConfigV1 {
    fn default() -> Self {
        Self {
            schema_version: INSTANCE_CONFIG_VERSION,
            id: Uuid::new_v4(),
            name: "Android".to_owned(),
            cpu_count: DEFAULT_CPU_COUNT,
            memory_mib: DEFAULT_MEMORY_MIB,
            display: DisplayConfig::default(),
            adb: AdbConfig::default(),
            artifacts: ArtifactConfig::default(),
            extra_kernel_args: Vec::new(),
        }
    }
}

impl InstanceConfigV1 {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != INSTANCE_CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.schema_version));
        }
        if self.name.trim().is_empty() {
            return Err(ConfigError::Invalid("instance name is empty"));
        }
        if !(1..=256).contains(&self.cpu_count) {
            return Err(ConfigError::Invalid("cpu_count must be in 1..=256"));
        }
        if !(512..=1_048_576).contains(&self.memory_mib) {
            return Err(ConfigError::Invalid("memory_mib must be in 512..=1048576"));
        }
        let (width, height) = self.display.oriented_size();
        if !(320..=16_384).contains(&width) || !(320..=16_384).contains(&height) {
            return Err(ConfigError::Invalid(
                "display dimensions must be in 320..=16384",
            ));
        }
        if !(72..=960).contains(&self.display.dpi) {
            return Err(ConfigError::Invalid("dpi must be in 72..=960"));
        }
        if !(1..=1000).contains(&self.display.refresh_rate_hz) {
            return Err(ConfigError::Invalid("refresh_rate_hz must be in 1..=1000"));
        }
        if !self.adb.auto_port && self.adb.host_port.is_none() {
            return Err(ConfigError::Invalid(
                "adb.host_port is required when auto_port is false",
            ));
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let bytes = std::fs::read(path).map_err(ConfigError::Read)?;
        let config = serde_json::from_slice::<Self>(&bytes).map_err(ConfigError::Decode)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ConfigError::Write)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(ConfigError::Encode)?;
        std::fs::write(&tmp, bytes).map_err(ConfigError::Write)?;
        std::fs::rename(tmp, path).map_err(ConfigError::Write)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("unsupported config schema version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid config: {0}")]
    Invalid(&'static str),
    #[error("failed to read config: {0}")]
    Read(std::io::Error),
    #[error("failed to decode config: {0}")]
    Decode(serde_json::Error),
    #[error("failed to encode config: {0}")]
    Encode(serde_json::Error),
    #[error("failed to write config: {0}")]
    Write(std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_product_contract() {
        let cfg = InstanceConfigV1::default();
        assert_eq!(cfg.cpu_count, 4);
        assert_eq!(cfg.memory_mib, 8192);
        assert_eq!(cfg.display.oriented_size(), (1280, 720));
        assert_eq!(cfg.display.dpi, 320);
        assert_eq!(cfg.display.refresh_rate_hz, 60);
        assert_eq!(cfg.display.vsync, VsyncMode::On);
        assert!(cfg.adb.enabled);
        assert!(cfg.adb.auto_port);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn orientation_swaps_dimensions() {
        let display = DisplayConfig {
            orientation: Orientation::Portrait,
            ..Default::default()
        };
        assert_eq!(display.oriented_size(), (720, 1280));
    }

    #[test]
    fn config_round_trip_is_stable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("instance.json");
        let cfg = InstanceConfigV1::default();
        cfg.save(&path).expect("save");
        let loaded = InstanceConfigV1::load(&path).expect("load");
        assert_eq!(cfg, loaded);
    }

    #[test]
    fn custom_adb_requires_a_port() {
        let mut cfg = InstanceConfigV1::default();
        cfg.adb.auto_port = false;
        assert!(cfg.validate().is_err());
    }
}
