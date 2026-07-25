use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use casimir::packets::rf;
use clap::Parser;
use hd_core::{
    ApiErrorV2, COMPONENT_PROTOCOL_VERSION, DeviceControlCommandV2, DeviceControlRequestV2,
    DeviceControlResponseV2, DeviceControlTokenV2, DeviceSerialEndpointV2,
    FormalComponentConfigurationV2, FormalComponentLaunchV2, FormalComponentProbeV2,
    FormalComponentReadyV2, InstanceActionV2, NfcTagActionV2,
};
use pdl_runtime::Packet as _;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;
const PIPE_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const TAG_START_TIMEOUT: Duration = Duration::from_secs(3);
const TYPE_2_MEMORY_BYTES: usize = 1024;
const TYPE_2_MAX_NDEF_BYTES: usize = TYPE_2_MEMORY_BYTES - 24;

#[derive(Debug, Parser)]
#[command(about = "Formal HD Windows adapter for the AOSP Casimir NFC emulator")]
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
    rf_address: std::net::SocketAddr,
    tag: Mutex<Option<TagHandle>>,
}

struct TagHandle {
    task: JoinHandle<Result<()>>,
}

impl Drop for TagHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
enum TagKind {
    Type2 { memory: Vec<u8> },
    Type4 { ndef: Vec<u8> },
}

#[derive(Clone, Copy)]
enum Type4File {
    CapabilityContainer,
    Ndef,
}

#[tokio::main]
async fn main() -> Result<()> {
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
        component: "casimir-adapter".to_owned(),
        formal: true,
        features: vec![
            "guest-serial-v2".to_owned(),
            "nci-v2".to_owned(),
            "tag-control-v2".to_owned(),
        ],
    }
}

async fn serve_formal(launch_path: &Path) -> Result<()> {
    let launch_bytes = hd_platform::read_regular_nofollow_limited(launch_path, 64 * 1024)
        .context("read Casimir adapter launch")?;
    let launch: FormalComponentLaunchV2 =
        serde_json::from_slice(&launch_bytes).context("decode Casimir adapter launch")?;
    ensure!(
        launch.protocol_version == COMPONENT_PROTOCOL_VERSION
            && launch.component == "casimir-adapter",
        "formal launch protocol or component mismatch"
    );
    ensure!(
        launch.component_ready_marker.parent() == launch_path.parent()
            && launch
                .component_ready_marker
                .file_name()
                .and_then(|name| name.to_str())
                == Some("casimir-adapter-ready-v2.json"),
        "formal ready marker is outside the launch directory"
    );
    let (control_endpoint, control_token, nfc_endpoint) = match &launch.configuration {
        FormalComponentConfigurationV2::DeviceAdapter {
            control_endpoint,
            control_token,
            guest_endpoints,
            ..
        } => (
            control_endpoint.clone(),
            control_token.clone(),
            guest_endpoints
                .get("nfc")
                .cloned()
                .context("Casimir adapter requires the nfc Guest endpoint")?,
        ),
        _ => bail!("casimir-adapter requires a device_adapter configuration"),
    };

    let nci_listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .context("bind private Casimir NCI listener")?;
    let nci_address = nci_listener.local_addr()?;
    let rf_listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .context("bind private Casimir RF listener")?;
    let rf_address = rf_listener.local_addr()?;

    let engine = casimir::run_with_listeners(nci_listener, rf_listener);
    tokio::pin!(engine);
    let bridge = tokio::spawn(bridge_guest_nci(nfc_endpoint, nci_address));
    let context = Arc::new(FormalContext {
        launch,
        control_endpoint,
        control_token,
        rf_address,
        tag: Mutex::new(None),
    });

    tokio::select! {
        result = run_control_server(Arc::clone(&context), &launch_bytes) => result,
        result = &mut engine => match result {
            Ok(()) => bail!("Casimir engine exited unexpectedly"),
            Err(error) => Err(error).context("Casimir engine failed"),
        },
        result = bridge => match result {
            Ok(Ok(())) => bail!("Casimir Guest NCI bridge exited unexpectedly"),
            Ok(Err(error)) => Err(error).context("Casimir Guest NCI bridge failed"),
            Err(error) => Err(error).context("Casimir Guest NCI bridge task failed"),
        },
    }
}

async fn bridge_guest_nci(
    endpoint: DeviceSerialEndpointV2,
    nci_address: std::net::SocketAddr,
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
                "Guest NFC output pipe did not appear",
            )
            .await?;
            let mut guest_input = open_pipe_with_retry(
                || {
                    ClientOptions::new()
                        .read(false)
                        .write(true)
                        .open(&endpoint.guest_input)
                },
                "Guest NFC input pipe did not appear",
            )
            .await?;
            let nci = TcpStream::connect(nci_address)
                .await
                .context("connect private Casimir NCI listener")?;
            let (mut nci_read, mut nci_write) = nci.into_split();
            let guest_to_casimir = tokio::io::copy(&mut guest_output, &mut nci_write);
            let casimir_to_guest = tokio::io::copy(&mut nci_read, &mut guest_input);
            tokio::pin!(guest_to_casimir, casimir_to_guest);
            let (direction, result) = tokio::select! {
                result = &mut guest_to_casimir => ("guest_to_casimir", result),
                result = &mut casimir_to_guest => ("casimir_to_guest", result),
            };
            tracing::warn!(
                direction,
                ?result,
                "Guest NFC pipe disconnected; reconnecting"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (endpoint, nci_address);
        bail!("Casimir Guest NCI bridge is currently implemented for Windows only")
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
                .context("connect Casimir control pipe")?;
            let connection_context = Arc::clone(&context);
            tokio::spawn(async move {
                if let Err(error) = serve_connection(server, connection_context).await {
                    tracing::warn!(%error, "Casimir control connection failed");
                }
            });
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (context, launch_bytes);
        bail!("Casimir formal control server is currently implemented for Windows only")
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
        .context("read Casimir control request")?
    {
        ensure!(
            line.len() <= MAX_CONTROL_MESSAGE_BYTES,
            "Casimir control request exceeded the size limit"
        );
        let request: DeviceControlRequestV2 =
            serde_json::from_str(&line).context("decode Casimir control request")?;
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
                "Casimir control protocol or run identity mismatch",
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
                "Casimir control authorization failed",
            )),
        );
    }
    match request.command {
        DeviceControlCommandV2::Ping => response(true, None),
        DeviceControlCommandV2::Action { action } => match apply_action(context, action).await {
            Ok(()) => response(true, None),
            Err(error) => response(
                false,
                Some(ApiErrorV2::new("nfc_action_failed", error.to_string())),
            ),
        },
    }
}

async fn apply_action(context: &FormalContext, action: InstanceActionV2) -> Result<()> {
    let tag_kind = match action {
        InstanceActionV2::NfcTag {
            action: NfcTagActionV2::PresentType2 { ndef_hex },
        } => {
            let ndef = hex::decode(ndef_hex).context("decode Type 2 NDEF")?;
            ensure!(
                ndef.len() <= TYPE_2_MAX_NDEF_BYTES,
                "Type 2 NDEF exceeds the {TYPE_2_MAX_NDEF_BYTES} byte emulated tag capacity"
            );
            Some(TagKind::Type2 {
                memory: type_2_memory(&ndef)?,
            })
        }
        InstanceActionV2::NfcTag {
            action: NfcTagActionV2::PresentType4 { ndef_hex },
        } => Some(TagKind::Type4 {
            ndef: hex::decode(ndef_hex).context("decode Type 4 NDEF")?,
        }),
        InstanceActionV2::NfcTag {
            action: NfcTagActionV2::Remove,
        } => None,
        _ => bail!("action is not implemented by casimir-adapter"),
    };

    let mut active = context.tag.lock().await;
    *active = None;
    if let Some(tag_kind) = tag_kind {
        let (ready_tx, ready_rx) = oneshot::channel();
        let address = context.rf_address;
        let task = tokio::spawn(run_tag(address, tag_kind, ready_tx));
        tokio::time::timeout(TAG_START_TIMEOUT, ready_rx)
            .await
            .context("Casimir RF tag connection timed out")?
            .context("Casimir RF tag exited before becoming ready")?;
        *active = Some(TagHandle { task });
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn run_tag(
    address: std::net::SocketAddr,
    tag: TagKind,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    let stream = TcpStream::connect(address)
        .await
        .context("connect private Casimir RF listener")?;
    let (read, write) = stream.into_split();
    let mut reader = casimir::RfReader::new(read);
    let mut writer = casimir::RfWriter::new(write);
    let _ = ready.send(());
    let mut peer = None;
    let mut selected_file = None;
    let mut type_2_memory = match &tag {
        TagKind::Type2 { memory } => Some(memory.clone()),
        TagKind::Type4 { .. } => None,
    };

    loop {
        let bytes = reader.read().await.context("read Casimir RF packet")?;
        let packet = rf::RfPacket::parse(&bytes).context("decode Casimir RF packet")?;
        match packet.specialize() {
            rf::RfPacketChild::PollCommand(command)
                if command.get_technology() == rf::Technology::NfcA =>
            {
                let int_protocol = match tag {
                    TagKind::Type2 { .. } => 0,
                    TagKind::Type4 { .. } => 1,
                };
                write_rf(
                    &mut writer,
                    rf::NfcAPollResponseBuilder {
                        bit_frame_sdd: 0,
                        int_protocol,
                        nfcid1: vec![0x08, 0x48, 0x44, 0x02],
                        protocol: rf::Protocol::Undetermined,
                        receiver: command.get_sender(),
                        sender: 0,
                    }
                    .into(),
                )
                .await?;
            }
            rf::RfPacketChild::SelectCommand(command)
                if matches!(tag, TagKind::Type2 { .. })
                    && command.get_protocol() == rf::Protocol::T2t =>
            {
                peer = Some(command.get_sender());
                write_rf(
                    &mut writer,
                    rf::SelectResponseBuilder {
                        protocol: rf::Protocol::T2t,
                        receiver: command.get_sender(),
                        sender: 0,
                        technology: rf::Technology::NfcA,
                    }
                    .into(),
                )
                .await?;
            }
            rf::RfPacketChild::T4ATSelectCommand(command)
                if matches!(tag, TagKind::Type4 { .. }) =>
            {
                peer = Some(command.get_sender());
                write_rf(
                    &mut writer,
                    rf::T4ATSelectResponseBuilder {
                        receiver: command.get_sender(),
                        sender: 0,
                        rats_response: vec![0x78, 0x80, 0x70, 0x02],
                    }
                    .into(),
                )
                .await?;
            }
            rf::RfPacketChild::Data(command) if peer == Some(command.get_sender()) => {
                let response = match &tag {
                    TagKind::Type2 { .. } => {
                        handle_type_2(command.get_data(), type_2_memory.as_mut().unwrap())
                    }
                    TagKind::Type4 { ndef } => {
                        handle_type_4(command.get_data(), ndef, &mut selected_file)
                    }
                };
                write_rf(
                    &mut writer,
                    rf::DataBuilder {
                        data: response,
                        protocol: command.get_protocol(),
                        receiver: command.get_sender(),
                        sender: 0,
                        technology: command.get_technology(),
                    }
                    .into(),
                )
                .await?;
            }
            rf::RfPacketChild::DeactivateNotification(_) => {
                peer = None;
                selected_file = None;
            }
            _ => {}
        }
    }
}

async fn write_rf(writer: &mut casimir::RfWriter, packet: rf::RfPacket) -> Result<()> {
    writer.write(&packet.encode_to_vec()?).await
}

fn type_2_memory(ndef: &[u8]) -> Result<Vec<u8>> {
    let mut memory = vec![0_u8; TYPE_2_MEMORY_BYTES];
    memory[..12].copy_from_slice(&[
        0x04, 0x48, 0x44, 0x08, 0x02, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
    ]);
    memory[12..16].copy_from_slice(&[0xe1, 0x10, 0x7e, 0x00]);
    let mut cursor = 16;
    memory[cursor] = 0x03;
    cursor += 1;
    if ndef.len() < 0xff {
        memory[cursor] = u8::try_from(ndef.len())?;
        cursor += 1;
    } else {
        memory[cursor..cursor + 3].copy_from_slice(&[
            0xff,
            u8::try_from(ndef.len() >> 8)?,
            u8::try_from(ndef.len() & 0xff)?,
        ]);
        cursor += 3;
    }
    ensure!(
        cursor + ndef.len() < memory.len(),
        "Type 2 NDEF does not fit"
    );
    memory[cursor..cursor + ndef.len()].copy_from_slice(ndef);
    memory[cursor + ndef.len()] = 0xfe;
    Ok(memory)
}

fn handle_type_2(command: &[u8], memory: &mut [u8]) -> Vec<u8> {
    match command {
        [0x30, page, ..] => read_type_2(memory, usize::from(*page) * 4, 16),
        [0x3a, first, last, ..] if first <= last => read_type_2(
            memory,
            usize::from(*first) * 4,
            (usize::from(*last) - usize::from(*first) + 1) * 4,
        ),
        [0xa2, page, a, b, c, d, ..] => {
            let offset = usize::from(*page) * 4;
            if offset >= 16 && offset + 4 <= memory.len() {
                memory[offset..offset + 4].copy_from_slice(&[*a, *b, *c, *d]);
                vec![0x0a]
            } else {
                vec![0x00]
            }
        }
        [0x60, ..] => vec![0x00, 0x04, 0x04, 0x02, 0x01, 0x00, 0x13, 0x03],
        [0x3c, ..] => vec![0; 32],
        _ => vec![0x00],
    }
}

fn read_type_2(memory: &[u8], offset: usize, length: usize) -> Vec<u8> {
    if offset >= memory.len() {
        return vec![0x00];
    }
    let end = offset.saturating_add(length).min(memory.len());
    memory[offset..end].to_vec()
}

fn handle_type_4(command: &[u8], ndef: &[u8], selected: &mut Option<Type4File>) -> Vec<u8> {
    if command.len() >= 5 && command[..4] == [0x00, 0xa4, 0x04, 0x00] {
        return status(0x90, 0x00);
    }
    if command.len() >= 7 && command[0..4] == [0x00, 0xa4, 0x00, 0x0c] && command[4] == 2 {
        *selected = match &command[5..7] {
            [0xe1, 0x03] => Some(Type4File::CapabilityContainer),
            [0xe1, 0x04] => Some(Type4File::Ndef),
            _ => return status(0x6a, 0x82),
        };
        return status(0x90, 0x00);
    }
    if command.len() >= 5 && command[0..2] == [0x00, 0xb0] {
        let offset = (usize::from(command[2]) << 8) | usize::from(command[3]);
        let length = if command[4] == 0 {
            256
        } else {
            usize::from(command[4])
        };
        let file = match selected {
            Some(Type4File::CapabilityContainer) => type_4_capability_container(ndef.len()),
            Some(Type4File::Ndef) => {
                let mut file = Vec::with_capacity(ndef.len() + 2);
                file.extend_from_slice(
                    &u16::try_from(ndef.len()).unwrap_or(u16::MAX).to_be_bytes(),
                );
                file.extend_from_slice(ndef);
                file
            }
            None => return status(0x69, 0x86),
        };
        if offset > file.len() {
            return status(0x6b, 0x00);
        }
        let mut response = file[offset..offset.saturating_add(length).min(file.len())].to_vec();
        response.extend_from_slice(&[0x90, 0x00]);
        return response;
    }
    status(0x6d, 0x00)
}

fn type_4_capability_container(ndef_len: usize) -> Vec<u8> {
    let max = u16::try_from(ndef_len.saturating_add(2)).unwrap_or(u16::MAX);
    let [max_hi, max_lo] = max.to_be_bytes();
    vec![
        0x00, 0x0f, 0x20, 0x00, 0xff, 0x00, 0xff, 0x04, 0x06, 0xe1, 0x04, max_hi, max_lo, 0x00,
        0xff,
    ]
}

fn status(sw1: u8, sw2: u8) -> Vec<u8> {
    vec![sw1, sw2]
}

async fn write_line<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_CONTROL_MESSAGE_BYTES,
        "Casimir control response exceeded the size limit"
    );
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
