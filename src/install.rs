//! `install` / `uninstall`: wire Claude Code hooks into `settings.json` and
//! register the daemon as a per-user background service.
//!
//!   Linux:   systemd user unit, OpenRC user service, dinit user service,
//!            or an XDG autostart entry when no supported init is found
//!   macOS:   launchd LaunchAgent
//!   Windows: Task Scheduler logon task (falls back to the HKCU Run key)

use crate::{ipc, paths};
use serde_json::{Map, Value, json};
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
    format!("\"{s}\"")
}

pub fn hook_command(exe: &Path, event: &str) -> String {
    format!("{} hook {event}", quote(exe))
}

fn is_ours(h: &Value) -> bool {
    h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains(MARK) && c.contains(" hook "))
}

/// Remove our hook entries from a settings value, dropping emptied groups.
fn strip_hooks(settings: &mut Value) {
    let Some(obj) = settings.as_object_mut() else { return };
    let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    hooks.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else { return true };
        for g in groups.iter_mut() {
            if let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                list.retain(|h| !is_ours(h));
            }
        }
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|l| !l.is_empty()));
        !groups.is_empty()
    });
    if hooks.is_empty() {
        obj.remove("hooks");
    }
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

pub fn unwire_hooks(settings: &str) -> io::Result<String> {
    let mut v = parse(settings)?;
    strip_hooks(&mut v);
    pretty(&v)
}

fn write_atomic(path: &Path, data: &str) -> io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp-claude-presence");
    fs::write(&tmp, data)?;
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
    let next = unwire_hooks(&current)?;
    if next.trim() != current.trim() {
        write_atomic(&path, &next)?;
        return Ok(true);
    }
    Ok(false)
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

/// Stop the running daemon and remove every service flavor we might have
/// installed.
pub fn uninstall_service() -> Vec<String> {
    let mut done = Vec::new();
    // Asked first so it saves stats and clears the card itself, and so
    // daemons no service manager tracks (XDG autostart, the Run key, a
    // detached spawn) stop too. systemd, launchd and Task Scheduler don't
    // restart a clean exit; OpenRC and dinit respawn after 5 s, but are
    // stopped right below.
    let sock = paths::hook_socket();
    // TODO: drop the legacy path a couple of releases after the private
    // socket dir shipped.
    let legacy = paths::legacy_hook_socket().filter(|old| *old != sock && old.exists());
    for addr in std::iter::once(sock.clone()).chain(legacy) {
        match stop_daemon(&addr, Duration::from_secs(2)) {
            Some(true) => done.push("stopped the running daemon".into()),
            Some(false) => crate::warn!(
                "the running daemon has not stopped after 2 s (still busy, or a version without __shutdown)"
            ),
            None => {}
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            v["hooks"]["Stop"][1]["hooks"][0]["command"].as_str(),
            Some("\"/opt/bin/claude-presence\" hook Stop")
        );
        // Key order is preserved.
        let pos = |k: &str| wired.find(k).unwrap();
        assert!(pos("\"model\"") < pos("\"hooks\"") && pos("\"hooks\"") < pos("\"permissions\""));
        assert!(pos("\"matcher\"") < pos("notify-send"));

        // Idempotent.
        let again = wire_hooks(&wired, exe).unwrap();
        assert_eq!(again, wired);

        // Uninstall restores the user's hooks only.
        let un = unwire_hooks(&wired).unwrap();
        let v: Value = serde_json::from_str(&un).unwrap();
        assert_eq!(v["hooks"].as_object().unwrap().len(), 1);
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str(), Some("notify-send done"));

        // Empty settings.
        let fresh = wire_hooks("", exe).unwrap();
        let un = unwire_hooks(&fresh).unwrap();
        assert_eq!(un.trim(), "{}");
    }

    #[cfg(unix)]
    #[test]
    fn stops_a_running_daemon() {
        let p = std::env::temp_dir().join(format!("cp-stop-{}.sock", std::process::id()));
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
