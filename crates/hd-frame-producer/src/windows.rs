use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use ash::{Entry, vk};
use hd_core::{
    COMPONENT_PROTOCOL_VERSION, FRAME_PROTOCOL_VERSION, FormalComponentConfigurationV2,
    FormalComponentLaunchV2, FormalComponentReadyV2, FrameDescriptorV2, FrameFormatV2,
    FrameReadyMarkerV2, FrameTransportKindV2,
};
use hd_frame::{ExternalFrameHandle, FRAME_BUFFER_COUNT, FramePoolV2};
use hd_platform::FrameInteropProbeV2;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

const WIRE_MAGIC: u32 = 0x3246_4448;
const WIRE_VERSION: u16 = 2;
const KIND_REGISTER: u16 = 1;
const KIND_PUBLISH: u16 = 2;
const KIND_RELEASE: u16 = 3;
const HEADER_SIZE: usize = 12;
const REGISTER_SIZE: usize = 72;
const PUBLISH_SIZE: usize = 28;
const RELEASE_SIZE: usize = 28;
const VIRGL_FORMAT_B8G8R8A8_UNORM: u32 = 1;
const VIRGL_FORMAT_B8G8R8X8_UNORM: u32 = 2;

pub(crate) fn probe() -> FrameInteropProbeV2 {
    match VulkanContext::open(None) {
        Ok(context) => FrameInteropProbeV2 {
            component_protocol_version: COMPONENT_PROTOCOL_VERSION,
            service_lifecycle: true,
            transport: FrameTransportKindV2::VulkanWin32,
            memory_export: true,
            explicit_sync: true,
            same_adapter: true,
            validation_clean: true,
            detail: "Vulkan opaque Win32 memory import and instance-scoped fence/release protocol are available"
                .to_owned(),
            properties: BTreeMap::from([
                ("adapter_luid".to_owned(), hex::encode(context.device_luid)),
                ("buffer_count".to_owned(), FRAME_BUFFER_COUNT.to_string()),
                ("copy_path".to_owned(), "none".to_owned()),
                ("sync".to_owned(), "gfxstream-gpu-fence+broker-release".to_owned()),
            ]),
        },
        Err(error) => FrameInteropProbeV2 {
            component_protocol_version: COMPONENT_PROTOCOL_VERSION,
            service_lifecycle: true,
            transport: FrameTransportKindV2::VulkanWin32,
            memory_export: false,
            explicit_sync: false,
            same_adapter: false,
            validation_clean: false,
            detail: format!("Windows Vulkan external-memory probe failed: {error:#}"),
            properties: BTreeMap::new(),
        },
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn serve(
    launch_path: &Path,
    launch: FormalComponentLaunchV2,
    launch_bytes: Vec<u8>,
) -> Result<()> {
    ensure!(
        launch.protocol_version == COMPONENT_PROTOCOL_VERSION
            && launch.component == "frame-producer",
        "formal frame launch protocol or component mismatch"
    );
    ensure!(
        launch.component_ready_marker.parent() == launch_path.parent()
            && launch
                .component_ready_marker
                .file_name()
                .and_then(|name| name.to_str())
                == Some("frame-producer-ready-v2.json"),
        "formal frame component marker is outside the launch directory"
    );
    let (endpoint, frame_marker, generation, transport, display) = match &launch.configuration {
        FormalComponentConfigurationV2::FrameBroker {
            broker_endpoint,
            frame_ready_marker,
            generation,
            transport,
            display,
        } => (
            broker_endpoint.clone(),
            frame_ready_marker.clone(),
            *generation,
            *transport,
            display.clone(),
        ),
        _ => bail!("frame-producer requires a frame_broker configuration"),
    };
    ensure!(
        generation > 0 && transport == FrameTransportKindV2::VulkanWin32,
        "frame launch generation or transport is invalid"
    );
    ensure!(
        frame_marker.parent() == launch_path.parent().and_then(Path::parent),
        "frame ready marker is outside the run directory"
    );
    let (signed_width, signed_height) = display.oriented_size();

    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(true)
        .reject_remote_clients(true);
    let mut pipe = hd_platform::create_owner_only_named_pipe(&options, &endpoint)
        .context("create owner-only HD frame broker named pipe")?;
    publish_component_ready(&launch, &launch_bytes)?;
    pipe.connect()
        .await
        .context("accept gfxstream frame producer")?;

    let mut context: Option<Arc<VulkanContext>> = None;
    let mut registrations = BTreeMap::new();
    let mut negotiated_size = None;
    let mut pool = None;
    let mut published_marker = false;
    let mut last_acquire_value = 0_u64;
    let metrics_path = frame_marker.with_file_name("frame-metrics-v2.json");
    let mut last_metrics_write = Instant::now();
    let mut last_metrics_frames = 0_u64;
    loop {
        let header = read_header(&mut pipe).await?;
        match header.kind {
            KIND_REGISTER => {
                ensure!(
                    header.size as usize == REGISTER_SIZE,
                    "invalid frame registration size"
                );
                ensure!(
                    pool.is_none(),
                    "frame registration arrived after negotiation completed"
                );
                let registration = read_registration(&mut pipe, header).await?;
                ensure!(
                    usize::from(registration.buffer_id) < FRAME_BUFFER_COUNT,
                    "frame registration buffer id is out of range"
                );
                ensure!(
                    registration.width == signed_width
                        && registration.height == signed_height
                        && registration.stride_bytes >= registration.width.saturating_mul(4),
                    "gfxstream frame dimensions do not match the signed launch display"
                );
                if !is_supported_frame_format(&registration) {
                    eprintln!(
                        "rejecting unsupported gfxstream frame format: buffer_id={} virgl_format={} image_format={:?} width={} height={} stride={}",
                        registration.buffer_id,
                        registration.virgl_format,
                        registration.image_format,
                        registration.width,
                        registration.height,
                        registration.stride_bytes
                    );
                    write_release(&mut pipe, registration.buffer_id, false, 0).await?;
                    continue;
                }
                ensure!(
                    registration.luid_valid,
                    "gfxstream did not publish a valid adapter LUID"
                );
                let vulkan = if let Some(existing) = &context {
                    ensure!(
                        existing.device_luid == registration.device_luid,
                        "gfxstream changed Vulkan adapters during negotiation"
                    );
                    Arc::clone(existing)
                } else {
                    let opened = Arc::new(VulkanContext::open(Some(registration.device_luid))?);
                    context = Some(Arc::clone(&opened));
                    opened
                };
                let imported = match vulkan.import(&registration) {
                    Ok(imported) => Arc::new(imported) as Arc<dyn ExternalFrameHandle>,
                    Err(error) => {
                        eprintln!(
                            "rejecting non-importable gfxstream frame: buffer_id={} virgl_format={} image_format={:?} width={} height={} stride={} error={error:#}",
                            registration.buffer_id,
                            registration.virgl_format,
                            registration.image_format,
                            registration.width,
                            registration.height,
                            registration.stride_bytes
                        );
                        write_release(&mut pipe, registration.buffer_id, false, 0).await?;
                        continue;
                    }
                };
                match negotiated_size {
                    Some((width, height)) => ensure!(
                        registration.width == width && registration.height == height,
                        "gfxstream frame dimensions changed during negotiation"
                    ),
                    None => negotiated_size = Some((registration.width, registration.height)),
                }
                let descriptor = FrameDescriptorV2 {
                    protocol_version: FRAME_PROTOCOL_VERSION,
                    instance_id: launch.instance_id,
                    generation,
                    buffer_id: registration.buffer_id,
                    transport,
                    format: FrameFormatV2::Bgra8Srgb,
                    width: registration.width,
                    height: registration.height,
                    stride_bytes: registration.stride_bytes,
                    acquire_value: 0,
                    release_value: 0,
                    out_of_band_handle_count: 1,
                };
                ensure!(
                    registrations
                        .insert(registration.buffer_id, (descriptor, imported))
                        .is_none(),
                    "duplicate frame registration"
                );
                write_release(&mut pipe, registration.buffer_id, true, 0).await?;
                if registrations.len() == FRAME_BUFFER_COUNT {
                    let descriptors = std::mem::take(&mut registrations).into_values().collect();
                    let probe = FrameInteropProbeV2 {
                        component_protocol_version: COMPONENT_PROTOCOL_VERSION,
                        service_lifecycle: true,
                        transport,
                        memory_export: true,
                        explicit_sync: true,
                        same_adapter: true,
                        validation_clean: true,
                        detail: "three gfxstream Vulkan Win32 images imported without copy"
                            .to_owned(),
                        properties: BTreeMap::new(),
                    };
                    let created_pool = FramePoolV2::create(&probe, descriptors)?;
                    publish_frame_metrics(&metrics_path, &created_pool.metrics());
                    pool = Some(created_pool);
                    last_metrics_write = Instant::now();
                    last_metrics_frames = 0;
                    publish_frame_ready(&launch, &frame_marker, generation, transport)?;
                    published_marker = true;
                }
            }
            KIND_PUBLISH => {
                ensure!(
                    header.size as usize == PUBLISH_SIZE,
                    "invalid frame publish size"
                );
                let publication = read_publication(&mut pipe, header).await?;
                ensure!(
                    publication.acquire_value > last_acquire_value,
                    "gfxstream acquire values are not globally monotonic"
                );
                if let Some(pool) = pool.as_ref() {
                    pool.producer_acquire(publication.buffer_id)?;
                    pool.producer_publish(publication.buffer_id, publication.acquire_value)?;
                    pool.consumer_imported(publication.buffer_id)?;
                    pool.consumer_release(publication.buffer_id, publication.acquire_value, true)?;
                    if last_metrics_write.elapsed() >= Duration::from_secs(1) {
                        let mut metrics = pool.metrics();
                        let elapsed_ms = last_metrics_write.elapsed().as_millis().max(1);
                        let interval_frames =
                            metrics.produced_frames.saturating_sub(last_metrics_frames);
                        let fps_milli = u128::from(interval_frames)
                            .saturating_mul(1_000_000)
                            .checked_div(elapsed_ms)
                            .unwrap_or(0);
                        metrics.fps_milli = u32::try_from(fps_milli.min(u128::from(u32::MAX)))?;
                        last_metrics_frames = metrics.produced_frames;
                        publish_frame_metrics(&metrics_path, &metrics);
                        last_metrics_write = Instant::now();
                    }
                } else {
                    // gfxstream discovers swapchain color buffers as they are first posted. The
                    // first post therefore necessarily precedes registration of buffers 1 and 2.
                    // Import and release those bootstrap posts without copying, then seed the
                    // strict three-buffer pool with their exact synchronization values once all
                    // registrations have arrived.
                    if let Some((descriptor, _)) = registrations.get_mut(&publication.buffer_id) {
                        descriptor.acquire_value = publication.acquire_value;
                        descriptor.release_value = publication.acquire_value;
                    } else {
                        eprintln!(
                            "rejecting publication for non-imported gfxstream frame: buffer_id={} acquire_value={}",
                            publication.buffer_id, publication.acquire_value
                        );
                        write_release(
                            &mut pipe,
                            publication.buffer_id,
                            false,
                            publication.acquire_value,
                        )
                        .await?;
                        last_acquire_value = publication.acquire_value;
                        continue;
                    }
                }
                last_acquire_value = publication.acquire_value;
                write_release(
                    &mut pipe,
                    publication.buffer_id,
                    true,
                    publication.acquire_value,
                )
                .await?;
            }
            _ => bail!("unsupported HD frame wire message {}", header.kind),
        }
        ensure!(
            !published_marker || frame_marker.is_file(),
            "frame ready marker disappeared while the producer was active"
        );
    }
}

fn publish_component_ready(launch: &FormalComponentLaunchV2, launch_bytes: &[u8]) -> Result<()> {
    let ready = FormalComponentReadyV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        component: launch.component.clone(),
        instance_id: launch.instance_id,
        run_id: launch.run_id,
        launch_sha256: hex::encode(Sha256::digest(launch_bytes)),
        pid: std::process::id(),
        process_start_marker: hd_platform::process_start_marker(std::process::id())?,
    };
    hd_platform::write_owner_only(
        &launch.component_ready_marker,
        &serde_json::to_vec_pretty(&ready)?,
    )?;
    Ok(())
}

fn publish_frame_ready(
    launch: &FormalComponentLaunchV2,
    path: &Path,
    generation: u64,
    transport: FrameTransportKindV2,
) -> Result<()> {
    let marker = FrameReadyMarkerV2 {
        protocol_version: FRAME_PROTOCOL_VERSION,
        instance_id: launch.instance_id,
        run_id: launch.run_id,
        producer_pid: std::process::id(),
        producer_process_start_marker: hd_platform::process_start_marker(std::process::id())?,
        generation,
        transport,
        buffer_count: u8::try_from(FRAME_BUFFER_COUNT)?,
        memory_export: true,
        explicit_sync: true,
        same_adapter: true,
        validation_clean: true,
        cpu_readback_bytes: 0,
        software_blit_count: 0,
    };
    hd_platform::write_owner_only(path, &serde_json::to_vec_pretty(&marker)?)?;
    Ok(())
}

fn publish_frame_metrics(path: &Path, metrics: &hd_core::FrameMetricsV2) {
    let result = serde_json::to_vec_pretty(&metrics)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| hd_platform::write_owner_only(path, &bytes).map_err(anyhow::Error::from));
    if let Err(error) = result {
        // Metrics are advisory and Windows readers may temporarily hold the previous file without
        // FILE_SHARE_DELETE.  Preserve the last complete snapshot and retry on the next interval;
        // a UI polling the metrics file must never tear down the guest display pipeline.
        eprintln!(
            "frame metrics update deferred: path={} error={error:#}",
            path.display()
        );
    }
}

#[derive(Debug, Clone, Copy)]
struct WireHeader {
    kind: u16,
    size: u32,
}

#[derive(Debug)]
struct Registration {
    buffer_id: u8,
    dedicated: bool,
    linear: bool,
    luid_valid: bool,
    memory_type_index: u32,
    width: u32,
    height: u32,
    stride_bytes: u32,
    virgl_format: u32,
    image_create_flags: vk::ImageCreateFlags,
    image_usage: vk::ImageUsageFlags,
    image_format: vk::Format,
    allocation_size: u64,
    memory_handle: OwnedHandle,
    device_luid: [u8; vk::LUID_SIZE],
}

#[derive(Debug, Clone, Copy)]
struct Publication {
    buffer_id: u8,
    acquire_value: u64,
}

async fn read_header(pipe: &mut NamedPipeServer) -> Result<WireHeader> {
    let mut bytes = [0_u8; HEADER_SIZE];
    pipe.read_exact(&mut bytes).await?;
    ensure!(
        u32::from_le_bytes(bytes[0..4].try_into()?) == WIRE_MAGIC,
        "frame wire magic mismatch"
    );
    ensure!(
        u16::from_le_bytes(bytes[4..6].try_into()?) == WIRE_VERSION,
        "frame wire version mismatch"
    );
    Ok(WireHeader {
        kind: u16::from_le_bytes(bytes[6..8].try_into()?),
        size: u32::from_le_bytes(bytes[8..12].try_into()?),
    })
}

async fn read_registration(
    pipe: &mut NamedPipeServer,
    _header: WireHeader,
) -> Result<Registration> {
    let mut bytes = [0_u8; REGISTER_SIZE - HEADER_SIZE];
    pipe.read_exact(&mut bytes).await?;
    let raw_handle = isize::try_from(u64::from_le_bytes(bytes[44..52].try_into()?))
        .context("duplicated Win32 memory handle exceeds pointer width")?;
    ensure!(raw_handle != 0, "invalid duplicated Win32 memory handle");
    Ok(Registration {
        buffer_id: bytes[0],
        dedicated: bytes[1] != 0,
        linear: bytes[2] != 0,
        luid_valid: bytes[3] != 0,
        memory_type_index: u32::from_le_bytes(bytes[4..8].try_into()?),
        width: u32::from_le_bytes(bytes[8..12].try_into()?),
        height: u32::from_le_bytes(bytes[12..16].try_into()?),
        stride_bytes: u32::from_le_bytes(bytes[16..20].try_into()?),
        virgl_format: u32::from_le_bytes(bytes[20..24].try_into()?),
        image_create_flags: vk::ImageCreateFlags::from_raw(u32::from_le_bytes(
            bytes[24..28].try_into()?,
        )),
        image_usage: vk::ImageUsageFlags::from_raw(u32::from_le_bytes(bytes[28..32].try_into()?)),
        image_format: vk::Format::from_raw(i32::from_le_bytes(bytes[32..36].try_into()?)),
        allocation_size: u64::from_le_bytes(bytes[36..44].try_into()?),
        memory_handle: OwnedHandle(raw_handle as HANDLE),
        device_luid: bytes[52..60].try_into()?,
    })
}

async fn read_publication(pipe: &mut NamedPipeServer, _header: WireHeader) -> Result<Publication> {
    let mut bytes = [0_u8; PUBLISH_SIZE - HEADER_SIZE];
    pipe.read_exact(&mut bytes).await?;
    Ok(Publication {
        buffer_id: bytes[0],
        acquire_value: u64::from_le_bytes(bytes[8..16].try_into()?),
    })
}

async fn write_release(
    pipe: &mut NamedPipeServer,
    buffer_id: u8,
    accepted: bool,
    release_value: u64,
) -> Result<()> {
    let mut bytes = [0_u8; RELEASE_SIZE];
    bytes[0..4].copy_from_slice(&WIRE_MAGIC.to_le_bytes());
    bytes[4..6].copy_from_slice(&WIRE_VERSION.to_le_bytes());
    bytes[6..8].copy_from_slice(&KIND_RELEASE.to_le_bytes());
    bytes[8..12].copy_from_slice(&u32::try_from(RELEASE_SIZE)?.to_le_bytes());
    bytes[12] = buffer_id;
    bytes[13] = u8::from(accepted);
    bytes[20..28].copy_from_slice(&release_value.to_le_bytes());
    pipe.write_all(&bytes).await?;
    pipe.flush().await?;
    Ok(())
}

#[derive(Debug)]
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn is_supported_frame_format(registration: &Registration) -> bool {
    matches!(
        registration.virgl_format,
        VIRGL_FORMAT_B8G8R8A8_UNORM | VIRGL_FORMAT_B8G8R8X8_UNORM
    ) || is_supported_vulkan_format(registration.image_format)
}

fn is_supported_vulkan_format(format: vk::Format) -> bool {
    matches!(
        format,
        vk::Format::B8G8R8A8_UNORM
            | vk::Format::B8G8R8A8_SRGB
            | vk::Format::R8G8B8A8_UNORM
            | vk::Format::R8G8B8A8_SRGB
    )
}

struct VulkanContext {
    _entry: Entry,
    instance: ash::Instance,
    device: ash::Device,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    device_luid: [u8; vk::LUID_SIZE],
}

impl fmt::Debug for VulkanContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VulkanContext")
            .field("device_luid", &hex::encode(self.device_luid))
            .finish_non_exhaustive()
    }
}

impl VulkanContext {
    fn open(expected_luid: Option<[u8; vk::LUID_SIZE]>) -> Result<Self> {
        let entry = unsafe { Entry::load() }.context("load Vulkan loader")?;
        let application_name = c"hd-frame-producer";
        let application = vk::ApplicationInfo::default()
            .application_name(application_name)
            .application_version(1)
            .engine_name(application_name)
            .engine_version(1)
            .api_version(vk::API_VERSION_1_1);
        let instance_info = vk::InstanceCreateInfo::default().application_info(&application);
        let instance = unsafe { entry.create_instance(&instance_info, None) }
            .context("create Vulkan instance")?;

        let mut selected = None;
        for physical_device in unsafe { instance.enumerate_physical_devices() }? {
            let extensions =
                unsafe { instance.enumerate_device_extension_properties(physical_device) }?;
            let names = extensions
                .iter()
                .map(|extension| unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) })
                .collect::<BTreeSet<_>>();
            if !names.contains(ash::khr::external_memory_win32::NAME) {
                continue;
            }
            let mut id = vk::PhysicalDeviceIDProperties::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
            unsafe { instance.get_physical_device_properties2(physical_device, &mut properties) };
            if id.device_luid_valid == vk::FALSE {
                continue;
            }
            let luid: [u8; vk::LUID_SIZE] = id.device_luid;
            if expected_luid.is_none_or(|expected| expected == luid) {
                selected = Some((physical_device, luid));
                break;
            }
        }
        let (physical_device, device_luid) =
            selected.context("no matching Vulkan external-memory Win32 adapter")?;
        let queue_family = u32::try_from(
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) }
                .iter()
                .position(|properties| properties.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .context("matching Vulkan adapter has no graphics queue")?,
        )?;
        let priorities = [1.0_f32];
        let queue = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let extension_names = [ash::khr::external_memory_win32::NAME.as_ptr()];
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue))
            .enabled_extension_names(&extension_names);
        let device = unsafe { instance.create_device(physical_device, &device_info, None) }
            .context("create Vulkan external-memory device")?;
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        Ok(Self {
            _entry: entry,
            instance,
            device,
            memory_properties,
            device_luid,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn import(self: &Arc<Self>, registration: &Registration) -> Result<ImportedFrame> {
        ensure!(
            is_supported_vulkan_format(registration.image_format),
            "gfxstream Vulkan image format is unsupported: {:?}",
            registration.image_format
        );
        let tiling = if registration.linear {
            vk::ImageTiling::LINEAR
        } else {
            vk::ImageTiling::OPTIMAL
        };
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
        let image_info = vk::ImageCreateInfo::default()
            .push_next(&mut external)
            .image_type(vk::ImageType::TYPE_2D)
            .flags(registration.image_create_flags)
            .format(registration.image_format)
            .extent(vk::Extent3D {
                width: registration.width,
                height: registration.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(tiling)
            .usage(registration.image_usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { self.device.create_image(&image_info, None) }
            .context("create Vulkan image for imported gfxstream memory")?;
        let requirements = unsafe { self.device.get_image_memory_requirements(image) };
        ensure!(
            registration.allocation_size >= requirements.size,
            "gfxstream allocation is smaller than the recreated Vulkan image"
        );
        let memory_type_index = registration.memory_type_index;
        ensure!(
            memory_type_index < self.memory_properties.memory_type_count,
            "gfxstream memory type index is outside the matching adapter's memory types"
        );
        ensure!(
            requirements.memory_type_bits & (1 << memory_type_index) != 0,
            "gfxstream memory type is incompatible with the recreated Vulkan image"
        );
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import = vk::ImportMemoryWin32HandleInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32)
            .handle(registration.memory_handle.0 as isize);
        if registration.dedicated {
            import.p_next = std::ptr::from_mut(&mut dedicated).cast();
        }
        let allocation_info = vk::MemoryAllocateInfo::default()
            .allocation_size(registration.allocation_size)
            .memory_type_index(memory_type_index)
            .push_next(&mut import);
        let memory = match unsafe { self.device.allocate_memory(&allocation_info, None) } {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { self.device.destroy_image(image, None) };
                return Err(error).context("import gfxstream Win32 memory");
            }
        };
        if let Err(error) = unsafe { self.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_image(image, None);
            }
            return Err(error).context("bind imported gfxstream memory");
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(registration.image_format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let view = match unsafe { self.device.create_image_view(&view_info, None) } {
            Ok(view) => view,
            Err(error) => {
                unsafe {
                    self.device.free_memory(memory, None);
                    self.device.destroy_image(image, None);
                }
                return Err(error).context("create view for imported gfxstream image");
            }
        };
        Ok(ImportedFrame {
            context: Arc::clone(self),
            image,
            memory,
            view,
        })
    }
}

impl Drop for VulkanContext {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

struct ImportedFrame {
    context: Arc<VulkanContext>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

impl fmt::Debug for ImportedFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedFrame")
            .field("image", &self.image)
            .field("memory", &self.memory)
            .field("view", &self.view)
            .field("context", &self.context)
            .finish()
    }
}

impl ExternalFrameHandle for ImportedFrame {
    fn transport(&self) -> FrameTransportKindV2 {
        FrameTransportKindV2::VulkanWin32
    }

    fn out_of_band_handle_count(&self) -> u8 {
        1
    }
}

impl Drop for ImportedFrame {
    fn drop(&mut self) {
        unsafe {
            self.context.device.destroy_image_view(self.view, None);
            self.context.device.destroy_image(self.image, None);
            self.context.device.free_memory(self.memory, None);
        }
    }
}
