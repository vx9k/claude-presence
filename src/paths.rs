//! Per-OS filesystem locations. Everything resolves from environment variables
//! the OS already provides; no directory crates needed.

use std::env;
use std::path::PathBuf;

pub const APP: &str = "claude-presence";

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn home() -> PathBuf {
    #[cfg(windows)]
    let h = env_path("USERPROFILE").or_else(|| env_path("HOME"));
    #[cfg(not(windows))]
    let h = env_path("HOME");
    h.unwrap_or_else(|| PathBuf::from("."))
}

/// Where `config.toml` lives.
///   Linux:   $XDG_CONFIG_HOME/claude-presence   (~/.config/claude-presence)
///   macOS:   ~/Library/Application Support/claude-presence
///   Windows: %APPDATA%\claude-presence
pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    return home().join("Library/Application Support").join(APP);
    #[cfg(windows)]
    return env_path("APPDATA").unwrap_or_else(|| home().join("AppData").join("Roaming")).join(APP);
    #[cfg(all(unix, not(target_os = "macos")))]
    return env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config")).join(APP);
}

/// Where the lifetime ledger lives.
///   Linux:   $XDG_DATA_HOME/claude-presence     (~/.local/share/claude-presence)
///   macOS:   ~/Library/Application Support/claude-presence
///   Windows: %LOCALAPPDATA%\claude-presence
pub fn data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    return config_dir();
    #[cfg(windows)]
    return env_path("LOCALAPPDATA").unwrap_or_else(|| home().join("AppData").join("Local")).join(APP);
    #[cfg(all(unix, not(target_os = "macos")))]
    return env_path("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local/share")).join(APP);
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn ledger_file() -> PathBuf {
    data_dir().join("ledger.json")
}

pub fn seen_file() -> PathBuf {
    data_dir().join("seen.bin")
}

/// Claude Code's home. Honors `CLAUDE_CONFIG_DIR` like Claude Code does.
pub fn claude_home() -> PathBuf {
    env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home().join(".claude"))
}

pub fn claude_projects() -> PathBuf {
    claude_home().join("projects")
}

pub fn claude_settings() -> PathBuf {
    claude_home().join("settings.json")
}

/// A per-user directory the OS already keeps private: the Darwin user temp
/// dir, `$XDG_RUNTIME_DIR`, or `/run/user/<uid>`.
#[cfg(unix)]
fn user_runtime_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Some(p) = darwin_user_temp_dir() {
            return Some(p);
        }
    }
    if let Some(p) = env_path("XDG_RUNTIME_DIR") {
        return Some(p);
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getuid never fails.
        let p = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// The temp dir, which may be shared with other users.
#[cfg(unix)]
fn tmp_dir() -> PathBuf {
    for key in ["TMPDIR", "TMP", "TEMP"] {
        if let Some(p) = env_path(key) {
            return p;
        }
    }
    PathBuf::from("/tmp")
}

/// Per-user directory for sockets. On Linux this is where Discord puts its
/// own IPC socket too; on macOS it's the per-user `$TMPDIR`.
#[cfg(unix)]
pub fn runtime_dir() -> PathBuf {
    user_runtime_dir().unwrap_or_else(tmp_dir)
}

/// `<tmp>/claude-presence-<uid>`: the hook socket's directory when the OS
/// offers no private runtime dir. `None` when it does. The daemon creates
/// this directory `0700` and refuses to start unless it is a real directory
/// owned by it with no group/other access (see `ipc::ensure_private_dir`),
/// so another user can't pre-create the socket in a shared `/tmp`.
#[cfg(unix)]
pub fn private_socket_dir() -> Option<PathBuf> {
    match user_runtime_dir() {
        Some(_) => None,
        None => Some(private_dir()),
    }
}

#[cfg(unix)]
fn private_dir() -> PathBuf {
    // SAFETY: getuid never fails.
    tmp_dir().join(format!("{APP}-{}", unsafe { libc::getuid() }))
}

/// `confstr(_CS_DARWIN_USER_TEMP_DIR)`: the real per-user temp dir, which is
/// what `$TMPDIR` normally points at, but also correct for launchd agents
/// whose environment might lack `TMPDIR`.
#[cfg(target_os = "macos")]
pub fn darwin_user_temp_dir() -> Option<PathBuf> {
    use std::ffi::CStr;
    let mut buf = [0 as libc::c_char; 1024];
    // SAFETY: buffer and its length are passed together.
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    // SAFETY: confstr NUL-terminates within `n` bytes.
    let s = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Some(PathBuf::from(s.to_string_lossy().into_owned()))
}

/// Address the daemon listens on for hook events.
#[cfg(unix)]
pub fn hook_socket() -> PathBuf {
    hook_endpoint().0
}

/// The hook socket, and whether it lies in `private_socket_dir()` (which a
/// client must verify before connecting, see `ipc::send_to`).
#[cfg(unix)]
pub fn hook_endpoint() -> (PathBuf, bool) {
    match user_runtime_dir() {
        Some(dir) => (dir.join(format!("{APP}.sock")), false),
        // Short name: AF_UNIX paths are limited to ~100 bytes.
        None => (private_dir().join("hook.sock"), true),
    }
}

#[cfg(windows)]
pub fn hook_socket() -> PathBuf {
    let user = env::var("USERNAME").unwrap_or_default();
    let user: String =
        user.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    PathBuf::from(format!(r"\\.\pipe\{APP}-{user}"))
}

/// Where versions before the private socket dir listened, if that differs
/// from today's path: `install`/`uninstall` stop a daemon found there too.
// TODO: remove a couple of releases after the private socket dir shipped.
#[cfg(unix)]
pub fn legacy_hook_socket() -> Option<PathBuf> {
    match user_runtime_dir() {
        Some(_) => None, // unchanged
        // SAFETY: getuid never fails.
        None => Some(legacy_socket_in(&tmp_dir(), unsafe { libc::getuid() })),
    }
}

#[cfg(unix)]
fn legacy_socket_in(tmp: &std::path::Path, uid: u32) -> PathBuf {
    if tmp == std::path::Path::new("/tmp") {
        tmp.join(format!("{APP}-{uid}.sock"))
    } else {
        tmp.join(format!("{APP}.sock"))
    }
}

#[cfg(windows)]
pub fn legacy_hook_socket() -> Option<PathBuf> {
    None
}

#[cfg(windows)]
pub fn hook_endpoint() -> (PathBuf, bool) {
    (hook_socket(), false)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn legacy_socket_names() {
        assert_eq!(legacy_socket_in(Path::new("/tmp"), 1000), Path::new("/tmp/claude-presence-1000.sock"));
        assert_eq!(legacy_socket_in(Path::new("/var/tmp/me"), 1000), Path::new("/var/tmp/me/claude-presence.sock"));
    }
}
