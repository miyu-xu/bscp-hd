#![allow(unsafe_code)]
#![cfg_attr(not(windows), allow(clippy::unused_async))]

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::time::Duration;

#[cfg(any(windows, test))]
use anyhow::ensure;
use anyhow::{Context as _, Result, bail};
use clap::Parser;
#[cfg(windows)]
use hd_core::{
    ApiErrorV2, DeviceControlCommandV2, DeviceControlRequestV2, DeviceControlResponseV2,
    DeviceControlTokenV2, DeviceSerialEndpointV2, FormalComponentConfigurationV2,
    FormalComponentLaunchV2, FormalComponentReadyV2,
};
use hd_core::{COMPONENT_PROTOCOL_VERSION, FormalComponentProbeV2};
#[cfg(windows)]
use sha2::{Digest as _, Sha256};
#[cfg(windows)]
use subtle::ConstantTimeEq as _;
#[cfg(windows)]
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[cfg(windows)]
const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;
#[cfg(windows)]
const MAX_GUEST_MESSAGE_BYTES: usize = 64 * 1024;
#[cfg(windows)]
const PIPE_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(windows)]
const MODEM_VSOCK_PORT: u32 = 9697;
#[cfg(windows)]
const VSOCK_PIPE_HEADER_BYTES: usize = 8;

#[derive(Debug, Parser)]
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

#[derive(Debug)]
struct AdapterProfile {
    component: &'static str,
    #[cfg(windows)]
    guest_role: &'static str,
    features: &'static [&'static str],
}

#[cfg(windows)]
#[derive(Debug)]
struct FormalContext {
    launch: FormalComponentLaunchV2,
    control_endpoint: String,
    control_token: DeviceControlTokenV2,
}

pub fn run(component: &'static str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(format!("hd-{component}"))
        .build()
        .context("create peripheral adapter runtime")?;
    runtime.block_on(run_async(profile(component)?))
}

async fn run_async(profile: AdapterProfile) -> Result<()> {
    let arguments = Arguments::parse();
    match (
        arguments.probe_v2,
        arguments.json,
        arguments.serve_v2,
        arguments.launch.as_deref(),
    ) {
        (true, true, false, None) => {
            println!("{}", serde_json::to_string(&probe(&profile))?);
            Ok(())
        }
        (false, false, true, Some(path)) => serve_formal(&profile, path).await,
        _ => bail!("select --probe-v2 --json or --serve-v2 --launch <path>"),
    }
}

fn profile(component: &str) -> Result<AdapterProfile> {
    let profile = match component {
        "uwb-adapter" => AdapterProfile {
            component: "uwb-adapter",
            #[cfg(windows)]
            guest_role: "uwb",
            features: &[
                "guest-serial-v2",
                "uwb-control-v2",
                "deterministic-ranging-v2",
            ],
        },
        "modem-adapter" => AdapterProfile {
            component: "modem-adapter",
            #[cfg(windows)]
            guest_role: "modem",
            features: &[
                "guest-vsock-v2",
                "modem-control-v2",
                "at-command-baseline-v2",
            ],
        },
        "network-adapter" => AdapterProfile {
            component: "network-adapter",
            #[cfg(windows)]
            guest_role: "network-control",
            features: &["guest-serial-v2", "network-control-v2", "virtio-net-v2"],
        },
        "audio-adapter" => AdapterProfile {
            component: "audio-adapter",
            #[cfg(windows)]
            guest_role: "audio-control",
            features: &["guest-serial-v2", "audio-control-v2", "virtio-snd-v2"],
        },
        "camera-adapter" => AdapterProfile {
            component: "camera-adapter",
            #[cfg(windows)]
            guest_role: "camera-control",
            features: &["guest-serial-v2", "camera-control-v2", "guest-camera-v2"],
        },
        other => bail!("unsupported peripheral adapter component {other}"),
    };
    Ok(profile)
}

fn probe(profile: &AdapterProfile) -> FormalComponentProbeV2 {
    FormalComponentProbeV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        component: profile.component.to_owned(),
        formal: true,
        features: profile
            .features
            .iter()
            .chain([
                &"ready-marker-v2",
                &"lifecycle-v2",
                &"windows-owner-pipe-v2",
            ])
            .map(|feature| (*feature).to_owned())
            .collect(),
    }
}

async fn serve_formal(profile: &AdapterProfile, launch_path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        serve_formal_windows(profile, launch_path).await
    }
    #[cfg(not(windows))]
    {
        let _ = (profile, launch_path);
        bail!("peripheral adapters currently require Windows")
    }
}

#[cfg(windows)]
async fn serve_formal_windows(profile: &AdapterProfile, launch_path: &Path) -> Result<()> {
    let launch_bytes = hd_platform::read_regular_nofollow_limited(launch_path, 64 * 1024)
        .context("read peripheral adapter launch")?;
    let launch: FormalComponentLaunchV2 =
        serde_json::from_slice(&launch_bytes).context("decode peripheral adapter launch")?;
    ensure!(
        launch.protocol_version == COMPONENT_PROTOCOL_VERSION
            && launch.component == profile.component,
        "formal launch protocol or component mismatch"
    );
    let expected_marker = format!("{}-ready-v2.json", profile.component);
    ensure!(
        launch.component_ready_marker.parent() == launch_path.parent()
            && launch
                .component_ready_marker
                .file_name()
                .and_then(|name| name.to_str())
                == Some(expected_marker.as_str()),
        "formal ready marker is outside the launch directory"
    );
    let (control_endpoint, control_token, guest_cid, guest_endpoint) = match &launch.configuration {
        FormalComponentConfigurationV2::DeviceAdapter {
            control_endpoint,
            control_token,
            guest_cid,
            guest_endpoints,
            ..
        } => (
            control_endpoint.clone(),
            control_token.clone(),
            *guest_cid,
            guest_endpoints.get(profile.guest_role).cloned(),
        ),
        _ => bail!(
            "{} requires a device_adapter configuration",
            profile.component
        ),
    };
    let context = Arc::new(FormalContext {
        launch,
        control_endpoint,
        control_token,
    });
    let guest = if profile.component == "modem-adapter" {
        tokio::spawn(serve_modem_vsock(guest_cid))
    } else {
        let guest_endpoint = guest_endpoint.with_context(|| {
            format!(
                "{} requires the {} Guest endpoint",
                profile.component, profile.guest_role
            )
        })?;
        tokio::spawn(serve_guest_channel(profile.component, guest_endpoint))
    };
    let control = run_control_server(Arc::clone(&context), &launch_bytes);
    tokio::pin!(control);
    tokio::select! {
        result = &mut control => result,
        result = guest => match result {
            Ok(Ok(())) => bail!("{} Guest channel exited unexpectedly", profile.component),
            Ok(Err(error)) => Err(error).with_context(|| format!("{} Guest channel failed", profile.component)),
            Err(error) => Err(error).with_context(|| format!("{} Guest task failed", profile.component)),
        },
    }
}

#[cfg(windows)]
async fn serve_modem_vsock(guest_cid: u32) -> Result<()> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;

        let endpoint = format!(r"\\.\pipe\binder_rpc_vsock_{guest_cid}_{MODEM_VSOCK_PORT}");
        let mut first_instance = true;
        loop {
            let mut options = ServerOptions::new();
            options
                .first_pipe_instance(first_instance)
                .reject_remote_clients(true);
            let mut pipe = hd_platform::create_owner_only_named_pipe(&options, &endpoint)
                .context("create owner-only modem vsock pipe")?;
            first_instance = false;
            pipe.connect()
                .await
                .context("accept guest modem vsock connection")?;
            if let Err(error) = serve_modem_vsock_connection(&mut pipe).await {
                let disconnected = error.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::UnexpectedEof
                    )
                });
                if !disconnected {
                    return Err(error).context("serve guest modem vsock connection");
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = guest_cid;
        bail!("modem Guest vsock is currently implemented for Windows only")
    }
}

#[cfg(windows)]
async fn serve_modem_vsock_connection<T>(pipe: &mut T) -> Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut pending = Vec::new();
    loop {
        let mut header = [0_u8; VSOCK_PIPE_HEADER_BYTES];
        pipe.read_exact(&mut header).await?;
        let payload_size = u32::from_le_bytes(header[..4].try_into()?) as usize;
        let handle_count = u32::from_le_bytes(header[4..].try_into()?);
        ensure!(
            payload_size <= MAX_GUEST_MESSAGE_BYTES,
            "modem vsock payload exceeds the formal limit"
        );
        ensure!(handle_count == 0, "modem vsock frame contains handles");
        let mut payload = vec![0_u8; payload_size];
        pipe.read_exact(&mut payload).await?;
        pending.extend_from_slice(&payload);
        ensure!(
            pending.len() <= MAX_GUEST_MESSAGE_BYTES,
            "modem AT command exceeds the formal limit"
        );
        let response = take_modem_responses(&mut pending);
        if response.is_empty() {
            continue;
        }
        let response_size = u32::try_from(response.len())?;
        pipe.write_all(&response_size.to_le_bytes()).await?;
        pipe.write_all(&0_u32.to_le_bytes()).await?;
        pipe.write_all(&response).await?;
        pipe.flush().await?;
    }
}

#[cfg(any(windows, test))]
fn take_modem_responses(pending: &mut Vec<u8>) -> Vec<u8> {
    let Some(last_delimiter) = pending
        .iter()
        .rposition(|byte| matches!(byte, b'\r' | b'\n'))
    else {
        return Vec::new();
    };
    let complete: Vec<_> = pending.drain(..=last_delimiter).collect();
    complete
        .split(|byte| matches!(byte, b'\r' | b'\n'))
        .filter(|command| !command.is_empty())
        .flat_map(modem_response)
        .collect()
}

#[cfg(any(windows, test))]
fn modem_response(command: &[u8]) -> Vec<u8> {
    let command = String::from_utf8_lossy(command).trim().to_ascii_uppercase();
    let response = if command.contains("AT+COPS=3,0;") && command.matches("+COPS?").count() == 3 {
        "\r\n+COPS: 0,0,\"HD Mobile\"\r\n+COPS: 0,1,\"HD\"\r\n+COPS: 0,2,\"00101\",7\r\n\r\nOK\r\n"
    } else if command == "AT+CSQ" {
        "\r\n+CSQ: 20,2,80,100,70,100,6,70,90,15,100,12,100\r\n\r\nOK\r\n"
    } else if command == "AT+COPS?" {
        "\r\n+COPS: 0,2,\"00101\",7\r\n\r\nOK\r\n"
    } else if command == "AT+CREG?" {
        "\r\n+CREG: 2,1,\"0001\",\"00000001\",7\r\n\r\nOK\r\n"
    } else if command == "AT+CGREG?" {
        "\r\n+CGREG: 2,1,\"0001\",\"00000001\",7\r\n\r\nOK\r\n"
    } else if command == "AT+CEREG?" {
        "\r\n+CEREG: 2,1,\"0001\",\"00000001\",7\r\n\r\nOK\r\n"
    } else if command == "AT+CPIN?" {
        "\r\n+CPIN: READY\r\n\r\nOK\r\n"
    } else if command == "AT+CFUN?" {
        "\r\n+CFUN: 1\r\n\r\nOK\r\n"
    } else if command == "AT+CTEC?" {
        "\r\n+CTEC: 5,20\r\n\r\nOK\r\n"
    } else if command == "AT+CTEC=?" {
        "\r\n+CTEC: 0,1,2,3,4,5\r\n\r\nOK\r\n"
    } else if command == "AT+CIMI" {
        "\r\n001010123456789\r\n\r\nOK\r\n"
    } else if command == "AT+CGSN" {
        "\r\n867400022047199\r\n\r\nOK\r\n"
    } else if command == "AT+CICCID" {
        "\r\n89014103211118510720\r\n\r\nOK\r\n"
    } else if command.starts_with("AT+CLCK=") && command.contains(",2,") {
        "\r\n+CLCK: 0\r\n\r\nOK\r\n"
    } else if command == "AT+CCSS?" {
        "\r\n+CCSS: 0\r\n\r\nOK\r\n"
    } else if command == "AT+CGACT?" {
        "\r\n+CGACT: 1,1\r\n\r\nOK\r\n"
    } else {
        "\r\nOK\r\n"
    };
    response.as_bytes().to_vec()
}

#[cfg(windows)]
async fn serve_guest_channel(component: &str, endpoint: DeviceSerialEndpointV2) -> Result<()> {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;

        loop {
            let mut output =
                open_pipe_with_retry(|| ClientOptions::new().open(&endpoint.guest_output))
                    .await
                    .with_context(|| format!("open {component} Guest output"))?;
            let mut input =
                open_pipe_with_retry(|| ClientOptions::new().open(&endpoint.guest_input))
                    .await
                    .with_context(|| format!("open {component} Guest input"))?;
            let mut buffer = vec![0_u8; MAX_GUEST_MESSAGE_BYTES];
            let mut pending = Vec::new();
            let mut uwb_session = UwbSession::default();
            loop {
                let count = match output.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if is_pipe_disconnect(&error) => break,
                    Err(error) => return Err(error.into()),
                };
                let response = if component == "uwb-adapter" {
                    pending.extend_from_slice(&buffer[..count]);
                    ensure!(
                        pending.len() <= MAX_GUEST_MESSAGE_BYTES,
                        "UWB Guest request exceeds the formal limit"
                    );
                    let mut response = Vec::new();
                    while let Some(frame) = take_uci_frame(&mut pending)? {
                        response.extend(uwb_response(&mut uwb_session, &frame)?);
                    }
                    response
                } else {
                    guest_response(component, &buffer[..count])
                };
                if response.is_empty() {
                    continue;
                }
                if let Err(error) = input.write_all(&response).await {
                    if is_pipe_disconnect(&error) {
                        break;
                    }
                    return Err(error.into());
                }
                if let Err(error) = input.flush().await {
                    if is_pipe_disconnect(&error) {
                        break;
                    }
                    return Err(error.into());
                }
            }
            tracing::warn!(component, "Guest device pipe disconnected; reconnecting");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (component, endpoint);
        bail!("peripheral Guest channels currently require Windows")
    }
}

#[cfg(windows)]
fn is_pipe_disconnect(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
    ) || matches!(error.raw_os_error(), Some(109 | 232 | 233))
}

#[cfg(windows)]
fn take_uci_frame(pending: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    if pending.len() < 4 {
        return Ok(None);
    }
    let message_type = (pending[0] >> 5) & 0x07;
    let payload_len = if message_type == 0 {
        usize::from(u16::from_le_bytes([pending[2], pending[3]]))
    } else {
        usize::from(pending[3])
    };
    let frame_len = 4_usize
        .checked_add(payload_len)
        .context("UCI frame length overflow")?;
    ensure!(
        frame_len <= MAX_GUEST_MESSAGE_BYTES,
        "UCI frame exceeds the formal limit"
    );
    if pending.len() < frame_len {
        return Ok(None);
    }
    Ok(Some(pending.drain(..frame_len).collect()))
}

#[cfg(any(windows, test))]
#[derive(Debug)]
struct UwbSession {
    token: [u8; 4],
    state: u8,
    sequence_number: u32,
    ranging_count: u32,
}

#[cfg(any(windows, test))]
impl Default for UwbSession {
    fn default() -> Self {
        Self {
            token: [0; 4],
            state: 0x01, // SESSION_STATE_DEINIT
            sequence_number: 0,
            ranging_count: 0,
        }
    }
}

#[cfg(any(windows, test))]
fn uci_control_packet(
    message_type: u8,
    group_id: u8,
    opcode: u8,
    payload: &[u8],
) -> Result<Vec<u8>> {
    ensure!(payload.len() <= u8::MAX.into(), "UCI packet is too large");
    let mut packet = Vec::with_capacity(4 + payload.len());
    packet.push((message_type << 5) | group_id);
    packet.push(opcode);
    packet.push(0x00);
    packet.push(u8::try_from(payload.len()).context("convert UCI payload length")?);
    packet.extend_from_slice(payload);
    Ok(packet)
}

#[cfg(any(windows, test))]
fn uwb_session_status_notification(session: &UwbSession) -> Result<Vec<u8>> {
    let mut payload = session.token.to_vec();
    payload.extend_from_slice(&[session.state, 0x00]);
    uci_control_packet(0x03, 0x01, 0x02, &payload)
}

#[cfg(any(windows, test))]
fn uwb_short_range_notification(session: &mut UwbSession) -> Result<Vec<u8>> {
    session.sequence_number = session.sequence_number.wrapping_add(1);
    session.ranging_count = session.ranging_count.wrapping_add(1);

    let mut payload = Vec::with_capacity(56);
    payload.extend_from_slice(&session.sequence_number.to_le_bytes());
    payload.extend_from_slice(&session.token);
    payload.push(0x00); // range-control-role indicator
    payload.extend_from_slice(&1000_u32.to_le_bytes());
    payload.extend_from_slice(&[0x01, 0x00, 0x00]); // two-way, reserved, short MAC
    payload.extend_from_slice(&0_u32.to_le_bytes()); // HUS primary session
    payload.extend_from_slice(&0_u32.to_le_bytes());
    payload.push(0x01); // one deterministic measurement
    payload.extend_from_slice(&[0x03, 0x04]); // peer short address
    payload.extend_from_slice(&[0x00, 0x00]); // status OK, line of sight
    payload.extend_from_slice(&250_u16.to_le_bytes()); // 250 cm
    for _ in 0..4 {
        payload.extend_from_slice(&0_u16.to_le_bytes());
        payload.push(0x00); // angle and figure of merit
    }
    payload.extend_from_slice(&[0x00, 100]); // slot zero, UCI half-dBm magnitude => -50 dBm
    payload.extend_from_slice(&[0x00; 11]);
    uci_control_packet(0x03, 0x02, 0x00, &payload)
}

#[cfg(any(windows, test))]
fn uwb_capabilities_payload() -> Result<Vec<u8>> {
    // Android 15's GenericDecoder v2 power-stats prefix plus the exact FiRa v2 capability
    // profile exercised by AOSP FiraDecoderTest. Do not advertise CCC, radar, or vendor ranging
    // features that this deterministic adapter does not provide.
    const FIRA_V2_CAPS: &str = concat!(
        "c00101",
        "000120",
        "010110",
        "020401010102",
        "030401050103",
        "040101",
        "05020301",
        "0602ff00",
        "070103",
        "080103",
        "090100",
        "0a0100",
        "0b0100",
        "0c0101",
        "0d0101",
        "0e0109",
        "0f010b",
        "100103",
        "110101",
        "12050300000000",
        "13010f",
        "140101",
        "150100",
        "160101",
        "180110",
        "190101",
        "1a0100",
        "e30101",
        "e40401010101",
        "e50403000000",
        "e601ff",
        "e70101",
        "e80401010101",
        "e90401000000"
    );
    let mut payload = vec![0x00, 34];
    payload.extend(hex::decode(FIRA_V2_CAPS).context("decode FiRa v2 capabilities")?);
    Ok(payload)
}

#[cfg(any(windows, test))]
fn uwb_response(session: &mut UwbSession, request: &[u8]) -> Result<Vec<u8>> {
    ensure!(request.len() >= 4, "UCI request is shorter than its header");
    let message_type = (request[0] >> 5) & 0x07;
    ensure!(message_type == 1, "UCI Guest packet is not a command");
    let group_id = request[0] & 0x0f;
    let opcode = request[1] & 0x3f;

    let command_payload = &request[4..];
    let mut notifications = Vec::new();
    let payload = match (group_id, opcode) {
        // CORE_GET_DEVICE_INFO_RSP: UCI 2.0 with no vendor-specific bytes.
        (0x00, 0x02) => {
            let mut payload = vec![0x00];
            payload.extend_from_slice(&0x0200_u16.to_le_bytes());
            payload.extend_from_slice(&0x0100_u16.to_le_bytes());
            payload.extend_from_slice(&0x0100_u16.to_le_bytes());
            payload.extend_from_slice(&0x0100_u16.to_le_bytes());
            payload.push(0x00);
            payload
        }
        (0x00, 0x03) => uwb_capabilities_payload()?,
        // Core config and deterministic session app-config reads have no failed items/TLVs.
        (0x00, 0x04 | 0x05) | (0x01, 0x04) => vec![0x00, 0x00],
        // SESSION_INIT_RSP v2 returns the UWBS-generated handle. Reusing the caller's
        // session ID keeps this deterministic while preserving the UCI v2 contract.
        (0x01, 0x00) => {
            ensure!(
                command_payload.len() >= 5,
                "SESSION_INIT payload is truncated"
            );
            session.token.copy_from_slice(&command_payload[..4]);
            session.state = 0x00;
            session.sequence_number = 0;
            session.ranging_count = 0;
            notifications.extend(uwb_session_status_notification(session)?);
            let mut payload = vec![0x00];
            payload.extend_from_slice(&session.token);
            payload
        }
        (0x01, 0x01) => {
            session.state = 0x01;
            notifications.extend(uwb_session_status_notification(session)?);
            vec![0x00]
        }
        // Accept the complete FiRa configuration. Android waits for IDLE before RANGE_START.
        (0x01, 0x03) => {
            ensure!(
                command_payload.len() >= 5,
                "SESSION_SET_APP_CONFIG payload is truncated"
            );
            session.state = 0x03;
            notifications.extend(uwb_session_status_notification(session)?);
            vec![0x00, 0x00]
        }
        (0x01, 0x05) => vec![0x00, u8::from(session.state != 0x01)],
        (0x01, 0x06) => vec![0x00, session.state],
        (0x02, 0x00) => {
            session.state = 0x02;
            notifications.extend(uwb_session_status_notification(session)?);
            notifications.extend(uwb_short_range_notification(session)?);
            vec![0x00]
        }
        (0x02, 0x01) => {
            session.state = 0x03;
            notifications.extend(uwb_session_status_notification(session)?);
            vec![0x00]
        }
        (0x02, 0x03) => {
            let mut payload = vec![0x00];
            payload.extend_from_slice(&session.ranging_count.to_le_bytes());
            payload
        }
        // Android vendor SET_COUNTRY_CODE_RSP.
        (0x0c, 0x01) => vec![0x00],
        // Android vendor GET_POWER_STATS_RSP.
        (0x0c, 0x00) => vec![0x00; 17],
        _ => bail!("unsupported UCI command group {group_id:#04x} opcode {opcode:#04x}"),
    };
    let mut response = uci_control_packet(0x02, group_id, opcode, &payload)?;
    response.extend(notifications);
    Ok(response)
}

#[cfg(windows)]
async fn open_pipe_with_retry<F>(
    mut open: F,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient>
where
    F: FnMut() -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient>,
{
    let started = tokio::time::Instant::now();
    loop {
        match open() {
            Ok(pipe) => return Ok(pipe),
            Err(error) if started.elapsed() < PIPE_OPEN_TIMEOUT => {
                let _ = error;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(windows)]
fn guest_response(component: &str, request: &[u8]) -> Vec<u8> {
    debug_assert_ne!(component, "modem-adapter");
    let request_id = hex::encode(Sha256::digest(request));
    format!(
        "{{\"protocol_version\":2,\"component\":\"{component}\",\"request_sha256\":\"{request_id}\",\"status\":\"ready\"}}\n"
    )
    .into_bytes()
}

#[cfg(windows)]
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

#[cfg(windows)]
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
                first = false;
            }
            server.connect().await?;
            handle_control_connection(Arc::clone(&context), server).await?;
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (context, launch_bytes);
        bail!("peripheral adapter control currently requires Windows")
    }
}

#[cfg(windows)]
async fn handle_control_connection(
    context: Arc<FormalContext>,
    stream: tokio::net::windows::named_pipe::NamedPipeServer,
) -> Result<()> {
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = Vec::new();
    let count = tokio::io::AsyncBufReadExt::read_until(&mut reader, b'\n', &mut line).await?;
    ensure!(
        count != 0 && line.len() <= MAX_CONTROL_MESSAGE_BYTES,
        "peripheral control message is empty or too large"
    );
    let request: DeviceControlRequestV2 = serde_json::from_slice(&line)?;
    let mut response = DeviceControlResponseV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        request_id: request.request_id,
        instance_id: context.launch.instance_id,
        run_id: context.launch.run_id,
        ok: false,
        error: None,
    };
    let identity_valid = request.protocol_version == COMPONENT_PROTOCOL_VERSION
        && request.instance_id == context.launch.instance_id
        && request.run_id == context.launch.run_id;
    let bearer_valid: bool = request
        .bearer_token
        .as_bytes()
        .ct_eq(context.control_token.as_bytes())
        .into();
    if !identity_valid {
        response.error = Some(ApiErrorV2::new(
            "device_control_identity",
            "peripheral control request identity mismatch",
        ));
    } else if !bearer_valid {
        response.error = Some(ApiErrorV2::new(
            "device_control_auth",
            "peripheral control bearer was rejected",
        ));
    } else if matches!(request.command, DeviceControlCommandV2::Ping) {
        response.ok = true;
    } else {
        response.error = Some(ApiErrorV2::new(
            "device_action_unsupported",
            "this peripheral has no runtime action in the V2 public contract",
        ));
    }
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    reader.get_mut().write_all(&bytes).await?;
    reader.get_mut().flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modem_waits_for_a_complete_at_command() {
        let mut pending = b"AT+CLCK=\"FD\",2,\"\",7".to_vec();
        assert!(take_modem_responses(&mut pending).is_empty());
        pending.push(b'\r');
        let response = String::from_utf8(take_modem_responses(&mut pending)).unwrap();
        assert!(response.contains("+CLCK: 0"));
        assert!(pending.is_empty());
    }

    #[test]
    fn modem_reports_lte_registration_and_full_signal_profile() {
        let registration = String::from_utf8(modem_response(b"AT+CREG?")).unwrap();
        assert!(registration.contains("\"00000001\",7"));
        let signal = String::from_utf8(modem_response(b"AT+CSQ")).unwrap();
        assert_eq!(signal.matches(',').count(), 12);
    }

    #[test]
    fn uwb_advertises_android_15_fira_v2_capabilities() {
        let response = uwb_response(&mut UwbSession::default(), &[0x20, 0x03, 0x00, 0x00]).unwrap();
        assert_eq!(&response[..4], &[0x40, 0x03, 0x00, response[3]]);
        assert_eq!(usize::from(response[3]), response.len() - 4);
        assert_eq!(response[4], 0x00);
        assert_eq!(response[5], 34);
        assert_eq!(&response[6..9], &[0xc0, 0x01, 0x01]);
    }

    #[test]
    fn uwb_runs_a_deterministic_fira_session() {
        let mut session = UwbSession::default();
        let init = uwb_response(
            &mut session,
            &[0x21, 0x00, 0x00, 0x05, 0x07, 0x00, 0x00, 0x00, 0x00],
        )
        .unwrap();
        assert_eq!(&init[..9], &[0x41, 0x00, 0x00, 0x05, 0x00, 0x07, 0, 0, 0]);
        assert!(
            init.windows(4)
                .any(|header| header == [0x61, 0x02, 0x00, 0x06])
        );

        let configured = uwb_response(
            &mut session,
            &[0x21, 0x03, 0x00, 0x05, 0x07, 0x00, 0x00, 0x00, 0x00],
        )
        .unwrap();
        assert_eq!(session.state, 0x03);
        assert_eq!(&configured[..6], &[0x41, 0x03, 0x00, 0x02, 0x00, 0x00]);

        let started = uwb_response(
            &mut session,
            &[0x22, 0x00, 0x00, 0x04, 0x07, 0x00, 0x00, 0x00],
        )
        .unwrap();
        assert_eq!(session.state, 0x02);
        assert_eq!(session.ranging_count, 1);
        assert!(
            started
                .windows(4)
                .any(|header| header == [0x62, 0x00, 0x00, 56])
        );
        assert!(
            started
                .windows(2)
                .any(|distance| distance == 250_u16.to_le_bytes())
        );
    }
}
