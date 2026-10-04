use claude_presence::install::{self, Init};
use claude_presence::{config, daemon, ipc, ledger, paths, timeutil};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
claude-presence — Discord Rich Presence for Claude Code

USAGE:
    claude-presence <COMMAND>

COMMANDS:
    install [--init <kind>] [--no-service] [--no-hooks]
                     Wire Claude Code hooks, write a default config and
                     register + start the background service. <kind> is one
                     of: systemd, openrc, dinit, xdg-autostart, launchd,
                     schtasks, run-key, none (default: auto-detect)
    uninstall [--purge]
                     Remove hooks and service (--purge also deletes config
                     and lifetime stats)
    status           Show daemon state and lifetime stats
    daemon           Run the daemon in the foreground
    hook <Event>     Forward a Claude Code hook event (used by Claude Code)
    config           Print the config file path
    help, --help     Show this message
    --version        Print the version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let rest = &args[args.len().min(1)..];
    match cmd {
        "hook" => hook(rest.first().map(String::as_str).unwrap_or("")),
        "daemon" => ExitCode::from(daemon::run(daemon::Options { log_file: None }) as u8),
        "install" => install_cmd(rest),
        "uninstall" => uninstall_cmd(rest),
        "status" => status(),
        "config" => {
            println!("{}", paths::config_file().display());
            ExitCode::SUCCESS
        }
        "-V" | "--version" | "version" => {
            println!("claude-presence {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Runs on every Claude Code hook: forward stdin to the daemon and get out of
/// the way. Never fails loudly — presence must not break the user's session.
fn hook(event: &str) -> ExitCode {
    if !forwardable(event) {
        // Drain stdin so Claude Code never sees a broken pipe.
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        return ExitCode::SUCCESS;
    }
    let mut msg = Vec::with_capacity(4096);
    msg.extend_from_slice(event.as_bytes());
    msg.push(b'\n');
    let _ = std::io::stdin().lock().read_to_end(&mut msg);
    let (sock, in_private_dir) = paths::hook_endpoint();
    let _ = ipc::send_hook(&sock, in_private_dir, paths::legacy_hook_socket, &msg);
    ExitCode::SUCCESS
}

/// Reserved `__` control events are only sent by claude-presence itself, so
/// a `settings.json` hook entry can't, say, stop the daemon.
fn forwardable(event: &str) -> bool {
    !ipc::is_control(event.as_bytes())
}

fn exe_name(base: &str) -> String {
    format!("{base}{}", std::env::consts::EXE_SUFFIX)
}

/// On Windows, binaries are often run from Downloads; copy them to the
/// standard per-user programs folder so hooks keep working.
///
/// A running daemon holds its .exe open. With a service, `install` already
/// stopped it (and starts the new one); with `service == false` stop it here
/// and start the new binary detached, so it isn't left stopped.
#[cfg(windows)]
fn stable_location(exe: &Path, service: bool) -> std::io::Result<PathBuf> {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home().join("AppData").join("Local"))
        .join("Programs")
        .join(paths::APP);
    if exe.parent() == Some(dir.as_path()) {
        return Ok(exe.to_path_buf());
    }
    std::fs::create_dir_all(&dir)?;
    let mut restart = false;
    if !service {
        for line in install::stop_daemons() {
            println!("  {line} (to replace its binary)");
            restart = true;
        }
    }
    let names = [exe_name("claude-presence"), exe_name("claude-presenced")];
    install::remove_stale_old(&dir, &names);
    let src_dir = exe.parent().unwrap_or(Path::new("."));
    for name in &names {
        let src = src_dir.join(name);
        if src.exists() {
            install::replace_binary(&src, &dir.join(name))?;
        }
    }
    println!("  copied binaries to {}", dir.display());
    if restart {
        match install::spawn_detached(&dir.join(exe_name("claude-presenced"))) {
            Ok(()) => println!("  restarted the daemon"),
            Err(e) => eprintln!("  could not restart the daemon: {e}"),
        }
    }
    Ok(dir.join(exe_name("claude-presence")))
}

#[cfg(not(windows))]
fn stable_location(exe: &Path, _service: bool) -> std::io::Result<PathBuf> {
    Ok(exe.to_path_buf())
}

fn install_cmd(args: &[String]) -> ExitCode {
    let mut init = None;
    let (mut service, mut hooks) = (true, true);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--init" => match it.next().and_then(|v| Init::parse(v)) {
                Some(i) => init = Some(i),
                None => {
                    eprintln!(
                        "--init needs one of: systemd, openrc, dinit, xdg-autostart, launchd, schtasks, run-key, none"
                    );
                    return ExitCode::from(2);
                }
            },
            "--no-service" => service = false,
            "--no-hooks" => hooks = false,
            other => {
                eprintln!("unknown option: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let exe = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot locate own executable: {e}");
            return ExitCode::FAILURE;
        }
    };
    if exe.components().any(|c| c.as_os_str() == "target") && exe.to_string_lossy().contains("debug") {
        eprintln!("note: installing from a debug build directory; consider `cargo install --path .` first");
    }
    println!("Installing claude-presence {}", env!("CARGO_PKG_VERSION"));

    // Stop a previous daemon first so its binary can be replaced.
    if service {
        for line in install::uninstall_service() {
            println!("  {line} (reinstalling)");
        }
    }
    let exe = match stable_location(&exe, service) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot copy binaries: {e}");
            return ExitCode::FAILURE;
        }
    };

    let cfg = paths::config_file();
    if !cfg.exists() {
        let r = std::fs::create_dir_all(paths::config_dir()).and_then(|_| std::fs::write(&cfg, config::DEFAULT_TOML));
        match r {
            Ok(()) => println!("  wrote default config {}", cfg.display()),
            Err(e) => eprintln!("  could not write {}: {e}", cfg.display()),
        }
    } else {
        println!("  keeping existing config {}", cfg.display());
    }

    if hooks {
        match install::install_hooks(&exe) {
            Ok(p) => println!("  wired {} hook events into {}", install::HOOK_EVENTS.len(), p.display()),
            Err(e) => {
                eprintln!("  failed to update Claude Code settings: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if service {
        let daemon = exe.with_file_name(exe_name("claude-presenced"));
        if !daemon.exists() {
            eprintln!(
                "  {} not found next to {}; build/install both binaries (cargo install --path .)",
                daemon.display(),
                exe.display()
            );
            return ExitCode::FAILURE;
        }
        let init = init.unwrap_or_else(install::detect_init);
        match install::install_service(init, &daemon) {
            Ok(msg) => println!("  {msg}"),
            Err(e) => {
                eprintln!("  service setup failed: {e}");
                return ExitCode::FAILURE;
            }
        }
        // Service managers (Task Scheduler especially) can take a few seconds.
        let sock = paths::hook_socket();
        let up = (0..20).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(250));
            ipc::daemon_running(&sock)
        });
        if up {
            println!("  daemon is running");
        } else {
            println!("  daemon not reachable yet — check `\"{}\" status` in a moment", exe.display());
        }
    }
    println!("Done. Open Claude Code with the Discord desktop app running.");
    #[cfg(windows)]
    println!("Tip: run `\"{}\" status` to check on it.", exe.display());
    ExitCode::SUCCESS
}

fn uninstall_cmd(args: &[String]) -> ExitCode {
    let purge = args.iter().any(|a| a == "--purge");
    match install::uninstall_hooks() {
        Ok(true) => println!("removed hooks from {}", paths::claude_settings().display()),
        Ok(false) => println!("no hooks to remove"),
        Err(e) => eprintln!("could not update Claude Code settings: {e}"),
    }
    for line in install::uninstall_service() {
        println!("{line}");
    }
    if purge {
        for d in [paths::config_dir(), paths::data_dir()] {
            if d.exists() && std::fs::remove_dir_all(&d).is_ok() {
                println!("deleted {}", d.display());
            }
        }
    }
    ExitCode::SUCCESS
}

fn status() -> ExitCode {
    let sock = paths::hook_socket();
    let running = ipc::daemon_running(&sock);
    let l = ledger::Ledger::load(paths::ledger_file(), paths::seen_file());
    // A reader that quit early (`status | head -1`) is not an error.
    let _ = write_status(&mut std::io::stdout().lock(), running, &sock, &l);
    if running { ExitCode::SUCCESS } else { ExitCode::from(3) }
}

/// The `status` report. `writeln!` instead of `println!`, which panics when
/// stdout is closed.
fn write_status(w: &mut impl Write, running: bool, sock: &Path, l: &ledger::Ledger) -> std::io::Result<()> {
    writeln!(w, "daemon:   {}", if running { "running" } else { "not running" })?;
    writeln!(w, "socket:   {}", sock.display())?;
    writeln!(w, "config:   {}", paths::config_file().display())?;
    writeln!(w, "stats:    {}", paths::ledger_file().display())?;
    let s = l.snapshot(timeutil::now_ms(), timeutil::local_offset_secs());
    let u = &l.totals.usage;
    writeln!(w)?;
    writeln!(
        w,
        "today:    {} active · {} prompts · {} tokens",
        timeutil::fmt_hours_ms(s.today_ms),
        s.today_prompts,
        timeutil::fmt_count(s.today_tokens)
    )?;
    writeln!(
        w,
        "lifetime: {} active · {} sessions · {} prompts · {} turns",
        timeutil::fmt_hours_ms(s.total_ms),
        s.total_sessions,
        s.total_prompts,
        l.totals.turns
    )?;
    writeln!(
        w,
        "tokens:   {} total ({} in · {} out · {} cache read · {} cache write)",
        timeutil::fmt_count(u.total()),
        timeutil::fmt_count(u.input),
        timeutil::fmt_count(u.output),
        timeutil::fmt_count(u.cache_read),
        timeutil::fmt_count(u.cache_write)
    )?;
    writeln!(w, "streak:   {} day{}", s.streak, if s.streak == 1 { "" } else { "s" })?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// stdout closed early, as in `claude-presence status | head -1`.
    struct ClosedPipe;
    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn status_survives_a_closed_stdout() {
        let dir = std::env::temp_dir().join(format!("cp-status-{}", std::process::id()));
        let l = ledger::Ledger::load(dir.join("l.json"), dir.join("s.bin"));
        let sock = Path::new("/x.sock");
        assert!(write_status(&mut ClosedPipe, false, sock, &l).is_err());
        let mut out = Vec::new();
        write_status(&mut out, true, sock, &l).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("daemon:   running\nsocket:   /x.sock\n"));
        assert!(out.contains("streak:   0 days"));
    }

    #[test]
    fn hook_never_forwards_control_events() {
        assert!(!forwardable("__shutdown"));
        assert!(!forwardable("__x"));
        assert!(forwardable("Stop"));
        assert!(forwardable("UserPromptSubmit"));
        assert!(forwardable(""));
    }
}
