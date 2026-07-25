use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use clap::Parser;
use hd_core::{
    COMPONENT_PROTOCOL_VERSION, FormalComponentConfigurationV2, FormalComponentLaunchV2,
    FormalComponentProbeV2, FormalComponentReadyV2,
};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;

const LAUNCH_LIMIT: u64 = 64 * 1024;
// The first invocation of the large Windows crosvm binary can spend more than ten seconds in
// loader/AV scanning before it reaches the already-running VM control pipe.  Keep this aligned
// with the runtime control timeout so a cold helper launch does not leave ADB permanently offline.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const PIPE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
#[command(about = "HD owner-only loopback ADB to guest-vsock bridge")]
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
struct BridgeContext {
    crosvm_executable: PathBuf,
    guest_cid: u32,
    guest_port: u32,
    vm_control_endpoint: String,
    connection_gate: tokio::sync::Mutex<()>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_ansi(false)
        .init();
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
        (false, false, true, Some(path)) => serve(path).await,
        _ => bail!("select exactly one mode: --probe-v2 --json or --serve-v2 --launch <path>"),
    }
}

fn probe() -> FormalComponentProbeV2 {
    FormalComponentProbeV2 {
        protocol_version: COMPONENT_PROTOCOL_VERSION,
        component: "adb-bridge".to_owned(),
        formal: cfg!(windows),
        features: if cfg!(windows) {
            vec![
                "loopback-tcp-v2".to_owned(),
                "vsock-guest-v2".to_owned(),
                "ready-marker-v2".to_owned(),
                "lifecycle-v2".to_owned(),
                "windows-owner-pipe-v2".to_owned(),
            ]
        } else {
            Vec::new()
        },
    }
}

async fn serve(launch_path: &Path) -> Result<()> {
    ensure!(cfg!(windows), "hd-adb-bridge currently requires Windows");
    let launch_bytes = hd_platform::read_regular_nofollow_limited(launch_path, LAUNCH_LIMIT)
        .context("read ADB bridge launch")?;
    let launch: FormalComponentLaunchV2 =
        serde_json::from_slice(&launch_bytes).context("decode ADB bridge launch")?;
    ensure!(
        launch.protocol_version == COMPONENT_PROTOCOL_VERSION && launch.component == "adb-bridge",
        "ADB bridge launch protocol or component mismatch"
    );
    ensure!(
        launch.component_ready_marker.parent() == launch_path.parent()
            && launch
                .component_ready_marker
                .file_name()
                .and_then(|name| name.to_str())
                == Some("adb-bridge-ready-v2.json"),
        "ADB bridge ready marker is outside the launch directory"
    );
    let FormalComponentConfigurationV2::AdbBridge {
        listen_address,
        listen_port,
        guest_cid,
        guest_port,
        vm_control_endpoint,
        crosvm_executable,
    } = &launch.configuration
    else {
        bail!("adb-bridge requires an adb_bridge configuration");
    };
    ensure!(
        listen_address == "127.0.0.1",
        "ADB bridge must use IPv4 loopback"
    );
    ensure!(*listen_port != 0, "ADB bridge listen port must be nonzero");
    ensure!(*guest_cid >= 3, "ADB bridge guest CID is reserved");
    ensure!(*guest_port >= 1024, "ADB bridge guest port is privileged");
    ensure!(crosvm_executable.is_file(), "crosvm executable is missing");
    #[cfg(windows)]
    ensure!(
        vm_control_endpoint.starts_with(r"\\.\pipe\"),
        "crosvm control endpoint is not a local named pipe"
    );

    let listener = TcpListener::bind((listen_address.as_str(), *listen_port))
        .await
        .context("bind ADB loopback listener")?;
    tracing::info!(
        event = "adb.bridge.listen",
        address = %listen_address,
        port = *listen_port,
        guest_cid = *guest_cid,
        guest_port = *guest_port,
        vm_control_endpoint = %vm_control_endpoint,
        "ADB bridge listening"
    );
    publish_ready(&launch, &launch_bytes)?;
    let context = Arc::new(BridgeContext {
        crosvm_executable: crosvm_executable.clone(),
        guest_cid: *guest_cid,
        guest_port: *guest_port,
        vm_control_endpoint: vm_control_endpoint.clone(),
        connection_gate: tokio::sync::Mutex::new(()),
    });
    loop {
        let (stream, peer) = listener.accept().await.context("accept ADB client")?;
        ensure!(peer.ip().is_loopback(), "ADB client is not loopback");
        tracing::info!(event = "adb.bridge.accepted", %peer, "accepted ADB client");
        let connection = Arc::clone(&context);
        tokio::spawn(async move {
            if let Err(error) = bridge_connection(stream, &connection).await {
                tracing::warn!(event = "adb.bridge.connection.failed", %error, "ADB bridge connection failed");
            }
        });
    }
}

fn publish_ready(launch: &FormalComponentLaunchV2, launch_bytes: &[u8]) -> Result<()> {
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

async fn bridge_connection(mut tcp: TcpStream, context: &BridgeContext) -> Result<()> {
    // The Worker proves TCP readiness with a connect-and-close probe. Do not turn that empty
    // probe into a crosvm host-vsock listener: an unconsumed listener occupies crosvm's sole
    // named-pipe instance and prevents the real ADB transport from connecting.
    let mut initial = [0_u8; 4096];
    let Ok(initial_result) = tokio::time::timeout(FIRST_BYTE_TIMEOUT, tcp.read(&mut initial)).await
    else {
        tracing::debug!(
            event = "adb.bridge.empty_probe.timeout",
            "discarding an ADB TCP connection that sent no bytes"
        );
        return Ok(());
    };
    let initial_size = initial_result.context("read first ADB TCP bytes")?;
    if initial_size == 0 {
        tracing::debug!(
            event = "adb.bridge.empty_probe.closed",
            "discarding an empty ADB TCP readiness probe"
        );
        return Ok(());
    }
    // crosvm exposes one named-pipe instance for a guest CID/port pair. ADB may open a new TCP
    // transport while it is tearing down an offline one, so serialize the corresponding vsock
    // sessions instead of racing multiple connect_vsock commands against the same pipe.
    let _connection_guard = context.connection_gate.lock().await;
    tracing::info!(
        event = "adb.bridge.connect_vsock.started",
        guest_cid = context.guest_cid,
        guest_port = context.guest_port,
        vm_control_endpoint = %context.vm_control_endpoint,
        "requesting crosvm connect_vsock"
    );
    let mut command = Command::new(&context.crosvm_executable);
    command
        .args([
            "connect_vsock",
            &context.guest_port.to_string(),
            &context.vm_control_endpoint,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    hd_platform::configure_transient_command(command.as_std_mut())
        .context("configure hidden crosvm connect_vsock process")?;
    let status = tokio::time::timeout(CONTROL_TIMEOUT, command.status())
        .await
        .context("crosvm connect_vsock timed out")?
        .context("start crosvm connect_vsock")?;
    if status.success() {
        tracing::info!(
            event = "adb.bridge.connect_vsock.succeeded",
            guest_cid = context.guest_cid,
            guest_port = context.guest_port,
            "crosvm connect_vsock succeeded"
        );
    } else {
        tracing::warn!(
            event = "adb.bridge.connect_vsock.nonzero",
            guest_cid = context.guest_cid,
            guest_port = context.guest_port,
            %status,
            "crosvm connect_vsock returned nonzero; attempting to consume a pending host-vsock pipe"
        );
    }
    bridge_named_pipe(&mut tcp, context, &initial[..initial_size]).await
}

#[cfg(windows)]
async fn bridge_named_pipe(
    tcp: &mut TcpStream,
    context: &BridgeContext,
    initial: &[u8],
) -> Result<()> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let endpoint = format!(
        r"\\.\pipe\binder_rpc_vsock_{}_{}",
        context.guest_cid, context.guest_port
    );
    tracing::info!(
        event = "adb.bridge.pipe.connecting",
        endpoint = %endpoint,
        "waiting for guest-vsock named pipe"
    );
    let pipe = tokio::time::timeout(PIPE_CONNECT_TIMEOUT, async {
        loop {
            match ClientOptions::new().open(&endpoint) {
                Ok(pipe) => {
                    tracing::info!(
                        event = "adb.bridge.pipe.connected",
                        endpoint = %endpoint,
                        "guest-vsock named pipe connected"
                    );
                    return Ok::<_, anyhow::Error>(pipe);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::WouldBlock
                    ) || is_pipe_instance_busy_error(&error) =>
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error).context("connect guest-vsock named pipe"),
            }
        }
    })
    .await
    .context("guest-vsock named pipe timed out")??;
    let (closed_side, bytes) = {
        let (mut tcp_reader, mut tcp_writer) = tcp.split();
        let (mut pipe_reader, mut pipe_writer) = tokio::io::split(pipe);
        pipe_writer
            .write_all(initial)
            .await
            .context("copy initial ADB TCP bytes to guest-vsock")?;
        let finished = tokio::select! {
            result = tokio::io::copy(&mut tcp_reader, &mut pipe_writer) => {
                ("tcp", result.context("copy ADB TCP to guest-vsock")?)
            }
            result = tokio::io::copy(&mut pipe_reader, &mut tcp_writer) => {
                ("guest_vsock", result.context("copy guest-vsock to ADB TCP")?)
            }
        };
        // Closing both pipe halves releases crosvm's sole named-pipe instance before the next
        // serialized ADB transport is allowed to connect.
        drop(pipe_reader);
        drop(pipe_writer);
        finished
    };
    tracing::info!(
        event = "adb.bridge.copy.finished",
        closed_side,
        bytes,
        "ADB bridge byte copy finished"
    );
    tcp.shutdown().await.context("shutdown ADB TCP stream")?;
    Ok(())
}

#[cfg(not(windows))]
async fn bridge_named_pipe(
    _tcp: &mut TcpStream,
    _context: &BridgeContext,
    _initial: &[u8],
) -> Result<()> {
    bail!("Windows named-pipe guest-vsock transport is unavailable")
}

fn is_pipe_instance_busy_error(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(231)
}
