use anyhow::{Context as _, Result, bail};
use clap::Parser;
use hd_device_sim::{DeviceRequestV2, DeviceSimulatorV2, MAX_DEVICE_MESSAGE_BYTES, probe};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

#[derive(Debug, Parser)]
#[command(about = "Signed HD host-side Android device simulator")]
struct Arguments {
    /// Emit the versioned capability document and exit.
    #[arg(long)]
    probe_v2: bool,
    /// Encode the probe as JSON. Required with --probe-v2.
    #[arg(long)]
    json: bool,
    /// Serve newline-delimited V2 requests on stdin/stdout.
    #[arg(long)]
    stdio: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    match (arguments.probe_v2, arguments.json, arguments.stdio) {
        (true, true, false) => {
            println!("{}", serde_json::to_string(&probe())?);
            Ok(())
        }
        (false, false, true) => serve_stdio().await,
        _ => bail!("select exactly one mode: --probe-v2 --json, or --stdio"),
    }
}

async fn serve_stdio() -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    let mut simulator = DeviceSimulatorV2::default();
    while let Some(line) = lines.next_line().await.context("read device request")? {
        if line.len() > MAX_DEVICE_MESSAGE_BYTES {
            bail!("device request exceeded {MAX_DEVICE_MESSAGE_BYTES} bytes");
        }
        let request: DeviceRequestV2 =
            serde_json::from_str(&line).context("decode device request V2")?;
        let response = simulator.handle(request);
        let mut bytes = serde_json::to_vec(&response).context("encode device response V2")?;
        bytes.push(b'\n');
        stdout
            .write_all(&bytes)
            .await
            .context("write device response")?;
        stdout.flush().await.context("flush device response")?;
    }
    Ok(())
}
