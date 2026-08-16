use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};

pub const NETWORK_STATUS_TIMEOUT: Duration = Duration::from_secs(3);

const NETWORK_SETUP_SCRIPT_BYTES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/macos-network-setup.sh"
));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkSetupAction {
    Status,
    Install,
}

impl NetworkSetupAction {
    const fn argument(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Install => "install",
        }
    }
}

pub fn script_matches_embedded(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
        && std::fs::read(path).is_ok_and(|bytes| bytes.as_slice() == NETWORK_SETUP_SCRIPT_BYTES)
}

pub fn run_status_script(path: &Path, timeout: Duration) -> Result<Output, String> {
    if !script_matches_embedded(path) {
        return Err("网络兼容服务脚本缺失、被替换或与当前 HD 版本不匹配".to_owned());
    }
    let mut command = Command::new(path);
    command.arg(NetworkSetupAction::Status.argument());
    run_command_with_timeout(command, timeout)
        .map_err(|error| format!("读取网络兼容服务状态失败：{error}"))
}

#[doc(hidden)]
pub fn run_command_with_timeout(mut command: Command, timeout: Duration) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    hd_platform::configure_transient_command(&mut command)
        .map_err(|error| format!("无法隔离命令进程组：{error}"))?;
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动命令：{error}"))?;
    let containment = match hd_platform::contain_process(child.id()) {
        Ok(containment) => containment,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("无法接管命令进程组：{error}"));
        }
    };
    let started = Instant::now();
    loop {
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                drop(containment);
                let _ = child.wait();
                return Err(format!("无法读取命令状态：{error}"));
            }
        };
        match status {
            Some(_) => {
                drop(containment);
                return child
                    .wait_with_output()
                    .map_err(|error| format!("无法读取命令输出：{error}"));
            }
            None if started.elapsed() >= timeout => {
                drop(containment);
                let _ = child.wait();
                return Err(format!("命令超过 {} 毫秒未完成", timeout.as_millis()));
            }
            None => thread::sleep(Duration::from_millis(20)),
        }
    }
}

pub fn staged_shell_command(action: NetworkSetupAction) -> String {
    let encoded_script = STANDARD.encode(NETWORK_SETUP_SCRIPT_BYTES);
    format!(
        "set -eu; stage=$(/usr/bin/mktemp -d /private/tmp/hd-network-ui.XXXXXX); /bin/chmod 0700 \"$stage\"; cleanup() {{ /bin/rm -rf -- \"$stage\"; }}; trap cleanup EXIT HUP INT TERM; /usr/bin/printf %s {} | /usr/bin/base64 -D > \"$stage/macos-network-setup.sh\"; /bin/chmod 0700 \"$stage/macos-network-setup.sh\"; \"$stage/macos-network-setup.sh\" {}",
        shell_quote(&encoded_script),
        action.argument()
    )
}

pub fn administrator_install_apple_script() -> String {
    format!(
        "do shell script {} with administrator privileges",
        apple_script_string(&staged_shell_command(NetworkSetupAction::Install))
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn apple_script_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
