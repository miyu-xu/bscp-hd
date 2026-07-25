use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hd_core::{
    DeviceSerialEndpointV2, DisplayConfigV2, InstanceSpecV2, KeyActionV2, LaunchPlanV2, VsyncModeV2,
};
use hd_platform::{PlatformError, VmBackend, VmLaunchContextV2};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
#[cfg(unix)]
use tokio::sync::Mutex;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_OUTPUT_LIMIT: u64 = 64 * 1024;

fn guest_kernel_arguments(spec: &InstanceSpecV2) -> Vec<String> {
    let mut arguments = vec![
        "console=hvc0,ttyS0".to_owned(),
        "init=/init".to_owned(),
        format!("androidboot.lcd_density={}", spec.display.dpi),
    ];
    #[cfg(windows)]
    arguments.extend([
        // Windows HD requires Guest ANGLE over ranchu/gfxstream. Explicitly disable the CPU
        // Vulkan fallback so an artifact bootconfig cannot silently select Guest SwiftShader.
        "androidboot.cpuvulkan.version=0".to_owned(),
        "androidboot.hardware.gralloc=minigbm".to_owned(),
        "androidboot.hardware.hwcomposer=ranchu".to_owned(),
        "androidboot.hardware.hwcomposer.display_finder_mode=drm".to_owned(),
        "androidboot.hardware.hwcomposer.display_framebuffer_format=rgba".to_owned(),
        "androidboot.hardware.egl=angle".to_owned(),
        "androidboot.hardware.vulkan=ranchu".to_owned(),
        "androidboot.hardware.gltransport=virtio-gpu-asg".to_owned(),
        "androidboot.opengles.version=196609".to_owned(),
    ]);
    if spec.devices.uwb {
        arguments.push("androidboot.uwbcountrycode=US".to_owned());
    }
    arguments.extend(spec.boot.kernel_arguments());
    arguments
}

#[cfg(unix)]
#[derive(Debug)]
enum KeyboardEndpoint {
    Listening(tokio::net::UnixListener),
    Connected(tokio::net::UnixStream),
}

#[cfg(unix)]
type KeyboardStreams = BTreeMap<String, KeyboardEndpoint>;

/// Direct crosvm adapter. All launch arguments are derived from typed V2 configuration and
/// verified immutable bundles; there is deliberately no free-form argument escape hatch.
#[derive(Debug, Clone)]
pub struct CrosvmBackend {
    executable: PathBuf,
    #[cfg(unix)]
    keyboard_streams: Arc<Mutex<KeyboardStreams>>,
}

impl CrosvmBackend {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            #[cfg(unix)]
            keyboard_streams: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// On Unix crosvm connects to the input socket during VM construction, so the worker must
    /// bind it before spawning crosvm. Windows crosvm owns the named-pipe server instead.
    #[allow(clippy::unused_async)]
    pub async fn prepare_keyboard_endpoint(&self, endpoint: &str) -> Result<(), PlatformError> {
        #[cfg(windows)]
        {
            if !endpoint.starts_with(r"\\.\pipe\") {
                return Err(PlatformError::Vm(
                    "Windows keyboard endpoint is not a local named pipe".to_owned(),
                ));
            }
            Ok(())
        }
        #[cfg(unix)]
        {
            let path = PathBuf::from(endpoint);
            if path.exists() {
                std::fs::remove_file(&path).map_err(|source| PlatformError::Io {
                    operation: "remove stale keyboard socket",
                    path: path.clone(),
                    source,
                })?;
            }
            let listener =
                tokio::net::UnixListener::bind(&path).map_err(|source| PlatformError::Io {
                    operation: "bind keyboard socket",
                    path: path.clone(),
                    source,
                })?;
            self.keyboard_streams
                .lock()
                .await
                .insert(endpoint.to_owned(), KeyboardEndpoint::Listening(listener));
            Ok(())
        }
    }

    fn gpu_argument(display: &DisplayConfigV2) -> String {
        let (width, height) = display.oriented_size();
        let base = format!(
            "backend=gfxstream,max-num-displays=1,audio-device-mode=one-global,displays=[[mode=windowed[{width},{height}],dpi=[{dpi},{dpi}],refresh-rate={refresh},hidden=true]],context-types=gfxstream-vulkan:gfxstream-composer,angle=true,gles=false,vulkan=true,wsi=vk",
            dpi = display.dpi,
            refresh = display.refresh_rate_hz,
        );
        if crate::dev::native_display_direct_enabled() {
            // The native HWND path presents through gfxstream's own Vulkan swapchain. External
            // blobs and VulkanAllocateHostMemory belong to the frame-export broker path and route
            // guest allocations through a separate, experimental Win32 external-memory flow.
            // Enabling them without a broker can leave the native subwindow at its clear color
            // even though SurfaceFlinger is actively composing.
            base
        } else {
            format!(
                "{base},external-blob=true,udmabuf=false,renderer-features=VulkanAllocateHostMemory:enabled"
            )
        }
    }

    fn display_argument(display: &DisplayConfigV2) -> String {
        let (width, height) = display.oriented_size();
        format!(
            "mode=windowed[{width},{height}],dpi=[{dpi},{dpi}],refresh-rate={refresh}",
            dpi = display.dpi,
            refresh = display.refresh_rate_hz,
        )
    }

    fn serial_argument(
        number: u8,
        pci_slot: u8,
        endpoint: Option<&DeviceSerialEndpointV2>,
        fallback_output: Option<&Path>,
    ) -> Result<String, PlatformError> {
        let prefix =
            format!("hardware=legacy-virtio-console,num={number},pci-address=00:{pci_slot:02x}.0");
        if let Some(endpoint) = endpoint {
            let output = safe_key_value_path(&endpoint.guest_output)?;
            let input = safe_key_value_path(&endpoint.guest_input)?;
            #[cfg(windows)]
            return Ok(format!("{prefix},type=file,path={output},input={input}"));
            #[cfg(unix)]
            {
                return Ok(format!("{prefix},type=unix,path={output},input={input}"));
            }
        }
        if let Some(path) = fallback_output {
            return Ok(format!(
                "{prefix},type=file,path={}",
                safe_key_value_path(&path.to_string_lossy())?
            ));
        }
        Ok(format!("{prefix},type=sink"))
    }

    async fn control(&self, arguments: &[String]) -> Result<(), PlatformError> {
        let mut command = Command::new(&self.executable);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        hd_platform::configure_transient_command(command.as_std_mut())?;
        let mut child = command
            .spawn()
            .map_err(|error| PlatformError::Vm(format!("spawn crosvm control command: {error}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PlatformError::Vm("capture crosvm stdout".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| PlatformError::Vm("capture crosvm stderr".to_owned()))?;
        let collect = async move {
            let read_stdout = read_capped(stdout);
            let read_stderr = read_capped(stderr);
            let (stdout, stderr) = tokio::try_join!(read_stdout, read_stderr).map_err(|error| {
                PlatformError::Vm(format!("read crosvm control output: {error}"))
            })?;
            let status = child
                .wait()
                .await
                .map_err(|error| PlatformError::Vm(format!("wait for crosvm control: {error}")))?;
            Ok::<_, PlatformError>((status, stdout, stderr))
        };
        let (status, stdout, stderr) = tokio::time::timeout(CONTROL_TIMEOUT, collect)
            .await
            .map_err(|_| PlatformError::Vm("crosvm control command timed out".to_owned()))??;
        if status.success() {
            Ok(())
        } else {
            let detail = if stderr.is_empty() { stdout } else { stderr };
            Err(PlatformError::Vm(format!(
                "crosvm control failed with {:?}: {}",
                status.code(),
                String::from_utf8_lossy(&detail).trim()
            )))
        }
    }
}

#[async_trait]
impl VmBackend for CrosvmBackend {
    #[allow(clippy::too_many_lines)]
    async fn build_launch_plan(
        &self,
        context: &VmLaunchContextV2,
    ) -> Result<LaunchPlanV2, PlatformError> {
        let spec = &context.spec;
        let mut arguments = vec![
            "--log-level".to_owned(),
            "warn".to_owned(),
            if cfg!(windows) { "run-mp" } else { "run" }.to_owned(),
            "--cid".to_owned(),
            context.guest_cid.to_string(),
            "--mem".to_owned(),
            spec.memory_mib.to_string(),
            "--cpus".to_owned(),
            spec.cpu_count.to_string(),
            "--no-balloon".to_owned(),
            "--no-usb".to_owned(),
            "--socket".to_owned(),
            context.control_endpoint.clone(),
            "--input".to_owned(),
            format!(
                "keyboard[path={}]",
                safe_key_value_path(&context.keyboard_endpoint)?
            ),
            "--gpu".to_owned(),
            Self::gpu_argument(&spec.display),
        ];

        if spec.devices.network {
            arguments.extend([
                "--net".to_owned(),
                format!(
                    "mac=02:48:{:02x}:{:02x}:{:02x}:00,pci-address=00:01.1",
                    spec.id.as_bytes()[0],
                    spec.id.as_bytes()[1],
                    spec.id.as_bytes()[2]
                ),
                "--net".to_owned(),
                format!(
                    "mac=02:49:{:02x}:{:02x}:{:02x}:00,pci-address=00:01.2",
                    spec.id.as_bytes()[3],
                    spec.id.as_bytes()[4],
                    spec.id.as_bytes()[5]
                ),
            ]);
        }

        if spec.devices.audio {
            arguments.extend([
                "--virtio-snd".to_owned(),
                "capture=true,backend=null,num_output_devices=1,num_input_devices=1,num_output_streams=1,num_input_streams=1"
                    .to_owned(),
            ]);
        }

        arguments.extend([
            "--block".to_owned(),
            format!(
                "path={},ro=false,lock=true,sparse=false,pci-address=00:03.0",
                safe_key_value_path(&context.disk_overlay.to_string_lossy())?
            ),
            "--serial".to_owned(),
            format!(
                "type=file,path={},hardware=serial,num=1,earlycon=true",
                safe_key_value_path(&context.run_dir.join("serial.txt").to_string_lossy())?
            ),
            "--serial".to_owned(),
            "type=sink,hardware=serial,num=2".to_owned(),
            "--serial".to_owned(),
            format!(
                "hardware=legacy-virtio-console,num=1,type=file,path={},console=true,pci-address=00:02.0",
                safe_key_value_path(&context.run_dir.join("console-hvc0.txt").to_string_lossy())?
            ),
        ]);

        let serial_roles: [(u8, u8, &str); 18] = [
            (2, 0x04, "reserved"),
            (3, 0x05, "logcat"),
            (4, 0x06, "keymaster"),
            (5, 0x07, "gatekeeper"),
            (6, 0x08, "bluetooth"),
            (7, 0x09, "gnss"),
            (8, 0x0a, "location"),
            (9, 0x0b, "confui"),
            (10, 0x0c, "uwb"),
            (11, 0x0d, "oemlock"),
            (12, 0x0e, "keymint"),
            (13, 0x0f, "nfc"),
            (14, 0x10, "sensors"),
            (15, 0x11, "mcu-control"),
            (16, 0x12, "mcu-uart"),
            (18, 0x16, "network-control"),
            (19, 0x17, "audio-control"),
            (20, 0x18, "camera-control"),
        ];
        for (number, slot, role) in serial_roles {
            let endpoint = context.device_endpoints.get(role);
            let fallback = matches!(
                role,
                "logcat" | "keymaster" | "gatekeeper" | "confui" | "oemlock" | "keymint"
            )
            .then(|| {
                context
                    .run_dir
                    .join(format!("{role}-hvc{}.txt", number - 1))
            });
            arguments.push("--serial".to_owned());
            arguments.push(Self::serial_argument(
                number,
                slot,
                endpoint,
                fallback.as_deref(),
            )?);
        }

        if let Some(system) = &context.artifacts.system_image {
            arguments.extend([
                "--block".to_owned(),
                format!(
                    "path={},ro=true,lock=true,sparse=false,pci-address=00:13.0",
                    safe_key_value_path(&system.to_string_lossy())?
                ),
            ]);
        }
        if let Some(vendor) = &context.artifacts.vendor_image {
            arguments.extend([
                "--block".to_owned(),
                format!(
                    "path={},ro=true,lock=true,sparse=false,pci-address=00:14.0",
                    safe_key_value_path(&vendor.to_string_lossy())?
                ),
            ]);
        }

        // On Windows HD owns the direct-render graphics contract: Guest ANGLE uses the ranchu
        // Vulkan ICD and gfxstream forwards work to the selected host GPU. Other platforms retain
        // the imported image bootconfig until their native direct-render paths are implemented.
        // The virtio-gpu EDID carries the configured DPI, but Android's SurfaceFlinger does not
        // derive its logical/physical density from that EDID on this guest profile.  Android's
        // emulator boot contract consumes androidboot.lcd_density, so keep the guest UI density
        // in the same cold-start transaction as the crosvm display parameters.
        let kernel_arguments = guest_kernel_arguments(spec);
        arguments.extend([
            "--android-fstab".to_owned(),
            context
                .artifacts
                .android_fstab
                .to_string_lossy()
                .into_owned(),
            "--initrd".to_owned(),
            context.artifacts.initrd.to_string_lossy().into_owned(),
            "--params".to_owned(),
            kernel_arguments.join(" "),
            context.artifacts.kernel.to_string_lossy().into_owned(),
        ]);

        let dev_display_copy_fallback = crate::dev::allow_display_copy_fallback_enabled();
        let native_display_direct = crate::dev::native_display_direct_enabled();
        let strict_frame_broker = !dev_display_copy_fallback && !native_display_direct;
        let mut environment = BTreeMap::from([
            (
                "HD_FRAME_REQUIRED".to_owned(),
                if strict_frame_broker { "1" } else { "0" }.to_owned(),
            ),
            ("HD_FRAME_PROTOCOL".to_owned(), "2".to_owned()),
            ("HD_FRAME_INSTANCE".to_owned(), spec.id.to_string()),
            (
                "HD_VSYNC".to_owned(),
                match spec.display.vsync {
                    VsyncModeV2::On => "1",
                    VsyncModeV2::Off => "0",
                }
                .to_owned(),
            ),
        ]);
        if strict_frame_broker {
            environment.insert(
                "HD_FRAME_BROKER_V2".to_owned(),
                context.frame_endpoint.clone(),
            );
        }
        #[cfg(windows)]
        {
            let gfxstream = context
                .artifacts
                .host_tools
                .get("gfxstream-backend")
                .ok_or_else(|| {
                    PlatformError::Vm("host bundle has no gfxstream backend".to_owned())
                })?;
            let angle_egl = context
                .artifacts
                .host_tools
                .get("angle-egl")
                .ok_or_else(|| {
                    PlatformError::Vm("host bundle has no ANGLE EGL runtime".to_owned())
                })?;
            let angle_gles = context
                .artifacts
                .host_tools
                .get("angle-glesv2")
                .ok_or_else(|| {
                    PlatformError::Vm("host bundle has no ANGLE GLES runtime".to_owned())
                })?;
            let angle_vulkan_loader = context
                .artifacts
                .host_tools
                .get("angle-vulkan-loader")
                .ok_or_else(|| {
                    PlatformError::Vm("host bundle has no ANGLE Vulkan loader".to_owned())
                })?;
            let gfxstream_dir = gfxstream
                .parent()
                .ok_or_else(|| PlatformError::Vm("gfxstream backend has no parent".to_owned()))?;
            let angle_dir = angle_egl
                .parent()
                .ok_or_else(|| PlatformError::Vm("ANGLE EGL runtime has no parent".to_owned()))?;
            if angle_gles.parent() != Some(angle_dir)
                || angle_vulkan_loader.parent() != Some(angle_dir)
            {
                return Err(PlatformError::Vm(
                    "ANGLE EGL, GLES and Vulkan loader runtimes are not colocated".to_owned(),
                ));
            }
            let crosvm_dir = self
                .executable
                .parent()
                .ok_or_else(|| PlatformError::Vm("crosvm executable has no parent".to_owned()))?;
            environment.insert(
                "GFXSTREAM_PATH".to_owned(),
                gfxstream_dir.to_string_lossy().into_owned(),
            );
            environment.insert(
                "GFXSTREAM_ANGLE_ROOT".to_owned(),
                angle_dir.to_string_lossy().into_owned(),
            );
            // Keep Intel validation deterministic on hybrid-GPU laptops. This deliberately avoids
            // gfxstream's default preference for a discrete adapter and never selects SwiftShader.
            environment.insert("ANDROID_EMU_VK_SELECT_GPU".to_owned(), "intel".to_owned());
            let inherited_path = std::env::var_os("PATH").unwrap_or_default();
            let mut search_dirs = vec![
                crosvm_dir.to_owned(),
                gfxstream_dir.to_owned(),
                angle_dir.to_owned(),
            ];
            search_dirs.extend(std::env::split_paths(&inherited_path));
            let search_path = std::env::join_paths(search_dirs)
                .map_err(|error| PlatformError::Vm(format!("construct crosvm PATH: {error}")))?;
            environment.insert(
                "PATH".to_owned(),
                search_path.to_string_lossy().into_owned(),
            );
        }
        let adb_serial = context
            .adb_host_port
            .map(|port| format!("127.0.0.1:{port}"));
        Ok(LaunchPlanV2 {
            schema_version: 2,
            instance_id: spec.id,
            run_id: context.run_id,
            executable: self.executable.clone(),
            arguments,
            environment,
            working_directory: context.run_dir.clone(),
            control_endpoint: context.control_endpoint.clone(),
            frame_endpoint: context.frame_endpoint.clone(),
            keyboard_endpoint: context.keyboard_endpoint.clone(),
            device_endpoints: context.device_endpoints.clone(),
            device_control_endpoints: context.device_control_endpoints.clone(),
            adb_serial,
            guest_cid: context.guest_cid,
        })
    }

    async fn send_key(
        &self,
        keyboard_endpoint: &str,
        key: KeyActionV2,
    ) -> Result<(), PlatformError> {
        let key_code = match key {
            KeyActionV2::Home => 102_u16,
            KeyActionV2::Recent => 0x244_u16,
            KeyActionV2::Back => 158_u16,
            KeyActionV2::Power => 116_u16,
            KeyActionV2::VolumeUp => 115_u16,
            KeyActionV2::VolumeDown => 114_u16,
        };
        let events = key_report(key_code);
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ClientOptions;
            let mut last_error = None;
            for _ in 0..40 {
                match ClientOptions::new().open(keyboard_endpoint) {
                    Ok(mut pipe) => {
                        pipe.write_all(&events)
                            .await
                            .map_err(|source| PlatformError::Io {
                                operation: "write virtio keyboard report",
                                path: PathBuf::from(keyboard_endpoint),
                                source,
                            })?;
                        pipe.flush().await.map_err(|source| PlatformError::Io {
                            operation: "flush virtio keyboard report",
                            path: PathBuf::from(keyboard_endpoint),
                            source,
                        })?;
                        return Ok(());
                    }
                    Err(error) => {
                        last_error = Some(error);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            Err(PlatformError::Vm(format!(
                "connect keyboard named pipe: {}",
                last_error.map_or_else(|| "retry exhausted".to_owned(), |error| error.to_string())
            )))
        }
        #[cfg(unix)]
        {
            let mut endpoints = self.keyboard_streams.lock().await;
            let endpoint = endpoints.get_mut(keyboard_endpoint).ok_or_else(|| {
                PlatformError::Vm("keyboard endpoint was not prepared".to_owned())
            })?;
            if let KeyboardEndpoint::Listening(listener) = endpoint {
                let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                    .await
                    .map_err(|_| {
                        PlatformError::Vm(
                            "crosvm did not connect to the keyboard endpoint".to_owned(),
                        )
                    })?
                    .map_err(|source| PlatformError::Io {
                        operation: "accept virtio keyboard connection",
                        path: PathBuf::from(keyboard_endpoint),
                        source,
                    })?;
                *endpoint = KeyboardEndpoint::Connected(stream);
            }
            let KeyboardEndpoint::Connected(stream) = endpoint else {
                return Err(PlatformError::Vm(
                    "keyboard endpoint state is invalid".to_owned(),
                ));
            };
            stream
                .write_all(&events)
                .await
                .map_err(|source| PlatformError::Io {
                    operation: "write virtio keyboard report",
                    path: PathBuf::from(keyboard_endpoint),
                    source,
                })
        }
    }

    async fn pause(&self, control_endpoint: &str) -> Result<(), PlatformError> {
        self.control(&["suspend".to_owned(), control_endpoint.to_owned()])
            .await
    }

    async fn resume(&self, control_endpoint: &str) -> Result<(), PlatformError> {
        self.control(&["resume".to_owned(), control_endpoint.to_owned()])
            .await
    }

    async fn power_button(&self, control_endpoint: &str) -> Result<(), PlatformError> {
        self.control(&["powerbtn".to_owned(), control_endpoint.to_owned()])
            .await
    }

    async fn replace_display(
        &self,
        control_endpoint: &str,
        display: &DisplayConfigV2,
    ) -> Result<(), PlatformError> {
        self.control(&[
            "gpu".to_owned(),
            "replace-display".to_owned(),
            "--display-id".to_owned(),
            "0".to_owned(),
            "--gpu-display".to_owned(),
            Self::display_argument(display),
            control_endpoint.to_owned(),
        ])
        .await
    }
}

fn safe_key_value_path(value: &str) -> Result<String, PlatformError> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| matches!(byte, b',' | b'\n' | b'\r' | b'[' | b']'))
    {
        return Err(PlatformError::Vm(format!(
            "path cannot be represented in crosvm key-value syntax: {value:?}"
        )));
    }
    Ok(value.to_owned())
}

fn key_report(code: u16) -> [u8; 32] {
    let mut bytes = [0_u8; 32];
    encode_input_event(&mut bytes[0..8], 1, code, 1);
    encode_input_event(&mut bytes[8..16], 0, 0, 0);
    encode_input_event(&mut bytes[16..24], 1, code, 0);
    encode_input_event(&mut bytes[24..32], 0, 0, 0);
    bytes
}

fn encode_input_event(output: &mut [u8], event_type: u16, code: u16, value: i32) {
    output[0..2].copy_from_slice(&event_type.to_le_bytes());
    output[2..4].copy_from_slice(&code.to_le_bytes());
    output[4..8].copy_from_slice(&value.to_le_bytes());
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(reader: R) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .take(CONTROL_OUTPUT_LIMIT + 1)
        .read_to_end(&mut output)
        .await?;
    if output.len() as u64 > CONTROL_OUTPUT_LIMIT {
        output.truncate(usize::try_from(CONTROL_OUTPUT_LIMIT).unwrap_or(usize::MAX));
        output.extend_from_slice(b"\n[output truncated]");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_report_is_little_endian_virtio_input() {
        let report = key_report(102);
        assert_eq!(&report[..8], &[1, 0, 102, 0, 1, 0, 0, 0]);
        assert_eq!(&report[24..], &[0; 8]);
    }

    #[test]
    fn unsafe_key_value_path_is_rejected() {
        assert!(safe_key_value_path("c:/guest,image.img").is_err());
    }

    #[test]
    fn windows_guest_graphics_contract_requires_hardware_angle() {
        let mut spec = InstanceSpecV2::default();
        spec.display.dpi = 420;

        let arguments = guest_kernel_arguments(&spec);

        assert!(arguments.contains(&"androidboot.lcd_density=420".to_owned()));
        for required in [
            "androidboot.cpuvulkan.version=0",
            "androidboot.hardware.egl=angle",
            "androidboot.hardware.gltransport=virtio-gpu-asg",
            "androidboot.hardware.gralloc=minigbm",
            "androidboot.hardware.hwcomposer=ranchu",
            "androidboot.hardware.vulkan=ranchu",
            "androidboot.opengles.version=196609",
        ] {
            assert!(
                arguments.iter().any(|argument| argument == required),
                "Windows HD must require the hardware Guest ANGLE contract: {required}"
            );
        }
    }
}
