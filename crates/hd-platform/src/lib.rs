//! Cross-platform boundaries for native display, process, storage and VM integration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hd_core::{DisplayConfig, DisplayLeaseV1, InstanceConfigV1, LaunchPlanV1};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::NativeDisplayEmbedder;

#[cfg(not(windows))]
pub type NativeDisplayEmbedder = PassthroughDisplayEmbedder;

pub const HD_DATA_DIR_ENV: &str = "HD_DATA_DIR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPaths {
    pub root: PathBuf,
    pub instances: PathBuf,
    pub runs: PathBuf,
    pub logs: PathBuf,
    pub disks: PathBuf,
}

impl DataPaths {
    pub fn discover() -> Result<Self, PlatformError> {
        let root = match std::env::var_os(HD_DATA_DIR_ENV) {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => dirs::data_local_dir()
                .ok_or(PlatformError::DataDirectoryUnavailable)?
                .join("bscp")
                .join("hd"),
        };
        Ok(Self::from_root(root))
    }

    pub fn from_root(root: PathBuf) -> Self {
        Self {
            instances: root.join("instances"),
            runs: root.join("runs"),
            logs: root.join("logs"),
            disks: root.join("disks"),
            root,
        }
    }

    pub fn ensure(&self) -> Result<(), PlatformError> {
        for path in [
            &self.root,
            &self.instances,
            &self.runs,
            &self.logs,
            &self.disks,
        ] {
            std::fs::create_dir_all(path).map_err(|source| PlatformError::Io {
                operation: "create data directory",
                path: path.clone(),
                source,
            })?;
        }
        Ok(())
    }

    pub fn instance_dir(&self, id: Uuid) -> PathBuf {
        self.instances.join(id.to_string())
    }

    pub fn instance_config(&self, id: Uuid) -> PathBuf {
        self.instance_dir(id).join("instance.json")
    }

    pub fn run_dir(&self, id: Uuid, run_id: Uuid) -> PathBuf {
        self.runs.join(id.to_string()).join(run_id.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeWindowBinding {
    Win32Hwnd(u64),
    X11Window(u64),
    WaylandSurface(String),
    AppKitView(u64),
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformDisplayLease {
    pub contract: DisplayLeaseV1,
    pub vm_parent_handle: Option<u64>,
}

pub trait DisplayEmbedder {
    fn acquire(
        &mut self,
        parent: &NativeWindowBinding,
        rect: DisplayRect,
    ) -> Result<PlatformDisplayLease, PlatformError>;

    fn resize(&mut self, lease_id: Uuid, rect: DisplayRect) -> Result<(), PlatformError>;

    fn release(&mut self, lease_id: Uuid) -> Result<(), PlatformError>;
}

#[derive(Debug, Default)]
pub struct PassthroughDisplayEmbedder {
    leases: BTreeMap<Uuid, DisplayRect>,
}

impl DisplayEmbedder for PassthroughDisplayEmbedder {
    fn acquire(
        &mut self,
        parent: &NativeWindowBinding,
        rect: DisplayRect,
    ) -> Result<PlatformDisplayLease, PlatformError> {
        let lease_id = Uuid::new_v4();
        self.leases.insert(lease_id, rect);
        let (platform, binding, raw) = match parent {
            NativeWindowBinding::X11Window(value) => {
                ("x11", format!("x11-window:{value}"), Some(*value))
            }
            NativeWindowBinding::WaylandSurface(value) => {
                ("wayland", format!("wayland-surface:{value}"), None)
            }
            NativeWindowBinding::AppKitView(value) => {
                ("appkit", format!("appkit-view:{value}"), Some(*value))
            }
            NativeWindowBinding::Win32Hwnd(value) => {
                ("win32", format!("win32-hwnd:{value}"), Some(*value))
            }
            NativeWindowBinding::Unsupported => ("unsupported", "unsupported".to_owned(), None),
        };
        Ok(PlatformDisplayLease {
            contract: DisplayLeaseV1 {
                lease_id,
                platform: platform.to_owned(),
                binding,
                width: rect.width,
                height: rect.height,
            },
            vm_parent_handle: raw,
        })
    }

    fn resize(&mut self, lease_id: Uuid, rect: DisplayRect) -> Result<(), PlatformError> {
        let existing = self
            .leases
            .get_mut(&lease_id)
            .ok_or(PlatformError::UnknownDisplayLease(lease_id))?;
        *existing = rect;
        Ok(())
    }

    fn release(&mut self, lease_id: Uuid) -> Result<(), PlatformError> {
        self.leases
            .remove(&lease_id)
            .map(|_| ())
            .ok_or(PlatformError::UnknownDisplayLease(lease_id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub success: bool,
}

#[async_trait]
pub trait ProcessSupervisor: Send + Sync {
    type Handle: Send + Sync;

    async fn spawn(&self, spec: &ProcessSpec) -> Result<Self::Handle, PlatformError>;
    async fn terminate(&self, handle: &mut Self::Handle) -> Result<(), PlatformError>;
    async fn wait(&self, handle: &mut Self::Handle) -> Result<ProcessExit, PlatformError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskProvisionMethod {
    BlockClone,
    FullCopyFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskProvisionResult {
    pub path: PathBuf,
    pub method: DiskProvisionMethod,
    pub bytes: u64,
}

#[async_trait]
pub trait DiskProvisioner: Send + Sync {
    async fn provision_full_copy(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<DiskProvisionResult, PlatformError>;
}

#[async_trait]
pub trait GuestPortBridge: Send + Sync {
    async fn allocate_local_port(&self) -> Result<u16, PlatformError>;
    async fn connect_adb(&self, instance_id: Uuid, host_port: u16)
    -> Result<String, PlatformError>;
}

#[async_trait]
pub trait VmBackend: Send + Sync {
    async fn build_launch_plan(
        &self,
        config: &InstanceConfigV1,
        display: Option<&PlatformDisplayLease>,
        run_dir: &Path,
    ) -> Result<LaunchPlanV1, PlatformError>;

    async fn send_key(&self, config: &InstanceConfigV1, key_code: u32)
    -> Result<(), PlatformError>;

    async fn replace_display(
        &self,
        config: &InstanceConfigV1,
        display: &DisplayConfig,
    ) -> Result<(), PlatformError>;
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("local data directory is unavailable")]
    DataDirectoryUnavailable,
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unknown display lease {0}")]
    UnknownDisplayLease(Uuid),
    #[error("native display operation failed: {0}")]
    NativeDisplay(String),
    #[error("process operation failed: {0}")]
    Process(String),
    #[error("operation is unsupported on this platform: {0}")]
    Unsupported(&'static str),
    #[error("VM backend error: {0}")]
    Vm(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_tree_is_deterministic() {
        let paths = DataPaths::from_root(PathBuf::from("root"));
        let id = Uuid::nil();
        assert_eq!(
            paths.instance_config(id),
            PathBuf::from("root/instances/00000000-0000-0000-0000-000000000000/instance.json")
        );
    }

    #[test]
    fn passthrough_lease_is_ephemeral() {
        let mut embedder = PassthroughDisplayEmbedder::default();
        let rect = DisplayRect {
            x: 0,
            y: 0,
            width: 1280,
            height: 720,
        };
        let lease = embedder
            .acquire(&NativeWindowBinding::X11Window(42), rect)
            .expect("acquire");
        assert_eq!(lease.vm_parent_handle, Some(42));
        embedder.release(lease.contract.lease_id).expect("release");
    }
}
