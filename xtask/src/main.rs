use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use hd_core::{
    ControlCommandV1, ControlPayloadV1, ControlRequestV1, InstanceAction, InstanceConfigV1,
    InstanceState, Orientation,
};
use hd_platform::DataPaths;
use hd_runtime::{CrosvmBackend, Supervisor};

#[derive(Debug, Parser)]
#[command(about = "HD developer and quality tasks")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Run formatting, compile, Clippy and the non-unittest mock smoke gate.
    Quality,
    /// Compile the portable workspace for the current host.
    CheckPortable,
    /// Exercise the supervisor lifecycle through the mock backend without a test harness.
    Smoke,
    /// Audit Windows PE imports for accidental MSVC runtime dependencies.
    PeAudit {
        #[arg(long)]
        bin_dir: PathBuf,
        #[arg(long, default_value = "objdump")]
        objdump: PathBuf,
    },
    /// Assemble HD binaries and docs into a staging directory.
    Package {
        #[arg(long)]
        target_dir: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}

fn main() -> Result<()> {
    let root = workspace_root()?;
    match Cli::parse().command {
        Task::Quality => {
            require_windows_gnu()?;
            run(&root, "cargo", &["fmt", "--all", "--", "--check"])?;
            let target = quality_target_args();
            let mut check = vec!["check", "--workspace", "--all-targets"];
            check.extend(target.iter().copied());
            run(&root, "cargo", &check)?;
            let mut clippy = vec!["clippy", "--workspace", "--all-targets"];
            clippy.extend(target.iter().copied());
            clippy.extend(["--", "-D", "warnings"]);
            run(&root, "cargo", &clippy)?;
            smoke(&root)
        }
        Task::CheckPortable => {
            require_windows_gnu()?;
            let target = quality_target_args();
            let mut arguments = vec!["check", "--workspace", "--all-targets"];
            arguments.extend(target.iter().copied());
            run(&root, "cargo", &arguments)
        }
        Task::Smoke => {
            require_windows_gnu()?;
            smoke(&root)
        }
        Task::PeAudit { bin_dir, objdump } => pe_audit(&bin_dir, &objdump),
        Task::Package { target_dir, output } => package(&root, &target_dir, &output),
    }
}

fn require_windows_gnu() -> Result<()> {
    if cfg!(all(windows, not(target_env = "gnu"))) {
        bail!(
            "Windows developer tasks must themselves use x86_64-pc-windows-gnu; run cargo run \
             --target x86_64-pc-windows-gnu -p xtask -- <task>"
        );
    }
    Ok(())
}

fn quality_target_args() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["--target", "x86_64-pc-windows-gnu"]
    } else {
        Vec::new()
    }
}

#[allow(clippy::too_many_lines)]
fn smoke(root: &Path) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create smoke runtime")?;
    runtime.block_on(async {
        let temp = tempfile::tempdir().context("create smoke data directory")?;
        let paths = DataPaths::from_root(temp.path().join("data"));
        let supervisor = Arc::new(Supervisor::new(
            paths.clone(),
            CrosvmBackend::new(root.join("smoke-crosvm-not-launched")),
        )?);
        let config = InstanceConfigV1 {
            name: "HD smoke".to_owned(),
            ..Default::default()
        };
        let id = config.id;
        expect_ok(
            supervisor
                .handle(ControlRequestV1::new(ControlCommandV1::Create { config }))
                .await,
        )?;
        expect_ok(
            supervisor
                .handle(ControlRequestV1::new(ControlCommandV1::Start {
                    id,
                    mock: true,
                }))
                .await,
        )?;
        let summary = instance_payload(
            supervisor
                .handle(ControlRequestV1::new(ControlCommandV1::Show { id }))
                .await,
        )?;
        ensure!(
            summary.state.state == InstanceState::Ready,
            "mock did not reach ready"
        );

        for action in [
            InstanceAction::Home,
            InstanceAction::Recent,
            InstanceAction::Back,
            InstanceAction::Rotate,
        ] {
            expect_ok(
                supervisor
                    .handle(ControlRequestV1::new(ControlCommandV1::Action {
                        id,
                        action,
                    }))
                    .await,
            )?;
        }
        let apk = temp.path().join("smoke.apk");
        std::fs::write(&apk, b"not launched").context("create smoke APK fixture")?;
        expect_ok(
            supervisor
                .handle(ControlRequestV1::new(ControlCommandV1::InstallApk {
                    id,
                    path: apk,
                }))
                .await,
        )?;
        let mut display = supervisor
            .config(id)
            .await
            .context("missing smoke config")?
            .display;
        display.orientation = Orientation::Portrait;
        expect_ok(
            supervisor
                .handle(ControlRequestV1::new(ControlCommandV1::ApplyDisplay {
                    id,
                    display,
                }))
                .await,
        )?;
        expect_ok(
            supervisor
                .handle(ControlRequestV1::new(ControlCommandV1::Stop { id }))
                .await,
        )?;

        let blocked = supervisor
            .handle(ControlRequestV1::new(ControlCommandV1::Start {
                id,
                mock: false,
            }))
            .await;
        ensure!(!blocked.ok, "real smoke launch unexpectedly succeeded");
        let summary = instance_payload(
            supervisor
                .handle(ControlRequestV1::new(ControlCommandV1::Show { id }))
                .await,
        )?;
        ensure!(
            summary.state.state == InstanceState::Blocked,
            "missing artifacts did not produce blocked state"
        );
        expect_ok(
            supervisor
                .handle(ControlRequestV1::new(ControlCommandV1::Stop { id }))
                .await,
        )?;

        let runs = paths.runs.join(id.to_string());
        let run_dirs = std::fs::read_dir(&runs)
            .with_context(|| format!("read smoke runs at {}", runs.display()))?
            .collect::<Result<Vec<_>, _>>()?;
        ensure!(run_dirs.len() == 2, "smoke expected two run records");
        for run in run_dirs {
            for name in ["manifest.json", "events.jsonl", "result.json"] {
                ensure!(
                    run.path().join(name).is_file(),
                    "smoke run {} is missing {name}",
                    run.path().display()
                );
            }
        }
        println!(
            "HD mock and blocked-launch smoke passed: {}",
            runs.display()
        );
        Ok(())
    })
}

fn expect_ok(response: hd_core::ControlResponseV1) -> Result<()> {
    if response.ok {
        Ok(())
    } else {
        let message = response.error.map_or_else(
            || "unknown control failure".to_owned(),
            |error| error.message,
        );
        bail!("smoke control request failed: {message}")
    }
}

fn instance_payload(response: hd_core::ControlResponseV1) -> Result<hd_core::InstanceSummaryV1> {
    expect_ok(response.clone())?;
    match response.payload {
        Some(ControlPayloadV1::Instance(summary)) => Ok(summary),
        payload => bail!("expected instance payload, got {payload:?}"),
    }
}

fn workspace_root() -> Result<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_owned)
        .context("xtask manifest has no parent")
}

fn run(root: &Path, program: &str, arguments: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("start {program} {}", arguments.join(" ")))?;
    if !status.success() {
        bail!("{program} {} failed with {status}", arguments.join(" "));
    }
    Ok(())
}

fn pe_audit(bin_dir: &Path, objdump: &Path) -> Result<()> {
    let mut audited = 0_u32;
    for entry in std::fs::read_dir(bin_dir)
        .with_context(|| format!("read PE directory {}", bin_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("exe") {
            continue;
        }
        let output = Command::new(objdump)
            .args(["-p", &path.to_string_lossy()])
            .output()
            .with_context(|| format!("run {} for {}", objdump.display(), path.display()))?;
        if !output.status.success() {
            bail!("objdump failed for {}", path.display());
        }
        let imports = String::from_utf8_lossy(&output.stdout).to_ascii_uppercase();
        for forbidden in ["VCRUNTIME", "MSVCP", "CONCRT", "MFC"] {
            if imports.contains(forbidden) {
                bail!(
                    "{} imports forbidden MSVC runtime {forbidden}",
                    path.display()
                );
            }
        }
        audited = audited.saturating_add(1);
    }
    if audited == 0 {
        bail!("no .exe files found under {}", bin_dir.display());
    }
    println!("PE audit passed for {audited} executables");
    Ok(())
}

fn package(root: &Path, target_dir: &Path, output: &Path) -> Result<()> {
    std::fs::create_dir_all(output)
        .with_context(|| format!("create package directory {}", output.display()))?;
    for name in ["hd.exe", "hdctl.exe"] {
        let source = target_dir.join(name);
        let destination = output.join(name);
        std::fs::copy(&source, &destination)
            .with_context(|| format!("copy {} to {}", source.display(), destination.display()))?;
    }
    std::fs::copy(root.join("README.md"), output.join("README.md"))?;
    std::fs::copy(root.join("LICENSE"), output.join("LICENSE"))?;
    let docs_output = output.join("docs");
    std::fs::create_dir_all(&docs_output)?;
    for name in [
        "PLAN.md",
        "ARCHITECTURE.md",
        "DEVELOPMENT.md",
        "TESTING.md",
        "AI_WORKFLOW.md",
        "RUNBOOK.md",
    ] {
        std::fs::copy(root.join("docs").join(name), docs_output.join(name))?;
    }
    Ok(())
}
