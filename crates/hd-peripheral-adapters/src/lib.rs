#![allow(unsafe_code)]
#![cfg_attr(not(any(windows, target_os = "macos")), allow(clippy::unused_async))]

use std::path::{Path, PathBuf};
#[cfg(any(windows, target_os = "macos"))]
use std::sync::Arc;
#[cfg(any(windows, target_os = "macos"))]
use std::time::Duration;

#[cfg(any(windows, target_os = "macos", test))]
use anyhow::ensure;
use anyhow::{Context as _, Result, bail};
use clap::Parser;
#[cfg(any(windows, target_os = "macos", test))]
use hd_core::ModemStateV2;
#[cfg(any(windows, target_os = "macos"))]
use hd_core::{
    ApiErrorV2, DeviceControlCommandV2, DeviceControlRequestV2, DeviceControlResponseV2,
    DeviceControlTokenV2, DeviceSerialEndpointV2, FormalComponentConfigurationV2,
    FormalComponentLaunchV2, FormalComponentReadyV2,
};
use hd_core::{COMPONENT_PROTOCOL_VERSION, FormalComponentProbeV2};
#[cfg(any(windows, target_os = "macos"))]
use sha2::{Digest as _, Sha256};
#[cfg(any(windows, target_os = "macos"))]
use subtle::ConstantTimeEq as _;
#[cfg(any(windows, target_os = "macos"))]
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

#[cfg(any(windows, target_os = "macos"))]
const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;
#[cfg(any(windows, target_os = "macos"))]
const MAX_GUEST_MESSAGE_BYTES: usize = 64 * 1024;
#[cfg(windows)]
const PIPE_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(any(windows, target_os = "macos"))]
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
    guest_role: &'static str,
    features: &'static [&'static str],
}

#[cfg(any(windows, target_os = "macos"))]
#[derive(Debug)]
struct FormalContext {
    launch: FormalComponentLaunchV2,
    control_endpoint: String,
    control_token: DeviceControlTokenV2,
    uwb_distance_cm: tokio::sync::watch::Sender<u16>,
    modem_state: tokio::sync::watch::Sender<ModemStateV2>,
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
            guest_role: "uwb",
            features: &[
                "guest-serial-v2",
                "uwb-control-v2",
                "deterministic-ranging-v2",
                "runtime-ranging-control-v2",
            ],
        },
        "modem-adapter" => AdapterProfile {
            component: "modem-adapter",
            guest_role: "modem",
            features: &[
                "guest-vsock-v2",
                "modem-control-v2",
                "at-command-baseline-v2",
                "runtime-modem-state-v2",
                "runtime-modem-unsolicited-v2",
            ],
        },
        "network-adapter" => AdapterProfile {
            component: "network-adapter",
            guest_role: "network-control",
            features: &["guest-serial-v2", "network-control-v2", "virtio-net-v2"],
        },
        "audio-adapter" => AdapterProfile {
            component: "audio-adapter",
            guest_role: "audio-control",
            features: &["guest-serial-v2", "audio-control-v2", "virtio-snd-v2"],
        },
        "camera-adapter" => AdapterProfile {
            component: "camera-adapter",
            guest_role: "camera-control",
            features: &["guest-serial-v2", "camera-control-v2", "guest-camera-v2"],
        },
        other => bail!("unsupported peripheral adapter component {other}"),
    };
    Ok(profile)
}

fn probe(profile: &AdapterProfile) -> FormalComponentProbeV2 {
    // Only UWB and modem currently own a real Guest data plane. The network, audio and camera
    // binaries are reserved profiles; advertising them as formal would turn a control-only stub
    // into a false product capability on Windows.
    let formal = matches!(profile.component, "uwb-adapter" | "modem-adapter")
        && (cfg!(windows) || cfg!(target_os = "macos"));
    let mut features = profile
        .features
        .iter()
        .map(|feature| (*feature).to_owned())
        .collect::<Vec<_>>();
    features.extend(["ready-marker-v2".to_owned(), "lifecycle-v2".to_owned()]);
    if formal && cfg!(windows) {
        features.push("windows-owner-pipe-v2".to_owned());
    } else if formal && cfg!(target_os = "macos") {
        features.push("unix-owner-socket-v2".to_owned());
        if profile.component == "uwb-adapter" {
            features.push("unix-file-fifo-serial-v2".to_owned());
        }
    }
    FormalComponentProbeV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        component: profile.component.to_owned(),
        formal,
        features,
    }
}

async fn serve_formal(profile: &AdapterProfile, launch_path: &Path) -> Result<()> {
    if !matches!(profile.component, "uwb-adapter" | "modem-adapter") {
        bail!(
            "{} has no formal Guest data plane; only UWB and modem adapters may be launched",
            profile.component
        );
    }
    #[cfg(windows)]
    {
        serve_formal_component(profile, launch_path).await
    }
    #[cfg(target_os = "macos")]
    {
        serve_formal_component(profile, launch_path).await
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (profile, launch_path);
        bail!("peripheral adapters currently require Windows")
    }
}

#[cfg(any(windows, target_os = "macos"))]
async fn serve_formal_component(profile: &AdapterProfile, launch_path: &Path) -> Result<()> {
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
    let (uwb_distance_cm, uwb_distance_updates) = tokio::sync::watch::channel(250_u16);
    let (modem_state, modem_state_updates) = tokio::sync::watch::channel(ModemStateV2::default());
    let context = Arc::new(FormalContext {
        launch,
        control_endpoint,
        control_token,
        uwb_distance_cm,
        modem_state,
    });
    let guest = if profile.component == "modem-adapter" {
        tokio::spawn(serve_modem_vsock(guest_cid, modem_state_updates))
    } else {
        let guest_endpoint = guest_endpoint.with_context(|| {
            format!(
                "{} requires the {} Guest endpoint",
                profile.component, profile.guest_role
            )
        })?;
        tokio::spawn(serve_guest_channel(
            profile.component,
            guest_endpoint,
            uwb_distance_updates,
        ))
    };
    let control = run_control_server(Arc::clone(&context), &launch_bytes);
    tokio::pin!(control);
    #[cfg(windows)]
    tokio::select! {
        result = &mut control => result,
        result = guest => match result {
            Ok(Ok(())) => bail!("{} Guest channel exited unexpectedly", profile.component),
            Ok(Err(error)) => Err(error).with_context(|| format!("{} Guest channel failed", profile.component)),
            Err(error) => Err(error).with_context(|| format!("{} Guest task failed", profile.component)),
        },
    }
    #[cfg(target_os = "macos")]
    {
        let mut guest = guest;
        tokio::select! {
            result = &mut control => result,
            result = &mut guest => match result {
                Ok(Ok(())) => bail!("{} Guest channel exited unexpectedly", profile.component),
                Ok(Err(error)) => Err(error).with_context(|| format!("{} Guest channel failed", profile.component)),
                Err(error) => Err(error).with_context(|| format!("{} Guest task failed", profile.component)),
            },
            result = wait_for_termination_signal() => {
                result?;
                guest.abort();
                let _ = guest.await;
                Ok(())
            },
        }
    }
}

#[cfg(target_os = "macos")]
async fn wait_for_termination_signal() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install peripheral adapter SIGTERM handler")?;
    terminate
        .recv()
        .await
        .context("peripheral adapter SIGTERM stream closed")?;
    Ok(())
}

#[cfg(windows)]
async fn serve_modem_vsock(
    guest_cid: u32,
    modem_state: tokio::sync::watch::Receiver<ModemStateV2>,
) -> Result<()> {
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
            if let Err(error) = serve_modem_vsock_connection(&mut pipe, modem_state.clone()).await {
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
        let _ = (guest_cid, modem_state);
        bail!("modem Guest vsock is currently implemented for Windows only")
    }
}

#[cfg(target_os = "macos")]
async fn serve_modem_vsock(
    guest_cid: u32,
    modem_state: tokio::sync::watch::Receiver<ModemStateV2>,
) -> Result<()> {
    let endpoint = PathBuf::from(format!(
        "/tmp/binder_rpc_vsock_{guest_cid}_{MODEM_VSOCK_PORT}.sock"
    ));
    let (listener, _guard) = bind_owner_only_unix_listener(&endpoint).await?;
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .context("accept Guest modem vsock connection")?;
        let state = modem_state.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_modem_vsock_connection_unix(&mut stream, state).await {
                tracing::warn!(%error, "Guest modem vsock connection failed");
            }
        });
    }
}

#[cfg(target_os = "macos")]
async fn serve_modem_vsock_connection_unix<T>(
    stream: &mut T,
    mut modem_state: tokio::sync::watch::Receiver<ModemStateV2>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut pending = Vec::new();
    let mut buffer = vec![0_u8; MAX_GUEST_MESSAGE_BYTES];
    let mut last_state = modem_state.borrow().clone();
    loop {
        let count = tokio::select! {
            result = reader.read(&mut buffer) => result?,
            changed = modem_state.changed() => {
                changed.context("macOS modem runtime control channel closed")?;
                let next_state = modem_state.borrow_and_update().clone();
                let frames = modem_unsolicited_frames(&last_state, &next_state);
                for (index, frame) in frames.iter().enumerate() {
                    writer.write_all(frame).await?;
                    writer.flush().await?;
                    if index + 1 < frames.len() {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                tracing::info!(
                    event = "modem.guest_state.unsolicited",
                    operator_numeric = %next_state.operator_numeric,
                    signal_strength = next_state.signal_strength,
                    registered = next_state.registered,
                    "published runtime modem state to the macOS Guest"
                );
                last_state = next_state;
                continue;
            }
        };
        if count == 0 {
            return Ok(());
        }
        pending.extend_from_slice(&buffer[..count]);
        ensure!(
            pending.len() <= MAX_GUEST_MESSAGE_BYTES,
            "modem AT command exceeds the formal limit"
        );
        let response = take_modem_responses(&mut pending, &modem_state.borrow());
        if response.is_empty() {
            continue;
        }
        writer.write_all(&response).await?;
        writer.flush().await?;
    }
}

#[cfg(windows)]
async fn serve_modem_vsock_connection<T>(
    pipe: &mut T,
    mut modem_state: tokio::sync::watch::Receiver<ModemStateV2>,
) -> Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let mut pending = Vec::new();
    let mut last_state = modem_state.borrow().clone();
    loop {
        let mut header = [0_u8; VSOCK_PIPE_HEADER_BYTES];
        tokio::select! {
            result = reader.read_exact(&mut header) => { result?; }
            changed = modem_state.changed() => {
                changed.context("Windows modem runtime control channel closed")?;
                let next_state = modem_state.borrow_and_update().clone();
                let frames = modem_unsolicited_frames(&last_state, &next_state);
                for (index, frame) in frames.iter().enumerate() {
                    write_modem_vsock_frame(&mut writer, frame).await?;
                    if index + 1 < frames.len() {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                tracing::info!(
                    event = "modem.guest_state.unsolicited",
                    operator_numeric = %next_state.operator_numeric,
                    signal_strength = next_state.signal_strength,
                    registered = next_state.registered,
                    "published runtime modem state to the Windows Guest"
                );
                last_state = next_state;
                continue;
            }
        }
        let payload_size = u32::from_le_bytes(header[..4].try_into()?) as usize;
        let handle_count = u32::from_le_bytes(header[4..].try_into()?);
        ensure!(
            payload_size <= MAX_GUEST_MESSAGE_BYTES,
            "modem vsock payload exceeds the formal limit"
        );
        ensure!(handle_count == 0, "modem vsock frame contains handles");
        let mut payload = vec![0_u8; payload_size];
        reader.read_exact(&mut payload).await?;
        pending.extend_from_slice(&payload);
        ensure!(
            pending.len() <= MAX_GUEST_MESSAGE_BYTES,
            "modem AT command exceeds the formal limit"
        );
        let response = take_modem_responses(&mut pending, &modem_state.borrow());
        if response.is_empty() {
            continue;
        }
        write_modem_vsock_frame(&mut writer, &response).await?;
    }
}

#[cfg(windows)]
async fn write_modem_vsock_frame<W>(writer: &mut W, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload_size = u32::try_from(payload.len())?;
    writer.write_all(&payload_size.to_le_bytes()).await?;
    writer.write_all(&0_u32.to_le_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(any(windows, target_os = "macos", test))]
fn modem_unsolicited_frames(previous: &ModemStateV2, next: &ModemStateV2) -> Vec<Vec<u8>> {
    let operator_changed = previous.operator_numeric != next.operator_numeric
        || previous.operator_long_name != next.operator_long_name
        || previous.operator_short_name != next.operator_short_name;
    let mut frames = Vec::with_capacity(2);
    if operator_changed && previous.registered && next.registered {
        frames.push(modem_registration_unsolicited(false));
    }
    if operator_changed || previous.registered != next.registered {
        frames.push(modem_registration_unsolicited(next.registered));
    }
    if operator_changed || previous.signal_strength != next.signal_strength {
        frames.push(modem_signal_unsolicited(next.signal_strength));
    }
    if frames.is_empty() {
        frames.push(modem_registration_unsolicited(next.registered));
    }
    frames
}

#[cfg(any(windows, target_os = "macos", test))]
fn modem_registration_unsolicited(registered: bool) -> Vec<u8> {
    let state = u8::from(registered);
    let detail = if registered {
        ",\"0001\",\"00000001\",7"
    } else {
        ""
    };
    format!("\r\n+CREG: {state}{detail}\r\n+CGREG: {state}{detail}\r\n+CEREG: {state}{detail}\r\n")
        .into_bytes()
}

#[cfg(any(windows, target_os = "macos", test))]
fn modem_signal_unsolicited(signal_strength: u8) -> Vec<u8> {
    format!("\r\n+CSQ: {signal_strength},2,80,100,70,100,6,70,90,15,100,12,100\r\n").into_bytes()
}

#[cfg(any(windows, target_os = "macos", test))]
fn take_modem_responses(pending: &mut Vec<u8>, modem: &ModemStateV2) -> Vec<u8> {
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
        .flat_map(|command| modem_response(command, modem))
        .collect()
}

#[cfg(any(windows, target_os = "macos", test))]
fn modem_response(command: &[u8], modem: &ModemStateV2) -> Vec<u8> {
    let command = String::from_utf8_lossy(command).trim().to_ascii_uppercase();
    let response = if command.contains("AT+COPS=3,0;") && command.matches("+COPS?").count() == 3 {
        format!(
            "\r\n+COPS: 0,0,\"{}\"\r\n+COPS: 0,1,\"{}\"\r\n+COPS: 0,2,\"{}\",7\r\n\r\nOK\r\n",
            modem.operator_long_name, modem.operator_short_name, modem.operator_numeric
        )
    } else if command == "AT+CSQ" {
        format!(
            "\r\n+CSQ: {},2,80,100,70,100,6,70,90,15,100,12,100\r\n\r\nOK\r\n",
            modem.signal_strength
        )
    } else if command == "AT+COPS?" {
        format!(
            "\r\n+COPS: 0,2,\"{}\",7\r\n\r\nOK\r\n",
            modem.operator_numeric
        )
    } else if command == "AT+CREG?" {
        format!(
            "\r\n+CREG: 2,{},\"0001\",\"00000001\",7\r\n\r\nOK\r\n",
            u8::from(modem.registered)
        )
    } else if command == "AT+CGREG?" {
        format!(
            "\r\n+CGREG: 2,{},\"0001\",\"00000001\",7\r\n\r\nOK\r\n",
            u8::from(modem.registered)
        )
    } else if command == "AT+CEREG?" {
        format!(
            "\r\n+CEREG: 2,{},\"0001\",\"00000001\",7\r\n\r\nOK\r\n",
            u8::from(modem.registered)
        )
    } else if command == "AT+CPIN?" {
        "\r\n+CPIN: READY\r\n\r\nOK\r\n".to_owned()
    } else if command == "AT+CFUN?" {
        "\r\n+CFUN: 1\r\n\r\nOK\r\n".to_owned()
    } else if command == "AT+CTEC?" {
        "\r\n+CTEC: 5,20\r\n\r\nOK\r\n".to_owned()
    } else if command == "AT+CTEC=?" {
        "\r\n+CTEC: 0,1,2,3,4,5\r\n\r\nOK\r\n".to_owned()
    } else if command == "AT+CIMI" {
        "\r\n001010123456789\r\n\r\nOK\r\n".to_owned()
    } else if command == "AT+CGSN" {
        "\r\n867400022047199\r\n\r\nOK\r\n".to_owned()
    } else if command == "AT+CICCID" {
        "\r\n89014103211118510720\r\n\r\nOK\r\n".to_owned()
    } else if command.starts_with("AT+CLCK=") && command.contains(",2,") {
        "\r\n+CLCK: 0\r\n\r\nOK\r\n".to_owned()
    } else if command == "AT+CCSS?" {
        "\r\n+CCSS: 0\r\n\r\nOK\r\n".to_owned()
    } else if command == "AT+CGACT?" {
        "\r\n+CGACT: 1,1\r\n\r\nOK\r\n".to_owned()
    } else {
        "\r\nOK\r\n".to_owned()
    };
    response.into_bytes()
}

#[cfg(windows)]
async fn serve_guest_channel(
    component: &str,
    endpoint: DeviceSerialEndpointV2,
    mut uwb_distance_updates: tokio::sync::watch::Receiver<u16>,
) -> Result<()> {
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
                enum GuestEvent {
                    Output(usize),
                    UwbDistance(u16),
                }
                let event = tokio::select! {
                    result = output.read(&mut buffer) => match result {
                        Ok(0) => break,
                        Ok(count) => GuestEvent::Output(count),
                        Err(error) if is_pipe_disconnect(&error) => break,
                        Err(error) => return Err(error.into()),
                    },
                    changed = uwb_distance_updates.changed(), if component == "uwb-adapter" => {
                        changed.context("UWB runtime control channel closed")?;
                        GuestEvent::UwbDistance(*uwb_distance_updates.borrow_and_update())
                    }
                };
                let count = match event {
                    GuestEvent::Output(count) => count,
                    GuestEvent::UwbDistance(distance_cm) => {
                        if uwb_session.state != 0x02 {
                            continue;
                        }
                        let notification =
                            uwb_short_range_notification(&mut uwb_session, distance_cm)?;
                        if let Err(error) = input.write_all(&notification).await {
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
                        continue;
                    }
                };
                let response = if component == "uwb-adapter" {
                    pending.extend_from_slice(&buffer[..count]);
                    ensure!(
                        pending.len() <= MAX_GUEST_MESSAGE_BYTES,
                        "UWB Guest request exceeds the formal limit"
                    );
                    let mut response = Vec::new();
                    while let Some(frame) = take_uci_frame(&mut pending)? {
                        let distance_cm = *uwb_distance_updates.borrow();
                        response.extend(uwb_response(&mut uwb_session, &frame, distance_cm)?);
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

#[cfg(target_os = "macos")]
async fn serve_guest_channel(
    component: &str,
    endpoint: DeviceSerialEndpointV2,
    mut uwb_distance_updates: tokio::sync::watch::Receiver<u16>,
) -> Result<()> {
    ensure!(
        component == "uwb-adapter",
        "only the UWB Guest serial bridge is supported on macOS"
    );
    let mut output = tokio::fs::OpenOptions::new()
        .read(true)
        .open(&endpoint.guest_output)
        .await
        .with_context(|| format!("open {component} Guest output"))?;
    let mut input = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&endpoint.guest_input)
        .await
        .with_context(|| format!("open {component} Guest input"))?;
    let mut buffer = vec![0_u8; MAX_GUEST_MESSAGE_BYTES];
    let mut pending = Vec::new();
    let mut uwb_session = UwbSession::default();
    loop {
        enum GuestEvent {
            Output(usize),
            UwbDistance(u16),
        }
        let event = tokio::select! {
            result = output.read(&mut buffer) => {
                GuestEvent::Output(result.context("read macOS UWB Guest output")?)
            }
            changed = uwb_distance_updates.changed() => {
                changed.context("UWB runtime control channel closed")?;
                GuestEvent::UwbDistance(*uwb_distance_updates.borrow_and_update())
            }
        };
        let count = match event {
            GuestEvent::Output(count) => count,
            GuestEvent::UwbDistance(distance_cm) => {
                if uwb_session.state == 0x02 {
                    let notification = uwb_short_range_notification(&mut uwb_session, distance_cm)?;
                    input
                        .write_all(&notification)
                        .await
                        .context("write macOS UWB runtime ranging notification")?;
                    input
                        .flush()
                        .await
                        .context("flush macOS UWB runtime ranging notification")?;
                }
                continue;
            }
        };
        if count == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        }
        pending.extend_from_slice(&buffer[..count]);
        ensure!(
            pending.len() <= MAX_GUEST_MESSAGE_BYTES,
            "UWB Guest request exceeds the formal limit"
        );
        while let Some(frame) = take_uci_frame(&mut pending)? {
            let distance_cm = *uwb_distance_updates.borrow();
            let response = uwb_response(&mut uwb_session, &frame, distance_cm)?;
            input
                .write_all(&response)
                .await
                .context("write macOS UWB Guest input")?;
            input.flush().await.context("flush macOS UWB Guest input")?;
        }
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

#[cfg(any(windows, target_os = "macos"))]
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

#[cfg(any(windows, target_os = "macos", test))]
#[derive(Debug)]
struct UwbSession {
    token: [u8; 4],
    state: u8,
    sequence_number: u32,
    ranging_count: u32,
}

#[cfg(any(windows, target_os = "macos", test))]
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

#[cfg(any(windows, target_os = "macos", test))]
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

#[cfg(any(windows, target_os = "macos", test))]
fn uwb_session_status_notification(session: &UwbSession) -> Result<Vec<u8>> {
    let mut payload = session.token.to_vec();
    payload.extend_from_slice(&[session.state, 0x00]);
    uci_control_packet(0x03, 0x01, 0x02, &payload)
}

#[cfg(any(windows, target_os = "macos", test))]
fn uwb_short_range_notification(session: &mut UwbSession, distance_cm: u16) -> Result<Vec<u8>> {
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
    payload.extend_from_slice(&distance_cm.to_le_bytes());
    for _ in 0..4 {
        payload.extend_from_slice(&0_u16.to_le_bytes());
        payload.push(0x00); // angle and figure of merit
    }
    payload.extend_from_slice(&[0x00, 100]); // slot zero, UCI half-dBm magnitude => -50 dBm
    payload.extend_from_slice(&[0x00; 11]);
    uci_control_packet(0x03, 0x02, 0x00, &payload)
}

#[cfg(any(windows, target_os = "macos", test))]
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

#[cfg(any(windows, target_os = "macos", test))]
fn uwb_response(session: &mut UwbSession, request: &[u8], distance_cm: u16) -> Result<Vec<u8>> {
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
            notifications.extend(uwb_short_range_notification(session, distance_cm)?);
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

#[cfg(any(windows, target_os = "macos"))]
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

#[cfg(any(windows, target_os = "macos"))]
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
    #[cfg(target_os = "macos")]
    {
        let path = PathBuf::from(&context.control_endpoint);
        let (listener, _guard) = bind_owner_only_unix_listener(&path).await?;
        publish_ready(&context, launch_bytes)?;
        loop {
            let (stream, _) = listener.accept().await?;
            let connection_context = Arc::clone(&context);
            tokio::spawn(async move {
                if let Err(error) = handle_control_connection(connection_context, stream).await {
                    tracing::warn!(%error, "peripheral control connection failed");
                }
            });
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (context, launch_bytes);
        bail!("peripheral adapter control is unsupported on this platform")
    }
}

#[cfg(target_os = "macos")]
async fn bind_owner_only_unix_listener(
    path: &Path,
) -> Result<(tokio::net::UnixListener, SocketGuard)> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};

    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        ensure!(
            metadata.file_type().is_socket(),
            "stale peripheral control endpoint is not a socket"
        );
        let current_uid = hd_platform::current_user_scope()?
            .parse::<u32>()
            .context("parse current Unix user id")?;
        ensure!(
            metadata.uid() == current_uid,
            "peripheral Unix endpoint is not owned by the current user"
        );
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => bail!("peripheral control endpoint already has an active listener"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(path)?;
            }
            Err(error) => return Err(error).context("validate stale peripheral control socket"),
        }
    }
    let listener =
        tokio::net::UnixListener::bind(path).context("bind peripheral control socket")?;
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

#[cfg(target_os = "macos")]
struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[cfg(target_os = "macos")]
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

#[cfg(any(windows, target_os = "macos"))]
async fn handle_control_connection<S>(context: Arc<FormalContext>, stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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
    } else {
        match request.command {
            DeviceControlCommandV2::Ping => response.ok = true,
            DeviceControlCommandV2::Action {
                action: hd_core::InstanceActionV2::SetUwbRanging { ranging },
            } if context.launch.component == "uwb-adapter" && ranging.distance_cm > 0 => {
                context.uwb_distance_cm.send_replace(ranging.distance_cm);
                response.ok = true;
            }
            DeviceControlCommandV2::Action {
                action: hd_core::InstanceActionV2::SetUwbRanging { .. },
            } if context.launch.component == "uwb-adapter" => {
                response.error = Some(ApiErrorV2::new(
                    "device_action_invalid",
                    "UWB ranging distance must be between 1 and 65535 centimeters",
                ));
            }
            DeviceControlCommandV2::Action {
                action: hd_core::InstanceActionV2::SetModemState { modem },
            } if context.launch.component == "modem-adapter"
                && (hd_core::InstanceActionV2::SetModemState {
                    modem: modem.clone(),
                })
                .validate()
                .is_ok() =>
            {
                context.modem_state.send_replace(modem);
                response.ok = true;
            }
            DeviceControlCommandV2::Action {
                action: hd_core::InstanceActionV2::SetModemState { .. },
            } if context.launch.component == "modem-adapter" => {
                response.error = Some(ApiErrorV2::new(
                    "device_action_invalid",
                    "modem state must use a 5/6 digit operator, safe non-empty names and signal strength 0-31",
                ));
            }
            DeviceControlCommandV2::Action { .. } => {
                response.error = Some(ApiErrorV2::new(
                    "device_action_unsupported",
                    "this peripheral has no matching runtime action in the V2 public contract",
                ));
            }
        }
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
        let modem = ModemStateV2::default();
        let mut pending = b"AT+CLCK=\"FD\",2,\"\",7".to_vec();
        assert!(take_modem_responses(&mut pending, &modem).is_empty());
        pending.push(b'\r');
        let response = String::from_utf8(take_modem_responses(&mut pending, &modem)).unwrap();
        assert!(response.contains("+CLCK: 0"));
        assert!(pending.is_empty());
    }

    #[test]
    fn modem_reports_lte_registration_and_full_signal_profile() {
        let modem = ModemStateV2::default();
        let registration = String::from_utf8(modem_response(b"AT+CREG?", &modem)).unwrap();
        assert!(registration.contains("\"00000001\",7"));
        let signal = String::from_utf8(modem_response(b"AT+CSQ", &modem)).unwrap();
        assert_eq!(signal.matches(',').count(), 12);
    }

    #[test]
    fn uwb_advertises_android_15_fira_v2_capabilities() {
        let response =
            uwb_response(&mut UwbSession::default(), &[0x20, 0x03, 0x00, 0x00], 250).unwrap();
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
            250,
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
            250,
        )
        .unwrap();
        assert_eq!(session.state, 0x03);
        assert_eq!(&configured[..6], &[0x41, 0x03, 0x00, 0x02, 0x00, 0x00]);

        let started = uwb_response(
            &mut session,
            &[0x22, 0x00, 0x00, 0x04, 0x07, 0x00, 0x00, 0x00],
            321,
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
                .any(|distance| distance == 321_u16.to_le_bytes())
        );
    }
}
