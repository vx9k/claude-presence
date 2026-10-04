//! Background daemon entry point used by the per-user services. On Windows it
//! is built for the GUI subsystem so starting it at logon never flashes a
//! console window; elsewhere it is identical to `claude-presence daemon`.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let log_file = if cfg!(windows) { Some(claude_presence::paths::data_dir().join("daemon.log")) } else { None };
    std::process::exit(claude_presence::daemon::run(claude_presence::daemon::Options { log_file }));
}
