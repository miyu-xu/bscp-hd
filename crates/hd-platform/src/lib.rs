//! Narrow, explicit host-platform boundaries used by the HD daemon and workers.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use async_trait::async_trait;
use hd_core::{
    DeviceSerialEndpointV2, DisplayConfigV2, DisplayViewportV2, FrameTransportKindV2,
    InstanceSpecV2, KeyActionV2, LaunchPlanV2, NativeDisplayTargetV2, ResolvedGuestArtifactsV2,
    WorkerIdentityV2,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

mod native_display_host;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
pub use native_display_host::center_macos_traffic_lights;
#[cfg(target_os = "macos")]
pub use native_display_host::install_macos_titlebar_controls;
#[cfg(target_os = "macos")]
pub use native_display_host::set_macos_window_content_aspect_ratio;
pub use native_display_host::{
    NativeDisplayBounds, NativeDisplayHost, choose_apk_file, create_native_display_host,
};

pub const HD_DATA_DIR_ENV: &str = "HD_DATA_DIR";

pub fn native_display_toplevel_debug_enabled() -> bool {
    #[cfg(windows)]
    {
        windows::native_display_toplevel_debug_enabled()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn mark_file_sparse(file: &std::fs::File, path: &Path) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::mark_file_sparse(file, path)
    }
    #[cfg(not(windows))]
    {
        let _ = (file, path);
        Ok(())
    }
}

pub fn attach_native_display(
    child_pid: u32,
    target: &NativeDisplayTargetV2,
    viewport: &DisplayViewportV2,
) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::attach_native_display(child_pid, target, viewport)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (child_pid, viewport);
        validate_macos_native_target(target)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (child_pid, target, viewport);
        Err(PlatformError::Unsupported(
            "native display embedding is not implemented for this platform",
        ))
    }
}

/// Completes startup only after gfxstream has created its Vulkan render HWND directly below the
/// requested native viewport. Unlike `attach_native_display`, this never reparents crosvm's input
/// window across process boundaries.
pub fn prepare_native_display(
    child_pid: u32,
    target: &NativeDisplayTargetV2,
    viewport: &DisplayViewportV2,
) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::prepare_native_display(child_pid, target, viewport)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (child_pid, viewport);
        validate_macos_native_target(target)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (child_pid, target, viewport);
        Err(PlatformError::Unsupported(
            "native display preparation is not implemented for this platform",
        ))
    }
}

pub fn resize_native_display(
    child_pid: u32,
    target: &NativeDisplayTargetV2,
    viewport: &DisplayViewportV2,
) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::resize_native_display(child_pid, target, viewport)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (child_pid, viewport);
        validate_macos_native_target(target)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (child_pid, target, viewport);
        Err(PlatformError::Unsupported(
            "native display embedding is not implemented for this platform",
        ))
    }
}

pub fn set_native_display_visibility(child_pid: u32, visible: bool) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::set_native_display_visibility(child_pid, visible)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (child_pid, visible);
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (child_pid, visible);
        Err(PlatformError::Unsupported(
            "native display embedding is not implemented for this platform",
        ))
    }
}

pub fn detach_native_display(child_pid: u32) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::detach_native_display(child_pid)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = child_pid;
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = child_pid;
        Err(PlatformError::Unsupported(
            "native display embedding is not implemented for this platform",
        ))
    }
}

#[cfg(target_os = "macos")]
fn validate_macos_native_target(target: &NativeDisplayTargetV2) -> Result<(), PlatformError> {
    let NativeDisplayTargetV2::MacCaContext { endpoint, owner } = target else {
        return Err(PlatformError::Identity(
            "macOS display requires a CoreAnimation context endpoint".to_owned(),
        ));
    };
    let endpoint = Path::new(endpoint);
    if !endpoint.is_absolute() || endpoint.as_os_str().is_empty() {
        return Err(PlatformError::Identity(
            "macOS display endpoint is not an absolute local path".to_owned(),
        ));
    }
    if !process_identity_is_alive(owner) {
        return Err(PlatformError::Identity(
            "macOS display owner is not alive".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPaths {
    pub root: PathBuf,
    pub instances: PathBuf,
    pub runs: PathBuf,
    pub logs: PathBuf,
    pub disks: PathBuf,
    pub workers: PathBuf,
    pub uploads: PathBuf,
    pub diagnostics: PathBuf,
    pub certifications: PathBuf,
    pub cache: PathBuf,
}

impl DataPaths {
    /// User-visible screenshots intentionally live outside the private runtime data root.
    /// If the platform does not expose a Pictures directory, retain the previous private-root
    /// fallback so screenshot capture remains available in minimal Windows profiles.
    pub fn screenshot_directory(&self) -> PathBuf {
        dirs::picture_dir()
            .unwrap_or_else(|| self.root.join("screenshots"))
            .join("HD")
    }

    pub fn discover() -> Result<Self, PlatformError> {
        let root = match std::env::var_os(HD_DATA_DIR_ENV) {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => dirs::data_local_dir()
                .ok_or(PlatformError::DataDirectoryUnavailable)?
                .join("bscp")
                .join("hd"),
        };
        Self::resolve(root)
    }

    pub fn resolve(root: PathBuf) -> Result<Self, PlatformError> {
        let absolute = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()
                .map_err(|source| PlatformError::Io {
                    operation: "resolve data root current directory",
                    path: root.clone(),
                    source,
                })?
                .join(root)
        };
        let mut normalized = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(PlatformError::UnsafeDataRoot(absolute));
                    }
                }
                Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                    normalized.push(component.as_os_str());
                }
            }
        }
        let paths = Self::from_root(normalized);
        paths.validate_root()?;
        Ok(paths)
    }

    pub fn from_root(root: PathBuf) -> Self {
        Self {
            instances: root.join("instances"),
            runs: root.join("runs"),
            logs: root.join("logs"),
            disks: root.join("disks"),
            workers: root.join("workers"),
            uploads: root.join("uploads"),
            diagnostics: root.join("diagnostics"),
            certifications: root.join("certifications"),
            cache: root.join("cache"),
            root,
        }
    }

    pub fn ensure(&self) -> Result<(), PlatformError> {
        self.validate_root()?;
        for path in [
            &self.root,
            &self.instances,
            &self.runs,
            &self.logs,
            &self.disks,
            &self.workers,
            &self.uploads,
            &self.diagnostics,
            &self.certifications,
            &self.cache,
        ] {
            ensure_owner_only_directory(path)?;
        }
        Ok(())
    }

    pub fn validate_root(&self) -> Result<(), PlatformError> {
        if !self.root.is_absolute()
            || self.root.parent().is_none()
            || self
                .root
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(PlatformError::UnsafeDataRoot(self.root.clone()));
        }
        if self != &Self::from_root(self.root.clone()) {
            return Err(PlatformError::UnsafeDataRoot(self.root.clone()));
        }
        Ok(())
    }

    pub fn database(&self) -> PathBuf {
        self.root.join("host-v2.redb")
    }

    pub fn host_lock(&self) -> PathBuf {
        self.root.join("host.lock")
    }

    pub fn host_runtime_descriptor(&self) -> PathBuf {
        self.root.join("host-runtime-v2.json")
    }

    pub fn host_identity_secret(&self) -> PathBuf {
        self.root.join("host-identity.key")
    }

    pub fn instance_dir(&self, id: Uuid) -> PathBuf {
        self.instances.join(id.to_string())
    }

    pub fn legacy_instance_config(&self, id: Uuid) -> PathBuf {
        self.instance_dir(id).join("instance.json")
    }

    pub fn migration_backup(&self, id: Uuid) -> PathBuf {
        self.instance_dir(id).join("instance-v1.backup.json")
    }

    pub fn worker_dir(&self, id: Uuid) -> PathBuf {
        self.workers.join(id.to_string())
    }

    pub fn worker_descriptor(&self, id: Uuid) -> PathBuf {
        self.worker_dir(id).join("worker-v2.json")
    }

    pub fn worker_secret(&self, id: Uuid) -> PathBuf {
        self.worker_dir(id).join("worker.key")
    }

    pub fn worker_lock(&self, id: Uuid) -> PathBuf {
        self.worker_dir(id).join("worker.lock")
    }

    pub fn run_dir(&self, id: Uuid, run_id: Uuid) -> PathBuf {
        self.runs.join(id.to_string()).join(run_id.to_string())
    }

    pub fn disk_overlay(&self, id: Uuid) -> PathBuf {
        self.disks.join(format!("{id}.img"))
    }

    pub fn upload_path(&self, id: Uuid) -> PathBuf {
        self.uploads.join(format!("{id}.apk"))
    }

    pub fn host_certification(
        &self,
        platform: &str,
        architecture: &str,
        guest_digest: &str,
        host_digest: &str,
    ) -> PathBuf {
        self.certifications.join(format!(
            "{platform}-{architecture}-{guest_digest}-{host_digest}.json"
        ))
    }
}

pub fn ensure_owner_only_directory(path: &Path) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::ensure_owner_only_directory(path)
    }
    #[cfg(unix)]
    {
        unix::ensure_owner_only_directory(path)
    }
}

pub fn open_owner_only_rw(path: &Path) -> Result<std::fs::File, PlatformError> {
    if let Some(parent) = path.parent() {
        ensure_owner_only_directory(parent)?;
    }
    #[cfg(windows)]
    {
        windows::open_owner_only_rw(path)
    }
    #[cfg(unix)]
    {
        unix::open_owner_only_rw(path)
    }
}

pub fn read_regular_nofollow_limited(path: &Path, maximum: u64) -> Result<Vec<u8>, PlatformError> {
    use std::io::Read as _;

    let file = open_regular_read_nofollow(path)?;
    let metadata = file.metadata().map_err(|source| PlatformError::Io {
        operation: "inspect regular file handle",
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > maximum
    {
        return Err(PlatformError::Process(format!(
            "regular file is unsafe or exceeds {maximum} bytes: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| PlatformError::Io {
            operation: "read bounded regular file",
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() as u64 > maximum {
        return Err(PlatformError::Process(format!(
            "regular file changed while reading or exceeds {maximum} bytes: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

pub fn open_regular_read_nofollow(path: &Path) -> Result<std::fs::File, PlatformError> {
    #[cfg(windows)]
    {
        windows::open_regular_read_nofollow(path)
    }
    #[cfg(unix)]
    {
        unix::open_regular_read_nofollow(path)
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
    pub kill_on_drop: bool,
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
pub enum DiskProvisionMethodV2 {
    BlockClone,
    FullCopy,
    AndroidSparseExpanded,
    ExistingVerified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskProvisionResultV2 {
    pub path: PathBuf,
    pub method: DiskProvisionMethodV2,
    pub bytes: u64,
}

#[async_trait]
pub trait DiskProvisioner: Send + Sync {
    async fn provision(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<DiskProvisionResultV2, PlatformError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmLaunchContextV2 {
    pub spec: InstanceSpecV2,
    pub run_id: Uuid,
    pub guest_cid: u32,
    pub run_dir: PathBuf,
    pub disk_overlay: PathBuf,
    pub artifacts: ResolvedGuestArtifactsV2,
    pub control_endpoint: String,
    pub frame_endpoint: String,
    pub keyboard_endpoint: String,
    pub device_endpoints: BTreeMap<String, DeviceSerialEndpointV2>,
    pub device_control_endpoints: BTreeMap<String, String>,
    pub adb_host_port: Option<u16>,
}

#[async_trait]
pub trait VmBackend: Send + Sync {
    async fn build_launch_plan(
        &self,
        context: &VmLaunchContextV2,
    ) -> Result<LaunchPlanV2, PlatformError>;

    async fn send_key(
        &self,
        keyboard_endpoint: &str,
        key: KeyActionV2,
    ) -> Result<(), PlatformError>;

    async fn pause(&self, control_endpoint: &str) -> Result<(), PlatformError>;
    async fn resume(&self, control_endpoint: &str) -> Result<(), PlatformError>;
    async fn power_button(&self, control_endpoint: &str) -> Result<(), PlatformError>;

    async fn replace_display(
        &self,
        control_endpoint: &str,
        display: &DisplayConfigV2,
    ) -> Result<(), PlatformError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameInteropProbeV2 {
    pub component_protocol_version: u32,
    pub service_lifecycle: bool,
    pub transport: FrameTransportKindV2,
    pub memory_export: bool,
    pub explicit_sync: bool,
    pub same_adapter: bool,
    pub validation_clean: bool,
    pub detail: String,
    pub properties: BTreeMap<String, String>,
}

impl FrameInteropProbeV2 {
    pub const fn supported(&self) -> bool {
        self.component_protocol_version == hd_core::COMPONENT_PROTOCOL_VERSION
            && self.service_lifecycle
            && self.memory_export
            && self.explicit_sync
            && self.same_adapter
            && self.validation_clean
    }
}

pub trait FrameTransport: Send + Sync {
    fn probe(&self) -> Result<FrameInteropProbeV2, PlatformError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCapabilityProbe {
    pub supported: bool,
    pub detail: String,
    pub properties: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostResourceSnapshot {
    pub logical_cpus: usize,
    pub total_memory_bytes: u64,
    pub available_memory_bytes: u64,
    pub memory_source: &'static str,
}

pub fn host_resources() -> Result<HostResourceSnapshot, PlatformError> {
    let logical_cpus = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .map_err(|error| PlatformError::Process(format!("query logical CPU count: {error}")))?;
    #[cfg(windows)]
    let (total_memory_bytes, available_memory_bytes, memory_source) = windows::memory_capacity()?;
    #[cfg(unix)]
    let (total_memory_bytes, available_memory_bytes, memory_source) = unix::memory_capacity()?;
    Ok(HostResourceSnapshot {
        logical_cpus,
        total_memory_bytes,
        available_memory_bytes,
        memory_source,
    })
}

pub fn platform_baseline() -> NativeCapabilityProbe {
    #[cfg(windows)]
    {
        windows::platform_baseline()
    }
    #[cfg(unix)]
    {
        unix::platform_baseline()
    }
}

pub fn hypervisor_available() -> NativeCapabilityProbe {
    #[cfg(windows)]
    {
        windows::hypervisor_available()
    }
    #[cfg(unix)]
    {
        unix::hypervisor_available()
    }
}

pub fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "unsupported"
    }
}

pub fn architecture_name() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "unsupported"
    }
}

pub fn executable_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_owned()
    }
}

pub fn current_user_scope() -> Result<String, PlatformError> {
    #[cfg(windows)]
    {
        windows::current_user_sid()
    }
    #[cfg(unix)]
    {
        Ok(unix::current_user_scope())
    }
}

#[cfg(unix)]
pub fn ensure_open_file_limit(minimum: u64) -> Result<(), PlatformError> {
    unix::ensure_open_file_limit(minimum)
}

pub fn current_process_identity(nonce: Uuid) -> Result<WorkerIdentityV2, PlatformError> {
    let pid = std::process::id();
    Ok(WorkerIdentityV2 {
        pid,
        process_start_marker: process_start_marker(pid)?,
        nonce,
    })
}

pub fn process_start_marker(pid: u32) -> Result<String, PlatformError> {
    #[cfg(windows)]
    {
        windows::process_start_marker(pid)
    }
    #[cfg(unix)]
    {
        unix::process_start_marker(pid)
    }
}

pub fn process_identity_is_alive(identity: &WorkerIdentityV2) -> bool {
    #[cfg(windows)]
    {
        windows::process_identity_is_alive(identity)
    }
    #[cfg(unix)]
    {
        unix::process_identity_is_alive(identity)
    }
}

pub fn terminate_process_identity(identity: &WorkerIdentityV2) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::terminate_process_identity(identity)
    }
    #[cfg(unix)]
    {
        unix::terminate_process_identity(identity)
    }
}

pub fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), PlatformError> {
    if let Some(parent) = path.parent() {
        ensure_owner_only_directory(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    #[cfg(windows)]
    let write_result = windows::write_owner_only(&temporary, bytes);
    #[cfg(unix)]
    let write_result = unix::write_owner_only(&temporary, bytes);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = replace_file(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

pub fn replace_file(source: &Path, destination: &Path) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::replace_file(source, destination)
    }
    #[cfg(unix)]
    {
        unix::replace_file(source, destination)
    }
}

#[cfg(unix)]
pub fn create_owner_only_fifo(path: &Path) -> Result<(), PlatformError> {
    unix::create_owner_only_fifo(path)
}

pub fn spawn_detached(
    executable: &Path,
    arguments: &[String],
    environment: &BTreeMap<String, String>,
    working_directory: &Path,
) -> Result<u32, PlatformError> {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .envs(environment)
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    windows::configure_detached(&mut command);
    #[cfg(unix)]
    unix::configure_detached(&mut command);
    command
        .spawn()
        .map(|child| child.id())
        .map_err(|source| PlatformError::Io {
            operation: "spawn detached process",
            path: executable.to_owned(),
            source,
        })
}

#[cfg(windows)]
pub fn create_owner_only_named_pipe(
    options: &tokio::net::windows::named_pipe::ServerOptions,
    endpoint: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, PlatformError> {
    windows::create_owner_only_named_pipe(options, endpoint)
}

#[derive(Debug)]
pub struct ProcessContainment {
    #[cfg(windows)]
    inner: windows::WindowsJob,
    #[cfg(unix)]
    inner: unix::UnixContainment,
}

pub fn configure_managed_command(command: &mut Command) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::configure_managed(command);
        Ok(())
    }
    #[cfg(unix)]
    {
        unix::configure_managed(command);
        Ok(())
    }
}

/// Configures a bounded, short-lived helper process without suspending its initial thread.
///
/// Managed long-running processes are created suspended on Windows so they can be assigned to a
/// kill-on-close Job before execution. Callers of this function await and kill their own helper;
/// suspending it would deadlock because there is intentionally no containment/resume handshake.
pub fn configure_transient_command(command: &mut Command) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::configure_transient(command);
        Ok(())
    }
    #[cfg(unix)]
    {
        unix::configure_managed(command);
        Ok(())
    }
}

pub fn contain_process(pid: u32) -> Result<ProcessContainment, PlatformError> {
    #[cfg(windows)]
    {
        windows::contain_process(pid).map(|inner| ProcessContainment { inner })
    }
    #[cfg(unix)]
    {
        Ok(ProcessContainment {
            inner: unix::contain_process(pid),
        })
    }
}

pub fn resume_managed_process(pid: u32) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        windows::resume_managed_process(pid)
    }
    #[cfg(unix)]
    {
        let _ = pid;
        Ok(())
    }
}

impl ProcessContainment {
    pub fn process_id(&self) -> u32 {
        #[cfg(windows)]
        {
            self.inner.process_id()
        }
        #[cfg(unix)]
        {
            self.inner.process_id()
        }
    }
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("local data directory is unavailable")]
    DataDirectoryUnavailable,
    #[error("data root must be an absolute normalized non-root path: {0}")]
    UnsafeDataRoot(PathBuf),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("platform identity operation failed: {0}")]
    Identity(String),
    #[error("process operation failed: {0}")]
    Process(String),
    #[error("operation is unsupported on this platform: {0}")]
    Unsupported(&'static str),
    #[error("VM backend error: {0}")]
    Vm(String),
    #[error("frame interop error: {0}")]
    Frame(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_tree_is_deterministic() {
        let paths = DataPaths::from_root(PathBuf::from("root"));
        let id = Uuid::nil();
        assert_eq!(
            paths.worker_descriptor(id),
            PathBuf::from("root/workers/00000000-0000-0000-0000-000000000000/worker-v2.json")
        );
        assert_eq!(
            paths.worker_lock(id),
            PathBuf::from("root/workers/00000000-0000-0000-0000-000000000000/worker.lock")
        );
    }

    #[test]
    fn resolved_data_root_is_absolute_and_normalized() {
        let paths = DataPaths::resolve(PathBuf::from("hd-parent/../hd-data")).expect("resolve");
        assert!(paths.root.is_absolute());
        assert!(
            !paths
                .root
                .components()
                .any(|component| { matches!(component, Component::CurDir | Component::ParentDir) })
        );
    }
}
