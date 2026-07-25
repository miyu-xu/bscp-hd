use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail, ensure};

#[derive(Debug)]
pub struct ImportOptions {
    pub python: PathBuf,
    pub image_zip: Option<PathBuf>,
    pub target_files_zip: Option<PathBuf>,
    pub ota_metadata: Option<PathBuf>,
    pub sensor_injector: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub self_check: bool,
}

pub fn import(workspace_root: &Path, options: ImportOptions) -> Result<()> {
    let repository_root = workspace_root
        .parent()
        .context("HD workspace has no repository parent")?;
    let script = repository_root.join("scripts/import_cuttlefish_to_hd.py");
    ensure!(
        script.is_file(),
        "Cuttlefish importer is missing: {}",
        script.display()
    );

    let mut command = Command::new(&options.python);
    command
        .arg(&script)
        .current_dir(repository_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if options.self_check {
        ensure!(
            options.image_zip.is_none()
                && options.target_files_zip.is_none()
                && options.ota_metadata.is_none()
                && options.sensor_injector.is_none()
                && options.output.is_none(),
            "--self-check cannot be combined with import inputs"
        );
        command.arg("--self-check");
    } else {
        let image_zip = options.image_zip.context("--image-zip is required")?;
        let output = options.output.context("--output is required")?;
        for (flag, path) in [("--image-zip", image_zip), ("--output", output)] {
            command.arg(flag).arg(path);
        }
        if let Some(path) = options.target_files_zip {
            command.arg("--target-files-zip").arg(path);
        }
        if let Some(path) = options.ota_metadata {
            command.arg("--ota-metadata").arg(path);
        }
        if let Some(path) = options.sensor_injector {
            command.arg("--sensor-injector").arg(path);
        }
    }
    let status = command.status().with_context(|| {
        format!(
            "start Cuttlefish importer with {}",
            options.python.display()
        )
    })?;
    if !status.success() {
        bail!("Cuttlefish importer failed with {status}");
    }
    Ok(())
}
