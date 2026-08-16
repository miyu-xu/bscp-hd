use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use hd_runtime::microdroid_exit::{
    MicrodroidLauncherCompletion, classify_microdroid_launcher_completion,
    inspect_microdroid_launcher_completion,
};
use serde_json::json;
use uuid::Uuid;

struct TemporaryRunDirectory(PathBuf);

impl TemporaryRunDirectory {
    fn new() -> Result<Self> {
        let parent = std::env::temp_dir().canonicalize()?;
        let path = parent.join(format!("hd-microdroid-exit-{}", Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryRunDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn main() -> Result<()> {
    let run = TemporaryRunDirectory::new()?;
    let stdout = run.path().join("microdroid.stdout.log");
    let stderr = run.path().join("microdroid.stderr.log");
    let guest = run.path().join("microdroid-guest.log");

    std::fs::write(&stdout, "starting\nVM ended: Shutdown\n")?;
    std::fs::write(&stderr, "payload finished with exit code 0\n")?;
    std::fs::write(
        &guest,
        "payload finished with exit code 91\nVM ended: Crash\n",
    )?;
    ensure!(
        inspect_microdroid_launcher_completion(run.path(), Some(0))?
            == Some(MicrodroidLauncherCompletion::Completed {
                payload_exit_code: 0
            }),
        "guest-controlled log text influenced trusted host completion"
    );

    std::fs::write(&stderr, "payload finished with exit code 17\n")?;
    ensure!(
        inspect_microdroid_launcher_completion(run.path(), Some(0))?
            == Some(MicrodroidLauncherCompletion::PayloadFailed {
                payload_exit_code: 17
            }),
        "nonzero payload exit was not retained"
    );

    std::fs::remove_file(&stdout)?;
    std::fs::remove_file(&stderr)?;
    ensure!(
        inspect_microdroid_launcher_completion(run.path(), Some(0))?.is_none(),
        "guest-only completion evidence was trusted"
    );

    ensure!(
        classify_microdroid_launcher_completion(
            "VM ended: Shutdown\n",
            "payload finished with exit code 0\n",
            Some(9),
        )
        .is_none(),
        "nonzero vm launcher exit was trusted"
    );
    ensure!(
        classify_microdroid_launcher_completion(
            "VM ended: Killed\n",
            "payload finished with exit code 0\n",
            Some(0),
        )
        .is_none(),
        "non-Shutdown VM death was trusted"
    );
    for malformed in [
        "payload finished with exit code nope\n",
        "payload finished with exit code 999999999999999999999\n",
        "payload finished with exit code 0 extra\n",
        "payload finished with exit code +0\n",
        "payload finished with exit code 00\n",
    ] {
        ensure!(
            classify_microdroid_launcher_completion("VM ended: Shutdown\n", malformed, Some(0),)
                .is_none(),
            "malformed payload result was trusted: {malformed:?}"
        );
    }
    ensure!(
        classify_microdroid_launcher_completion(
            "VM ended: Shutdown\n",
            "payload finished with exit code 0\npayload finished with exit code 0\n",
            Some(0),
        )
        .is_none(),
        "duplicate payload callbacks were trusted"
    );

    println!(
        "{}",
        serde_json::to_string(&json!({
            "gate": "microdroid-exit-contract-smoke",
            "status": "pass",
            "cases": 7,
            "trusted_sources": [
                "microdroid.stdout.log",
                "microdroid.stderr.log",
                "vm_launcher_exit_code"
            ],
            "guest_log_trusted": false
        }))?
    );
    Ok(())
}
