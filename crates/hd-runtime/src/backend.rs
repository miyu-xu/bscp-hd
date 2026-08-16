use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hd_core::{
    DeviceSerialEndpointV2, DisplayConfigV2, InstanceSpecV2, KeyActionV2, LaunchPlanV2,
    MAX_SECONDARY_DISPLAYS, SecondaryDisplayConfigV2, TRACKPAD_AXIS_MAX, TrackpadEventV2,
    TrackpadPhaseV2, VsyncModeV2,
};
use hd_platform::{PlatformError, VmBackend, VmLaunchContextV2};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
use tokio::sync::Mutex;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_OUTPUT_LIMIT: u64 = 64 * 1024;

#[cfg(windows)]
fn windows_realtime_vcpu_arguments(cpu_count: u16) -> [String; 2] {
    // crosvm maps --rt-cpus to Windows MMCSS (the built-in Pro Audio task) for each selected
    // vCPU thread. Android's RenderThread and SurfaceFlinger otherwise suffer long scheduling
    // gaps under WHPX even when the broker process itself is AboveNormal. MMCSS retains the
    // system-responsiveness reserve, unlike promoting the whole VM to REALTIME_PRIORITY_CLASS.
    let last_vcpu = cpu_count.saturating_sub(1);
    ["--rt-cpus".to_owned(), format!("0-{last_vcpu}")]
}

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
    #[cfg(target_os = "macos")]
    arguments.extend([
        // Keep both GLES and Vulkan on the gfxstream hardware path. Guest EGL uses the
        // emulator encoder while host ANGLE presents through MoltenVK; CPU Vulkan is disabled.
        "androidboot.cpuvulkan.version=0".to_owned(),
        "androidboot.hardware.gralloc=minigbm".to_owned(),
        "androidboot.hardware.hwcomposer=ranchu".to_owned(),
        "androidboot.hardware.hwcomposer.display_finder_mode=drm".to_owned(),
        "androidboot.hardware.hwcomposer.display_framebuffer_format=rgba".to_owned(),
        "androidboot.hardware.egl=emulation".to_owned(),
        "androidboot.hardware.vulkan=ranchu".to_owned(),
        "androidboot.hardware.gltransport=virtio-gpu-asg".to_owned(),
        "androidboot.opengles.version=196609".to_owned(),
    ]);
    if spec.devices.uwb {
        arguments.push("androidboot.uwbcountrycode=US".to_owned());
    }
    if spec.devices.modem {
        // Cuttlefish's vendor RIL treats an absent port as the explicit no-RIL profile. The
        // fixed HD modem component listens on host-vsock 9697 and the Guest connects directly
        // through crosvm, so enable the RIL only for instances that requested the modem.
        arguments.push("androidboot.modem_simulator_ports=9697".to_owned());
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

#[cfg(windows)]
#[derive(Debug)]
struct TrackpadEndpoint {
    server: tokio::net::windows::named_pipe::NamedPipeServer,
    connected: bool,
}

#[cfg(windows)]
type TrackpadStreams = BTreeMap<String, TrackpadEndpoint>;

/// Direct crosvm adapter. All launch arguments are derived from typed V2 configuration and
/// verified immutable bundles; there is deliberately no free-form argument escape hatch.
#[derive(Debug, Clone)]
pub struct CrosvmBackend {
    executable: PathBuf,
    #[cfg(unix)]
    keyboard_streams: Arc<Mutex<KeyboardStreams>>,
    #[cfg(windows)]
    trackpad_streams: Arc<Mutex<TrackpadStreams>>,
}

impl CrosvmBackend {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            #[cfg(unix)]
            keyboard_streams: Arc::new(Mutex::new(BTreeMap::new())),
            #[cfg(windows)]
            trackpad_streams: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    fn backend_display_size(display: &DisplayConfigV2) -> (u32, u32) {
        #[cfg(target_os = "macos")]
        {
            // Cocoa exports the guest's natural portrait CALayer and the native host rotates that
            // layer to match Android's WMS orientation. Resizing crosvm to the oriented dimensions
            // would rotate the source a second time and desynchronize the virtio touch axes.
            (
                display.width.min(display.height),
                display.width.max(display.height),
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            display.oriented_size()
        }
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

    /// Prepare the caller-owned endpoint before crosvm connects its independent trackpad.
    pub async fn prepare_trackpad_endpoint(&self, endpoint: &str) -> Result<(), PlatformError> {
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;

            if !endpoint.starts_with(r"\\.\pipe\") {
                return Err(PlatformError::Vm(
                    "Windows trackpad endpoint is not a local named pipe".to_owned(),
                ));
            }
            let mut options = ServerOptions::new();
            options
                .first_pipe_instance(true)
                .reject_remote_clients(true);
            let server = hd_platform::create_owner_only_named_pipe(&options, endpoint)?;
            self.trackpad_streams.lock().await.insert(
                endpoint.to_owned(),
                TrackpadEndpoint {
                    server,
                    connected: false,
                },
            );
            Ok(())
        }
        #[cfg(unix)]
        {
            let path = PathBuf::from(endpoint);
            if path.exists() {
                std::fs::remove_file(&path).map_err(|source| PlatformError::Io {
                    operation: "remove stale trackpad socket",
                    path: path.clone(),
                    source,
                })?;
            }
            let listener =
                tokio::net::UnixListener::bind(&path).map_err(|source| PlatformError::Io {
                    operation: "bind trackpad socket",
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
        let (width, height) = Self::backend_display_size(display);
        let renderer_uses_gles = cfg!(target_os = "macos");
        let native_wsi = if cfg!(target_os = "macos") {
            ""
        } else {
            ",wsi=vk"
        };
        let mut displays = vec![format!(
            "[mode=windowed[{width},{height}],dpi=[{dpi},{dpi}],refresh-rate={refresh},hidden=true]",
            dpi = display.dpi,
            refresh = display.refresh_rate_hz,
        )];
        if !cfg!(windows) {
            displays.extend(display.secondary_displays.iter().map(|secondary| {
                format!(
                    "[mode=windowed[{width},{height}],dpi=[{dpi},{dpi}],refresh-rate={refresh},hidden=true]",
                    width = secondary.width,
                    height = secondary.height,
                    dpi = secondary.dpi,
                    refresh = secondary.refresh_rate_hz,
                )
            }));
        }
        // The pinned Windows crosvm advertises the portable multi-display control commands and
        // max-num-displays capacity, but its Win32 display backend rejects more than one display
        // in the initial `--gpu displays=` list. The Worker materializes configured Windows
        // secondaries through `gpu add-displays` immediately after the control socket is live and
        // before Guest readiness. Other platforms keep true launch-time materialization.
        // Reserve the full bounded product capacity so AOSP-compatible hotplug can add a display
        // without restarting the VM. Only configured displays are materialized at cold start.
        let max_num_displays = 1 + MAX_SECONDARY_DISPLAYS;
        let base = format!(
            "backend=gfxstream,max-num-displays={max_num_displays},audio-device-mode=one-global,displays=[{}],context-types=gfxstream-vulkan:gfxstream-composer,angle=true,gles={renderer_uses_gles},vulkan=true{native_wsi}",
            displays.join(",")
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
        let (width, height) = Self::backend_display_size(display);
        format!(
            "mode=windowed[{width},{height}],dpi=[{dpi},{dpi}],refresh-rate={refresh}",
            dpi = display.dpi,
            refresh = display.refresh_rate_hz,
        )
    }

    fn secondary_display_argument(display: &SecondaryDisplayConfigV2) -> String {
        format!(
            "mode=windowed[{width},{height}],dpi=[{dpi},{dpi}],refresh-rate={refresh},hidden=true",
            width = display.width,
            height = display.height,
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
                return Ok(format!("{prefix},type=file,path={output},input={input}"));
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

    async fn control_output(&self, arguments: &[String]) -> Result<Vec<u8>, PlatformError> {
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
            Ok(stdout)
        } else {
            let detail = if stderr.is_empty() { stdout } else { stderr };
            Err(PlatformError::Vm(format!(
                "crosvm control failed with {:?}: {}",
                status.code(),
                String::from_utf8_lossy(&detail).trim()
            )))
        }
    }

    async fn control(&self, arguments: &[String]) -> Result<(), PlatformError> {
        self.control_output(arguments).await.map(|_| ())
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
        ];
        #[cfg(windows)]
        {
            arguments.extend(["--no-balloon".to_owned(), "--no-usb".to_owned()]);
            arguments.extend(windows_realtime_vcpu_arguments(spec.cpu_count));
        }
        #[cfg(target_os = "macos")]
        arguments.extend([
            "--disable-sandbox".to_owned(),
            // The macOS Cocoa display bridge converts native mouse gestures into virtio
            // multitouch reports. Enable its paired input device so Android receives them.
            "--display-window-mouse".to_owned(),
        ]);
        arguments.extend([
            "--socket".to_owned(),
            context.control_endpoint.clone(),
            "--input".to_owned(),
            format!(
                "keyboard[path={}]",
                safe_key_value_path(&context.keyboard_endpoint)?
            ),
        ]);
        #[cfg(windows)]
        arguments.extend(windows_native_touch_arguments(&spec.display));
        match (spec.devices.touchpad, context.trackpad_endpoint.as_deref()) {
            (true, Some(endpoint)) => arguments.extend([
                "--input".to_owned(),
                format!(
                    "trackpad[path={},width={axis},height={axis}]",
                    safe_key_value_path(endpoint)?,
                    axis = TRACKPAD_AXIS_MAX,
                ),
            ]),
            (true, None) => {
                return Err(PlatformError::Vm(
                    "enabled trackpad has no runtime endpoint".to_owned(),
                ));
            }
            (false, Some(_)) => {
                return Err(PlatformError::Vm(
                    "disabled trackpad received a runtime endpoint".to_owned(),
                ));
            }
            (false, None) => {}
        }
        arguments.extend(["--gpu".to_owned(), Self::gpu_argument(&spec.display)]);

        if spec.devices.network {
            arguments.extend([
                "--net".to_owned(),
                network_argument(
                    "primary",
                    &format!(
                        "02:48:{:02x}:{:02x}:{:02x}:00",
                        spec.id.as_bytes()[0],
                        spec.id.as_bytes()[1],
                        spec.id.as_bytes()[2]
                    ),
                    "00:01.1",
                ),
                "--net".to_owned(),
                network_argument(
                    "secondary",
                    &format!(
                        "02:49:{:02x}:{:02x}:{:02x}:00",
                        spec.id.as_bytes()[3],
                        spec.id.as_bytes()[4],
                        spec.id.as_bytes()[5]
                    ),
                    "00:01.2",
                ),
                "--net".to_owned(),
                network_argument(
                    "ethernet",
                    &format!(
                        "02:4a:{:02x}:{:02x}:{:02x}:00",
                        spec.id.as_bytes()[6],
                        spec.id.as_bytes()[7],
                        spec.id.as_bytes()[8]
                    ),
                    "00:01.3",
                ),
            ]);
        }

        if spec.devices.audio {
            #[cfg(windows)]
            let audio_parameters = if matches!(
                spec.host_audio_input,
                hd_core::HostAudioInputV2::DefaultMicrophone
            ) {
                "capture=true,backend=winaudio,num_output_devices=1,num_input_devices=1,num_output_streams=1,num_input_streams=1"
            } else {
                "capture=false,backend=winaudio,num_output_devices=1,num_input_devices=0,num_output_streams=1,num_input_streams=0"
            };
            #[cfg(target_os = "macos")]
            let audio_parameters = if matches!(
                spec.host_audio_input,
                hd_core::HostAudioInputV2::DefaultMicrophone
            ) {
                "capture=true,backend=coreaudio,num_output_devices=1,num_input_devices=1,num_output_streams=1,num_input_streams=1"
            } else {
                "capture=false,backend=coreaudio,num_output_devices=1,num_input_devices=0,num_output_streams=1,num_input_streams=0"
            };
            #[cfg(not(any(windows, target_os = "macos")))]
            let audio_parameters = "capture=false,backend=null,num_output_devices=1,num_input_devices=0,num_output_streams=1,num_input_streams=0";
            arguments.extend(["--virtio-snd".to_owned(), audio_parameters.to_owned()]);
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
        let native_zero_copy_required = native_display_direct && !dev_display_copy_fallback;
        let mut environment = BTreeMap::from([
            (
                "HD_FRAME_REQUIRED".to_owned(),
                if strict_frame_broker { "1" } else { "0" }.to_owned(),
            ),
            (
                "HD_NATIVE_ZERO_COPY_REQUIRED".to_owned(),
                if native_zero_copy_required { "1" } else { "0" }.to_owned(),
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
        #[cfg(target_os = "macos")]
        {
            environment.insert("CROSVM_COCOA_DISPLAY".to_owned(), "1".to_owned());
            environment.insert("HD_DISPLAY_SELECTION".to_owned(), "1".to_owned());
            // Present the gfxstream CAMetalLayer through CAContext/CALayerHost. This is the
            // macOS zero-copy path; no virtio-gpu framebuffer readback is enabled.
            environment.insert("ANGLE_DEFAULT_PLATFORM".to_owned(), "metal".to_owned());
            let search_path = macos_gfxstream_library_search_path(&context.artifacts.host_tools)
                .ok_or_else(|| {
                    PlatformError::Vm(
                        "verified macOS host bundle is missing a gfxstream/ANGLE runtime path"
                            .to_owned(),
                    )
                })?;
            environment.insert("DYLD_LIBRARY_PATH".to_owned(), search_path);
        }
        #[cfg(windows)]
        {
            // Gfxstream owns one zero-copy presentation surface. Keep secondary Android displays
            // registered, but present only the scanout selected by HD instead of composing every
            // display into the primary Player viewport.
            environment.insert("HD_DISPLAY_SELECTION".to_owned(), "1".to_owned());
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
            trackpad_endpoint: context.trackpad_endpoint.clone(),
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

    async fn send_power_key(
        &self,
        keyboard_endpoint: &str,
        control_endpoint: &str,
    ) -> Result<(), PlatformError> {
        #[cfg(windows)]
        {
            let _ = keyboard_endpoint;
            self.power_button(control_endpoint).await
        }
        #[cfg(unix)]
        {
            let _ = control_endpoint;
            self.send_key(keyboard_endpoint, KeyActionV2::Power).await
        }
    }

    async fn send_trackpad(
        &self,
        trackpad_endpoint: &str,
        event: TrackpadEventV2,
    ) -> Result<(), PlatformError> {
        let events = trackpad_report(event)?;
        #[cfg(windows)]
        {
            let mut endpoints = self.trackpad_streams.lock().await;
            let endpoint = endpoints.get_mut(trackpad_endpoint).ok_or_else(|| {
                PlatformError::Vm("trackpad endpoint was not prepared".to_owned())
            })?;
            if !endpoint.connected {
                tokio::time::timeout(Duration::from_secs(2), endpoint.server.connect())
                    .await
                    .map_err(|_| {
                        PlatformError::Vm(
                            "crosvm did not connect to the trackpad endpoint".to_owned(),
                        )
                    })?
                    .map_err(|source| PlatformError::Io {
                        operation: "accept virtio trackpad connection",
                        path: PathBuf::from(trackpad_endpoint),
                        source,
                    })?;
                endpoint.connected = true;
            }
            endpoint
                .server
                .write_all(&events)
                .await
                .map_err(|source| PlatformError::Io {
                    operation: "write virtio trackpad report",
                    path: PathBuf::from(trackpad_endpoint),
                    source,
                })?;
            endpoint
                .server
                .flush()
                .await
                .map_err(|source| PlatformError::Io {
                    operation: "flush virtio trackpad report",
                    path: PathBuf::from(trackpad_endpoint),
                    source,
                })
        }
        #[cfg(unix)]
        {
            let mut endpoints = self.keyboard_streams.lock().await;
            let endpoint = endpoints.get_mut(trackpad_endpoint).ok_or_else(|| {
                PlatformError::Vm("trackpad endpoint was not prepared".to_owned())
            })?;
            if let KeyboardEndpoint::Listening(listener) = endpoint {
                let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                    .await
                    .map_err(|_| {
                        PlatformError::Vm(
                            "crosvm did not connect to the trackpad endpoint".to_owned(),
                        )
                    })?
                    .map_err(|source| PlatformError::Io {
                        operation: "accept virtio trackpad connection",
                        path: PathBuf::from(trackpad_endpoint),
                        source,
                    })?;
                *endpoint = KeyboardEndpoint::Connected(stream);
            }
            let KeyboardEndpoint::Connected(stream) = endpoint else {
                return Err(PlatformError::Vm(
                    "trackpad endpoint state is invalid".to_owned(),
                ));
            };
            stream
                .write_all(&events)
                .await
                .map_err(|source| PlatformError::Io {
                    operation: "write virtio trackpad report",
                    path: PathBuf::from(trackpad_endpoint),
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
        display_id: u32,
        display: &DisplayConfigV2,
    ) -> Result<(), PlatformError> {
        self.control(&[
            "gpu".to_owned(),
            "replace-display".to_owned(),
            "--display-id".to_owned(),
            display_id.to_string(),
            "--gpu-display".to_owned(),
            Self::display_argument(display),
            control_endpoint.to_owned(),
        ])
        .await
    }

    async fn add_secondary_display(
        &self,
        control_endpoint: &str,
        expected_scanout_id: u32,
        display: &SecondaryDisplayConfigV2,
    ) -> Result<(), PlatformError> {
        let maximum_scanout_id = u32::try_from(MAX_SECONDARY_DISPLAYS)
            .expect("secondary display product limit fits in u32");
        if !(1..=maximum_scanout_id).contains(&expected_scanout_id) {
            return Err(PlatformError::Vm(format!(
                "secondary display scanout {expected_scanout_id} is outside product capacity"
            )));
        }
        // Crosvm assigns AddDisplays to the lowest free scanout. Do not bracket this command with
        // ListDisplays: the pinned macOS crosvm can consume the config-change edge in that order,
        // leaving the scanout visible to control queries but absent from Android HWC. The Worker
        // also keeps guest display queries behind the Android HWC settling window.
        self.control(&[
            "gpu".to_owned(),
            "add-displays".to_owned(),
            "--gpu-display".to_owned(),
            Self::secondary_display_argument(display),
            control_endpoint.to_owned(),
        ])
        .await
    }

    async fn remove_display(
        &self,
        control_endpoint: &str,
        display_id: u32,
    ) -> Result<(), PlatformError> {
        self.control(&[
            "gpu".to_owned(),
            "remove-displays".to_owned(),
            "--display-id".to_owned(),
            display_id.to_string(),
            control_endpoint.to_owned(),
        ])
        .await
    }

    async fn replace_secondary_display(
        &self,
        control_endpoint: &str,
        display_id: u32,
        display: &SecondaryDisplayConfigV2,
    ) -> Result<(), PlatformError> {
        self.control(&[
            "gpu".to_owned(),
            "replace-display".to_owned(),
            "--display-id".to_owned(),
            display_id.to_string(),
            "--gpu-display".to_owned(),
            Self::secondary_display_argument(display),
            control_endpoint.to_owned(),
        ])
        .await
    }
}

#[cfg(target_os = "macos")]
fn macos_gfxstream_library_search_path(host_tools: &BTreeMap<String, PathBuf>) -> Option<String> {
    let mut directories = Vec::new();
    for role in [
        "gfxstream-backend",
        "angle-egl",
        "angle-glesv2",
        "angle-vulkan-loader",
    ] {
        let directory = host_tools.get(role)?.parent()?.to_owned();
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    std::env::join_paths(directories)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
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

#[cfg(windows)]
fn windows_native_touch_arguments(display: &DisplayConfigV2) -> Vec<String> {
    // Crosvm's Win32 display procedure routes pointer gestures to touchscreen event devices by
    // scanout id. Unlike macOS, the generic Windows product does not create those devices
    // implicitly, so preserve display order here: primary scanout 0 first, followed by each
    // configured secondary scanout. The path is an identity token on Windows; crosvm creates the
    // native broker pipe internally for multi-touch devices.
    let (width, height) = CrosvmBackend::backend_display_size(display);
    let mut arguments = vec![
        "--input".to_owned(),
        format!(
            "multi-touch[path=hd-native-window-0,width={width},height={height},name=HD Android Display 0]"
        ),
    ];
    for (index, secondary) in display.secondary_displays.iter().enumerate() {
        let scanout_id = index + 1;
        arguments.extend([
            "--input".to_owned(),
            format!(
                "multi-touch[path=hd-native-window-{scanout_id},width={width},height={height},name=HD Android Display {scanout_id}]",
                width = secondary.width,
                height = secondary.height,
            ),
        ]);
    }
    arguments
}

fn network_argument(interface_name: &str, mac: &str, pci_address: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        network_argument_for_macos(
            interface_name,
            mac,
            pci_address,
            macos_socket_vmnet_path().as_deref(),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = interface_name;
        format!("mac={mac},pci-address={pci_address}")
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn macos_socket_vmnet_path() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("HD_SOCKET_VMNET").filter(|value| !value.is_empty())
    {
        let path = PathBuf::from(configured);
        return path.exists().then_some(path);
    }
    let system_socket = PathBuf::from("/var/run/socket_vmnet");
    system_socket.exists().then_some(system_socket)
}

#[cfg(target_os = "macos")]
fn network_argument_for_macos(
    interface_name: &str,
    mac: &str,
    pci_address: &str,
    socket_vmnet: Option<&Path>,
) -> String {
    // Keep the AOSP Cuttlefish Wi-Fi and mobile transports in their original
    // slots. A dedicated third virtio-net device is discovered as eth2 and is
    // the unrestricted Ethernet uplink. Reusing and renaming eth1 after boot
    // repeatedly recreated NetworkAgent/NetworkMonitor until NetworkStack hit
    // Android's per-process receiver limit and restarted system_server.
    let tap_name = if interface_name == "ethernet" {
        socket_vmnet.map_or_else(
            || format!("hd-offline-{interface_name}"),
            |path| format!("hd-socket-vmnet:{}", path.to_string_lossy()),
        )
    } else {
        format!("hd-offline-{interface_name}")
    };
    format!("tap-name={tap_name},mac={mac},pci-address={pci_address}")
}

fn key_report(code: u16) -> [u8; 32] {
    let mut bytes = [0_u8; 32];
    encode_input_event(&mut bytes[0..8], 1, code, 1);
    encode_input_event(&mut bytes[8..16], 0, 0, 0);
    encode_input_event(&mut bytes[16..24], 1, code, 0);
    encode_input_event(&mut bytes[24..32], 0, 0, 0);
    bytes
}

fn trackpad_report(event: TrackpadEventV2) -> Result<Vec<u8>, PlatformError> {
    const EV_SYN: u16 = 0;
    const EV_KEY: u16 = 1;
    const EV_ABS: u16 = 3;
    const SYN_REPORT: u16 = 0;
    const ABS_X: u16 = 0;
    const ABS_Y: u16 = 1;
    const BTN_TOOL_FINGER: u16 = 0x145;
    const BTN_TOUCH: u16 = 0x14a;

    if event.x > TRACKPAD_AXIS_MAX || event.y > TRACKPAD_AXIS_MAX {
        return Err(PlatformError::Vm(
            "trackpad coordinates exceed the normalized input range".to_owned(),
        ));
    }
    let mut events = Vec::with_capacity(5 * 8);
    let mut push = |event_type, code, value| {
        let offset = events.len();
        events.resize(offset + 8, 0);
        encode_input_event(&mut events[offset..offset + 8], event_type, code, value);
    };
    push(EV_ABS, ABS_X, i32::from(event.x));
    push(EV_ABS, ABS_Y, i32::from(event.y));
    match event.phase {
        TrackpadPhaseV2::Down => {
            push(EV_KEY, BTN_TOOL_FINGER, 1);
            push(EV_KEY, BTN_TOUCH, 1);
        }
        TrackpadPhaseV2::Move => {}
        TrackpadPhaseV2::Up => {
            push(EV_KEY, BTN_TOUCH, 0);
            push(EV_KEY, BTN_TOOL_FINGER, 0);
        }
    }
    push(EV_SYN, SYN_REPORT, 0);
    Ok(events)
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

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_network_argument_selects_offline_backend() {
        assert_eq!(
            network_argument_for_macos("primary", "02:48:00:00:00:00", "00:01.1", None),
            "tap-name=hd-offline-primary,mac=02:48:00:00:00:00,pci-address=00:01.1"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_network_argument_selects_socket_vmnet_backend() {
        assert_eq!(
            network_argument_for_macos(
                "ethernet",
                "02:48:00:00:00:00",
                "00:01.1",
                Some(Path::new("/var/run/socket_vmnet")),
            ),
            "tap-name=hd-socket-vmnet:/var/run/socket_vmnet,mac=02:48:00:00:00:00,pci-address=00:01.1"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_primary_network_stays_offline_with_socket_vmnet() {
        assert_eq!(
            network_argument_for_macos(
                "primary",
                "02:48:00:00:00:00",
                "00:01.1",
                Some(Path::new("/var/run/socket_vmnet")),
            ),
            "tap-name=hd-offline-primary,mac=02:48:00:00:00:00,pci-address=00:01.1"
        );
    }

    #[cfg(windows)]
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

    #[cfg(windows)]
    #[test]
    fn windows_reserves_multi_display_capacity_but_boots_one_scanout() {
        let mut display = DisplayConfigV2::default();
        display.secondary_displays.push(SecondaryDisplayConfigV2 {
            id: uuid::Uuid::new_v4(),
            name: "副屏 1".to_owned(),
            width: 1280,
            height: 720,
            dpi: 240,
            refresh_rate_hz: 60,
        });

        let argument = CrosvmBackend::gpu_argument(&display);

        assert!(argument.contains("max-num-displays=4"));
        assert_eq!(argument.matches("mode=windowed").count(), 1);
        assert!(argument.contains("mode=windowed[1080,1920]"));
        assert!(!argument.contains("mode=windowed[1280,720]"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_touch_devices_follow_scanout_order_and_size() {
        let mut display = DisplayConfigV2::default();
        display.secondary_displays.push(SecondaryDisplayConfigV2 {
            id: uuid::Uuid::new_v4(),
            name: "副屏 1".to_owned(),
            width: 1280,
            height: 720,
            dpi: 240,
            refresh_rate_hz: 60,
        });

        let arguments = windows_native_touch_arguments(&display);

        assert_eq!(
            arguments.iter().filter(|value| *value == "--input").count(),
            2
        );
        assert_eq!(
            arguments[1],
            "multi-touch[path=hd-native-window-0,width=1080,height=1920,name=HD Android Display 0]"
        );
        assert_eq!(
            arguments[3],
            "multi-touch[path=hd-native-window-1,width=1280,height=720,name=HD Android Display 1]"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_android_vcpus_use_bounded_mmcss_realtime_threads() {
        assert_eq!(
            windows_realtime_vcpu_arguments(4),
            ["--rt-cpus", "0-3"].map(str::to_owned)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_guest_graphics_contract_disables_cpu_rendering() {
        let arguments = guest_kernel_arguments(&InstanceSpecV2::default());

        for required in [
            "androidboot.cpuvulkan.version=0",
            "androidboot.hardware.egl=emulation",
            "androidboot.hardware.gltransport=virtio-gpu-asg",
            "androidboot.hardware.vulkan=ranchu",
        ] {
            assert!(
                arguments.iter().any(|argument| argument == required),
                "macOS HD must keep guest graphics on gfxstream hardware: {required}"
            );
        }

        let gpu = CrosvmBackend::gpu_argument(&InstanceSpecV2::default().display);
        assert!(gpu.contains("gles=true"));
        assert!(!gpu.contains("wsi=vk"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_gfxstream_search_path_includes_angle_runtime() {
        let host_tools = BTreeMap::from([
            (
                "gfxstream-backend".to_owned(),
                PathBuf::from("/bundle/lib/libgfxstream_backend.dylib"),
            ),
            (
                "angle-egl".to_owned(),
                PathBuf::from("/bundle/gfx/angle/libEGL.dylib"),
            ),
            (
                "angle-glesv2".to_owned(),
                PathBuf::from("/bundle/gfx/angle/libGLESv2.dylib"),
            ),
            (
                "angle-vulkan-loader".to_owned(),
                PathBuf::from("/bundle/lib/libvulkan.dylib"),
            ),
        ]);
        let search_path = macos_gfxstream_library_search_path(&host_tools)
            .expect("bundle layout should produce a search path");
        let entries = std::env::split_paths(&search_path).collect::<Vec<_>>();

        assert_eq!(
            entries,
            [
                PathBuf::from("/bundle/lib"),
                PathBuf::from("/bundle/gfx/angle")
            ]
        );
    }
}
