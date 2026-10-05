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
    install [--init <kind>] [--no-service] [--no-hooks] [--no-path]
                     Wire Claude Code hooks, write a default config and
                     register + start the background service. <kind> is one
                     of: systemd, openrc, dinit, xdg-autostart, launchd,
                     schtasks, run-key, none (default: auto-detect). On
                     Windows, also adds the install dir to the user PATH
                     (--no-path skips that)
    uninstall [--purge]
                     Remove hooks, service and (Windows) the PATH entry
                     (--purge also deletes config and lifetime stats)
    status           Show daemon state and lifetime stats
    tui              Live dashboard: daemon, Discord card, sessions,
                     stats and config (q quits)
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
        "tui" => tui(),
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

/// The standard per-user programs folder `install` copies binaries to.
#[cfg(windows)]
fn install_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home().join("AppData").join("Local"))
        .join("Programs")
        .join(paths::APP)
}

/// Whether `exe` sits directly in `dir`. `exe` is canonical (`\\?\C:\...`
/// on Windows) and `dir` may not be, so compare canonical forms too:
/// otherwise a reinstall from the installed copy copies the running exe
/// onto itself and fails.
#[cfg_attr(not(windows), allow(dead_code))]
fn lives_in(exe: &Path, dir: &Path) -> bool {
    let Some(parent) = exe.parent() else {
        return false;
    };
    parent == dir || std::fs::canonicalize(dir).is_ok_and(|d| d == parent)
}

/// On Windows, binaries are often run from Downloads; copy them to the
/// standard per-user programs folder so hooks keep working.
///
/// A running daemon holds its .exe open. With a service, `install` already
/// stopped it (and starts the new one); with `service == false` stop it here
/// and start the new binary detached, so it isn't left stopped.
#[cfg(windows)]
fn stable_location(exe: &Path, service: bool) -> std::io::Result<PathBuf> {
    let dir = install_dir();
    if lives_in(exe, &dir) {
        // Not `exe`: hooks and the task get the plain path, not `\\?\C:\...`.
        return Ok(dir.join(exe.file_name().unwrap_or_default()));
    }
    std::fs::create_dir_all(&dir)?;
    // With a service the service manager starts the new daemon; without one,
    // restart whatever was running (a duplicate just exits "already running").
    let restart = !service && ipc::daemon_running(&paths::hook_socket());
    if !service {
        for line in install::stop_daemons() {
            println!("  {line} (to replace its binary)");
        }
    }
    let names = [exe_name("claude-presence"), exe_name("claude-presenced")];
    install::remove_stale_old(&dir, &names);
    let src_dir = exe.parent().unwrap_or(Path::new("."));
    // A failed copy is rolled back, so the binary on disk is usable either
    // way: restart before reporting the error.
    let copied = names.iter().try_for_each(|name| {
        let src = src_dir.join(name);
        if src.exists() { install::replace_binary(&src, &dir.join(name)) } else { Ok(()) }
    });
    if copied.is_ok() {
        println!("  copied binaries to {}", dir.display());
    }
    if restart {
        match install::spawn_detached(&dir.join(exe_name("claude-presenced"))) {
            Ok(()) => println!("  restarted the daemon"),
            Err(e) => eprintln!("  could not restart the daemon: {e}"),
        }
    }
    copied?;
    Ok(dir.join(exe_name("claude-presence")))
}

#[cfg(not(windows))]
fn stable_location(exe: &Path, _service: bool) -> std::io::Result<PathBuf> {
    Ok(exe.to_path_buf())
}

/// Put the install dir on the user `PATH` so `claude-presence` works from
/// new terminals. Never fatal.
#[cfg(windows)]
fn add_install_dir_to_path() {
    let dir = install_dir();
    match install::add_to_user_path(&dir) {
        Ok(install::PathEdit::Written) => {
            println!("  added {} to your user PATH (restart open terminals to pick it up)", dir.display())
        }
        Ok(install::PathEdit::Unchanged) => println!("  {} is already in your user PATH", dir.display()),
        Ok(install::PathEdit::TooLong) => eprintln!(
            "  warning: not adding {} to your user PATH: it would exceed {} characters",
            dir.display(),
            install::PATH_MAX_CHARS
        ),
        Err(e) => eprintln!("  could not update your user PATH: {e}"),
    }
}

#[cfg(windows)]
fn remove_install_dir_from_path() {
    let dir = install_dir();
    match install::remove_from_user_path(&dir) {
        Ok(install::PathEdit::Written) => {
            println!("removed {} from your user PATH (restart open terminals to pick it up)", dir.display())
        }
        // Not there: nothing to say.
        Ok(install::PathEdit::Unchanged) => {}
        // Removals never grow the value, so this is not expected; say so anyway.
        Ok(install::PathEdit::TooLong) => {
            eprintln!("warning: {} was left in your user PATH (value too long to rewrite)", dir.display())
        }
        Err(e) => eprintln!("could not update your user PATH: {e}"),
    }
}

fn install_cmd(args: &[String]) -> ExitCode {
    let mut init = None;
    let (mut service, mut hooks, mut path) = (true, true, true);
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
            "--no-path" => path = false,
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

    #[cfg(windows)]
    if path {
        add_install_dir_to_path();
    }
    #[cfg(not(windows))]
    let _ = path;

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
    #[cfg(windows)]
    remove_install_dir_from_path();
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
    let db = paths::ledger_file();
    let stats = ledger::load_stats(&db);
    // A reader that quit early (`status | head -1`) is not an error.
    let _ = write_status(&mut std::io::stdout().lock(), running, &sock, &db, &stats);
    if running { ExitCode::SUCCESS } else { ExitCode::from(3) }
}

#[cfg(feature = "tui")]
fn tui() -> ExitCode {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        eprintln!("tui needs a terminal; use `claude-presence status` for plain output");
        return ExitCode::from(2);
    }
    match claude_presence::tui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tui: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "tui"))]
fn tui() -> ExitCode {
    eprintln!("this build has no tui (built without the `tui` feature)");
    ExitCode::from(2)
}

/// The `status` report. `writeln!` instead of `println!`, which panics when
/// stdout is closed.
fn write_status(
    w: &mut impl Write,
    running: bool,
    sock: &Path,
    db: &Path,
    stats: &std::io::Result<ledger::Ledger>,
) -> std::io::Result<()> {
    writeln!(w, "daemon:   {}", if running { "running" } else { "not running" })?;
    writeln!(w, "socket:   {}", sock.display())?;
    writeln!(w, "config:   {}", paths::config_file().display())?;
    writeln!(w, "stats:    {}", db.display())?;
    writeln!(w)?;
    let l = match stats {
        Ok(l) => l,
        // Busy (being saved) or unreadable: zeros would look like lost stats.
        Err(e) => {
            writeln!(w, "stats unavailable: {e}")?;
            return w.flush();
        }
    };
    let s = l.snapshot(timeutil::now_ms(), timeutil::local_offset_secs());
    let u = &l.totals.usage;
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
    fn exe_in_install_dir_is_recognized() {
        let dir = std::env::temp_dir().join(format!("cp-lives-in-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // `install` canonicalizes its own path (`\\?\C:\...` on Windows); the
        // install dir it compares against is not canonical.
        let exe = std::fs::canonicalize(&dir).unwrap().join(exe_name("claude-presence"));
        assert!(lives_in(&exe, &dir));
        assert!(lives_in(&dir.join("x"), &dir));
        assert!(!lives_in(&exe, &dir.join("sub")));
        assert!(!lives_in(&exe, &std::env::temp_dir()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_survives_a_closed_stdout() {
        let dir = std::env::temp_dir().join(format!("cp-status-{}", std::process::id()));
        let db = dir.join("l.db");
        let l = ledger::load_stats(&db);
        let sock = Path::new("/x.sock");
        assert!(write_status(&mut ClosedPipe, false, sock, &db, &l).is_err());
        let mut out = Vec::new();
        write_status(&mut out, true, sock, &db, &l).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("daemon:   running\nsocket:   /x.sock\n"));
        assert!(out.contains(&format!("stats:    {}\n", db.display())));
        assert!(out.contains("streak:   0 days"));
    }

    #[test]
    fn status_says_when_stats_are_unavailable() {
        let busy = Err(std::io::Error::other("database is locked"));
        let mut out = Vec::new();
        write_status(&mut out, true, Path::new("/x.sock"), Path::new("/l.db"), &busy).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("stats unavailable: database is locked"));
        assert!(!out.contains("lifetime:"), "no zeros instead");
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
