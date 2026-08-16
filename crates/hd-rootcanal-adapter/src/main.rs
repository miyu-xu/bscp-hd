#![allow(unsafe_code)]

// The formal RootCanal runtime supports Windows named pipes and Unix file/FIFO serial bridges.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc as std_mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes::Aes128;
use aes::cipher::{BlockEncrypt as _, KeyInit as _};
use anyhow::{Context as _, Result, bail, ensure};
use clap::Parser;
use hd_core::{
    ApiErrorV2, BluetoothHciCaptureRecordV2, BluetoothPeerActionV2, COMPONENT_PROTOCOL_VERSION,
    DeviceControlCommandV2, DeviceControlRequestV2, DeviceControlResponseV2, DeviceControlTokenV2,
    DeviceSerialEndpointV2, FormalComponentConfigurationV2, FormalComponentLaunchV2,
    FormalComponentProbeV2, FormalComponentReadyV2, InstanceActionV2,
};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use time::OffsetDateTime;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::sync::{mpsc, oneshot};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_H4_PACKET_BYTES: usize = 65_535;
const MAX_HCI_CAPTURE_BYTES: u64 = 4 * 1024 * 1024;
const BTSNOOP_EPOCH_DELTA_MICROS: u64 = 0x00dc_ddb3_0f2f_8000;
#[cfg(windows)]
const PIPE_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const ENGINE_TICK: Duration = Duration::from_millis(5);
const ATT_DEFAULT_MTU: u16 = 23;
const GATT_MTU: u16 = 247;
const ATT_CID: u16 = 0x0004;
const SMP_CID: u16 = 0x0006;
const HID_INPUT_REPORT_HANDLE: u16 = 14;
const HID_INPUT_CCCD_HANDLE: u16 = 15;
const HID_OUTPUT_REPORT_HANDLE: u16 = 18;
const HID_REPORT_MAP: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00,
    0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05,
    0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01,
    0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00,
    0xc0,
];

#[derive(Debug, Parser)]
#[command(about = "Formal HD adapter embedding the AOSP RootCanal controller")]
struct Arguments {
    #[arg(long)]
    probe_v2: bool,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    serve_v2: bool,
    #[arg(long)]
    launch: Option<PathBuf>,
}

struct FormalContext {
    launch: FormalComponentLaunchV2,
    control_endpoint: String,
    control_token: DeviceControlTokenV2,
    engine: std_mpsc::Sender<EngineCommand>,
}

enum EngineCommand {
    GuestHci(H4Packet),
    Action {
        action: BluetoothPeerActionV2,
        response: oneshot::Sender<std::result::Result<(), String>>,
    },
    StartCapture {
        capture_id: Uuid,
        capture_path: PathBuf,
        metadata_path: PathBuf,
        requested_duration_ms: u32,
        response: oneshot::Sender<std::result::Result<(), String>>,
    },
    StopCapture {
        capture_id: Uuid,
        response: oneshot::Sender<std::result::Result<(), String>>,
    },
    Shutdown,
}

#[derive(Debug)]
struct H4Packet {
    packet_type: u8,
    payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum HciDirection {
    GuestToController,
    ControllerToGuest,
}

struct HciCapture {
    capture_id: Uuid,
    file_name: String,
    metadata_path: PathBuf,
    requested_duration_ms: u32,
    file: File,
    packets_captured: u64,
    packets_dropped: u64,
    output_size_bytes: u64,
    truncated: bool,
    started_at: OffsetDateTime,
    error: Option<String>,
}

impl HciCapture {
    fn start(
        capture_id: Uuid,
        capture_path: &Path,
        metadata_path: PathBuf,
        requested_duration_ms: u32,
    ) -> Result<Self> {
        ensure!(!capture_id.is_nil(), "Bluetooth HCI capture id is nil");
        ensure!(
            (1_000..=30_000).contains(&requested_duration_ms),
            "Bluetooth HCI capture duration is outside 1-30 seconds"
        );
        for path in [capture_path, metadata_path.as_path()] {
            match std::fs::symlink_metadata(path) {
                Ok(_) => bail!("Bluetooth HCI capture output already exists"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect Bluetooth HCI capture output"),
            }
        }
        let file_name = capture_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Bluetooth HCI capture file name is not UTF-8")?
            .to_owned();
        let mut header = Vec::with_capacity(16);
        header.extend_from_slice(b"btsnoop\0");
        header.extend_from_slice(&1_u32.to_be_bytes());
        header.extend_from_slice(&1_002_u32.to_be_bytes());
        hd_platform::write_owner_only(capture_path, &header)?;
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(capture_path)
            .context("open Bluetooth HCI capture output")?;
        Ok(Self {
            capture_id,
            file_name,
            metadata_path,
            requested_duration_ms,
            file,
            packets_captured: 0,
            packets_dropped: 0,
            output_size_bytes: header.len() as u64,
            truncated: false,
            started_at: OffsetDateTime::now_utc(),
            error: None,
        })
    }

    fn record(&mut self, direction: HciDirection, packet_type: u8, payload: &[u8]) {
        if self.error.is_some() {
            self.packets_dropped = self.packets_dropped.saturating_add(1);
            return;
        }
        let packet_length = 1_u64.saturating_add(payload.len() as u64);
        let record_length = 24_u64.saturating_add(packet_length);
        if self.output_size_bytes.saturating_add(record_length) > MAX_HCI_CAPTURE_BYTES {
            self.packets_dropped = self.packets_dropped.saturating_add(1);
            self.truncated = true;
            return;
        }
        let Ok(packet_length_u32) = u32::try_from(packet_length) else {
            self.packets_dropped = self.packets_dropped.saturating_add(1);
            self.truncated = true;
            return;
        };
        let unix_micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let timestamp = BTSNOOP_EPOCH_DELTA_MICROS.saturating_add(
            u64::try_from(unix_micros).unwrap_or(u64::MAX - BTSNOOP_EPOCH_DELTA_MICROS),
        );
        let flags = match direction {
            HciDirection::GuestToController => 0_u32,
            HciDirection::ControllerToGuest => 1_u32,
        };
        let Ok(record_capacity) = usize::try_from(record_length) else {
            self.packets_dropped = self.packets_dropped.saturating_add(1);
            self.truncated = true;
            return;
        };
        let mut record = Vec::with_capacity(record_capacity);
        record.extend_from_slice(&packet_length_u32.to_be_bytes());
        record.extend_from_slice(&packet_length_u32.to_be_bytes());
        record.extend_from_slice(&flags.to_be_bytes());
        record.extend_from_slice(
            &u32::try_from(self.packets_dropped)
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        record.extend_from_slice(&timestamp.to_be_bytes());
        record.push(packet_type);
        record.extend_from_slice(payload);
        if let Err(error) = self.file.write_all(&record) {
            self.error = Some(format!("write Bluetooth HCI capture: {error}"));
            self.packets_dropped = self.packets_dropped.saturating_add(1);
            return;
        }
        self.output_size_bytes = self.output_size_bytes.saturating_add(record_length);
        self.packets_captured = self.packets_captured.saturating_add(1);
    }

    fn finish(mut self) -> Result<BluetoothHciCaptureRecordV2> {
        if let Err(error) = self.file.flush() {
            self.error
                .get_or_insert_with(|| format!("flush Bluetooth HCI capture: {error}"));
        }
        if let Err(error) = self.file.sync_all() {
            self.error
                .get_or_insert_with(|| format!("sync Bluetooth HCI capture: {error}"));
        }
        if let Some(error) = self.error {
            bail!(error);
        }
        let record = BluetoothHciCaptureRecordV2 {
            capture_id: self.capture_id,
            file_name: self.file_name,
            requested_duration_ms: self.requested_duration_ms,
            packets_captured: self.packets_captured,
            packets_dropped: self.packets_dropped,
            output_size_bytes: self.output_size_bytes,
            truncated: self.truncated,
            started_at: self.started_at,
            finished_at: OffsetDateTime::now_utc(),
        };
        hd_platform::write_owner_only(&self.metadata_path, &serde_json::to_vec_pretty(&record)?)?;
        tracing::info!(
            event = "bluetooth.hci_capture.completed",
            capture_id = %record.capture_id,
            packets_captured = record.packets_captured,
            packets_dropped = record.packets_dropped,
            output_size_bytes = record.output_size_bytes,
            truncated = record.truncated,
            "bounded Bluetooth HCI capture completed"
        );
        Ok(record)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControllerSource {
    Guest,
    Peer(Uuid),
}

enum EngineEvent {
    Hci {
        source: ControllerSource,
        packet_type: u8,
        payload: Vec<u8>,
    },
    LinkLayer {
        source: ControllerSource,
        payload: Vec<u8>,
        phy: i32,
        tx_power: i32,
    },
}

struct CallbackContext {
    source: ControllerSource,
    events: std_mpsc::Sender<EngineEvent>,
}

struct Controller {
    raw: *mut c_void,
    _callbacks: Box<CallbackContext>,
}

struct Peer {
    controller: Controller,
    local_address: [u8; 6],
    name: String,
    advertising_data: Vec<u8>,
    connectable: bool,
    connection_handle: Option<u16>,
    advertising: bool,
    advertising_plan: AdvertisingPlan,
    remote_address: Option<[u8; 6]>,
    remote_address_type: u8,
    att_mtu: u16,
    encrypted: bool,
    hid_keyboard: Option<HidKeyboardState>,
}

struct HidKeyboardState {
    input_notifications: bool,
    input_report: [u8; 8],
    output_report: u8,
    pairing: Option<LegacyPairing>,
    bond: Option<LegacyBond>,
}

struct LegacyPairing {
    request: [u8; 7],
    response: [u8; 7],
    remote_confirm: Option<[u8; 16]>,
    local_random: [u8; 16],
    short_term_key: Option<[u8; 16]>,
    long_term_key: [u8; 16],
    encrypted_diversifier: u16,
    random_number: [u8; 8],
    distribute_long_term_key: bool,
}

struct LegacyBond {
    long_term_key: [u8; 16],
    encrypted_diversifier: u16,
    random_number: [u8; 8],
}

struct ScriptedFrame {
    advertising_data: Vec<u8>,
    duration: Duration,
}

enum AdvertisingPlan {
    Static,
    Scripted {
        frames: Vec<ScriptedFrame>,
        frame_index: usize,
        next_frame_at: Instant,
        repeat: bool,
        completed: bool,
    },
}

#[cfg(any(windows, target_os = "macos"))]
unsafe extern "C" {
    fn rootcanal_controller_new(
        address: *const u8,
        context: *mut c_void,
        hci_callback: unsafe extern "C" fn(*mut c_void, i32, *const u8, usize),
        link_layer_callback: unsafe extern "C" fn(*mut c_void, *const u8, usize, i32, i32),
    ) -> *mut c_void;
    fn rootcanal_controller_delete(controller: *mut c_void);
    fn rootcanal_controller_receive_hci(
        controller: *mut c_void,
        packet_type: i32,
        data: *const u8,
        size: usize,
    );
    fn rootcanal_controller_receive_ll(
        controller: *mut c_void,
        data: *const u8,
        size: usize,
        phy: i32,
        rssi: i32,
    );
    fn rootcanal_controller_tick(controller: *mut c_void);
}

#[cfg(not(any(windows, target_os = "macos")))]
unsafe fn rootcanal_controller_new(
    _address: *const u8,
    _context: *mut c_void,
    _hci_callback: unsafe extern "C" fn(*mut c_void, i32, *const u8, usize),
    _link_layer_callback: unsafe extern "C" fn(*mut c_void, *const u8, usize, i32, i32),
) -> *mut c_void {
    std::ptr::null_mut()
}

#[cfg(not(any(windows, target_os = "macos")))]
unsafe fn rootcanal_controller_delete(_controller: *mut c_void) {}

#[cfg(not(any(windows, target_os = "macos")))]
unsafe fn rootcanal_controller_receive_hci(
    _controller: *mut c_void,
    _packet_type: i32,
    _data: *const u8,
    _size: usize,
) {
}

#[cfg(not(any(windows, target_os = "macos")))]
unsafe fn rootcanal_controller_receive_ll(
    _controller: *mut c_void,
    _data: *const u8,
    _size: usize,
    _phy: i32,
    _rssi: i32,
) {
}

#[cfg(not(any(windows, target_os = "macos")))]
unsafe fn rootcanal_controller_tick(_controller: *mut c_void) {}

impl Controller {
    fn new(
        address: [u8; 6],
        source: ControllerSource,
        events: std_mpsc::Sender<EngineEvent>,
    ) -> Result<Self> {
        let mut callbacks = Box::new(CallbackContext { source, events });
        let context = std::ptr::from_mut(callbacks.as_mut()).cast::<c_void>();
        let raw = unsafe {
            rootcanal_controller_new(address.as_ptr(), context, hci_callback, link_layer_callback)
        };
        ensure!(
            !raw.is_null(),
            "AOSP RootCanal controller allocation failed"
        );
        Ok(Self {
            raw,
            _callbacks: callbacks,
        })
    }

    fn receive_hci(&self, packet_type: u8, payload: &[u8]) {
        unsafe {
            rootcanal_controller_receive_hci(
                self.raw,
                i32::from(packet_type),
                payload.as_ptr(),
                payload.len(),
            );
        }
    }

    fn receive_link_layer(&self, payload: &[u8], phy: i32, rssi: i32) {
        unsafe {
            rootcanal_controller_receive_ll(self.raw, payload.as_ptr(), payload.len(), phy, rssi);
        }
    }

    fn tick(&self) {
        unsafe { rootcanal_controller_tick(self.raw) };
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        unsafe { rootcanal_controller_delete(self.raw) };
    }
}

unsafe extern "C" fn hci_callback(
    context: *mut c_void,
    packet_type: i32,
    data: *const u8,
    size: usize,
) {
    if context.is_null() || data.is_null() || size > MAX_H4_PACKET_BYTES {
        return;
    }
    let callbacks = unsafe { &*context.cast::<CallbackContext>() };
    let payload = unsafe { std::slice::from_raw_parts(data, size) }.to_vec();
    let Ok(packet_type) = u8::try_from(packet_type) else {
        return;
    };
    let _ = callbacks.events.send(EngineEvent::Hci {
        source: callbacks.source,
        packet_type,
        payload,
    });
}

unsafe extern "C" fn link_layer_callback(
    context: *mut c_void,
    data: *const u8,
    size: usize,
    phy: i32,
    tx_power: i32,
) {
    if context.is_null() || data.is_null() || size > MAX_H4_PACKET_BYTES {
        return;
    }
    let callbacks = unsafe { &*context.cast::<CallbackContext>() };
    let payload = unsafe { std::slice::from_raw_parts(data, size) }.to_vec();
    let _ = callbacks.events.send(EngineEvent::LinkLayer {
        source: callbacks.source,
        payload,
        phy,
        tx_power,
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("hd_rootcanal_adapter=info")),
        )
        .init();
    let _ = rootcanal_rs::num_hci_command_packets;
    let arguments = Arguments::parse();
    match (
        arguments.probe_v2,
        arguments.json,
        arguments.serve_v2,
        arguments.launch.as_deref(),
    ) {
        (true, true, false, None) => {
            println!("{}", serde_json::to_string(&probe())?);
            Ok(())
        }
        (false, false, true, Some(path)) => serve_formal(path).await,
        _ => bail!("select --probe-v2 --json or --serve-v2 --launch <path>"),
    }
}

fn probe() -> FormalComponentProbeV2 {
    FormalComponentProbeV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        component: "rootcanal-adapter".to_owned(),
        formal: cfg!(any(windows, target_os = "macos")),
        features: vec![
            "guest-serial-v2".to_owned(),
            "hci-v2".to_owned(),
            "hci-h4-v2".to_owned(),
            "aosp-rootcanal-v2".to_owned(),
            "peer-control-v2".to_owned(),
            "gatt-peer-control-v2".to_owned(),
            "beacon-peer-control-v2".to_owned(),
            "scripted-beacon-control-v2".to_owned(),
            "hogp-keyboard-control-v2".to_owned(),
            "bounded-hci-capture-v2".to_owned(),
        ],
    }
}

async fn serve_formal(launch_path: &Path) -> Result<()> {
    if !cfg!(any(windows, target_os = "macos")) {
        bail!("the formal RootCanal runtime is supported only on Windows and macOS");
    }

    let launch_bytes = hd_platform::read_regular_nofollow_limited(launch_path, 64 * 1024)
        .context("read RootCanal adapter launch")?;
    let launch: FormalComponentLaunchV2 =
        serde_json::from_slice(&launch_bytes).context("decode RootCanal adapter launch")?;
    ensure!(
        launch.protocol_version == COMPONENT_PROTOCOL_VERSION
            && launch.component == "rootcanal-adapter",
        "formal launch protocol or component mismatch"
    );
    ensure!(
        launch.component_ready_marker.parent() == launch_path.parent()
            && launch
                .component_ready_marker
                .file_name()
                .and_then(|name| name.to_str())
                == Some("rootcanal-adapter-ready-v2.json"),
        "formal ready marker is outside the launch directory"
    );
    let (control_endpoint, control_token, bluetooth_endpoint) = match &launch.configuration {
        FormalComponentConfigurationV2::DeviceAdapter {
            control_endpoint,
            control_token,
            guest_endpoints,
            ..
        } => (
            control_endpoint.clone(),
            control_token.clone(),
            guest_endpoints
                .get("bluetooth")
                .cloned()
                .context("RootCanal adapter requires the bluetooth Guest endpoint")?,
        ),
        _ => bail!("rootcanal-adapter requires a device_adapter configuration"),
    };

    let (engine_tx, engine_rx) = std_mpsc::channel();
    let (guest_hci_tx, guest_hci_rx) = mpsc::unbounded_channel();
    let engine = std::thread::Builder::new()
        .name("hd-rootcanal-engine".to_owned())
        .spawn(move || run_engine(&engine_rx, &guest_hci_tx))
        .context("start AOSP RootCanal engine thread")?;
    let context = Arc::new(FormalContext {
        launch,
        control_endpoint,
        control_token,
        engine: engine_tx.clone(),
    });
    let bridge = tokio::spawn(bridge_guest_h4(
        bluetooth_endpoint,
        engine_tx.clone(),
        guest_hci_rx,
    ));
    let control = run_control_server(Arc::clone(&context), &launch_bytes);
    tokio::pin!(control);
    let outcome = tokio::select! {
        result = &mut control => result,
        result = bridge => match result {
            Ok(Ok(())) => bail!("RootCanal Guest H4 bridge exited unexpectedly"),
            Ok(Err(error)) => Err(error).context("RootCanal Guest H4 bridge failed"),
            Err(error) => Err(error).context("RootCanal Guest H4 bridge task failed"),
        },
    };
    let _ = engine_tx.send(EngineCommand::Shutdown);
    let joined = tokio::task::spawn_blocking(move || engine.join())
        .await
        .context("join RootCanal engine join task")?;
    match joined {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error).context("AOSP RootCanal engine failed"),
        Err(_) => bail!("AOSP RootCanal engine thread panicked"),
    }
    outcome
}

fn run_engine(
    commands: &std_mpsc::Receiver<EngineCommand>,
    guest_hci: &mpsc::UnboundedSender<H4Packet>,
) -> Result<()> {
    let (event_tx, event_rx) = std_mpsc::channel();
    let guest = Controller::new(
        [0xad, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb],
        ControllerSource::Guest,
        event_tx.clone(),
    )?;
    let mut peers = BTreeMap::new();
    let mut hci_capture: Option<HciCapture> = None;
    loop {
        drain_engine_events(&event_rx, &guest, &mut peers, guest_hci, &mut hci_capture)?;
        match commands.recv_timeout(ENGINE_TICK) {
            Ok(EngineCommand::GuestHci(packet)) => {
                if let Some(capture) = &mut hci_capture {
                    capture.record(
                        HciDirection::GuestToController,
                        packet.packet_type,
                        &packet.payload,
                    );
                }
                guest.receive_hci(packet.packet_type, &packet.payload);
            }
            Ok(EngineCommand::Action { action, response }) => {
                let result = apply_peer_action(action, &event_tx, &mut peers)
                    .map_err(|error| error.to_string());
                let _ = response.send(result);
            }
            Ok(EngineCommand::StartCapture {
                capture_id,
                capture_path,
                metadata_path,
                requested_duration_ms,
                response,
            }) => {
                let result = if hci_capture.is_some() {
                    Err("a Bluetooth HCI capture is already active".to_owned())
                } else {
                    HciCapture::start(
                        capture_id,
                        &capture_path,
                        metadata_path,
                        requested_duration_ms,
                    )
                    .map(|capture| hci_capture = Some(capture))
                    .map_err(|error| error.to_string())
                };
                let _ = response.send(result);
            }
            Ok(EngineCommand::StopCapture {
                capture_id,
                response,
            }) => {
                let result = match hci_capture.as_ref() {
                    Some(capture) if capture.capture_id != capture_id => {
                        Err("Bluetooth HCI capture identity mismatch".to_owned())
                    }
                    Some(_) => hci_capture
                        .take()
                        .expect("capture was checked above")
                        .finish()
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    None => Err("Bluetooth HCI capture is not active".to_owned()),
                };
                let _ = response.send(result);
            }
            Ok(EngineCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(capture) = hci_capture.take()
                    && let Err(error) = capture.finish()
                {
                    tracing::warn!(
                        event = "bluetooth.hci_capture.shutdown_failed",
                        %error,
                        "failed to finalize Bluetooth HCI capture during shutdown"
                    );
                }
                break;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
        }
        guest.tick();
        for peer in peers.values_mut() {
            peer.controller.tick();
            tick_scripted_peer(peer, Instant::now());
        }
    }
    Ok(())
}

fn drain_engine_events(
    events: &std_mpsc::Receiver<EngineEvent>,
    guest: &Controller,
    peers: &mut BTreeMap<Uuid, Peer>,
    guest_hci: &mpsc::UnboundedSender<H4Packet>,
    hci_capture: &mut Option<HciCapture>,
) -> Result<()> {
    while let Ok(event) = events.try_recv() {
        match event {
            EngineEvent::Hci {
                source: ControllerSource::Guest,
                packet_type,
                payload,
            } => {
                if let Some(capture) = hci_capture {
                    capture.record(HciDirection::ControllerToGuest, packet_type, &payload);
                }
                guest_hci
                    .send(H4Packet {
                        packet_type,
                        payload,
                    })
                    .map_err(|_| anyhow::anyhow!("Guest H4 writer closed"))?;
            }
            EngineEvent::Hci {
                source: ControllerSource::Peer(peer_id),
                packet_type,
                payload,
            } => {
                if let Some(peer) = peers.get_mut(&peer_id) {
                    handle_peer_hci(peer, packet_type, &payload);
                }
            }
            EngineEvent::LinkLayer {
                source,
                payload,
                phy,
                tx_power,
            } => {
                if source != ControllerSource::Guest {
                    guest.receive_link_layer(&payload, phy, tx_power - 20);
                }
                for (peer_id, peer) in &*peers {
                    if source != ControllerSource::Peer(*peer_id) {
                        peer.controller
                            .receive_link_layer(&payload, phy, tx_power - 20);
                    }
                }
            }
        }
    }
    Ok(())
}

fn apply_peer_action(
    action: BluetoothPeerActionV2,
    events: &std_mpsc::Sender<EngineEvent>,
    peers: &mut BTreeMap<Uuid, Peer>,
) -> Result<()> {
    match action {
        BluetoothPeerActionV2::CreateGattPeer { peer_id, name } => {
            create_peer(new_gatt_peer(peer_id, name), events, peers)?;
        }
        BluetoothPeerActionV2::CreateBeacon {
            peer_id,
            name,
            advertising_data_hex,
        } => {
            create_peer(
                new_beacon_peer(peer_id, name, &advertising_data_hex)?,
                events,
                peers,
            )?;
        }
        BluetoothPeerActionV2::CreateScriptedBeacon {
            peer_id,
            name,
            frames,
            repeat,
        } => {
            create_peer(
                new_scripted_beacon_peer(peer_id, name, frames, repeat)?,
                events,
                peers,
            )?;
        }
        BluetoothPeerActionV2::CreateHidKeyboard { peer_id, name } => {
            create_peer(new_hid_keyboard_peer(peer_id, name), events, peers)?;
        }
        BluetoothPeerActionV2::SendHidKeyboardReport {
            peer_id,
            modifiers,
            keys,
        } => {
            let peer = peers
                .get_mut(&peer_id)
                .context("Bluetooth HID keyboard does not exist")?;
            send_hid_keyboard_report(peer, modifiers, &keys)?;
        }
        BluetoothPeerActionV2::RemovePeer { peer_id } => {
            ensure!(
                peers.remove(&peer_id).is_some(),
                "Bluetooth peer does not exist"
            );
        }
        BluetoothPeerActionV2::SetAdvertising { peer_id, enabled } => {
            let peer = peers
                .get_mut(&peer_id)
                .context("Bluetooth peer does not exist")?;
            set_advertising(peer, enabled);
            peer.advertising = enabled;
            if enabled {
                reset_scripted_deadline(peer, Instant::now());
            }
        }
        BluetoothPeerActionV2::CaptureHci { .. } => {
            bail!("Bluetooth HCI capture requires the bounded control lifecycle")
        }
    }
    Ok(())
}

struct NewPeer {
    peer_id: Uuid,
    name: String,
    advertising_data: Vec<u8>,
    connectable: bool,
    advertising_plan: AdvertisingPlan,
    hid_keyboard: bool,
}

fn new_gatt_peer(peer_id: Uuid, name: String) -> NewPeer {
    NewPeer {
        peer_id,
        advertising_data: gatt_advertising_data(&name),
        name,
        connectable: true,
        advertising_plan: AdvertisingPlan::Static,
        hid_keyboard: false,
    }
}

fn new_beacon_peer(peer_id: Uuid, name: String, advertising_data_hex: &str) -> Result<NewPeer> {
    let advertising_data =
        hex::decode(advertising_data_hex).context("decode beacon advertising data")?;
    ensure!(
        !advertising_data.is_empty() && advertising_data.len() <= 31,
        "Bluetooth beacon advertising data must contain 1-31 bytes"
    );
    Ok(NewPeer {
        peer_id,
        name,
        advertising_data,
        connectable: false,
        advertising_plan: AdvertisingPlan::Static,
        hid_keyboard: false,
    })
}

fn new_scripted_beacon_peer(
    peer_id: Uuid,
    name: String,
    frames: Vec<hd_core::BluetoothAdvertisementFrameV2>,
    repeat: bool,
) -> Result<NewPeer> {
    let frames = frames
        .into_iter()
        .map(|frame| {
            Ok(ScriptedFrame {
                advertising_data: hex::decode(frame.advertising_data_hex)
                    .context("decode scripted Beacon advertising data")?,
                duration: Duration::from_millis(u64::from(frame.duration_ms)),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let first = frames
        .first()
        .context("scripted Beacon requires at least one frame")?;
    let advertising_data = first.advertising_data.clone();
    let next_frame_at = Instant::now() + first.duration;
    Ok(NewPeer {
        peer_id,
        name,
        advertising_data,
        connectable: false,
        advertising_plan: AdvertisingPlan::Scripted {
            frames,
            frame_index: 0,
            next_frame_at,
            repeat,
            completed: false,
        },
        hid_keyboard: false,
    })
}

fn new_hid_keyboard_peer(peer_id: Uuid, name: String) -> NewPeer {
    NewPeer {
        peer_id,
        advertising_data: hid_keyboard_advertising_data(&name),
        name,
        connectable: true,
        advertising_plan: AdvertisingPlan::Static,
        hid_keyboard: true,
    }
}

fn create_peer(
    new_peer: NewPeer,
    events: &std_mpsc::Sender<EngineEvent>,
    peers: &mut BTreeMap<Uuid, Peer>,
) -> Result<()> {
    let NewPeer {
        peer_id,
        name,
        advertising_data,
        connectable,
        advertising_plan,
        hid_keyboard,
    } = new_peer;
    ensure!(
        !peers.contains_key(&peer_id),
        "Bluetooth peer already exists"
    );
    let local_address = peer_address(peer_id);
    let controller = Controller::new(
        local_address,
        ControllerSource::Peer(peer_id),
        events.clone(),
    )?;
    let peer = Peer {
        controller,
        local_address,
        name,
        advertising_data,
        connectable,
        connection_handle: None,
        advertising: true,
        advertising_plan,
        remote_address: None,
        remote_address_type: 0,
        att_mtu: ATT_DEFAULT_MTU,
        encrypted: false,
        hid_keyboard: hid_keyboard.then_some(HidKeyboardState {
            input_notifications: false,
            input_report: [0; 8],
            output_report: 0,
            pairing: None,
            bond: None,
        }),
    };
    initialize_peer(&peer);
    peers.insert(peer_id, peer);
    Ok(())
}

fn peer_address(peer_id: Uuid) -> [u8; 6] {
    let mut address = [0_u8; 6];
    address.copy_from_slice(&peer_id.as_bytes()[..6]);
    address[0] = (address[0] & 0xfe) | 0x02;
    address
}

fn initialize_peer(peer: &Peer) {
    send_command(&peer.controller, 0x0c03, &[]);
    // A standalone RootCanal controller does not inherit the Android host's event masks.
    // Enable the LE Meta event in the base mask, then the connection, enhanced connection,
    // remote feature and long-term-key events needed by a connectable simulated peripheral.
    send_command(&peer.controller, 0x0c01, &[0xff; 8]);
    send_command(
        &peer.controller,
        0x2001,
        &[0x1d, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    );
    send_command(
        &peer.controller,
        0x2006,
        &[
            0xa0,
            0x00,
            0xa0,
            0x00,
            if peer.connectable { 0x00 } else { 0x03 },
            0x00,
            0x00,
            0,
            0,
            0,
            0,
            0,
            0,
            0x07,
            0x00,
        ],
    );
    let mut parameters = vec![u8::try_from(peer.advertising_data.len()).unwrap_or(31)];
    parameters.extend_from_slice(&peer.advertising_data);
    parameters.resize(32, 0);
    send_command(&peer.controller, 0x2008, &parameters);
    set_advertising(peer, true);
}

fn gatt_advertising_data(name: &str) -> Vec<u8> {
    let mut advertising_data = Vec::with_capacity(31);
    advertising_data.extend_from_slice(&[0x02, 0x01, 0x06]);
    let name = name.as_bytes();
    let name_len = name.len().min(26);
    advertising_data.push(u8::try_from(name_len + 1).unwrap_or(27));
    advertising_data.push(0x09);
    advertising_data.extend_from_slice(&name[..name_len]);
    advertising_data
}

fn hid_keyboard_advertising_data(name: &str) -> Vec<u8> {
    let mut advertising_data = Vec::with_capacity(31);
    advertising_data.extend_from_slice(&[0x02, 0x01, 0x06]);
    advertising_data.extend_from_slice(&[0x03, 0x03, 0x12, 0x18]);
    advertising_data.extend_from_slice(&[0x03, 0x19, 0xc1, 0x03]);
    let name = name.as_bytes();
    let name_len = name.len().min(18);
    advertising_data.push(u8::try_from(name_len + 1).unwrap_or(19));
    advertising_data.push(0x09);
    advertising_data.extend_from_slice(&name[..name_len]);
    advertising_data
}

fn set_advertising(peer: &Peer, enabled: bool) {
    send_command(&peer.controller, 0x200a, &[u8::from(enabled)]);
}

fn set_advertising_data(peer: &mut Peer, advertising_data: Vec<u8>) {
    let mut parameters = vec![u8::try_from(advertising_data.len()).unwrap_or(31)];
    parameters.extend_from_slice(&advertising_data);
    parameters.resize(32, 0);
    send_command(&peer.controller, 0x2008, &parameters);
    peer.advertising_data = advertising_data;
}

fn reset_scripted_deadline(peer: &mut Peer, now: Instant) {
    if let AdvertisingPlan::Scripted {
        frames,
        frame_index,
        next_frame_at,
        ..
    } = &mut peer.advertising_plan
    {
        *next_frame_at = now + frames[*frame_index].duration;
    }
}

fn tick_scripted_peer(peer: &mut Peer, now: Instant) {
    if !peer.advertising {
        return;
    }
    let next_data = match &mut peer.advertising_plan {
        AdvertisingPlan::Static => return,
        AdvertisingPlan::Scripted {
            frames,
            frame_index,
            next_frame_at,
            repeat,
            completed,
        } => {
            if *completed {
                return;
            }
            if now < *next_frame_at {
                return;
            }
            let mut next_index = *frame_index + 1;
            if next_index == frames.len() {
                if !*repeat {
                    *completed = true;
                    return;
                }
                next_index = 0;
                *frame_index = next_index;
                *next_frame_at = now + frames[next_index].duration;
                frames[next_index].advertising_data.clone()
            } else {
                *frame_index = next_index;
                *next_frame_at = now + frames[next_index].duration;
                frames[next_index].advertising_data.clone()
            }
        }
    };
    set_advertising(peer, false);
    set_advertising_data(peer, next_data);
    set_advertising(peer, true);
}

fn send_command(controller: &Controller, opcode: u16, parameters: &[u8]) {
    let mut packet = Vec::with_capacity(parameters.len() + 3);
    packet.extend_from_slice(&opcode.to_le_bytes());
    packet.push(u8::try_from(parameters.len()).unwrap_or(u8::MAX));
    packet.extend_from_slice(parameters);
    controller.receive_hci(1, &packet);
}

fn handle_peer_hci(peer: &mut Peer, packet_type: u8, packet: &[u8]) {
    match packet_type {
        4 => handle_peer_event(peer, packet),
        2 => handle_peer_acl(peer, packet),
        _ => {}
    }
}

fn handle_peer_event(peer: &mut Peer, packet: &[u8]) {
    if packet.len() >= 7 && packet[0] == 0x3e && matches!(packet[2], 0x01 | 0x0a) && packet[3] == 0
    {
        peer.connection_handle = Some(u16::from_le_bytes([packet[4], packet[5]]) & 0x0fff);
        if packet.len() >= 14 {
            peer.remote_address_type = packet[7];
            peer.remote_address = packet[8..14].try_into().ok();
        }
        peer.att_mtu = ATT_DEFAULT_MTU;
        peer.encrypted = false;
        tracing::info!(
            peer = %peer.name,
            handle = peer.connection_handle,
            "Bluetooth peer connected"
        );
        if let Some(keyboard) = peer.hid_keyboard.as_mut() {
            keyboard.input_notifications = false;
            keyboard.input_report = [0; 8];
            keyboard.pairing = None;
        }
    } else if packet.len() >= 15 && packet[0] == 0x3e && packet[2] == 0x05 {
        handle_long_term_key_request(peer, packet);
    } else if packet.len() >= 6 && packet[0] == 0x08 && packet[2] == 0 {
        let handle = u16::from_le_bytes([packet[3], packet[4]]) & 0x0fff;
        if peer.connection_handle == Some(handle) {
            peer.encrypted = packet[5] != 0;
            if peer.encrypted {
                tracing::info!(peer = %peer.name, handle, "Bluetooth peer link encrypted");
                distribute_legacy_bond(peer);
            }
        }
    } else if packet.len() >= 6 && packet[0] == 0x05 && packet[2] == 0 {
        tracing::info!(peer = %peer.name, "Bluetooth peer disconnected");
        peer.connection_handle = None;
        peer.remote_address = None;
        peer.encrypted = false;
        if let Some(keyboard) = peer.hid_keyboard.as_mut() {
            keyboard.input_notifications = false;
            keyboard.input_report = [0; 8];
            keyboard.pairing = None;
        }
        if peer.advertising {
            set_advertising(peer, true);
        }
    }
}

fn handle_peer_acl(peer: &mut Peer, packet: &[u8]) {
    if packet.len() < 8 {
        return;
    }
    let handle = u16::from_le_bytes([packet[0], packet[1]]) & 0x0fff;
    let acl_length = usize::from(u16::from_le_bytes([packet[2], packet[3]]));
    if packet.len() < 4 + acl_length {
        return;
    }
    let cid = u16::from_le_bytes([packet[6], packet[7]]);
    let l2cap_length = usize::from(u16::from_le_bytes([packet[4], packet[5]]));
    if packet.len() < 8 + l2cap_length {
        return;
    }
    let request = &packet[8..8 + l2cap_length];
    tracing::debug!(
        peer = %peer.name,
        handle,
        cid,
        opcode = request.first().copied(),
        payload_bytes = request.len(),
        acl_bytes = acl_length,
        "Bluetooth peer received L2CAP packet"
    );
    if cid == ATT_CID {
        if let Some(response) = gatt_response(peer, request) {
            send_l2cap(&peer.controller, handle, ATT_CID, &response);
        }
    } else if cid == SMP_CID
        && let Err(error) = handle_smp(peer, handle, request)
    {
        tracing::warn!(%error, "Bluetooth HID pairing packet was rejected");
    }
}

struct GattAttribute {
    handle: u16,
    kind: u16,
    value: Vec<u8>,
    readable: bool,
    writable: bool,
    group_end: Option<u16>,
}

fn gatt_attributes(peer: &Peer) -> Vec<GattAttribute> {
    let mut attributes = vec![
        gatt_attribute(
            1,
            0x2800,
            0x1800_u16.to_le_bytes().to_vec(),
            true,
            false,
            Some(3),
        ),
        gatt_attribute(2, 0x2803, vec![0x02, 3, 0, 0x00, 0x2a], true, false, None),
        gatt_attribute(3, 0x2a00, peer.name.as_bytes().to_vec(), true, false, None),
    ];
    let Some(keyboard) = peer.hid_keyboard.as_ref() else {
        return attributes;
    };
    attributes.extend([
        gatt_attribute(
            4,
            0x2800,
            0x1812_u16.to_le_bytes().to_vec(),
            true,
            false,
            Some(19),
        ),
        gatt_attribute(5, 0x2803, vec![0x02, 6, 0, 0x4a, 0x2a], true, false, None),
        gatt_attribute(6, 0x2a4a, vec![0x11, 0x01, 0x00, 0x02], true, false, None),
        gatt_attribute(7, 0x2803, vec![0x02, 8, 0, 0x4b, 0x2a], true, false, None),
        gatt_attribute(8, 0x2a4b, HID_REPORT_MAP.to_vec(), true, false, None),
        gatt_attribute(9, 0x2803, vec![0x04, 10, 0, 0x4c, 0x2a], true, false, None),
        gatt_attribute(10, 0x2a4c, vec![0], false, true, None),
        gatt_attribute(13, 0x2803, vec![0x12, 14, 0, 0x4d, 0x2a], true, false, None),
        gatt_attribute(
            14,
            0x2a4d,
            keyboard.input_report.to_vec(),
            true,
            false,
            None,
        ),
        gatt_attribute(
            15,
            0x2902,
            u16::from(keyboard.input_notifications)
                .to_le_bytes()
                .to_vec(),
            true,
            true,
            None,
        ),
        gatt_attribute(16, 0x2908, vec![0x01, 0x01], true, false, None),
        gatt_attribute(17, 0x2803, vec![0x0e, 18, 0, 0x4d, 0x2a], true, false, None),
        gatt_attribute(18, 0x2a4d, vec![keyboard.output_report], true, true, None),
        gatt_attribute(19, 0x2908, vec![0x01, 0x02], true, false, None),
    ]);
    attributes
}

fn gatt_attribute(
    handle: u16,
    kind: u16,
    value: Vec<u8>,
    readable: bool,
    writable: bool,
    group_end: Option<u16>,
) -> GattAttribute {
    GattAttribute {
        handle,
        kind,
        value,
        readable,
        writable,
        group_end,
    }
}

fn gatt_response(peer: &mut Peer, request: &[u8]) -> Option<Vec<u8>> {
    let opcode = *request.first()?;
    let [mtu_low, mtu_high] = GATT_MTU.to_le_bytes();
    match opcode {
        0x02 if request.len() >= 3 => {
            let client_mtu = u16::from_le_bytes([request[1], request[2]]);
            peer.att_mtu = client_mtu.clamp(ATT_DEFAULT_MTU, GATT_MTU);
            Some(vec![0x03, mtu_low, mtu_high])
        }
        0x04 if request.len() >= 5 => Some(find_information_response(peer, request)),
        0x06 if request.len() >= 7 => Some(find_by_type_value_response(peer, request)),
        0x08 if request.len() >= 7 => Some(read_by_type_response(peer, request)),
        0x0a if request.len() >= 3 => {
            let handle = u16::from_le_bytes([request[1], request[2]]);
            Some(read_attribute_response(peer, opcode, handle, 0))
        }
        0x0c if request.len() >= 5 => {
            let handle = u16::from_le_bytes([request[1], request[2]]);
            let offset = usize::from(u16::from_le_bytes([request[3], request[4]]));
            Some(read_attribute_response(peer, opcode, handle, offset))
        }
        0x10 if request.len() >= 7 => Some(read_by_group_type_response(peer, request)),
        0x12 if request.len() >= 3 => {
            let handle = u16::from_le_bytes([request[1], request[2]]);
            Some(match write_attribute(peer, opcode, handle, &request[3..]) {
                Ok(()) => vec![0x13],
                Err(error) => error,
            })
        }
        0x52 if request.len() >= 3 => {
            let handle = u16::from_le_bytes([request[1], request[2]]);
            let _ = write_attribute(peer, opcode, handle, &request[3..]);
            None
        }
        0x1e => None,
        _ => Some(att_error(opcode, 0, 0x06)),
    }
}

fn find_information_response(peer: &Peer, request: &[u8]) -> Vec<u8> {
    let start = u16::from_le_bytes([request[1], request[2]]);
    let end = u16::from_le_bytes([request[3], request[4]]);
    let mut response = vec![0x05, 0x01];
    for attribute in gatt_attributes(peer)
        .into_iter()
        .filter(|attribute| (start..=end).contains(&attribute.handle))
    {
        if response.len() + 4 > usize::from(peer.att_mtu) {
            break;
        }
        response.extend_from_slice(&attribute.handle.to_le_bytes());
        response.extend_from_slice(&attribute.kind.to_le_bytes());
    }
    if response.len() == 2 {
        att_error(request[0], start, 0x0a)
    } else {
        response
    }
}

fn find_by_type_value_response(peer: &Peer, request: &[u8]) -> Vec<u8> {
    let start = u16::from_le_bytes([request[1], request[2]]);
    let end = u16::from_le_bytes([request[3], request[4]]);
    let kind = u16::from_le_bytes([request[5], request[6]]);
    let value = &request[7..];
    let mut response = vec![0x07];
    for attribute in gatt_attributes(peer).into_iter().filter(|attribute| {
        (start..=end).contains(&attribute.handle)
            && attribute.kind == kind
            && attribute.value == value
            && attribute.group_end.is_some()
    }) {
        if response.len() + 4 > usize::from(peer.att_mtu) {
            break;
        }
        response.extend_from_slice(&attribute.handle.to_le_bytes());
        response.extend_from_slice(
            &attribute
                .group_end
                .unwrap_or(attribute.handle)
                .to_le_bytes(),
        );
    }
    if response.len() == 1 {
        att_error(request[0], start, 0x0a)
    } else {
        response
    }
}

fn read_by_type_response(peer: &Peer, request: &[u8]) -> Vec<u8> {
    let start = u16::from_le_bytes([request[1], request[2]]);
    let end = u16::from_le_bytes([request[3], request[4]]);
    let kind = u16::from_le_bytes([request[5], request[6]]);
    let mut values = gatt_attributes(peer).into_iter().filter(|attribute| {
        (start..=end).contains(&attribute.handle) && attribute.kind == kind && attribute.readable
    });
    let Some(first) = values.next() else {
        return att_error(request[0], start, 0x0a);
    };
    if hid_access_requires_encryption(first.handle) && !peer.encrypted {
        return att_error(request[0], first.handle, 0x0f);
    }
    let value_length = first.value.len().min(usize::from(peer.att_mtu) - 4);
    let entry_length = value_length + 2;
    let mut response = vec![0x09, u8::try_from(entry_length).unwrap_or(u8::MAX)];
    response.extend_from_slice(&first.handle.to_le_bytes());
    response.extend_from_slice(&first.value[..value_length]);
    for attribute in values {
        if attribute.value.len() != value_length
            || response.len() + entry_length > usize::from(peer.att_mtu)
        {
            break;
        }
        response.extend_from_slice(&attribute.handle.to_le_bytes());
        response.extend_from_slice(&attribute.value);
    }
    response
}

fn read_by_group_type_response(peer: &Peer, request: &[u8]) -> Vec<u8> {
    let start = u16::from_le_bytes([request[1], request[2]]);
    let end = u16::from_le_bytes([request[3], request[4]]);
    let kind = u16::from_le_bytes([request[5], request[6]]);
    let mut response = vec![0x11, 0x06];
    if kind == 0x2800 {
        for attribute in gatt_attributes(peer)
            .into_iter()
            .filter(|attribute| (start..=end).contains(&attribute.handle) && attribute.kind == kind)
        {
            if response.len() + 6 > usize::from(peer.att_mtu) {
                break;
            }
            response.extend_from_slice(&attribute.handle.to_le_bytes());
            response.extend_from_slice(
                &attribute
                    .group_end
                    .unwrap_or(attribute.handle)
                    .to_le_bytes(),
            );
            response.extend_from_slice(&attribute.value);
        }
    }
    if response.len() == 2 {
        att_error(request[0], start, 0x0a)
    } else {
        response
    }
}

fn hid_access_requires_encryption(handle: u16) -> bool {
    handle >= 6
}

fn read_attribute_response(peer: &Peer, opcode: u8, handle: u16, offset: usize) -> Vec<u8> {
    let Some(attribute) = gatt_attributes(peer)
        .into_iter()
        .find(|attribute| attribute.handle == handle)
    else {
        return att_error(opcode, handle, 0x0a);
    };
    if !attribute.readable {
        return att_error(opcode, handle, 0x02);
    }
    if hid_access_requires_encryption(handle) && !peer.encrypted {
        return att_error(opcode, handle, 0x0f);
    }
    if offset > attribute.value.len() {
        return att_error(opcode, handle, 0x07);
    }
    let mut response = vec![if opcode == 0x0c { 0x0d } else { 0x0b }];
    response.extend_from_slice(&attribute.value[offset..]);
    response.truncate(usize::from(peer.att_mtu));
    response
}

fn write_attribute(
    peer: &mut Peer,
    opcode: u8,
    handle: u16,
    value: &[u8],
) -> std::result::Result<(), Vec<u8>> {
    let Some(attribute) = gatt_attributes(peer)
        .into_iter()
        .find(|attribute| attribute.handle == handle)
    else {
        return Err(att_error(opcode, handle, 0x0a));
    };
    if !attribute.writable {
        return Err(att_error(opcode, handle, 0x03));
    }
    if hid_access_requires_encryption(handle) && !peer.encrypted {
        return Err(att_error(opcode, handle, 0x0f));
    }
    let Some(keyboard) = peer.hid_keyboard.as_mut() else {
        return Err(att_error(opcode, handle, 0x03));
    };
    match handle {
        HID_INPUT_CCCD_HANDLE if value.len() == 2 => {
            let configuration = u16::from_le_bytes([value[0], value[1]]);
            if configuration > 1 {
                return Err(att_error(opcode, handle, 0x13));
            }
            keyboard.input_notifications = configuration == 1;
        }
        HID_OUTPUT_REPORT_HANDLE if value.len() == 1 => keyboard.output_report = value[0],
        10 if value.len() == 1 && matches!(value[0], 0 | 1) => {}
        _ => return Err(att_error(opcode, handle, 0x0d)),
    }
    Ok(())
}

fn att_error(request_opcode: u8, handle: u16, error: u8) -> Vec<u8> {
    let [low, high] = handle.to_le_bytes();
    vec![0x01, request_opcode, low, high, error]
}

fn send_l2cap(controller: &Controller, handle: u16, cid: u16, payload: &[u8]) {
    let Ok(l2cap_length) = u16::try_from(payload.len()) else {
        return;
    };
    let total_length = l2cap_length.saturating_add(4);
    let mut packet = Vec::with_capacity(usize::from(total_length) + 4);
    packet.extend_from_slice(&(handle | 0x2000).to_le_bytes());
    packet.extend_from_slice(&total_length.to_le_bytes());
    packet.extend_from_slice(&l2cap_length.to_le_bytes());
    packet.extend_from_slice(&cid.to_le_bytes());
    packet.extend_from_slice(payload);
    controller.receive_hci(2, &packet);
}

fn send_hid_keyboard_report(peer: &mut Peer, modifiers: u8, keys: &[u8]) -> Result<()> {
    let handle = peer
        .connection_handle
        .context("Bluetooth HID keyboard is not connected")?;
    ensure!(
        peer.encrypted,
        "Bluetooth HID keyboard link is not encrypted"
    );
    let keyboard = peer
        .hid_keyboard
        .as_mut()
        .context("Bluetooth peer is not a HID keyboard")?;
    ensure!(
        keyboard.input_notifications,
        "Bluetooth HID keyboard input notifications are not enabled"
    );
    let mut report = [0_u8; 8];
    report[0] = modifiers;
    report[2..2 + keys.len()].copy_from_slice(keys);
    let mut notification = vec![0x1b];
    notification.extend_from_slice(&HID_INPUT_REPORT_HANDLE.to_le_bytes());
    notification.extend_from_slice(&report);
    send_l2cap(&peer.controller, handle, ATT_CID, &notification);
    keyboard.input_report = report;
    Ok(())
}

fn handle_smp(peer: &mut Peer, handle: u16, packet: &[u8]) -> Result<()> {
    ensure!(
        peer.hid_keyboard.is_some(),
        "SMP is accepted only by Bluetooth HID keyboards"
    );
    ensure!(
        peer.connection_handle == Some(handle),
        "SMP connection handle does not match the active keyboard link"
    );
    match packet.first().copied() {
        Some(0x01) if packet.len() == 7 => begin_legacy_pairing(peer, handle, packet),
        Some(0x03) if packet.len() == 17 => receive_pairing_confirm(peer, handle, packet),
        Some(0x04) if packet.len() == 17 => receive_pairing_random(peer, handle, packet),
        Some(0x05) if packet.len() == 2 => {
            if let Some(keyboard) = peer.hid_keyboard.as_mut() {
                keyboard.pairing = None;
            }
            Ok(())
        }
        Some(0x06..=0x0a) => Ok(()),
        Some(opcode) => bail!("unsupported SMP opcode or length: {opcode:#04x}"),
        None => bail!("empty SMP packet"),
    }
}

fn begin_legacy_pairing(peer: &mut Peer, handle: u16, packet: &[u8]) -> Result<()> {
    let request: [u8; 7] = packet.try_into().context("decode SMP Pairing Request")?;
    ensure!(
        (7..=16).contains(&request[4]),
        "SMP encryption key size is outside 7-16 bytes"
    );
    let bonding = request[3] & 0x03 != 0;
    let distribute_long_term_key = bonding && request[6] & 0x01 != 0;
    let response = [
        0x02,
        0x03,
        0x00,
        u8::from(bonding),
        request[4],
        0x00,
        u8::from(distribute_long_term_key),
    ];
    let mut local_random = [0_u8; 16];
    let mut long_term_key = [0_u8; 16];
    let mut random_number = [0_u8; 8];
    let mut ediv = [0_u8; 2];
    getrandom::fill(&mut local_random).context("generate SMP responder random")?;
    getrandom::fill(&mut long_term_key).context("generate SMP long term key")?;
    getrandom::fill(&mut random_number).context("generate SMP master identification random")?;
    getrandom::fill(&mut ediv).context("generate SMP encrypted diversifier")?;
    mask_encryption_key(&mut long_term_key, request[4]);
    let keyboard = peer
        .hid_keyboard
        .as_mut()
        .context("Bluetooth peer is not a HID keyboard")?;
    keyboard.pairing = Some(LegacyPairing {
        request,
        response,
        remote_confirm: None,
        local_random,
        short_term_key: None,
        long_term_key,
        encrypted_diversifier: u16::from_le_bytes(ediv),
        random_number,
        distribute_long_term_key,
    });
    tracing::info!(
        peer = %peer.name,
        handle,
        bonding,
        key_size = request[4],
        distribute_long_term_key,
        "Bluetooth HID legacy pairing started"
    );
    send_l2cap(&peer.controller, handle, SMP_CID, &response);
    Ok(())
}

fn receive_pairing_confirm(peer: &mut Peer, handle: u16, packet: &[u8]) -> Result<()> {
    let remote_confirm: [u8; 16] = packet[1..]
        .try_into()
        .context("decode SMP Pairing Confirm")?;
    let remote_address = peer
        .remote_address
        .context("SMP initiator address is unavailable")?;
    let (local_random, request, response) = {
        let pairing = peer
            .hid_keyboard
            .as_mut()
            .and_then(|keyboard| keyboard.pairing.as_mut())
            .context("SMP Pairing Confirm arrived without a Pairing Request")?;
        pairing.remote_confirm = Some(remote_confirm);
        (pairing.local_random, pairing.request, pairing.response)
    };
    let confirm = legacy_confirm(
        local_random,
        request,
        response,
        peer.remote_address_type,
        remote_address,
        peer.local_address,
    );
    let mut response_packet = vec![0x03];
    response_packet.extend_from_slice(&confirm);
    send_l2cap(&peer.controller, handle, SMP_CID, &response_packet);
    Ok(())
}

fn receive_pairing_random(peer: &mut Peer, handle: u16, packet: &[u8]) -> Result<()> {
    let remote_random: [u8; 16] = packet[1..]
        .try_into()
        .context("decode SMP Pairing Random")?;
    let remote_address = peer
        .remote_address
        .context("SMP initiator address is unavailable")?;
    let (remote_confirm, local_random, request, response) = {
        let pairing = peer
            .hid_keyboard
            .as_ref()
            .and_then(|keyboard| keyboard.pairing.as_ref())
            .context("SMP Pairing Random arrived without a Pairing Request")?;
        (
            pairing
                .remote_confirm
                .context("SMP Pairing Random arrived before Pairing Confirm")?,
            pairing.local_random,
            pairing.request,
            pairing.response,
        )
    };
    let expected = legacy_confirm(
        remote_random,
        request,
        response,
        peer.remote_address_type,
        remote_address,
        peer.local_address,
    );
    if !bool::from(remote_confirm.ct_eq(&expected)) {
        send_l2cap(&peer.controller, handle, SMP_CID, &[0x05, 0x04]);
        if let Some(keyboard) = peer.hid_keyboard.as_mut() {
            keyboard.pairing = None;
        }
        bail!("SMP Pairing Confirm verification failed");
    }
    let mut short_term_key = [0_u8; 16];
    short_term_key[..8].copy_from_slice(&remote_random[..8]);
    short_term_key[8..].copy_from_slice(&local_random[..8]);
    short_term_key = ble_aes_128([0; 16], short_term_key);
    let key_size = request[4];
    mask_encryption_key(&mut short_term_key, key_size);
    let pairing = peer
        .hid_keyboard
        .as_mut()
        .and_then(|keyboard| keyboard.pairing.as_mut())
        .context("SMP pairing state disappeared")?;
    pairing.short_term_key = Some(short_term_key);
    let mut response_packet = vec![0x04];
    response_packet.extend_from_slice(&local_random);
    send_l2cap(&peer.controller, handle, SMP_CID, &response_packet);
    Ok(())
}

fn legacy_confirm(
    random: [u8; 16],
    request: [u8; 7],
    response: [u8; 7],
    initiator_address_type: u8,
    initiator_address: [u8; 6],
    responder_address: [u8; 6],
) -> [u8; 16] {
    let mut p1 = [0_u8; 16];
    p1[0] = initiator_address_type;
    p1[1] = 0;
    p1[2..9].copy_from_slice(&request);
    p1[9..].copy_from_slice(&response);
    let mut p2 = [0_u8; 16];
    p2[..6].copy_from_slice(&responder_address);
    p2[6..12].copy_from_slice(&initiator_address);
    let first = ble_aes_128([0; 16], xor_128(random, p1));
    ble_aes_128([0; 16], xor_128(first, p2))
}

fn ble_aes_128(key: [u8; 16], message: [u8; 16]) -> [u8; 16] {
    let mut reversed_key = key;
    let mut reversed_message = message;
    reversed_key.reverse();
    reversed_message.reverse();
    let cipher = Aes128::new(&reversed_key.into());
    let mut block = reversed_message.into();
    cipher.encrypt_block(&mut block);
    let mut output = [0_u8; 16];
    output.copy_from_slice(&block);
    output.reverse();
    output
}

fn xor_128(mut left: [u8; 16], right: [u8; 16]) -> [u8; 16] {
    for (left, right) in left.iter_mut().zip(right) {
        *left ^= right;
    }
    left
}

fn mask_encryption_key(key: &mut [u8; 16], size: u8) {
    key[usize::from(size)..].fill(0);
}

fn handle_long_term_key_request(peer: &mut Peer, packet: &[u8]) {
    let handle = u16::from_le_bytes([packet[3], packet[4]]) & 0x0fff;
    if peer.connection_handle != Some(handle) {
        return;
    }
    let random_number: [u8; 8] = packet[5..13].try_into().unwrap_or([0; 8]);
    let encrypted_diversifier = u16::from_le_bytes([packet[13], packet[14]]);
    let keyboard = peer.hid_keyboard.as_ref();
    let key = if random_number == [0; 8] && encrypted_diversifier == 0 {
        keyboard
            .and_then(|keyboard| keyboard.pairing.as_ref())
            .and_then(|pairing| pairing.short_term_key)
    } else {
        keyboard.and_then(|keyboard| {
            keyboard.bond.as_ref().and_then(|bond| {
                (bond.random_number == random_number
                    && bond.encrypted_diversifier == encrypted_diversifier)
                    .then_some(bond.long_term_key)
            })
        })
    };
    if let Some(key) = key {
        let mut parameters = Vec::with_capacity(18);
        parameters.extend_from_slice(&handle.to_le_bytes());
        parameters.extend_from_slice(&key);
        send_command(&peer.controller, 0x201a, &parameters);
    } else {
        send_command(&peer.controller, 0x201b, &handle.to_le_bytes());
    }
}

fn distribute_legacy_bond(peer: &mut Peer) {
    let Some(handle) = peer.connection_handle else {
        return;
    };
    let Some((long_term_key, encrypted_diversifier, random_number)) = peer
        .hid_keyboard
        .as_mut()
        .and_then(|keyboard| keyboard.pairing.as_mut())
        .and_then(|pairing| {
            if pairing.distribute_long_term_key {
                pairing.distribute_long_term_key = false;
                Some((
                    pairing.long_term_key,
                    pairing.encrypted_diversifier,
                    pairing.random_number,
                ))
            } else {
                None
            }
        })
    else {
        return;
    };
    let mut encryption_information = vec![0x06];
    encryption_information.extend_from_slice(&long_term_key);
    send_l2cap(&peer.controller, handle, SMP_CID, &encryption_information);
    let mut master_identification = vec![0x07];
    master_identification.extend_from_slice(&encrypted_diversifier.to_le_bytes());
    master_identification.extend_from_slice(&random_number);
    send_l2cap(&peer.controller, handle, SMP_CID, &master_identification);
    if let Some(keyboard) = peer.hid_keyboard.as_mut() {
        keyboard.bond = Some(LegacyBond {
            long_term_key,
            encrypted_diversifier,
            random_number,
        });
    }
}

async fn bridge_guest_h4(
    endpoint: DeviceSerialEndpointV2,
    engine: std_mpsc::Sender<EngineCommand>,
    mut controller_output: mpsc::UnboundedReceiver<H4Packet>,
) -> Result<()> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;

        loop {
            let mut guest_output = open_pipe_with_retry(
                || {
                    ClientOptions::new()
                        .read(true)
                        .write(false)
                        .open(&endpoint.guest_output)
                },
                "Guest Bluetooth output pipe did not appear",
            )
            .await?;
            let mut guest_input = open_pipe_with_retry(
                || {
                    ClientOptions::new()
                        .read(false)
                        .write(true)
                        .open(&endpoint.guest_input)
                },
                "Guest Bluetooth input pipe did not appear",
            )
            .await?;
            let reader = async {
                loop {
                    let packet = read_h4_packet(&mut guest_output).await?;
                    engine
                        .send(EngineCommand::GuestHci(packet))
                        .map_err(|_| anyhow::anyhow!("AOSP RootCanal engine stopped"))?;
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            };
            let writer = async {
                while let Some(packet) = controller_output.recv().await {
                    guest_input.write_all(&[packet.packet_type]).await?;
                    guest_input.write_all(&packet.payload).await?;
                    guest_input.flush().await?;
                }
                Err::<(), anyhow::Error>(anyhow::anyhow!(
                    "AOSP RootCanal HCI output channel closed"
                ))
            };
            match tokio::try_join!(reader, writer) {
                Err(error) if is_pipe_disconnect(&error) => {
                    tracing::warn!(%error, "Guest Bluetooth pipe disconnected; reconnecting");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error),
                Ok(_) => bail!("RootCanal Guest H4 bridge exited unexpectedly"),
            }
        }
    }
    #[cfg(unix)]
    {
        let mut guest_output = tokio::fs::OpenOptions::new()
            .read(true)
            .open(&endpoint.guest_output)
            .await
            .context("open Guest Bluetooth output")?;
        let mut guest_input = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&endpoint.guest_input)
            .await
            .context("open Guest Bluetooth input")?;
        let mut pending = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            tokio::select! {
                read = guest_output.read(&mut chunk) => {
                    let read = read.context("read Guest Bluetooth output")?;
                    if read == 0 {
                        // crosvm appends H4 bytes to a regular file on Unix. EOF is temporary.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        continue;
                    }
                    pending.extend_from_slice(&chunk[..read]);
                    while let Some(packet) = decode_h4_buffer(&mut pending)? {
                        engine
                            .send(EngineCommand::GuestHci(packet))
                            .map_err(|_| anyhow::anyhow!("AOSP RootCanal engine stopped"))?;
                    }
                }
                packet = controller_output.recv() => {
                    let packet = packet.context("AOSP RootCanal HCI output channel closed")?;
                    guest_input.write_all(&[packet.packet_type]).await?;
                    guest_input.write_all(&packet.payload).await?;
                    guest_input.flush().await?;
                }
            }
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (endpoint, engine, controller_output);
        bail!("RootCanal Guest H4 bridge is unsupported on this platform")
    }
}

#[cfg(windows)]
async fn open_pipe_with_retry<F>(
    mut open: F,
    timeout_message: &'static str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient>
where
    F: FnMut() -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient>,
{
    tokio::time::timeout(PIPE_OPEN_TIMEOUT, async {
        loop {
            match open() {
                Ok(pipe) => return Ok(pipe),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::WouldBlock
                    ) || error.raw_os_error() == Some(231) =>
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .context(timeout_message)?
    .map_err(Into::into)
}

#[cfg(windows)]
fn is_pipe_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::UnexpectedEof
            ) || matches!(io.raw_os_error(), Some(109 | 232 | 233))
        })
    })
}

#[cfg(windows)]
async fn read_h4_packet<R>(reader: &mut R) -> Result<H4Packet>
where
    R: AsyncRead + Unpin,
{
    let mut kind = [0_u8; 1];
    reader.read_exact(&mut kind).await?;
    let header_length = match kind[0] {
        1 | 3 => 3,
        2 | 5 => 4,
        other => bail!("Guest emitted unsupported H4 packet type {other}"),
    };
    let mut payload = vec![0_u8; header_length];
    reader.read_exact(&mut payload).await?;
    let body_length = match kind[0] {
        1 | 3 => usize::from(payload[2]),
        2 => usize::from(u16::from_le_bytes([payload[2], payload[3]])),
        5 => usize::from(u16::from_le_bytes([payload[2], payload[3]]) & 0x3fff),
        _ => unreachable!(),
    };
    ensure!(
        header_length + body_length <= MAX_H4_PACKET_BYTES,
        "Guest H4 packet exceeded the size limit"
    );
    payload.resize(header_length + body_length, 0);
    reader.read_exact(&mut payload[header_length..]).await?;
    Ok(H4Packet {
        packet_type: kind[0],
        payload,
    })
}

#[cfg(unix)]
fn decode_h4_buffer(buffer: &mut Vec<u8>) -> Result<Option<H4Packet>> {
    let Some(&packet_type) = buffer.first() else {
        return Ok(None);
    };
    let header_length = match packet_type {
        1 | 3 => 3,
        2 | 5 => 4,
        other => bail!("Guest emitted unsupported H4 packet type {other}"),
    };
    if buffer.len() < 1 + header_length {
        return Ok(None);
    }
    let payload = &buffer[1..];
    let body_length = match packet_type {
        1 | 3 => usize::from(payload[2]),
        2 => usize::from(u16::from_le_bytes([payload[2], payload[3]])),
        5 => usize::from(u16::from_le_bytes([payload[2], payload[3]]) & 0x3fff),
        _ => unreachable!(),
    };
    let payload_length = header_length + body_length;
    ensure!(
        payload_length <= MAX_H4_PACKET_BYTES,
        "Guest H4 packet exceeded the size limit"
    );
    let packet_length = 1 + payload_length;
    if buffer.len() < packet_length {
        return Ok(None);
    }
    let payload = buffer[1..packet_length].to_vec();
    buffer.drain(..packet_length);
    Ok(Some(H4Packet {
        packet_type,
        payload,
    }))
}

fn publish_ready(context: &FormalContext, launch_bytes: &[u8]) -> Result<()> {
    let ready = FormalComponentReadyV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        component: context.launch.component.clone(),
        instance_id: context.launch.instance_id,
        run_id: context.launch.run_id,
        launch_sha256: hex::encode(Sha256::digest(launch_bytes)),
        pid: std::process::id(),
        process_start_marker: hd_platform::process_start_marker(std::process::id())?,
    };
    hd_platform::write_owner_only(
        &context.launch.component_ready_marker,
        &serde_json::to_vec_pretty(&ready)?,
    )?;
    Ok(())
}

async fn run_control_server(context: Arc<FormalContext>, launch_bytes: &[u8]) -> Result<()> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;

        let mut first = true;
        loop {
            let mut options = ServerOptions::new();
            options
                .first_pipe_instance(first)
                .reject_remote_clients(true);
            let server =
                hd_platform::create_owner_only_named_pipe(&options, &context.control_endpoint)?;
            if first {
                publish_ready(&context, launch_bytes)?;
            }
            first = false;
            server
                .connect()
                .await
                .context("connect RootCanal control pipe")?;
            let connection_context = Arc::clone(&context);
            tokio::spawn(async move {
                if let Err(error) = serve_connection(server, connection_context).await {
                    tracing::warn!(%error, "RootCanal control connection failed");
                }
            });
        }
    }
    #[cfg(unix)]
    {
        let path = PathBuf::from(&context.control_endpoint);
        let (listener, _guard) = bind_owner_only_unix_listener(&path).await?;
        publish_ready(&context, launch_bytes)?;
        loop {
            let (stream, _) = listener.accept().await?;
            let connection_context = Arc::clone(&context);
            tokio::spawn(async move {
                if let Err(error) = serve_connection(stream, connection_context).await {
                    tracing::warn!(%error, "RootCanal control connection failed");
                }
            });
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (context, launch_bytes);
        bail!("RootCanal formal control server is unsupported on this platform")
    }
}

#[cfg(unix)]
async fn bind_owner_only_unix_listener(
    path: &Path,
) -> Result<(tokio::net::UnixListener, SocketGuard)> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};

    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        ensure!(
            metadata.file_type().is_socket(),
            "stale RootCanal control endpoint is not a socket"
        );
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => bail!("RootCanal control endpoint already has an active listener"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(path)?;
            }
            Err(error) => return Err(error).context("validate stale RootCanal control socket"),
        }
    }
    let listener = tokio::net::UnixListener::bind(path).context("bind RootCanal control socket")?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let metadata = std::fs::symlink_metadata(path)?;
    Ok((
        listener,
        SocketGuard {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    ))
}

#[cfg(unix)]
struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl Drop for SocketGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn serve_connection<S>(stream: S, context: Arc<FormalContext>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("read RootCanal control request")?
    {
        ensure!(
            line.len() <= MAX_CONTROL_MESSAGE_BYTES,
            "RootCanal control request exceeded the size limit"
        );
        let request: DeviceControlRequestV2 =
            serde_json::from_str(&line).context("decode RootCanal control request")?;
        let response = handle_control(&context, request).await;
        write_line(&mut writer, &response).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn handle_control(
    context: &FormalContext,
    request: DeviceControlRequestV2,
) -> DeviceControlResponseV2 {
    let response = |ok, error| DeviceControlResponseV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        request_id: request.request_id,
        instance_id: request.instance_id,
        run_id: request.run_id,
        ok,
        error,
    };
    if request.protocol_version != COMPONENT_PROTOCOL_VERSION
        || request.instance_id != context.launch.instance_id
        || request.run_id != context.launch.run_id
    {
        return response(
            false,
            Some(ApiErrorV2::new(
                "device_control_context",
                "RootCanal control protocol or run identity mismatch",
            )),
        );
    }
    let supplied = request.bearer_token.as_bytes();
    let expected = context.control_token.as_bytes();
    if supplied.len() != expected.len() || supplied.ct_eq(expected).unwrap_u8() != 1 {
        return response(
            false,
            Some(ApiErrorV2::new(
                "device_control_auth",
                "RootCanal control authorization failed",
            )),
        );
    }
    match request.command {
        DeviceControlCommandV2::Ping => response(true, None),
        DeviceControlCommandV2::Action {
            action: InstanceActionV2::BluetoothPeer { action },
        } => {
            if let Err(error) = (InstanceActionV2::BluetoothPeer {
                action: action.clone(),
            })
            .validate()
            {
                return response(
                    false,
                    Some(ApiErrorV2::new(
                        "bluetooth_action_invalid",
                        error.to_string(),
                    )),
                );
            }
            if let BluetoothPeerActionV2::CaptureHci {
                capture_id,
                duration_ms,
            } = &action
            {
                return match capture_hci(context, *capture_id, *duration_ms).await {
                    Ok(()) => response(true, None),
                    Err(error) => response(
                        false,
                        Some(ApiErrorV2::new("bluetooth_hci_capture_failed", error)),
                    ),
                };
            }
            let (result_tx, result_rx) = oneshot::channel();
            if context
                .engine
                .send(EngineCommand::Action {
                    action,
                    response: result_tx,
                })
                .is_err()
            {
                return response(
                    false,
                    Some(ApiErrorV2::new(
                        "bluetooth_action_failed",
                        "AOSP RootCanal engine is unavailable",
                    )),
                );
            }
            match result_rx.await {
                Ok(Ok(())) => response(true, None),
                Ok(Err(error)) => response(
                    false,
                    Some(ApiErrorV2::new("bluetooth_action_failed", error)),
                ),
                Err(_) => response(
                    false,
                    Some(ApiErrorV2::new(
                        "bluetooth_action_failed",
                        "AOSP RootCanal engine dropped the action response",
                    )),
                ),
            }
        }
        DeviceControlCommandV2::Action { .. } => response(
            false,
            Some(ApiErrorV2::new(
                "bluetooth_action_failed",
                "action is not implemented by rootcanal-adapter",
            )),
        ),
    }
}

async fn capture_hci(
    context: &FormalContext,
    capture_id: Uuid,
    duration_ms: u32,
) -> std::result::Result<(), String> {
    let component_directory = context
        .launch
        .component_ready_marker
        .parent()
        .ok_or_else(|| "RootCanal component directory is unavailable".to_owned())?;
    let capture_path = component_directory.join(format!("rootcanal-hci-{capture_id}.btsnoop"));
    let metadata_path = component_directory.join(format!("rootcanal-hci-{capture_id}.json"));
    let (start_tx, start_rx) = oneshot::channel();
    context
        .engine
        .send(EngineCommand::StartCapture {
            capture_id,
            capture_path,
            metadata_path,
            requested_duration_ms: duration_ms,
            response: start_tx,
        })
        .map_err(|_| "AOSP RootCanal engine is unavailable".to_owned())?;
    start_rx
        .await
        .map_err(|_| "AOSP RootCanal engine dropped the capture start response".to_owned())??;
    tokio::time::sleep(Duration::from_millis(u64::from(duration_ms))).await;
    let (stop_tx, stop_rx) = oneshot::channel();
    context
        .engine
        .send(EngineCommand::StopCapture {
            capture_id,
            response: stop_tx,
        })
        .map_err(|_| "AOSP RootCanal engine is unavailable".to_owned())?;
    stop_rx
        .await
        .map_err(|_| "AOSP RootCanal engine dropped the capture stop response".to_owned())?
}

async fn write_line<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_CONTROL_MESSAGE_BYTES,
        "RootCanal control response exceeded the size limit"
    );
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
