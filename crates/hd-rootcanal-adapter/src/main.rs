#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc as std_mpsc};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use clap::Parser;
use hd_core::{
    ApiErrorV2, BluetoothPeerActionV2, COMPONENT_PROTOCOL_VERSION, DeviceControlCommandV2,
    DeviceControlRequestV2, DeviceControlResponseV2, DeviceControlTokenV2, DeviceSerialEndpointV2,
    FormalComponentConfigurationV2, FormalComponentLaunchV2, FormalComponentProbeV2,
    FormalComponentReadyV2, InstanceActionV2,
};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_H4_PACKET_BYTES: usize = 65_535;
const PIPE_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const ENGINE_TICK: Duration = Duration::from_millis(5);
const GATT_MTU: u16 = 247;

#[derive(Debug, Parser)]
#[command(about = "Formal HD Windows adapter embedding the AOSP RootCanal controller")]
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
    Shutdown,
}

#[derive(Debug)]
struct H4Packet {
    packet_type: u8,
    payload: Vec<u8>,
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
    name: String,
    connection_handle: Option<u16>,
    advertising: bool,
}

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
        formal: true,
        features: vec![
            "guest-serial-v2".to_owned(),
            "hci-v2".to_owned(),
            "hci-h4-v2".to_owned(),
            "aosp-rootcanal-v2".to_owned(),
            "peer-control-v2".to_owned(),
            "gatt-peer-control-v2".to_owned(),
        ],
    }
}

async fn serve_formal(launch_path: &Path) -> Result<()> {
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
    loop {
        drain_engine_events(&event_rx, &guest, &mut peers, guest_hci)?;
        match commands.recv_timeout(ENGINE_TICK) {
            Ok(EngineCommand::GuestHci(packet)) => {
                guest.receive_hci(packet.packet_type, &packet.payload);
            }
            Ok(EngineCommand::Action { action, response }) => {
                let result = apply_peer_action(action, &event_tx, &mut peers)
                    .map_err(|error| error.to_string());
                let _ = response.send(result);
            }
            Ok(EngineCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
        }
        guest.tick();
        for peer in peers.values() {
            peer.controller.tick();
        }
    }
    Ok(())
}

fn drain_engine_events(
    events: &std_mpsc::Receiver<EngineEvent>,
    guest: &Controller,
    peers: &mut BTreeMap<Uuid, Peer>,
    guest_hci: &mpsc::UnboundedSender<H4Packet>,
) -> Result<()> {
    while let Ok(event) = events.try_recv() {
        match event {
            EngineEvent::Hci {
                source: ControllerSource::Guest,
                packet_type,
                payload,
            } => {
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
            ensure!(
                !peers.contains_key(&peer_id),
                "Bluetooth peer already exists"
            );
            let controller = Controller::new(
                peer_address(peer_id),
                ControllerSource::Peer(peer_id),
                events.clone(),
            )?;
            let peer = Peer {
                controller,
                name,
                connection_handle: None,
                advertising: true,
            };
            initialize_peer(&peer);
            peers.insert(peer_id, peer);
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
        }
    }
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
    send_command(
        &peer.controller,
        0x2006,
        &[
            0xa0, 0x00, 0xa0, 0x00, 0x00, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0x07, 0x00,
        ],
    );
    let mut advertising_data = Vec::with_capacity(31);
    advertising_data.extend_from_slice(&[0x02, 0x01, 0x06]);
    let name = peer.name.as_bytes();
    let name_len = name.len().min(26);
    advertising_data.push(u8::try_from(name_len + 1).unwrap_or(27));
    advertising_data.push(0x09);
    advertising_data.extend_from_slice(&name[..name_len]);
    let mut parameters = vec![u8::try_from(advertising_data.len()).unwrap_or(31)];
    parameters.extend_from_slice(&advertising_data);
    parameters.resize(32, 0);
    send_command(&peer.controller, 0x2008, &parameters);
    set_advertising(peer, true);
}

fn set_advertising(peer: &Peer, enabled: bool) {
    send_command(&peer.controller, 0x200a, &[u8::from(enabled)]);
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
    } else if packet.len() >= 6 && packet[0] == 0x05 && packet[2] == 0 {
        peer.connection_handle = None;
        if peer.advertising {
            set_advertising(peer, true);
        }
    }
}

fn handle_peer_acl(peer: &Peer, packet: &[u8]) {
    if packet.len() < 8 {
        return;
    }
    let handle = u16::from_le_bytes([packet[0], packet[1]]) & 0x0fff;
    let acl_length = usize::from(u16::from_le_bytes([packet[2], packet[3]]));
    if packet.len() < 4 + acl_length || u16::from_le_bytes([packet[6], packet[7]]) != 0x0004 {
        return;
    }
    let l2cap_length = usize::from(u16::from_le_bytes([packet[4], packet[5]]));
    if packet.len() < 8 + l2cap_length {
        return;
    }
    let request = &packet[8..8 + l2cap_length];
    if let Some(response) = gatt_response(&peer.name, request) {
        send_att(&peer.controller, handle, &response);
    }
}

fn gatt_response(name: &str, request: &[u8]) -> Option<Vec<u8>> {
    let opcode = *request.first()?;
    let [mtu_low, mtu_high] = GATT_MTU.to_le_bytes();
    match opcode {
        0x02 if request.len() >= 3 => Some(vec![0x03, mtu_low, mtu_high]),
        0x04 if request.len() >= 5 => {
            let start = u16::from_le_bytes([request[1], request[2]]);
            if start > 3 {
                Some(att_error(opcode, start, 0x0a))
            } else {
                let mut response = vec![0x05, 0x01];
                for (handle, uuid) in [(1_u16, 0x2800_u16), (2, 0x2803), (3, 0x2a00)] {
                    if handle >= start {
                        response.extend_from_slice(&handle.to_le_bytes());
                        response.extend_from_slice(&uuid.to_le_bytes());
                    }
                }
                Some(response)
            }
        }
        0x08 if request.len() >= 7 => {
            let start = u16::from_le_bytes([request[1], request[2]]);
            let kind = u16::from_le_bytes([request[5], request[6]]);
            match kind {
                0x2800 if start <= 1 => Some(vec![0x09, 0x06, 1, 0, 3, 0, 0, 0x18]),
                0x2803 if start <= 2 => Some(vec![0x09, 0x07, 2, 0, 0x02, 3, 0, 0, 0x2a]),
                _ => Some(att_error(opcode, start, 0x0a)),
            }
        }
        0x0a if request.len() >= 3 => {
            let handle = u16::from_le_bytes([request[1], request[2]]);
            if handle == 3 {
                let mut response = vec![0x0b];
                response.extend_from_slice(name.as_bytes());
                response.truncate(usize::from(GATT_MTU));
                Some(response)
            } else {
                Some(att_error(opcode, handle, 0x0a))
            }
        }
        0x10 if request.len() >= 7 => {
            let start = u16::from_le_bytes([request[1], request[2]]);
            let kind = u16::from_le_bytes([request[5], request[6]]);
            if kind == 0x2800 && start <= 1 {
                Some(vec![0x11, 0x06, 1, 0, 3, 0, 0, 0x18])
            } else {
                Some(att_error(opcode, start, 0x0a))
            }
        }
        0x12 if request.len() >= 3 => {
            let handle = u16::from_le_bytes([request[1], request[2]]);
            Some(att_error(opcode, handle, 0x03))
        }
        0x52 | 0x1e => None,
        _ => Some(att_error(opcode, 0, 0x06)),
    }
}

fn att_error(request_opcode: u8, handle: u16, error: u8) -> Vec<u8> {
    let [low, high] = handle.to_le_bytes();
    vec![0x01, request_opcode, low, high, error]
}

fn send_att(controller: &Controller, handle: u16, att: &[u8]) {
    let Ok(l2cap_length) = u16::try_from(att.len()) else {
        return;
    };
    let total_length = l2cap_length.saturating_add(4);
    let mut packet = Vec::with_capacity(usize::from(total_length) + 4);
    packet.extend_from_slice(&(handle | 0x2000).to_le_bytes());
    packet.extend_from_slice(&total_length.to_le_bytes());
    packet.extend_from_slice(&l2cap_length.to_le_bytes());
    packet.extend_from_slice(&0x0004_u16.to_le_bytes());
    packet.extend_from_slice(att);
    controller.receive_hci(2, &packet);
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
    #[cfg(not(windows))]
    {
        let _ = (endpoint, engine, controller_output);
        bail!("RootCanal Guest H4 bridge is currently implemented for Windows only")
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
    #[cfg(not(windows))]
    {
        let _ = (context, launch_bytes);
        bail!("RootCanal formal control server is currently implemented for Windows only")
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
