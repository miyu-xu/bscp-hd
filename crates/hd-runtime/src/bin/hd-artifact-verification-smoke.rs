use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context as _, bail};
use serde::Serialize;

#[derive(Serialize)]
struct ResultDocument {
    schema_version: u32,
    profile: &'static str,
    implementation: &'static str,
    path: PathBuf,
    logical_bytes: u64,
    expected_sha256: String,
    actual_sha256: String,
    duration_ms: u128,
    budget_ms: u128,
    complete_read: bool,
    cache_used: bool,
    external_process_used: bool,
    status: &'static str,
}

fn argument(name: &str) -> anyhow::Result<String> {
    let mut args = std::env::args().skip(1);
    while let Some(value) = args.next() {
        if value == name {
            return args
                .next()
                .with_context(|| format!("{name} requires a value"));
        }
    }
    bail!("missing required argument {name}")
}

fn main() -> anyhow::Result<()> {
    let path = PathBuf::from(argument("--file")?);
    if !path.is_absolute() {
        bail!("--file must be absolute");
    }
    let expected = argument("--expected-sha256")?.to_ascii_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("--expected-sha256 must contain 64 hexadecimal characters");
    }
    let budget_seconds = argument("--budget-seconds")?
        .parse::<u64>()
        .context("--budget-seconds must be an integer")?;
    if budget_seconds == 0 || budget_seconds > 600 {
        bail!("--budget-seconds must be in 1..=600");
    }
    let metadata =
        std::fs::symlink_metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!(
            "artifact is not a regular non-symlink file: {}",
            path.display()
        );
    }

    let started = Instant::now();
    let actual = hd_runtime::sha256_file(&path)?;
    let duration_ms = started.elapsed().as_millis();
    if actual != expected {
        bail!("artifact digest mismatch: expected {expected}, actual {actual}");
    }
    let budget_ms = u128::from(budget_seconds) * 1000;
    if duration_ms > budget_ms {
        bail!("artifact verification took {duration_ms} ms, budget is {budget_ms} ms");
    }

    let document = ResultDocument {
        schema_version: 1,
        profile: "macos-commoncrypto-artifact-hash-v1",
        implementation: if cfg!(target_os = "macos") {
            "commoncrypto"
        } else {
            "portable-sha2"
        },
        path,
        logical_bytes: metadata.len(),
        expected_sha256: expected,
        actual_sha256: actual,
        duration_ms,
        budget_ms,
        complete_read: true,
        cache_used: false,
        external_process_used: false,
        status: "pass",
    };
    println!("{}", serde_json::to_string_pretty(&document)?);
    Ok(())
}
