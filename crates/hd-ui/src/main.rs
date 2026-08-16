#![cfg_attr(windows, windows_subsystem = "windows")]

mod web_shell;

fn main() {
    let startup = hd_platform::configure_desktop_application_startup()
        .map_err(anyhow::Error::from)
        .and_then(|()| web_shell::run());
    if let Err(error) = startup {
        let details = format!("{error:#}");
        let message = format!(
            "HD 无法启动本地服务。\n\n请退出后重新打开 HD；如果问题仍然存在，请打开日志目录并保留其中的文件。\n\n错误详情：{details}"
        );
        let logs = hd_platform::DataPaths::discover()
            .ok()
            .map(|paths| paths.logs);
        // Emit the root cause before entering the modal AppKit/Win32 dialog loop so launchers,
        // automation, and headless support sessions can diagnose a startup failure immediately.
        eprintln!("HD startup failed: {error:#}");
        if let Err(dialog_error) =
            hd_platform::show_fatal_error_dialog("HD 无法启动", &message, logs.as_deref())
        {
            eprintln!("show HD startup error dialog failed: {dialog_error}");
        }
        std::process::exit(1);
    }
}
