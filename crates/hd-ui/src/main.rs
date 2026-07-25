#![cfg_attr(windows, windows_subsystem = "windows")]

mod web_shell;

fn main() -> anyhow::Result<()> {
    web_shell::run()
}
