//! Strict host-side evidence parsing for finite Microdroid payload completion.
//!
//! Guest-controlled console and trace files are deliberately outside this module's
//! input contract. A completion is trusted only when the AOSP `vm` launcher itself
//! reports both the payload result and an orderly VM shutdown, and then exits zero.

use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use hd_platform::PlatformError;

const HOST_LOG_SCAN_LIMIT: u64 = 1024 * 1024;
const PAYLOAD_FINISHED_PREFIX: &str = "payload finished with exit code ";
const VM_SHUTDOWN_LINE: &str = "VM ended: Shutdown";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrodroidLauncherCompletion {
    Completed { payload_exit_code: i32 },
    PayloadFailed { payload_exit_code: i32 },
}

impl MicrodroidLauncherCompletion {
    pub const fn payload_exit_code(self) -> i32 {
        match self {
            Self::Completed { payload_exit_code } | Self::PayloadFailed { payload_exit_code } => {
                payload_exit_code
            }
        }
    }
}

pub fn inspect_microdroid_launcher_completion(
    run_dir: &Path,
    process_exit_code: Option<i32>,
) -> Result<Option<MicrodroidLauncherCompletion>, PlatformError> {
    let Some(stdout) = read_optional_log_tail(&run_dir.join("microdroid.stdout.log"))? else {
        return Ok(None);
    };
    let Some(stderr) = read_optional_log_tail(&run_dir.join("microdroid.stderr.log"))? else {
        return Ok(None);
    };
    Ok(classify_microdroid_launcher_completion(
        &stdout,
        &stderr,
        process_exit_code,
    ))
}

pub fn classify_microdroid_launcher_completion(
    host_stdout: &str,
    host_stderr: &str,
    process_exit_code: Option<i32>,
) -> Option<MicrodroidLauncherCompletion> {
    if process_exit_code != Some(0)
        || !host_stdout
            .lines()
            .any(|line| line.trim() == VM_SHUTDOWN_LINE)
    {
        return None;
    }

    let mut payload_exit_code = None;
    for line in host_stderr.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix(PAYLOAD_FINISHED_PREFIX) else {
            continue;
        };
        // The callback is expected exactly once. Reject duplicates, suffixes,
        // overflow and malformed values instead of guessing at completion.
        if payload_exit_code.is_some() || value.is_empty() || value.trim() != value {
            return None;
        }
        let parsed = value.parse::<i32>().ok()?;
        if parsed.to_string() != value {
            return None;
        }
        payload_exit_code = Some(parsed);
    }

    match payload_exit_code? {
        0 => Some(MicrodroidLauncherCompletion::Completed {
            payload_exit_code: 0,
        }),
        payload_exit_code => {
            Some(MicrodroidLauncherCompletion::PayloadFailed { payload_exit_code })
        }
    }
}

fn read_optional_log_tail(path: &Path) -> Result<Option<String>, PlatformError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(PlatformError::Io {
                operation: "inspect Microdroid host launcher log path",
                path: path.to_owned(),
                source,
            });
        }
    }
    let mut file = hd_platform::open_regular_read_nofollow(path)?;
    let len = file
        .metadata()
        .map_err(|source| PlatformError::Io {
            operation: "inspect Microdroid host launcher log",
            path: path.to_owned(),
            source,
        })?
        .len();
    if len > HOST_LOG_SCAN_LIMIT {
        file.seek(SeekFrom::Start(len - HOST_LOG_SCAN_LIMIT))
            .map_err(|source| PlatformError::Io {
                operation: "seek Microdroid host launcher log",
                path: path.to_owned(),
                source,
            })?;
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(len.min(HOST_LOG_SCAN_LIMIT)).unwrap_or_default());
    file.take(HOST_LOG_SCAN_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(|source| PlatformError::Io {
            operation: "read Microdroid host launcher log",
            path: path.to_owned(),
            source,
        })?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}
