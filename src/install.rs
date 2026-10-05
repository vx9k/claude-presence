//! `install` / `uninstall`: wire Claude Code hooks into `settings.json` and
//! register the daemon as a per-user background service.
//!
//!   Linux:   systemd user unit, OpenRC user service, dinit user service,
//!            or an XDG autostart entry when no supported init is found
//!   macOS:   launchd LaunchAgent
//!   Windows: Task Scheduler logon task (falls back to the HKCU Run key)

use crate::{ipc, paths};
use serde_json::{Map, Value, json};
#[cfg(any(windows, test))]
use std::borrow::Cow;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "PreCompact",
    "Stop",
    "SubagentStop",
    "SessionEnd",
];

/// Marker that identifies hook commands we own.
const MARK: &str = "claude-presence";

pub const SERVICE: &str = "claude-presence";
#[cfg(target_os = "macos")]
pub const LAUNCHD_LABEL: &str = "io.github.vx9k.claude-presence";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Init {
    Systemd,
    OpenRc,
    Dinit,
    XdgAutostart,
    Launchd,
    TaskScheduler,
    RunKey,
    None,
}

impl Init {
    pub fn parse(s: &str) -> Option<Init> {
        Some(match s {
            "systemd" => Init::Systemd,
            "openrc" => Init::OpenRc,
            "dinit" => Init::Dinit,
            "xdg" | "xdg-autostart" | "autostart" => Init::XdgAutostart,
            "launchd" => Init::Launchd,
            "schtasks" | "task-scheduler" => Init::TaskScheduler,
            "run-key" | "registry" => Init::RunKey,
            "none" => Init::None,
            _ => return None,
        })
    }
}

// ------------------------------------------------------------ settings.json --

fn quote(p: &Path) -> String {
    // Claude Code runs hooks through a shell (bash on Windows too), where
    // forward slashes are safe and backslashes are not.
    let s = p.to_string_lossy();
    #[cfg(windows)]
    let s = s.replace('\\', "/");
    // Single quotes: bash expands `$` and backticks inside double quotes.
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn hook_command(exe: &Path, event: &str) -> String {
    format!("{} hook {event}", quote(exe))
}

fn is_ours(h: &Value) -> bool {
    h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains(MARK) && c.contains(" hook "))
}

/// Remove our hook entries from a settings value, dropping emptied groups.
/// Removes our hook entries; true if there were any.
fn strip_hooks(settings: &mut Value) -> bool {
    let Some(obj) = settings.as_object_mut() else { return false };
    let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut removed = false;
    hooks.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else { return true };
        for g in groups.iter_mut() {
            if let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                let n = list.len();
                list.retain(|h| !is_ours(h));
                removed |= list.len() != n;
            }
        }
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|l| !l.is_empty()));
        !groups.is_empty()
    });
    if hooks.is_empty() {
        obj.remove("hooks");
    }
    removed
}

fn parse(settings: &str) -> io::Result<Value> {
    if settings.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let v: Value = serde_json::from_str(settings)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("settings.json: {e}")))?;
    if !v.is_object() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "settings.json is not a JSON object"));
    }
    Ok(v)
}

fn pretty(v: &Value) -> io::Result<String> {
    let mut out = serde_json::to_string_pretty(v).map_err(io::Error::other)?;
    out.push('\n');
    Ok(out)
}

/// Return `settings` with our hooks (re)installed for every event. Other
/// keys and hooks are preserved in their original order.
pub fn wire_hooks(settings: &str, exe: &Path) -> io::Result<String> {
    let mut v = parse(settings)?;
    strip_hooks(&mut v);
    let obj = v.as_object_mut().expect("object");
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "\"hooks\" in settings.json is not an object"))?;
    for ev in HOOK_EVENTS {
        let entry = json!({ "matcher": "", "hooks": [ { "type": "command", "command": hook_command(exe, ev), "timeout": 5 } ] });
        match hooks.entry(*ev).or_insert_with(|| Value::Array(Vec::new())) {
            Value::Array(groups) => groups.push(entry),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("hooks.{ev} in settings.json is not an array"),
                ));
            }
        }
    }
    pretty(&v)
}

/// The settings without our hooks, or `None` if there were none (so a file
/// we never touched isn't reformatted).
pub fn unwire_hooks(settings: &str) -> io::Result<Option<String>> {
    let mut v = parse(settings)?;
    if !strip_hooks(&mut v) {
        return Ok(None);
    }
    pretty(&v).map(Some)
}

fn write_atomic(path: &Path, data: &str) -> io::Result<()> {
    // Write through a symlink (dotfile managers) and keep the file's mode.
    let path = &fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp-claude-presence");
    fs::write(&tmp, data)?;
    if let Ok(m) = fs::metadata(path) {
        fs::set_permissions(&tmp, m.permissions())?;
    }
    fs::rename(&tmp, path)
}

pub fn install_hooks(exe: &Path) -> io::Result<PathBuf> {
    let path = paths::claude_settings();
    let current = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if !current.is_empty() {
        let backup = path.with_extension("json.bak");
        if !backup.exists() {
            fs::write(&backup, &current)?;
        }
    }
    write_atomic(&path, &wire_hooks(&current, exe)?)?;
    Ok(path)
}

pub fn uninstall_hooks() -> io::Result<bool> {
    let path = paths::claude_settings();
    let current = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let Some(next) = unwire_hooks(&current)? else { return Ok(false) };
    write_atomic(&path, &next)?;
    Ok(true)
}

// ----------------------------------------------------------------- services --

fn run(cmd: &str, args: &[&str]) -> bool {
    match Command::new(cmd).args(args).output() {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let err = err.trim();
            if !err.is_empty() {
                eprintln!("  {cmd} {}: {err}", args.join(" "));
            }
            false
        }
        Err(_) => false,
    }
}

/// Like [`run`], but silent: for probes and cleanups where "not found" is expected.
#[cfg(windows)]
fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok_and(|o| o.status.success())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn in_path(bin: &str) -> bool {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file())).unwrap_or(false)
}

/// Best guess at the user's init system / service manager.
pub fn detect_init() -> Init {
    #[cfg(target_os = "macos")]
    return Init::Launchd;
    #[cfg(windows)]
    return Init::TaskScheduler;
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let pid1 = fs::read_to_string("/proc/1/comm").unwrap_or_default();
        let pid1 = pid1.trim();
        if Path::new("/run/systemd/system").is_dir() {
            return Init::Systemd;
        }
        if pid1 == "dinit" || (in_path("dinitctl") && !in_path("rc-service")) {
            return Init::Dinit;
        }
        if Path::new("/run/openrc").is_dir() || in_path("openrc") {
            return Init::OpenRc;
        }
        Init::XdgAutostart
    }
}

#[cfg(unix)]
fn config_home() -> PathBuf {
    #[cfg(target_os = "macos")]
    return paths::home().join(".config");
    #[cfg(not(target_os = "macos"))]
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home().join(".config"))
}

pub fn systemd_unit(daemon: &Path) -> String {
    format!(
        "[Unit]
Description=Discord Rich Presence for Claude Code
Documentation=https://github.com/vx9k/claude-presence

[Service]
Type=simple
ExecStart={exec}
Restart=on-failure
RestartSec=5
Nice=10
IOSchedulingClass=idle
# Hardening that works without user namespaces.
NoNewPrivileges=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
SystemCallArchitectures=native

[Install]
WantedBy=default.target
",
        exec = systemd_escape(daemon)
    )
}

fn systemd_escape(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.contains([' ', '"', '\\']) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.into_owned()
    }
}

pub fn openrc_script(daemon: &Path) -> String {
    format!(
        "#!/sbin/openrc-run
# OpenRC user service (OpenRC >= 0.60): rc-update --user add {SERVICE} default
description=\"Discord Rich Presence for Claude Code\"

supervisor=supervise-daemon
command=\"{}\"
respawn_delay=5
respawn_max=0
",
        daemon.to_string_lossy().replace('"', "\\\"")
    )
}

pub fn dinit_service(daemon: &Path) -> String {
    format!(
        "# dinit user service: dinitctl enable {SERVICE}
type = process
command = {}
restart = true
restart-delay = 5
smooth-recovery = true
",
        {
            let s = daemon.to_string_lossy();
            if s.contains(' ') { format!("\"{s}\"") } else { s.into_owned() }
        }
    )
}

pub fn xdg_desktop(daemon: &Path) -> String {
    format!(
        "[Desktop Entry]
Type=Application
Name=claude-presence
Comment=Discord Rich Presence for Claude Code
Exec=\"{}\"
Terminal=false
NoDisplay=true
X-GNOME-Autostart-enabled=true
",
        daemon.to_string_lossy()
    )
}

pub fn launchd_plist(daemon: &Path, log: &Path) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Background</string>
  <key>LowPriorityIO</key>
  <true/>
  <key>Nice</key>
  <integer>10</integer>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = "io.github.vx9k.claude-presence",
        exe = esc(&daemon.to_string_lossy()),
        log = esc(&log.to_string_lossy()),
    )
}

pub fn task_xml(daemon: &Path, user: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Discord Rich Presence for Claude Code</Description>
    <URI>\{SERVICE}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
    </Exec>
  </Actions>
</Task>
"#,
        user = esc(user),
        exe = esc(&daemon.to_string_lossy()),
    )
}

/// Start the daemon right now, detached from this terminal.
pub fn spawn_detached(daemon: &Path) -> io::Result<()> {
    let mut cmd = Command::new(daemon);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd.spawn().map(|_| ())
}

/// Register + start the service. Returns a human description of what was done.
pub fn install_service(init: Init, daemon: &Path) -> io::Result<String> {
    match init {
        Init::None => Ok("no service installed (--init none)".into()),
        #[cfg(unix)]
        Init::Systemd => {
            let dir = config_home().join("systemd/user");
            fs::create_dir_all(&dir)?;
            let unit = dir.join(format!("{SERVICE}.service"));
            fs::write(&unit, systemd_unit(daemon))?;
            run("systemctl", &["--user", "daemon-reload"]);
            // `restart` (not `start`) so a reinstall picks up a new binary.
            let ok = run("systemctl", &["--user", "enable", &format!("{SERVICE}.service")])
                && run("systemctl", &["--user", "restart", &format!("{SERVICE}.service")]);
            Ok(if ok {
                format!("systemd user service enabled and started ({})", unit.display())
            } else {
                format!("wrote {} — enable it with: systemctl --user enable --now {SERVICE}", unit.display())
            })
        }
        #[cfg(unix)]
        Init::OpenRc => {
            use std::os::unix::fs::PermissionsExt;
            let dir = config_home().join("rc/init.d");
            fs::create_dir_all(&dir)?;
            let script = dir.join(SERVICE);
            fs::write(&script, openrc_script(daemon))?;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
            let ok = run("rc-update", &["--user", "add", SERVICE, "default"])
                && (run("rc-service", &["--user", SERVICE, "restart"])
                    || run("rc-service", &["--user", SERVICE, "start"]));
            Ok(if ok {
                format!("OpenRC user service added to the default runlevel and started ({})", script.display())
            } else {
                format!(
                    "wrote {} — enable it with: rc-update --user add {SERVICE} default && rc-service --user {SERVICE} start \
                     (needs OpenRC >= 0.60 with user services set up)",
                    script.display()
                )
            })
        }
        #[cfg(unix)]
        Init::Dinit => {
            let dir = config_home().join("dinit.d");
            fs::create_dir_all(&dir)?;
            let svc = dir.join(SERVICE);
            fs::write(&svc, dinit_service(daemon))?;
            run("dinitctl", &["stop", SERVICE]);
            let ok = run("dinitctl", &["enable", SERVICE]) || run("dinitctl", &["start", SERVICE]);
            Ok(if ok {
                format!("dinit user service enabled and started ({})", svc.display())
            } else {
                format!(
                    "wrote {} — enable it with: dinitctl enable {SERVICE} (needs a running user dinit instance)",
                    svc.display()
                )
            })
        }
        #[cfg(unix)]
        Init::XdgAutostart => {
            let dir = config_home().join("autostart");
            fs::create_dir_all(&dir)?;
            let file = dir.join(format!("{SERVICE}.desktop"));
            fs::write(&file, xdg_desktop(daemon))?;
            spawn_detached(daemon)?;
            Ok(format!("XDG autostart entry written ({}) and daemon started", file.display()))
        }
        #[cfg(target_os = "macos")]
        Init::Launchd => {
            let dir = paths::home().join("Library/LaunchAgents");
            fs::create_dir_all(&dir)?;
            let plist = dir.join(format!("{LAUNCHD_LABEL}.plist"));
            let log = paths::home().join("Library/Logs/claude-presence.log");
            fs::write(&plist, launchd_plist(daemon, &log))?;
            // SAFETY: getuid never fails.
            let domain = format!("gui/{}", unsafe { libc::getuid() });
            run("launchctl", &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
            let ok = run("launchctl", &["bootstrap", &domain, &plist.to_string_lossy()]);
            Ok(if ok {
                format!("LaunchAgent loaded ({})", plist.display())
            } else {
                format!("wrote {} — load it with: launchctl bootstrap {domain} {}", plist.display(), plist.display())
            })
        }
        #[cfg(windows)]
        Init::TaskScheduler => {
            let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
                (Ok(d), Ok(u)) if !d.is_empty() => format!("{d}\\{u}"),
                (_, Ok(u)) => u,
                _ => return install_service(Init::RunKey, daemon),
            };
            let xml = task_xml(daemon, &user);
            // schtasks wants UTF-16 with a BOM when the XML says so.
            let mut bytes = vec![0xFF, 0xFE];
            for u in xml.encode_utf16() {
                bytes.extend_from_slice(&u.to_le_bytes());
            }
            let tmp = std::env::temp_dir().join("claude-presence-task.xml");
            fs::write(&tmp, bytes)?;
            run_quiet("schtasks", &["/End", "/TN", SERVICE]);
            let ok = run("schtasks", &["/Create", "/TN", SERVICE, "/XML", &tmp.to_string_lossy(), "/F"]);
            let _ = fs::remove_file(&tmp);
            if !ok {
                eprintln!("  Task Scheduler registration failed; using the Run registry key instead");
                return install_service(Init::RunKey, daemon);
            }
            run_quiet("reg", &["delete", RUN_KEY, "/v", SERVICE, "/f"]);
            if !run("schtasks", &["/Run", "/TN", SERVICE]) {
                spawn_detached(daemon)?;
            }
            Ok(format!("scheduled task \"{SERVICE}\" registered (runs at logon, restarts on failure) and started"))
        }
        #[cfg(windows)]
        Init::RunKey => {
            let value = format!("\"{}\"", daemon.display());
            if !run("reg", &["add", RUN_KEY, "/v", SERVICE, "/t", "REG_SZ", "/d", &value, "/f"]) {
                return Err(io::Error::other("could not write the Run registry key"));
            }
            spawn_detached(daemon)?;
            Ok("added to HKCU\\...\\Run (starts at logon) and started".into())
        }
        #[allow(unreachable_patterns)]
        other => {
            Err(io::Error::new(io::ErrorKind::Unsupported, format!("{other:?} is not available on this platform")))
        }
    }
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Ask a daemon listening at `addr` to shut down and wait up to `wait` for
/// it to go away. `None` if none was running, else whether it stopped.
fn stop_daemon(addr: &Path, wait: Duration) -> Option<bool> {
    ipc::send(addr, &ipc::shutdown_request()).ok()?;
    let deadline = Instant::now() + wait;
    loop {
        if !ipc::daemon_running(addr) {
            return Some(true);
        }
        if Instant::now() >= deadline {
            return Some(false);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// True for the errors copying over a running program's `.exe` gives on
/// Windows: access denied, sharing or lock violation.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_locked(e: &io::Error) -> bool {
    // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
    e.kind() == io::ErrorKind::PermissionDenied || (cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)))
}

/// Run `op` up to `attempts` times, `delay` apart, while it fails because
/// the target is locked (`is_locked`); any other outcome is returned at once.
#[cfg_attr(not(windows), allow(dead_code))]
fn copy_with_retry<T>(mut op: impl FnMut() -> io::Result<T>, attempts: u32, delay: Duration) -> io::Result<T> {
    let mut left = attempts.max(1);
    loop {
        match op() {
            Err(e) if is_locked(&e) && left > 1 => {
                left -= 1;
                std::thread::sleep(delay);
            }
            r => return r,
        }
    }
}

/// `<dst>.old`: where a locked binary is moved aside.
#[cfg_attr(not(windows), allow(dead_code))]
fn old_path(dst: &Path) -> PathBuf {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    dst.with_file_name(name)
}

/// `replace_binary` with the copy as a closure. If the copy fails after
/// `dst` was moved aside, it is moved back.
#[cfg_attr(not(windows), allow(dead_code))]
fn replace_with(
    src: &Path,
    dst: &Path,
    attempts: u32,
    delay: Duration,
    mut copy: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    // fs::copy errors don't say which side failed: a source we can't read
    // must not be mistaken for a locked target and get it moved away.
    drop(fs::File::open(src)?);
    match copy_with_retry(&mut copy, attempts, delay) {
        Err(e) if is_locked(&e) && dst.exists() => {
            let old = old_path(dst);
            let _ = fs::remove_file(&old);
            // A failed rename (say, an older `.old` still running) reports the original error.
            fs::rename(dst, &old).map_err(|_| e)?;
            copy().inspect_err(|_| {
                // Never leave the hooks without a binary.
                let _ = fs::remove_file(dst);
                let _ = fs::rename(&old, dst);
            })
        }
        r => r,
    }
}

/// Remove `<name>.old` files a previous install left in `dir`. Silent: one
/// may still be in use by an old daemon.
pub fn remove_stale_old(dir: &Path, names: &[String]) {
    for n in names {
        let _ = fs::remove_file(old_path(&dir.join(n)));
    }
}

/// Copy `src` over `dst`, which a daemon that is still exiting may hold
/// open: retry for up to 5 s, then move `dst` aside to `<name>.old`
/// (Windows lets a running `.exe` be renamed, not overwritten) and copy.
#[cfg(windows)]
pub fn replace_binary(src: &Path, dst: &Path) -> io::Result<()> {
    replace_with(src, dst, 20, Duration::from_millis(250), || fs::copy(src, dst).map(|_| ()))
}

/// Ask a running daemon (at today's address and the legacy one) to shut
/// down, so it saves its stats and releases its binary.
pub fn stop_daemons() -> Vec<String> {
    let mut done = Vec::new();
    let sock = paths::hook_socket();
    // TODO: drop the legacy path a couple of releases after the private
    // socket dir shipped.
    // No `exists()` probe: on Windows it would open (connect to) the pipe.
    let legacy = paths::legacy_hook_socket().filter(|old| *old != sock);
    for addr in std::iter::once(sock.clone()).chain(legacy) {
        match stop_daemon(&addr, Duration::from_secs(2)) {
            Some(true) => done.push("stopped the running daemon".into()),
            Some(false) => crate::warn!(
                "the running daemon has not stopped after 2 s (still busy, or a version without __shutdown)"
            ),
            None => {}
        }
    }
    done
}

/// Stop the running daemon and remove every service flavor we might have
/// installed.
pub fn uninstall_service() -> Vec<String> {
    // Asked first so it saves stats and clears the card itself, and so
    // daemons no service manager tracks (XDG autostart, the Run key, a
    // detached spawn) stop too. systemd, launchd and Task Scheduler don't
    // restart a clean exit; OpenRC and dinit respawn after 5 s, but are
    // stopped right below.
    let mut done = stop_daemons();
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let ch = config_home();
        let unit = ch.join(format!("systemd/user/{SERVICE}.service"));
        if unit.exists() {
            run("systemctl", &["--user", "disable", "--now", &format!("{SERVICE}.service")]);
            let _ = fs::remove_file(&unit);
            run("systemctl", &["--user", "daemon-reload"]);
            done.push(format!("removed {}", unit.display()));
        }
        let rc = ch.join("rc/init.d").join(SERVICE);
        if rc.exists() {
            run("rc-service", &["--user", SERVICE, "stop"]);
            run("rc-update", &["--user", "del", SERVICE, "default"]);
            let _ = fs::remove_file(&rc);
            done.push(format!("removed {}", rc.display()));
        }
        let di = ch.join("dinit.d").join(SERVICE);
        if di.exists() {
            run("dinitctl", &["disable", SERVICE]);
            run("dinitctl", &["stop", SERVICE]);
            let _ = fs::remove_file(&di);
            done.push(format!("removed {}", di.display()));
        }
        let xdg = ch.join(format!("autostart/{SERVICE}.desktop"));
        if xdg.exists() {
            let _ = fs::remove_file(&xdg);
            done.push(format!("removed {}", xdg.display()));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let plist = paths::home().join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist"));
        if plist.exists() {
            // SAFETY: getuid never fails.
            let domain = format!("gui/{}", unsafe { libc::getuid() });
            run("launchctl", &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
            let _ = fs::remove_file(&plist);
            done.push(format!("removed {}", plist.display()));
        }
    }
    #[cfg(windows)]
    {
        if run_quiet("schtasks", &["/Query", "/TN", SERVICE]) {
            run_quiet("schtasks", &["/End", "/TN", SERVICE]);
            run("schtasks", &["/Delete", "/TN", SERVICE, "/F"]);
            done.push(format!("removed scheduled task \"{SERVICE}\""));
        }
        if run_quiet("reg", &["delete", RUN_KEY, "/v", SERVICE, "/f"]) {
            done.push("removed Run registry key".into());
        }
    }
    done
}

// ---------------------------------------------------------------- user PATH --

/// Whether two `PATH` entries name the same directory: compared trimmed,
/// without surrounding quotes, with `%vars%` expanded by `expand`,
/// case-insensitively and without trailing slashes. Entries are only ever
/// written verbatim.
#[cfg(any(windows, test))]
fn same_path_entry(a: &str, b: &str, expand: Expand<'_>) -> bool {
    fn unquote(s: &str) -> &str {
        let s = s.trim();
        s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s).trim()
    }
    fn norm(s: &str) -> impl Iterator<Item = char> + '_ {
        s.trim_end_matches(['\\', '/']).chars().flat_map(char::to_lowercase)
    }
    let (a, b) = (expand(unquote(a)), expand(unquote(b)));
    norm(&a).eq(norm(&b))
}

/// Expands `%VAR%` references in a `PATH` entry, for comparison only.
#[cfg(any(windows, test))]
pub type Expand<'a> = &'a dyn Fn(&str) -> Cow<'_, str>;

/// `cur` with `dir` appended, or `None` if it is already there.
#[cfg(any(windows, test))]
pub fn add_path_entry(cur: &str, dir: &str, expand: Expand<'_>) -> Option<String> {
    if dir.trim().trim_end_matches(['\\', '/']).is_empty() || cur.split(';').any(|e| same_path_entry(e, dir, expand)) {
        return None;
    }
    let base = cur.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    Some(if base.is_empty() { dir.to_owned() } else { format!("{base};{dir}") })
}

/// `cur` without the entries naming `dir`, or `None` if there are none.
/// Every other entry is kept verbatim.
#[cfg(any(windows, test))]
pub fn remove_path_entry(cur: &str, dir: &str, expand: Expand<'_>) -> Option<String> {
    if !cur.split(';').any(|e| same_path_entry(e, dir, expand)) {
        return None;
    }
    Some(cur.split(';').filter(|e| !same_path_entry(e, dir, expand)).collect::<Vec<_>>().join(";"))
}

/// Longest user `PATH` we write, in UTF-16 units: past 2047 characters
/// Windows tools start to truncate or refuse it.
pub const PATH_MAX_CHARS: usize = 2047;

#[cfg(any(windows, test))]
fn path_too_long(path: &str) -> bool {
    path.encode_utf16().count() > PATH_MAX_CHARS
}

/// Whether replacing `cur` with `new` is refused for length: only growth is,
/// so a removal from an already overlong value still goes through.
#[cfg(any(windows, test))]
fn refuse_as_too_long(cur: &str, new: &str) -> bool {
    path_too_long(new) && new.encode_utf16().count() > cur.encode_utf16().count()
}

/// Whether a `PATH` value has no entries left (only separators or blanks).
#[cfg(any(windows, test))]
fn path_value_is_empty(v: &str) -> bool {
    v.chars().all(|c| c == ';' || c.is_whitespace())
}

/// Outcome of editing the user `PATH`.
#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
pub enum PathEdit {
    /// Already as wanted; nothing written.
    Unchanged,
    /// Written. Running programs (open terminals) keep their old copy.
    Written,
    /// Skipped: the new value would grow past [`PATH_MAX_CHARS`] (removals,
    /// which only shrink it, are never refused).
    TooLong,
}

/// Append `dir` to the user `PATH` (`HKCU\Environment\Path`) unless present.
#[cfg(windows)]
pub fn add_to_user_path(dir: &Path) -> io::Result<PathEdit> {
    let dir = dir.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path is not valid Unicode"))?;
    edit_user_path(|cur| add_path_entry(cur, dir, &expand_env))
}

/// Remove `dir` from the user `PATH`, leaving every other entry as it is.
#[cfg(windows)]
pub fn remove_from_user_path(dir: &Path) -> io::Result<PathEdit> {
    let dir = dir.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path is not valid Unicode"))?;
    edit_user_path(|cur| remove_path_entry(cur, dir, &expand_env))
}

/// `%VAR%` references expanded from this process's environment; `s` itself
/// if it has none or expansion fails.
#[cfg(windows)]
fn expand_env(s: &str) -> Cow<'_, str> {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    if !s.contains('%') {
        return Cow::Borrowed(s);
    }
    let src: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    let mut buf = vec![0u16; src.len() + 64];
    loop {
        // SAFETY: `src` is NUL-terminated and `buf` has room for the length passed.
        let n = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return Cow::Borrowed(s);
        }
        if n <= buf.len() {
            // `n` counts the terminator.
            return Cow::Owned(String::from_utf16_lossy(&buf[..n - 1]));
        }
        buf.resize(n, 0);
    }
}

/// Read `HKCU\Environment\Path`, apply `edit` and write the result back
/// with the value's own type (`REG_SZ` or `REG_EXPAND_SZ`; a missing value
/// is created as `REG_EXPAND_SZ`; one left with no entries is deleted).
/// `%vars%` are never expanded in what is written. Only the per-user key is
/// touched, never the machine-wide one.
#[cfg(windows)]
fn edit_user_path(edit: impl FnOnce(&str) -> Option<String>) -> io::Result<PathEdit> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegDeleteValueW,
        RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    fn check(r: u32) -> io::Result<()> {
        if r == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(r as i32)) }
    }
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the key was opened by RegOpenKeyExW and is closed only here.
            unsafe { RegCloseKey(self.0) };
        }
    }

    let (subkey, name) = (wide("Environment"), wide("Path"));
    let mut raw: HKEY = std::ptr::null_mut();
    // SAFETY: `subkey` is NUL-terminated and `raw` is a valid out pointer.
    check(unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_QUERY_VALUE | KEY_SET_VALUE, &mut raw) })?;
    let key = Key(raw);

    let mut ty = 0;
    let mut buf: Vec<u16> = Vec::new();
    let found = loop {
        let mut bytes = (buf.len() * 2) as u32;
        let data = if buf.is_empty() { std::ptr::null_mut() } else { buf.as_mut_ptr().cast() };
        // SAFETY: `name` is NUL-terminated; `data` is null (size query) or
        // points to `bytes` writable bytes; `ty` and `bytes` are valid out pointers.
        let r = unsafe { RegQueryValueExW(key.0, name.as_ptr(), std::ptr::null(), &mut ty, data, &mut bytes) };
        match r {
            ERROR_FILE_NOT_FOUND => break false,
            ERROR_SUCCESS if !buf.is_empty() || bytes == 0 => {
                buf.truncate(bytes as usize / 2);
                break true;
            }
            // Size known (or the value grew meanwhile): make room and read again.
            ERROR_SUCCESS | ERROR_MORE_DATA => buf.resize((bytes as usize).div_ceil(2) + 1, 0),
            r => return Err(io::Error::from_raw_os_error(r as i32)),
        }
    };
    if !found {
        ty = REG_EXPAND_SZ;
    } else if ty != REG_SZ && ty != REG_EXPAND_SZ {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "HKCU\\Environment\\Path is not a string value"));
    }
    // The stored data may or may not include its terminator.
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let cur = String::from_utf16(&buf[..end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "HKCU\\Environment\\Path is not valid UTF-16"))?;

    let Some(new) = edit(&cur) else {
        return Ok(PathEdit::Unchanged);
    };
    if refuse_as_too_long(&cur, &new) {
        return Ok(PathEdit::TooLong);
    }
    if path_value_is_empty(&new) {
        // Nothing left: delete the value rather than leave an empty one.
        if found {
            // SAFETY: `name` is NUL-terminated; the key was opened with KEY_SET_VALUE.
            check(unsafe { RegDeleteValueW(key.0, name.as_ptr()) })?;
        }
    } else {
        let data = wide(&new);
        // SAFETY: `data` is NUL-terminated and the length passed is its size in
        // bytes, terminator included, as REG_SZ / REG_EXPAND_SZ require.
        check(unsafe { RegSetValueExW(key.0, name.as_ptr(), 0, ty, data.as_ptr().cast(), (data.len() * 2) as u32) })?;
    }
    drop(key);
    broadcast_environment_change();
    Ok(PathEdit::Written)
}

/// Tell running programs (Explorer, so new terminals) that the environment
/// changed. Best effort: hung windows are skipped, failures ignored.
#[cfg(windows)]
fn broadcast_environment_change() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };
    let area: Vec<u16> = "Environment".encode_utf16().chain(Some(0)).collect();
    let mut result = 0;
    // SAFETY: `area` is a NUL-terminated string that outlives the call (the
    // system marshals WM_SETTINGCHANGE strings to other processes), and
    // `result` is a valid out pointer.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            area.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            1000,
            &mut result,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_expand(s: &str) -> Cow<'_, str> {
        Cow::Borrowed(s)
    }
    const NO_EXPAND: Expand<'static> = &no_expand;

    #[test]
    fn hook_command_is_single_quoted() {
        // bash expands `$` and backticks inside double quotes.
        assert_eq!(hook_command(Path::new("/home/jo$h/it's/cp"), "Stop"), r"'/home/jo$h/it'\''s/cp' hook Stop");
    }

    #[test]
    fn unwire_leaves_foreign_settings_alone() {
        let s =
            "{\n    \"hooks\": {\"Stop\": [{\"hooks\": [{\"type\": \"command\", \"command\": \"notify-send\"}]}]}\n}";
        assert_eq!(unwire_hooks(s).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_keeps_symlinks_and_mode() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("cp-install-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (real, link) = (dir.join("real.json"), dir.join("settings.json"));
        fs::write(&real, "{}").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&real, &link).unwrap();
        write_atomic(&link, "{\"a\":1}").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "{\"a\":1}");
        assert_eq!(fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_entries_compare_unquoted_and_expanded() {
        let dir = r"C:\Users\me\AppData\Local\Programs\claude-presence";
        fn expand(s: &str) -> Cow<'_, str> {
            if s.contains("%LOCALAPPDATA%") {
                Cow::Owned(s.replace("%LOCALAPPDATA%", r"C:\Users\me\AppData\Local"))
            } else {
                Cow::Borrowed(s)
            }
        }
        for cur in [
            format!(r#"C:\a;"{dir}""#),
            format!(r#"C:\a; "{dir}\" "#),
            r"C:\a;%LOCALAPPDATA%\Programs\claude-presence".to_owned(),
            r#""%LOCALAPPDATA%\Programs\claude-presence\";C:\b"#.to_owned(),
        ] {
            assert_eq!(add_path_entry(&cur, dir, &expand), None, "{cur}");
        }
        // Removal drops the matching entry and keeps the rest verbatim.
        assert_eq!(
            remove_path_entry(r#"%X%\b;"%LOCALAPPDATA%\Programs\claude-presence";C:\b"#, dir, &expand).as_deref(),
            Some(r"%X%\b;C:\b")
        );
        // An unknown variable is not a match.
        assert!(add_path_entry(r"%NOPE%\Programs\claude-presence", dir, &expand).is_some());
        // A lone quote is not stripped as a pair.
        assert!(add_path_entry(&format!("\"{dir}"), dir, NO_EXPAND).is_some());
        // Added entries are written verbatim.
        assert_eq!(add_path_entry(r"%X%\b", dir, &expand), Some(format!(r"%X%\b;{dir}")));
    }

    #[test]
    fn path_cap_applies_only_to_growth() {
        let long = "a".repeat(PATH_MAX_CHARS + 10);
        let longer = format!("{long};b");
        assert!(refuse_as_too_long(&long, &longer), "an addition past the cap is refused");
        assert!(!refuse_as_too_long(&longer, &long), "a removal only shrinks: always allowed");
        assert!(!refuse_as_too_long("a", "a;b"));
        assert!(refuse_as_too_long("", &"a".repeat(PATH_MAX_CHARS + 1)));
    }

    #[test]
    fn empty_path_values() {
        for v in ["", ";", " ; ;", "\t"] {
            assert!(path_value_is_empty(v), "{v:?}");
        }
        for v in ["a", ";a;", " %X% "] {
            assert!(!path_value_is_empty(v), "{v:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_expand_env() {
        assert_eq!(expand_env(r"C:\plain"), r"C:\plain");
        let windir = std::env::var("SystemRoot").unwrap();
        assert_eq!(expand_env(r"%SystemRoot%\x"), format!(r"{windir}\x"));
        assert_eq!(expand_env(r"%CP_SURELY_UNSET_VAR%\x"), r"%CP_SURELY_UNSET_VAR%\x");
        let long = format!("%SystemRoot%{}", "y".repeat(500));
        assert_eq!(expand_env(&long), format!("{windir}{}", "y".repeat(500)));
    }

    #[test]
    fn path_entry_is_added_once() {
        let dir = r"C:\Users\me\AppData\Local\Programs\claude-presence";
        assert_eq!(add_path_entry("", dir, NO_EXPAND).as_deref(), Some(dir));
        assert_eq!(add_path_entry("  ", dir, NO_EXPAND).as_deref(), Some(dir));
        assert_eq!(add_path_entry(r"C:\a", dir, NO_EXPAND), Some(format!(r"C:\a;{dir}")));
        // No empty segment from a trailing separator.
        assert_eq!(add_path_entry(r"C:\a;", dir, NO_EXPAND), Some(format!(r"C:\a;{dir}")));
        assert_eq!(add_path_entry(r"C:\a;; ", dir, NO_EXPAND), Some(format!(r"C:\a;{dir}")));
        // %vars% are kept as written, never expanded.
        assert_eq!(add_path_entry(r"%USERPROFILE%\bin", dir, NO_EXPAND), Some(format!(r"%USERPROFILE%\bin;{dir}")));
        // Already present: case, surrounding blanks and a trailing slash don't matter.
        for cur in [
            dir.to_owned(),
            format!(r"C:\a;{dir};C:\b"),
            format!(r"C:\a; {} ", dir.to_uppercase()),
            format!(r"{dir}\;C:\b"),
            format!("C:\\a;{dir}/"),
        ] {
            assert_eq!(add_path_entry(&cur, dir, NO_EXPAND), None, "{cur}");
        }
        assert_eq!(add_path_entry(&format!(r"C:\a;{dir}x"), dir, NO_EXPAND), Some(format!(r"C:\a;{dir}x;{dir}")));
        // A trailing separator on `dir` itself is tolerated too.
        assert_eq!(add_path_entry(dir, &format!(r"{dir}\"), NO_EXPAND), None);
    }

    #[test]
    fn path_entry_is_removed_alone() {
        let dir = r"C:\Users\me\AppData\Local\Programs\claude-presence";
        assert_eq!(remove_path_entry("", dir, NO_EXPAND), None);
        assert_eq!(remove_path_entry(r"C:\a;C:\b", dir, NO_EXPAND), None);
        assert_eq!(remove_path_entry(&format!(r"C:\a;{dir}x"), dir, NO_EXPAND), None, "prefix is not a match");
        assert_eq!(remove_path_entry(dir, dir, NO_EXPAND).as_deref(), Some(""));
        assert_eq!(remove_path_entry(&format!(r"C:\a;{dir}"), dir, NO_EXPAND).as_deref(), Some(r"C:\a"));
        assert_eq!(remove_path_entry(&format!(r"{dir};C:\a"), dir, NO_EXPAND).as_deref(), Some(r"C:\a"));
        assert_eq!(
            remove_path_entry(&format!(r"%X%\bin;{}\;C:\b;{dir}/", dir.to_lowercase()), dir, NO_EXPAND).as_deref(),
            Some(r"%X%\bin;C:\b")
        );
        // Everything else is kept verbatim.
        assert_eq!(
            remove_path_entry(&format!(r" C:\a ;{dir};;C:\b"), dir, NO_EXPAND).as_deref(),
            Some(r" C:\a ;;C:\b")
        );
        // Round trip.
        let cur = r"C:\a;%USERPROFILE%\bin";
        assert_eq!(
            remove_path_entry(&add_path_entry(cur, dir, NO_EXPAND).unwrap(), dir, NO_EXPAND).as_deref(),
            Some(cur)
        );
    }

    #[test]
    fn path_length_limit() {
        assert!(!path_too_long(&"a".repeat(PATH_MAX_CHARS)));
        assert!(path_too_long(&"a".repeat(PATH_MAX_CHARS + 1)));
        // Counted in UTF-16 units, as Windows does.
        assert!(path_too_long(&"\u{1F600}".repeat(PATH_MAX_CHARS / 2 + 1)));
    }

    #[test]
    fn wires_and_unwires_hooks_preserving_settings() {
        let original = r#"{
  "model": "opus",
  "hooks": {
    "Stop": [ { "matcher": "", "hooks": [ { "type": "command", "command": "notify-send done" } ] } ]
  },
  "permissions": { "allow": ["Bash(ls:*)"] }
}"#;
        let exe = Path::new("/opt/bin/claude-presence");
        let wired = wire_hooks(original, exe).unwrap();
        let v: Value = serde_json::from_str(&wired).unwrap();
        assert_eq!(v["model"].as_str(), Some("opus"));
        assert_eq!(v["permissions"]["allow"][0].as_str(), Some("Bash(ls:*)"));
        for ev in HOOK_EVENTS {
            let groups = v["hooks"][*ev].as_array().unwrap();
            let ours = groups.iter().flat_map(|g| g["hooks"].as_array().unwrap().iter()).filter(|h| is_ours(h)).count();
            assert_eq!(ours, 1, "{ev}");
        }
        // The user's own Stop hook survives, ours is added beside it.
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(v["hooks"]["Stop"][1]["hooks"][0]["command"].as_str(), Some("'/opt/bin/claude-presence' hook Stop"));
        // Key order is preserved.
        let pos = |k: &str| wired.find(k).unwrap();
        assert!(pos("\"model\"") < pos("\"hooks\"") && pos("\"hooks\"") < pos("\"permissions\""));
        assert!(pos("\"matcher\"") < pos("notify-send"));

        // Idempotent.
        let again = wire_hooks(&wired, exe).unwrap();
        assert_eq!(again, wired);

        // Uninstall restores the user's hooks only.
        let un = unwire_hooks(&wired).unwrap().unwrap();
        let v: Value = serde_json::from_str(&un).unwrap();
        assert_eq!(v["hooks"].as_object().unwrap().len(), 1);
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str(), Some("notify-send done"));

        // Empty settings.
        let fresh = wire_hooks("", exe).unwrap();
        let un = unwire_hooks(&fresh).unwrap().unwrap();
        assert_eq!(un.trim(), "{}");
    }

    #[test]
    fn reinstall_adds_new_events_once() {
        assert!(HOOK_EVENTS.contains(&"PostToolUseFailure"));
        let exe = Path::new("/opt/bin/claude-presence");
        // Settings wired by a version that didn't know PostToolUseFailure.
        let mut old: Value = serde_json::from_str(&wire_hooks("", exe).unwrap()).unwrap();
        old["hooks"].as_object_mut().unwrap().remove("PostToolUseFailure");
        let v: Value = serde_json::from_str(&wire_hooks(&old.to_string(), exe).unwrap()).unwrap();
        for ev in HOOK_EVENTS {
            let groups = v["hooks"][*ev].as_array().unwrap();
            let ours = groups.iter().flat_map(|g| g["hooks"].as_array().unwrap().iter()).filter(|h| is_ours(h)).count();
            assert_eq!(ours, 1, "{ev}");
        }
        assert_eq!(
            v["hooks"]["PostToolUseFailure"][0]["hooks"][0]["command"].as_str(),
            Some("'/opt/bin/claude-presence' hook PostToolUseFailure")
        );
    }

    #[test]
    fn copy_retries_only_while_locked() {
        let locked = || Err::<u32, _>(io::Error::from(io::ErrorKind::PermissionDenied));
        // Succeeds on the third try.
        let mut n = 0;
        let r = copy_with_retry(
            || {
                n += 1;
                if n < 3 { locked() } else { Ok(n) }
            },
            20,
            Duration::ZERO,
        );
        assert_eq!(r.unwrap(), 3);
        // Gives up after `attempts`.
        let mut n = 0;
        let r = copy_with_retry(
            || {
                n += 1;
                locked()
            },
            4,
            Duration::ZERO,
        );
        assert_eq!((r.unwrap_err().kind(), n), (io::ErrorKind::PermissionDenied, 4));
        // Anything else is not retried.
        let mut n = 0;
        let r = copy_with_retry(
            || {
                n += 1;
                Err::<(), _>(io::Error::from(io::ErrorKind::NotFound))
            },
            4,
            Duration::ZERO,
        );
        assert_eq!((r.unwrap_err().kind(), n), (io::ErrorKind::NotFound, 1));
        #[cfg(windows)]
        for code in [32, 33] {
            // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
            let mut n = 0;
            let r = copy_with_retry(
                || {
                    n += 1;
                    if n < 2 { Err(io::Error::from_raw_os_error(code)) } else { Ok(()) }
                },
                4,
                Duration::ZERO,
            );
            assert!(r.is_ok() && n == 2, "{code}");
        }
    }

    #[test]
    fn locked_binary_is_moved_aside() {
        let dir = std::env::temp_dir().join(format!("cp-replace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("bin.exe");
        let old = dir.join("bin.exe.old");
        assert_eq!(old_path(&dst), old);
        fs::write(&dst, "v1").unwrap();
        let src = dir.join("new.exe");
        fs::write(&src, "v2").unwrap();
        // Like a running .exe on Windows: can't be overwritten, can be renamed.
        let mut tries = 0;
        let copy = |tries: &mut u32| {
            *tries += 1;
            if dst.exists() { Err(io::Error::from(io::ErrorKind::PermissionDenied)) } else { fs::write(&dst, "v2") }
        };
        replace_with(&src, &dst, 3, Duration::ZERO, || copy(&mut tries)).unwrap();
        assert_eq!(tries, 4, "three locked tries, then one after moving it aside");
        assert_eq!(fs::read_to_string(&dst).unwrap(), "v2");
        assert_eq!(fs::read_to_string(&old).unwrap(), "v1");
        // The next install cleans up.
        remove_stale_old(&dir, &["bin.exe".into(), "other.exe".into()]);
        assert!(!old.exists() && dst.exists());
        // Other failures don't move anything.
        let e = replace_with(&src, &dst, 3, Duration::ZERO, || Err(io::Error::from(io::ErrorKind::NotFound)));
        assert_eq!(e.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(!old.exists() && dst.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_replacement_keeps_the_old_binary() {
        let dir = std::env::temp_dir().join(format!("cp-replace-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("bin.exe");
        let src = dir.join("new.exe");
        fs::write(&dst, "v1").unwrap();
        fs::write(&src, "v2").unwrap();
        // Locked, and the copy after moving it aside fails too (say, the
        // source is locked by a scanner): hooks must still find a binary.
        let e = replace_with(&src, &dst, 2, Duration::ZERO, || Err(io::Error::from(io::ErrorKind::PermissionDenied)));
        assert_eq!(e.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "v1");
        assert!(!old_path(&dst).exists());
        // An unreadable source fails at once: no retries, nothing moved.
        let mut tries = 0;
        let e = replace_with(&dir.join("missing.exe"), &dst, 20, Duration::from_secs(1), || {
            tries += 1;
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert_eq!((e.unwrap_err().kind(), tries), (io::ErrorKind::NotFound, 0));
        assert_eq!(fs::read_to_string(&dst).unwrap(), "v1");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A hook endpoint unique to this test and process.
    fn test_addr(name: &str) -> PathBuf {
        #[cfg(unix)]
        return std::env::temp_dir().join(format!("cp-{name}-{}.sock", std::process::id()));
        #[cfg(windows)]
        return PathBuf::from(format!(r"\\.\pipe\cp-test-{name}-{}", std::process::id()));
    }

    #[test]
    fn stops_a_running_daemon() {
        let p = test_addr("install-stop");
        assert_eq!(stop_daemon(&p, Duration::from_millis(100)), None, "nothing running");

        // A daemon that honors the request: stops serving and unbinds.
        let l = ipc::Listener::bind(&p).unwrap();
        let t = std::thread::spawn(move || l.serve(|m| ipc::event_name(&m) != ipc::SHUTDOWN.as_bytes()));
        assert_eq!(stop_daemon(&p, Duration::from_secs(2)), Some(true));
        t.join().unwrap();

        // An old daemon that ignores it.
        let l = ipc::Listener::bind(&p).unwrap();
        std::thread::spawn(move || l.serve(|_| true));
        assert_eq!(stop_daemon(&p, Duration::from_millis(200)), Some(false));
    }

    #[test]
    fn service_files_render() {
        let d = Path::new("/home/me/.cargo/bin/claude-presenced");
        assert!(systemd_unit(d).contains("ExecStart=/home/me/.cargo/bin/claude-presenced\n"));
        assert!(systemd_unit(Path::new("/a b/c")).contains("ExecStart=\"/a b/c\""));
        assert!(openrc_script(d).contains("supervisor=supervise-daemon"));
        assert!(dinit_service(d).contains("type = process"));
        assert!(
            launchd_plist(d, Path::new("/tmp/l.log")).contains("<string>/home/me/.cargo/bin/claude-presenced</string>")
        );
        assert!(task_xml(Path::new(r"C:\x\claude-presenced.exe"), r"PC\me").contains(r"<UserId>PC\me</UserId>"));
    }
}
