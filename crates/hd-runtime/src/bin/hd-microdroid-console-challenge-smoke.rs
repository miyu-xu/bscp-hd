#[cfg(unix)]
use std::io::{BufRead as _, BufReader};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use anyhow::{Context as _, ensure};
#[cfg(unix)]
use hd_runtime::{
    MicrodroidConsoleChallengeChannel, MicrodroidConsoleChallengeError,
    MicrodroidConsoleChallengeReceiptV2,
};
#[cfg(unix)]
use serde_json::json;
#[cfg(unix)]
use uuid::Uuid;

#[cfg(not(unix))]
fn main() {
    println!(
        "{{\"schema_version\":1,\"gate\":\"microdroid-console-challenge-smoke\",\"status\":\"not_applicable\",\"reason\":\"unix_fifo_required\"}}"
    );
}

#[cfg(unix)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let temp_root = std::env::temp_dir().canonicalize().with_context(|| {
        format!(
            "canonicalize system temporary root {}",
            std::env::temp_dir().display()
        )
    })?;
    let root = temp_root.join(format!(
        "hd-microdroid-console-challenge-smoke-{}",
        Uuid::new_v4()
    ));
    hd_platform::ensure_owner_only_directory(&root)?;
    verify_unsafe_replacement(&root)?;
    let success = verify_success_path(&root).await?;
    verify_failure_paths(&root).await?;
    let result = json!({
        "schema_version": 1,
        "gate": "microdroid-console-challenge-smoke",
        "status": "pass",
        "challenge_id": success.receipt.challenge_id,
        "request_size_bytes": success.receipt.request_size_bytes,
        "nonce_sha256": success.receipt.nonce_sha256,
        "response_verified": success.receipt.response_verified,
        "one_shot_enforced": true,
        "explicit_confirmation_enforced": true,
        "nil_id_rejected": true,
        "timeout_enforced": true,
        "owner_only_fifo": true,
        "fifo_cleanup": true,
        "unsafe_replacement_rejected": true,
        "raw_nonce_absent_from_audit": success.raw_nonce_absent_from_audit,
        "synthetic_request_prefix": success.request_prefix,
    });
    std::fs::remove_dir_all(&root)
        .with_context(|| format!("remove owned smoke root {}", root.display()))?;
    publish_result(&result)
}

#[cfg(unix)]
struct SuccessEvidence {
    receipt: MicrodroidConsoleChallengeReceiptV2,
    raw_nonce_absent_from_audit: bool,
    request_prefix: String,
}

#[cfg(unix)]
fn verify_unsafe_replacement(root: &Path) -> anyhow::Result<()> {
    let unsafe_input = root.join("unsafe-console-in");
    hd_platform::write_owner_only(&unsafe_input, b"not a fifo")?;
    ensure!(
        MicrodroidConsoleChallengeChannel::create(
            &unsafe_input,
            root.join("unsafe-console-out"),
            root.join("unsafe-audit.json"),
        )
        .is_err(),
        "console challenge replaced a non-FIFO path"
    );
    Ok(())
}

#[cfg(unix)]
async fn verify_success_path(root: &Path) -> anyhow::Result<SuccessEvidence> {
    let input = root.join("microdroid-console-in.fifo");
    let output = root.join("microdroid-console.txt");
    let audit = root.join("microdroid-console-challenge.json");
    let mut channel = MicrodroidConsoleChallengeChannel::create(&input, &output, &audit)?;
    ensure!(
        channel.input_path() == input,
        "channel returned a different FIFO path"
    );
    let fifo_metadata = std::fs::symlink_metadata(&input)?;
    ensure!(
        fifo_metadata.file_type().is_fifo(),
        "console input is not a FIFO"
    );
    ensure!(
        fifo_metadata.permissions().mode() & 0o777 == 0o600,
        "console FIFO is not owner-only"
    );
    let guest = spawn_synthetic_guest(input.clone(), output);
    let challenge_id = Uuid::new_v4();
    let receipt = channel
        .send_and_verify(challenge_id, true, Duration::from_secs(2))
        .await?;
    let request = guest
        .join()
        .map_err(|_| anyhow::anyhow!("synthetic trusted Payload thread panicked"))??;
    ensure!(receipt.response_verified, "Guest response was not verified");
    ensure!(
        receipt.challenge_id == challenge_id,
        "challenge identity drifted"
    );
    ensure!(
        receipt.request_size_bytes <= 160,
        "challenge frame exceeded its bound"
    );
    ensure!(
        matches!(
            channel
                .send_and_verify(Uuid::new_v4(), true, Duration::from_millis(100))
                .await,
            Err(MicrodroidConsoleChallengeError::AlreadyUsed)
        ),
        "channel accepted a second challenge in one run"
    );
    let audit_json: serde_json::Value = serde_json::from_slice(&std::fs::read(&audit)?)?;
    ensure!(
        audit_json["response_verified"] == true,
        "success audit was not verified"
    );
    ensure!(
        audit_json.get("nonce").is_none(),
        "success audit retained a raw nonce"
    );
    ensure!(
        std::fs::metadata(&audit)?.permissions().mode() & 0o777 == 0o600,
        "success audit is not owner-only"
    );
    drop(channel);
    ensure!(!input.exists(), "successful channel left its FIFO behind");
    Ok(SuccessEvidence {
        receipt,
        raw_nonce_absent_from_audit: audit_json.get("nonce").is_none(),
        request_prefix: request.split(' ').next().unwrap_or_default().to_owned(),
    })
}

#[cfg(unix)]
fn spawn_synthetic_guest(
    input: std::path::PathBuf,
    output: std::path::PathBuf,
) -> std::thread::JoinHandle<anyhow::Result<String>> {
    std::thread::spawn(move || {
        let mut request = String::new();
        BufReader::new(
            std::fs::File::open(&input).with_context(|| format!("open {}", input.display()))?,
        )
        .read_line(&mut request)?;
        let fields = request.trim_end().split(' ').collect::<Vec<_>>();
        ensure!(
            fields.len() == 3,
            "challenge frame did not have exactly three fields"
        );
        ensure!(
            fields[0] == "HD_CONSOLE_CHALLENGE_V1",
            "challenge prefix drifted"
        );
        Uuid::parse_str(fields[1]).context("parse challenge id")?;
        ensure!(
            fields[2].len() == 64 && fields[2].bytes().all(|byte| byte.is_ascii_hexdigit()),
            "challenge nonce was not 32-byte hex"
        );
        let response = format!(
            "trusted payload ready\nHD_CONSOLE_RESPONSE_V1 {} {}\n",
            fields[1], fields[2]
        );
        hd_platform::write_owner_only(&output, response.as_bytes())?;
        Ok(request)
    })
}

#[cfg(unix)]
async fn verify_failure_paths(root: &Path) -> anyhow::Result<()> {
    let input = root.join("timeout-console-in.fifo");
    let audit = root.join("timeout-console-challenge.json");
    let mut channel = MicrodroidConsoleChallengeChannel::create(
        &input,
        root.join("timeout-console.txt"),
        &audit,
    )?;
    ensure!(
        matches!(
            channel
                .send_and_verify(Uuid::new_v4(), false, Duration::from_millis(100))
                .await,
            Err(MicrodroidConsoleChallengeError::ConfirmationRequired)
        ),
        "unconfirmed console challenge was accepted"
    );
    ensure!(
        matches!(
            channel
                .send_and_verify(Uuid::nil(), true, Duration::from_millis(100))
                .await,
            Err(MicrodroidConsoleChallengeError::NilChallengeId)
        ),
        "nil console challenge id was accepted"
    );
    ensure!(
        matches!(
            channel
                .send_and_verify(Uuid::new_v4(), true, Duration::from_millis(100))
                .await,
            Err(MicrodroidConsoleChallengeError::Timeout)
        ),
        "unanswered console challenge did not time out"
    );
    let audit_json: serde_json::Value = serde_json::from_slice(&std::fs::read(&audit)?)?;
    ensure!(
        audit_json["response_verified"] == false
            && audit_json["error_code"] == "microdroid_console_challenge_timeout",
        "timeout audit did not preserve the bounded failure"
    );
    drop(channel);
    ensure!(!input.exists(), "timed-out channel left its FIFO behind");
    Ok(())
}

#[cfg(unix)]
fn publish_result(result: &serde_json::Value) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(result)?;
    if let Some(output) = std::env::var_os("HD_SMOKE_OUTPUT") {
        let output = std::path::PathBuf::from(output);
        ensure!(output.is_absolute(), "HD_SMOKE_OUTPUT must be absolute");
        hd_platform::write_owner_only(&output, &bytes)?;
    }
    println!("{}", String::from_utf8(bytes)?);
    Ok(())
}
