use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hd_core::{InstanceConfigV1, LaunchPlanV1, VsyncMode};
use hd_platform::{PlatformDisplayLease, PlatformError, VmBackend};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct CrosvmBackend {
    executable: PathBuf,
}

impl CrosvmBackend {
    pub fn new(executable: PathBuf) -> Self {
        Self { executable }
    }

    pub fn discover(repo_root: Option<&Path>) -> Self {
        if let Some(path) = std::env::var_os("HD_CROSVM_PATH") {
            return Self::new(PathBuf::from(path));
        }
        if let Ok(executable) = std::env::current_exe()
            && let Some(directory) = executable.parent()
        {
            let sibling = directory.join(executable_name("crosvm"));
            if sibling.is_file() {
                return Self::new(sibling);
            }
        }
        let path = repo_root.map_or_else(
            || PathBuf::from(executable_name("crosvm")),
            |root| {
                root.join("out")
                    .join("dist")
                    .join("windows")
                    .join("bin")
                    .join(executable_name("crosvm"))
            },
        );
        Self::new(path)
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn control_endpoint(instance_id: uuid::Uuid) -> String {
        #[cfg(windows)]
        {
            format!(r"\\.\pipe\bscp-hd-vm-{instance_id}")
        }
        #[cfg(not(windows))]
        {
            format!("/tmp/bscp-hd-vm-{instance_id}.sock")
        }
    }

    pub fn gpu_stats_endpoint(instance_id: uuid::Uuid) -> String {
        #[cfg(windows)]
        {
            format!(r"\\.\pipe\bscp-hd-gpu-{instance_id}")
        }
        #[cfg(not(windows))]
        {
            format!("/tmp/bscp-hd-gpu-{instance_id}.sock")
        }
    }

    fn gpu_argument(config: &InstanceConfigV1, display: Option<&PlatformDisplayLease>) -> String {
        let (width, height) = config.display.oriented_size();
        let mut value = format!(
            "backend=gfxstream,displays=[[mode=windowed[{width},{height}],dpi=[{dpi},{dpi}],refresh-rate={refresh}]],context-types=gfxstream-vulkan:gfxstream-composer,angle=true,gles=false,vulkan=true,wsi=vk,external-blob=false,udmabuf=false",
            dpi = config.display.dpi,
            refresh = config.display.refresh_rate_hz,
        );
        if let Some(parent) = display.and_then(|lease| lease.vm_parent_handle) {
            let _ = write!(value, ",parent-window-handle={parent}");
        }
        value
    }

    fn display_argument(config: &InstanceConfigV1) -> String {
        let (width, height) = config.display.oriented_size();
        format!(
            "mode=windowed[{width},{height}],dpi=[{dpi},{dpi}],refresh-rate={refresh}",
            dpi = config.display.dpi,
            refresh = config.display.refresh_rate_hz,
        )
    }
}

#[async_trait]
impl VmBackend for CrosvmBackend {
    async fn build_launch_plan(
        &self,
        config: &InstanceConfigV1,
        display: Option<&PlatformDisplayLease>,
        run_dir: &Path,
    ) -> Result<LaunchPlanV1, PlatformError> {
        let control_endpoint = Self::control_endpoint(config.id);
        let gpu_stats_endpoint = Self::gpu_stats_endpoint(config.id);
        let guest_cid_space = u128::from(u32::MAX) - 2;
        let guest_cid_offset = u32::try_from(config.id.as_u128() % guest_cid_space)
            .map_err(|error| PlatformError::Vm(format!("derive guest CID: {error}")))?;
        let guest_cid = 3 + guest_cid_offset;
        let serial_log = run_dir.join("serial.txt");
        let hvc_log = run_dir.join("hvc.txt");
        let mut arguments = vec![
            "--log-level".to_owned(),
            "info".to_owned(),
            "run-mp".to_owned(),
            "--disable-sandbox".to_owned(),
            "--cid".to_owned(),
            guest_cid.to_string(),
            "--mem".to_owned(),
            config.memory_mib.to_string(),
            "--cpus".to_owned(),
            config.cpu_count.to_string(),
            "--no-balloon".to_owned(),
            "--no-usb".to_owned(),
            "--socket".to_owned(),
            control_endpoint.clone(),
            "--gpu".to_owned(),
            Self::gpu_argument(config, display),
            "--block".to_owned(),
            format!(
                "path={},ro=false,lock=false,sparse=false,pci-address=00:03.0",
                config.artifacts.rootfs.display()
            ),
            "--serial".to_owned(),
            format!(
                "type=file,path={},hardware=serial,num=1,earlycon=true",
                serial_log.display()
            ),
            "--serial".to_owned(),
            "type=sink,hardware=serial,num=2".to_owned(),
            "--serial".to_owned(),
            format!(
                "hardware=legacy-virtio-console,num=1,type=file,path={},console=true,pci-address=00:02.0",
                hvc_log.display()
            ),
            "--android-fstab".to_owned(),
            config
                .artifacts
                .android_fstab
                .to_string_lossy()
                .into_owned(),
            "--initrd".to_owned(),
            config.artifacts.initrd.to_string_lossy().into_owned(),
            "--params".to_owned(),
            format!(
                "console=hvc0,ttyS0 loglevel=7 printk.devkmsg=on init=/init {}",
                config.extra_kernel_args.join(" ")
            )
            .trim()
            .to_owned(),
            config.artifacts.kernel.to_string_lossy().into_owned(),
        ];
        arguments.retain(|argument| !argument.is_empty());

        let mut environment = BTreeMap::new();
        environment.insert("HD_GPU_STATS_PIPE".to_owned(), gpu_stats_endpoint.clone());
        environment.insert("HD_GPU_STATS_INSTANCE".to_owned(), config.id.to_string());
        environment.insert(
            "HD_VSYNC".to_owned(),
            match config.display.vsync {
                VsyncMode::On => "1",
                VsyncMode::Off => "0",
            }
            .to_owned(),
        );

        Ok(LaunchPlanV1 {
            schema_version: 1,
            instance_id: config.id,
            executable: self.executable.clone(),
            arguments,
            environment,
            working_directory: run_dir.to_owned(),
            display_lease: display.map(|lease| lease.contract.clone()),
            control_endpoint,
            gpu_stats_endpoint,
            adb_serial: if config.adb.enabled && !config.adb.auto_port {
                config.adb.host_port.map(|port| format!("127.0.0.1:{port}"))
            } else {
                None
            },
        })
    }

    async fn send_key(
        &self,
        _config: &InstanceConfigV1,
        _key_code: u32,
    ) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(
            "crosvm key injection is not implemented; use ADB",
        ))
    }

    async fn replace_display(
        &self,
        config: &InstanceConfigV1,
        display: &hd_core::DisplayConfig,
    ) -> Result<(), PlatformError> {
        let mut target = config.clone();
        target.display = display.clone();
        let output = Command::new(&self.executable)
            .args([
                "gpu",
                "replace-display",
                "--display-id",
                "0",
                "--gpu-display",
                &Self::display_argument(&target),
                &Self::control_endpoint(config.id),
            ])
            .output()
            .await
            .map_err(|error| PlatformError::Vm(format!("start crosvm display replace: {error}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(PlatformError::Vm(format!(
                "display replace failed (code {:?}): {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

fn executable_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn launch_plan_carries_ephemeral_parent_and_vsync() {
        let backend = CrosvmBackend::new(PathBuf::from("crosvm"));
        let config = InstanceConfigV1::default();
        let lease = PlatformDisplayLease {
            contract: hd_core::DisplayLeaseV1 {
                lease_id: uuid::Uuid::new_v4(),
                platform: "win32".to_owned(),
                binding: "test".to_owned(),
                width: 1280,
                height: 720,
            },
            vm_parent_handle: Some(42),
        };
        let plan = backend
            .build_launch_plan(&config, Some(&lease), Path::new("run"))
            .await
            .expect("plan");
        assert!(
            plan.arguments
                .iter()
                .any(|arg| arg.contains("parent-window-handle=42"))
        );
        assert_eq!(
            plan.environment.get("HD_VSYNC").map(String::as_str),
            Some("1")
        );
    }
}
