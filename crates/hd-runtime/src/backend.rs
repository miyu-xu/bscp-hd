use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hd_core::{DeviceSerialEndpointV2, DisplayConfigV2, KeyActionV2, LaunchPlanV2, VsyncModeV2};
use hd_platform::{PlatformError, VmBackend, VmLaunchContextV2};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
#[cfg(unix)]
use tokio::sync::Mutex;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_OUTPUT_LIMIT: u64 = 64 * 1024;

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
        format!(
            "backend=gfxstream,displays=[[mode=windowed[{width},{height}],dpi=[{dpi},{dpi}],refresh-rate={refresh}]],context-types=gfxstream-vulkan:gfxstream-composer,angle=true,gles=false,vulkan=true,wsi=vk,external-blob=true,udmabuf=false,renderer-features=VulkanAllocateHostMemory:enabled",
            dpi = display.dpi,
            refresh = display.refresh_rate_hz,
        )
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
            "info".to_owned(),
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

        let serial_roles: [(u8, u8, &str); 15] = [
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

        let mut kernel_arguments = vec!["console=hvc0,ttyS0".to_owned(), "init=/init".to_owned()];
        kernel_arguments.extend(spec.boot.kernel_arguments());
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

        let environment = BTreeMap::from([
            ("HD_FRAME_REQUIRED".to_owned(), "1".to_owned()),
            ("HD_FRAME_PROTOCOL".to_owned(), "2".to_owned()),
            (
                "HD_FRAME_BROKER_V2".to_owned(),
                context.frame_endpoint.clone(),
            ),
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
}
