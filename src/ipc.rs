//! Hook → daemon channel. A hook invocation connects, writes
//! `<EventName>\n<raw JSON from Claude Code>`, and disconnects. The daemon
//! does all parsing, so the hook process stays a few hundred microseconds of
//! work. Unix domain socket (mode 0600, in a directory only we can enter) on
//! Linux/macOS, a local-only named pipe whose DACL admits only the current
//! user on Windows.
//!
//! Event names starting with `__` are reserved for control messages sent by
//! claude-presence itself (`__shutdown`, `__reload`, `__state`); the `hook`
//! command never forwards them. Control messages have no body. `__state` is
//! the one request with a reply: the daemon writes one JSON line back
//! (`state::StateSnapshot`, at most `MAX_STATE` bytes) and closes; an older
//! daemon closes without a word.

use std::io;
use std::path::Path;

/// Largest message the daemon accepts (Write tool payloads carry file contents).
pub const MAX_MSG: usize = 16 << 20;

/// Largest `__state` reply, newline included; clients refuse anything longer.
pub const MAX_STATE: usize = 64 << 10;

/// Control event: stop the daemon cleanly, like SIGTERM.
pub const SHUTDOWN: &str = "__shutdown";

/// Control event: reload `config.toml`, like SIGHUP (which Windows lacks).
pub const RELOAD: &str = "__reload";

/// Control request: reply with the daemon's state snapshot.
pub const STATE: &str = "__state";

/// What a listener does after handing over a message.
pub enum Action {
    /// Close the connection and wait for the next one.
    Continue,
    /// Close the connection and stop serving.
    Stop,
    /// Write this back to the client, then close the connection.
    Reply(Vec<u8>),
}

/// `true` keeps serving, `false` stops.
impl From<bool> for Action {
    fn from(keep: bool) -> Action {
        if keep { Action::Continue } else { Action::Stop }
    }
}

/// True for reserved control event names (`__` prefix).
pub fn is_control(event: &[u8]) -> bool {
    event.starts_with(b"__")
}

/// The event name of a message: everything before the first newline.
pub fn event_name(msg: &[u8]) -> &[u8] {
    match memchr::memchr(b'\n', msg) {
        Some(i) => &msg[..i],
        None => msg,
    }
}

/// The message asking a running daemon to shut down.
pub fn shutdown_request() -> Vec<u8> {
    control(SHUTDOWN)
}

/// The message asking a running daemon to reload its configuration.
pub fn reload_request() -> Vec<u8> {
    control(RELOAD)
}

/// The message asking a running daemon for its state snapshot.
pub fn state_request() -> Vec<u8> {
    control(STATE)
}

fn control(event: &str) -> Vec<u8> {
    let mut m = event.as_bytes().to_vec();
    m.push(b'\n');
    m
}

/// Ask the daemon at `addr` to reload `config.toml`, with the same endpoint
/// checks as hook events (`send_to`). An older daemon ignores it.
pub fn send_reload(addr: &Path, in_private_dir: bool) -> io::Result<()> {
    send_to(addr, in_private_dir, &reload_request())
}

/// A `__state` reply as read (at most `MAX_STATE + 1` bytes): the JSON line
/// without its newline, `None` if the daemon closed without one (an older
/// daemon, which ignores the request).
fn parse_reply(buf: Vec<u8>) -> io::Result<Option<String>> {
    if buf.len() > MAX_STATE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "state reply too large"));
    }
    let s = String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let line = s.trim_end();
    Ok(if line.is_empty() { None } else { Some(line.to_owned()) })
}

// ------------------------------------------------------------------ client --

#[cfg(unix)]
pub fn send(addr: &Path, msg: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let mut s = UnixStream::connect(addr)?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(msg)?;
    s.shutdown(std::net::Shutdown::Write)
}

/// `send` for hook events. When `addr` is in the fallback
/// `paths::private_socket_dir()` (`in_private_dir`), first check that the
/// directory is ours (one `lstat`, one `stat` of its parent): a squatter who
/// pre-created it must not receive hook payloads. Once it passes, nobody
/// else can swap it: it is ours with mode 0700, and its parent is sticky
/// (only the owner may rename entries) or writable only by us/root.
/// OS-provided runtime dirs cost nothing extra.
#[cfg(unix)]
pub fn send_to(addr: &Path, in_private_dir: bool, msg: &[u8]) -> io::Result<()> {
    if let (true, Some(dir)) = (in_private_dir, addr.parent()) {
        verify_private_dir(dir)?;
    }
    send(addr, msg)
}

/// Ask the daemon at `addr` for its state snapshot (one JSON line, see
/// `state::StateSnapshot`), with the same fallback-dir check as `send_to`:
/// the reply is trusted, so it must come from our own daemon. `None` if the
/// daemon closed without replying (an older one). Each read and write gives
/// up after `timeout`.
#[cfg(unix)]
pub fn query_state(addr: &Path, in_private_dir: bool, timeout: std::time::Duration) -> io::Result<Option<String>> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    if let (true, Some(dir)) = (in_private_dir, addr.parent()) {
        verify_private_dir(dir)?;
    }
    let mut s = UnixStream::connect(addr)?;
    s.set_write_timeout(Some(timeout))?;
    s.set_read_timeout(Some(timeout))?;
    s.write_all(&state_request())?;
    // The daemon reads until end of input, as for hook events.
    s.shutdown(std::net::Shutdown::Write)?;
    let mut buf = Vec::with_capacity(4096);
    s.take(MAX_STATE as u64 + 1).read_to_end(&mut buf)?;
    parse_reply(buf)
}

/// What the `hook` command sends through: `send_to`, but when nothing
/// listens at `addr` (or its private dir doesn't exist yet), try the socket
/// `legacy()` names, where a daemon from before the private socket dir may
/// still be running until the next install or logon. The legacy path is in
/// the shared temp dir, so only a socket we own (`lstat`) is used.
// TODO: remove a couple of releases after the private socket dir shipped.
#[cfg(unix)]
pub fn send_hook(
    addr: &Path,
    in_private_dir: bool,
    legacy: impl FnOnce() -> Option<std::path::PathBuf>,
    msg: &[u8],
) -> io::Result<()> {
    match send_to(addr, in_private_dir, msg) {
        // Not a refusal of an untrusted dir (PermissionDenied): nobody's home.
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => match legacy() {
            Some(old) if old != addr => send_legacy(&old, msg),
            _ => Err(e),
        },
        r => r,
    }
}

/// `send` to an older daemon's socket, if it is a socket owned by us in a
/// directory where nobody else can swap it (`check_parent`).
#[cfg(unix)]
fn send_legacy(addr: &Path, msg: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    // SAFETY: geteuid never fails.
    let euid = unsafe { libc::geteuid() };
    let parent = match addr.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let m = std::fs::metadata(parent)?;
    check_parent(m.is_dir(), m.uid(), m.mode(), euid)?;
    let m = std::fs::symlink_metadata(addr)?;
    check_legacy(m.file_type().is_socket(), m.uid(), euid)?;
    send(addr, msg)
}

/// The `lstat` facts `send_legacy` requires.
#[cfg(unix)]
fn check_legacy(is_socket: bool, uid: u32, euid: u32) -> io::Result<()> {
    let why = if !is_socket {
        "not a socket (or a symlink)"
    } else if uid != euid {
        "owned by another user"
    } else {
        return Ok(());
    };
    Err(io::Error::new(io::ErrorKind::PermissionDenied, why))
}

/// `send_to` for hook events; Windows has no legacy endpoint.
#[cfg(windows)]
pub fn send_hook(
    addr: &Path,
    in_private_dir: bool,
    _legacy: impl FnOnce() -> Option<std::path::PathBuf>,
    msg: &[u8],
) -> io::Result<()> {
    send_to(addr, in_private_dir, msg)
}

/// `send` for hook events. Our pipe's DACL keeps other users out, but not
/// a squatter who created the pipe name first: only write once the pipe's
/// owner checks out (`pipe_owner_trusted`).
#[cfg(windows)]
pub fn send_to(addr: &Path, _in_private_dir: bool, msg: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    let mut f = open_pipe(addr, GENERIC_WRITE)?;
    check_pipe_owner(&f)?;
    f.write_all(msg)
}

/// Fail unless the pipe `f` is connected to is owned by someone
/// `pipe_owner_trusted` accepts.
#[cfg(windows)]
fn check_pipe_owner(f: &std::fs::File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let owner = SecurityInfo::query(f.as_raw_handle(), windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION)?
        .owner_string()?;
    if !pipe_owner_trusted(&owner, &current_user_sid()?) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "hook pipe owned by someone else"));
    }
    Ok(())
}

#[cfg(windows)]
pub fn send(addr: &Path, msg: &[u8]) -> io::Result<()> {
    use std::io::Write;
    open_pipe(addr, windows_sys::Win32::Foundation::GENERIC_WRITE)?.write_all(msg)
}

/// Ask the daemon at `addr` for its state snapshot (one JSON line, see
/// `state::StateSnapshot`). Like `send_to`, only from a pipe whose owner
/// checks out: the reply is trusted. `None` if the daemon closed without
/// replying, or its pipe is inbound-only (both: an older daemon). Gives up
/// after `timeout` without a complete reply.
///
/// The request has no end-of-input (a pipe can't be half-closed): the
/// daemon stops reading at the newline. The reply is read as it arrives
/// (`PeekNamedPipe`, so a stalled daemon can't block us past `timeout`)
/// until the daemon disconnects, which it does once we have read it all.
#[cfg(windows)]
pub fn query_state(addr: &Path, _in_private_dir: bool, timeout: std::time::Duration) -> io::Result<Option<String>> {
    use std::io::{Read, Write};
    use std::os::windows::io::AsRawHandle;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE,
    };
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let deadline = Instant::now() + timeout;
    let mut f = match open_pipe(addr, GENERIC_READ | GENERIC_WRITE) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => return Ok(None),
        r => r?,
    };
    check_pipe_owner(&f)?;
    f.write_all(&state_request())?;
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 16 * 1024];
    while buf.len() <= MAX_STATE {
        let mut avail = 0u32;
        // SAFETY: `f` is an open pipe handle; no buffer is passed, only the
        // byte count out-pointer, which is valid.
        let ok = unsafe {
            PeekNamedPipe(
                f.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut avail,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error().map(|c| c as u32) {
                // The daemon is done (disconnected or closed its end).
                Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED) => break,
                _ => return Err(e),
            }
        }
        if avail > 0 {
            let want = (avail as usize).min(chunk.len()).min(MAX_STATE + 1 - buf.len());
            // Doesn't block: at least `want` bytes are waiting.
            let n = f.read(&mut chunk[..want])?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        } else if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "no state reply"));
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    parse_reply(buf)
}

/// Connect to the pipe at `addr` with `access`, waiting while all instances
/// are busy. `READ_CONTROL` is added so `check_pipe_owner` can read the
/// owner; `SECURITY_IDENTIFICATION` keeps the server from impersonating us.
#[cfg(windows)]
fn open_pipe(addr: &Path, access: u32) -> io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
    use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, SECURITY_IDENTIFICATION};
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
    let wide: Vec<u16> = addr.as_os_str().encode_wide().chain(Some(0)).collect();
    for _ in 0..10 {
        match std::fs::OpenOptions::new()
            .access_mode(access | READ_CONTROL)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(addr)
        {
            Ok(f) => return Ok(f),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                // SAFETY: NUL-terminated wide string.
                unsafe { WaitNamedPipeW(wide.as_ptr(), 250) };
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "daemon pipe busy"))
}

/// Whether a hook may write to a pipe owned by `owner` (a SID string) when
/// running as `user`. Our daemon's pipe is owned by the user, or by
/// `BUILTIN\Administrators` when the daemon runs elevated; LocalSystem is
/// trusted anyway. Anyone else is a squatter.
pub fn pipe_owner_trusted(owner: &str, user: &str) -> bool {
    !owner.is_empty() && (owner == user || owner == "S-1-5-32-544" || owner == "S-1-5-18")
}

/// True if a daemon is accepting connections at `addr`.
pub fn daemon_running(addr: &Path) -> bool {
    send(addr, b"").is_ok()
}

// ------------------------------------------------------------------ server --

/// How long the Unix listener may spend writing a reply.
#[cfg(unix)]
const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// `Listener::bind`, but while another daemon still owns `addr` keep
/// retrying for up to `wait`: on reinstall the old one may still be saving
/// stats after being asked to shut down.
pub fn bind_waiting(addr: &Path, wait: std::time::Duration) -> io::Result<Listener> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + wait;
    loop {
        match Listener::bind(addr) {
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            r => return r,
        }
    }
}

/// SDDL for the hook pipe: a protected DACL whose single ACE grants the
/// user `sid` full access. The user's SID, not `OW` (owner rights): an
/// elevated token makes `Administrators` the owner, which would lock out
/// the same user's non-elevated hooks.
pub fn pipe_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
}

/// Bind the listening endpoint. Fails with `AddrInUse` if another daemon
/// already owns it.
#[cfg(unix)]
pub struct Listener(std::os::unix::net::UnixListener, std::path::PathBuf);

/// Create `dir` owner-only if missing, then insist it is a real directory
/// owned by us with no group/other access. Never chmods or removes a
/// directory that fails the check: it may belong to someone else.
#[cfg(unix)]
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::PermissionsExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        // Exactly 0700 whatever the (process-wide) umask is right now. Fine
        // to chmod: we just created it.
        Ok(()) => std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    verify_private_dir(dir)
}

/// Check `dir` with `check_private` and its parent with `check_parent`,
/// without creating anything.
#[cfg(unix)]
fn verify_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid never fails.
    let euid = unsafe { libc::geteuid() };
    let at = |p: &Path, e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", p.display()));
    // The parent is followed (`stat`): `/tmp` may be a symlink (macOS), and
    // what matters is who can rename entries in the directory it points to.
    let parent = match dir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let m = std::fs::metadata(parent)?;
    check_parent(m.is_dir(), m.uid(), m.mode(), euid).map_err(|e| at(parent, e))?;
    let m = std::fs::symlink_metadata(dir)?;
    check_private(m.file_type().is_dir(), m.uid(), m.mode(), euid).map_err(|e| at(dir, e))
}

/// Nobody else may be able to rename our directory away and put their own
/// in its place: the parent must be ours or root's, and either sticky (like
/// `/tmp`) or not writable by group/other.
#[cfg(unix)]
fn check_parent(is_dir: bool, uid: u32, mode: u32, euid: u32) -> io::Result<()> {
    let why = if !is_dir {
        "not a directory"
    } else if uid != euid && uid != 0 {
        "parent owned by another user"
    } else if mode & 0o1000 == 0 && mode & 0o022 != 0 {
        "parent writable by others and not sticky"
    } else {
        return Ok(());
    };
    Err(io::Error::new(io::ErrorKind::PermissionDenied, why))
}

/// The `lstat` facts `ensure_private_dir` requires.
#[cfg(unix)]
fn check_private(is_dir: bool, uid: u32, mode: u32, euid: u32) -> io::Result<()> {
    let why = if !is_dir {
        "not a directory (or a symlink)"
    } else if uid != euid {
        "owned by another user"
    } else if mode & 0o077 != 0 {
        "accessible by group/other"
    } else if mode & 0o700 != 0o700 {
        "not rwx for its owner"
    } else {
        return Ok(());
    };
    Err(io::Error::new(io::ErrorKind::PermissionDenied, why))
}

#[cfg(unix)]
impl Listener {
    pub fn bind(addr: &Path) -> io::Result<Listener> {
        // Our own `<tmp>/claude-presence-<uid>` must be verified before use;
        // OS-provided runtime dirs are trusted as they are.
        let private = addr.parent().is_some_and(|d| crate::paths::private_socket_dir().as_deref() == Some(d));
        Self::bind_with(addr, private)
    }

    fn bind_with(addr: &Path, private_dir: bool) -> io::Result<Listener> {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::{UnixListener, UnixStream};
        if let (true, Some(dir)) = (private_dir, addr.parent()) {
            ensure_private_dir(dir)?;
        }
        if addr.exists() {
            if UnixStream::connect(addr).is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "daemon already running"));
            }
            // Another starting daemon may have removed it first.
            match std::fs::remove_file(addr) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        if let Some(dir) = addr.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Create the socket with no group/other permissions from the start.
        // SAFETY: umask is process-global; we restore it right after bind.
        let old = unsafe { libc::umask(0o177) };
        let l = UnixListener::bind(addr);
        unsafe { libc::umask(old) };
        let l = l?;
        std::fs::set_permissions(addr, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener(l, addr.to_path_buf()))
    }

    /// Accept connections forever, handing each complete message to `on_msg`
    /// (`false` or `Action::Stop` stops; `Action::Reply` is written back).
    pub fn serve<A: Into<Action>>(self, mut on_msg: impl FnMut(Vec<u8>) -> A) {
        use std::io::{Read, Write};
        use std::time::Duration;
        for conn in self.0.incoming() {
            let Ok(mut s) = conn else {
                // EMFILE/ENFILE persist until a descriptor frees up; back off
                // instead of spinning on accept().
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = Vec::with_capacity(4096);
            if (&mut s).take(MAX_MSG as u64).read_to_end(&mut buf).is_err() || buf.is_empty() {
                continue;
            }
            match on_msg(buf).into() {
                Action::Continue => {}
                Action::Stop => return,
                Action::Reply(r) => {
                    // A client that stops reading can't hold up the hooks.
                    let _ = s.set_write_timeout(Some(REPLY_TIMEOUT));
                    let _ = s.write_all(&r);
                }
            }
        }
    }
}

#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.1);
    }
}

/// The pipe's security descriptor: normally `pipe_sddl`, granting only the
/// current user access, so other users can neither connect to our pipe nor
/// add instances to it.
#[cfg(windows)]
struct OwnerOnly(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

/// The current process token's user SID as a string (`S-1-5-21-…`).
#[cfg(windows)]
fn current_user_sid() -> io::Result<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo-handle from GetCurrentProcess needs no closing;
    // `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // TOKEN_USER plus the SID it points into; a SID is at most 68 bytes.
    // usize elements keep the buffer pointer-aligned for TOKEN_USER.
    let mut buf = [0usize; 32];
    let mut len = 0u32;
    // SAFETY: `buf` is writable for the length passed; `token` is open.
    let ok = unsafe {
        GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), std::mem::size_of_val(&buf) as u32, &mut len)
    };
    let err = io::Error::last_os_error();
    // SAFETY: closing the token we opened, once.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(err);
    }
    // SAFETY: on success the buffer starts with an initialized, aligned
    // TOKEN_USER whose SID points into `buf`, which is still alive.
    unsafe { sid_string((*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid) }
}

/// `sid` as a string (`S-1-5-…`).
///
/// # Safety
/// `sid` must point to a valid SID.
#[cfg(windows)]
unsafe fn sid_string(sid: windows_sys::Win32::Security::PSID) -> io::Result<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    let mut wide = std::ptr::null_mut();
    // SAFETY: `sid` is valid (caller); `wide` is a valid out-pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ConvertSidToStringSidW returned a NUL-terminated wide string,
    // which we read up to the NUL and then LocalFree exactly once.
    let s = unsafe {
        let n = (0..).take_while(|&i| *wide.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(wide, n));
        LocalFree(wide.cast());
        s
    };
    Ok(s)
}

/// Parts of a kernel object's security descriptor, read from a handle
/// opened with `READ_CONTROL`. `owner` and `dacl` point into `sd` and are
/// null unless requested.
#[cfg(windows)]
struct SecurityInfo {
    sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
    owner: windows_sys::Win32::Security::PSID,
    #[cfg_attr(not(test), allow(dead_code))]
    dacl: *mut windows_sys::Win32::Security::ACL,
}

#[cfg(windows)]
impl SecurityInfo {
    fn query(
        h: windows_sys::Win32::Foundation::HANDLE,
        what: windows_sys::Win32::Security::OBJECT_SECURITY_INFORMATION,
    ) -> io::Result<SecurityInfo> {
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
        let mut si = SecurityInfo { sd: std::ptr::null_mut(), owner: std::ptr::null_mut(), dacl: std::ptr::null_mut() };
        // SAFETY: `h` is an open handle; every out-pointer is valid. On
        // success `sd` is LocalAlloc'd and freed by Drop.
        let r = unsafe {
            GetSecurityInfo(
                h,
                SE_KERNEL_OBJECT,
                what,
                &mut si.owner,
                std::ptr::null_mut(),
                &mut si.dacl,
                std::ptr::null_mut(),
                &mut si.sd,
            )
        };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        Ok(si)
    }

    fn owner_string(&self) -> io::Result<String> {
        if self.owner.is_null() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "pipe has no owner"));
        }
        // SAFETY: a non-null owner points into `self.sd`, which is alive.
        unsafe { sid_string(self.owner) }
    }
}

#[cfg(windows)]
impl Drop for SecurityInfo {
    fn drop(&mut self) {
        // SAFETY: allocated by GetSecurityInfo (or null) and freed exactly once, here.
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.sd) };
    }
}

#[cfg(windows)]
impl OwnerOnly {
    fn from_sddl(sddl: &str) -> io::Result<OwnerOnly> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut sd = std::ptr::null_mut();
        // SAFETY: NUL-terminated wide string and a valid out-pointer; the
        // size out-parameter is optional. On success `sd` is a LocalAlloc'd
        // descriptor that Drop frees.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnerOnly(sd))
    }

    fn attributes(&self) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

#[cfg(windows)]
impl Drop for OwnerOnly {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW
        // and freed exactly once, here.
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
    }
}

#[cfg(windows)]
pub struct Listener {
    name: Vec<u16>,
    sd: OwnerOnly,
    first: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: a pipe HANDLE is just a kernel object reference, and the security
// descriptor is an immutable heap block only read by CreateNamedPipeW.
#[cfg(windows)]
unsafe impl Send for Listener {}

#[cfg(windows)]
impl Listener {
    fn create(name: &[u16], sd: &OwnerOnly, first: bool) -> io::Result<windows_sys::Win32::Foundation::HANDLE> {
        use windows_sys::Win32::Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE, SetLastError,
        };
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
        use windows_sys::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
            PIPE_WAIT,
        };
        // Duplex for `__state` replies; hook clients still open it write-only.
        let flags = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let sa = sd.attributes();
        // SAFETY: only resets this thread's last-error value.
        unsafe { SetLastError(0) };
        // SAFETY: valid NUL-terminated name; `sa` and the descriptor it points
        // to outlive the call.
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                MAX_STATE as u32,
                64 * 1024,
                0,
                &sa,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let e = io::Error::last_os_error();
            if first && e.kind() == io::ErrorKind::PermissionDenied {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "daemon already running"));
            }
            return Err(e);
        }
        // Windows refuses a second first instance (above); Wine ignores the
        // flag and only reports that the pipe already existed.
        // SAFETY: GetLastError right after the call.
        if first && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: the handle we just created, closed once.
            unsafe { CloseHandle(h) };
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "daemon already running"));
        }
        Ok(h)
    }

    pub fn bind(addr: &Path) -> io::Result<Listener> {
        Self::bind_sddl(addr, &pipe_sddl(&current_user_sid()?))
    }

    /// `bind` with the pipe's security descriptor given as SDDL.
    fn bind_sddl(addr: &Path, sddl: &str) -> io::Result<Listener> {
        use std::os::windows::ffi::OsStrExt;
        let name: Vec<u16> = addr.as_os_str().encode_wide().chain(Some(0)).collect();
        let sd = OwnerOnly::from_sddl(sddl)?;
        let first = Self::create(&name, &sd, true)?;
        Ok(Listener { name, sd, first })
    }

    /// Read one client's message until it closes its end (`ERROR_BROKEN_PIPE`),
    /// or, for a control message (no body), its first line: a `__state`
    /// client keeps the pipe open for the reply. Empty if it sent nothing or
    /// more than `MAX_MSG`.
    fn read_msg(h: windows_sys::Win32::Foundation::HANDLE) -> Vec<u8> {
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let mut n = 0u32;
            // SAFETY: chunk is a valid writable buffer of the given length.
            let r = unsafe { ReadFile(h, chunk.as_mut_ptr(), chunk.len() as u32, &mut n, std::ptr::null_mut()) };
            if r == 0 || n == 0 {
                return buf; // ERROR_BROKEN_PIPE: client finished writing
            }
            buf.extend_from_slice(&chunk[..n as usize]);
            if let Some(end) = control_line_end(&buf) {
                buf.truncate(end);
                return buf;
            }
            if buf.len() > MAX_MSG {
                buf.clear();
                return buf;
            }
        }
    }

    pub fn serve<A: Into<Action>>(mut self, mut on_msg: impl FnMut(Vec<u8>) -> A) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows_sys::Win32::Foundation::{
            CloseHandle, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, DisconnectNamedPipe};
        let close = |h: HANDLE| {
            // SAFETY: closing a server pipe handle we own, exactly once.
            unsafe {
                DisconnectNamedPipe(h);
                CloseHandle(h);
            }
        };
        // One reply in flight at a time (see `reply`); others get none.
        let replying = Arc::new(AtomicBool::new(false));
        // Ours from here on; Drop must not close it again.
        let mut next = std::mem::replace(&mut self.first, INVALID_HANDLE_VALUE);
        loop {
            // A client that connected before this call reports PIPE_CONNECTED;
            // one that already wrote everything and closed reports NO_DATA.
            // Either way its bytes are buffered and readable until BROKEN_PIPE.
            // SAFETY: `next` is a valid server pipe handle we own; GetLastError
            // runs right after the failed call.
            let connected = unsafe { ConnectNamedPipe(next, std::ptr::null_mut()) } != 0
                || matches!(unsafe { GetLastError() }, ERROR_PIPE_CONNECTED | ERROR_NO_DATA);
            let cur = next;
            // Keep an instance listening while we read this one, so the pipe
            // name can never be grabbed by another process in between.
            let created = Self::create(&self.name, &self.sd, false).or_else(|e| {
                crate::error!("pipe: {e}");
                std::thread::sleep(std::time::Duration::from_secs(1));
                Self::create(&self.name, &self.sd, false)
            });
            // Read the current client even if no new instance could be made.
            let mut keep = true;
            let buf = if connected { Self::read_msg(cur) } else { Vec::new() };
            match if buf.is_empty() { Action::Continue } else { on_msg(buf).into() } {
                Action::Continue => close(cur),
                Action::Stop => {
                    close(cur);
                    keep = false;
                }
                Action::Reply(r) if !replying.swap(true, Ordering::AcqRel) => {
                    let (h, busy) = (Handle(cur), replying.clone());
                    let spawned =
                        std::thread::Builder::new().name("reply".into()).stack_size(64 * 1024).spawn(move || {
                            // Move the whole (Send) `Handle`, not just its raw pointer field.
                            let h = h;
                            reply(h.0, &r);
                            busy.store(false, Ordering::Release);
                        });
                    if spawned.is_err() {
                        close(cur);
                        replying.store(false, Ordering::Release);
                    }
                }
                Action::Reply(_) => close(cur),
            }
            match created {
                Ok(h) if keep => next = h,
                Ok(h) => return close(h),
                Err(e) => {
                    crate::error!("pipe: {e}; no longer accepting hook events");
                    return;
                }
            }
        }
    }
}

/// End of a control message's first line in `buf` (after the newline), if
/// `buf` holds a complete one. Hook events never start with `__`.
#[cfg_attr(not(windows), allow(dead_code))]
fn control_line_end(buf: &[u8]) -> Option<usize> {
    if !is_control(buf) {
        return None;
    }
    memchr::memchr(b'\n', buf).map(|i| i + 1)
}

/// A server pipe handle moved to the `reply` thread, which closes it.
#[cfg(windows)]
struct Handle(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a pipe HANDLE is a kernel object reference usable from any
// thread; only the receiving thread uses (and closes) it afterwards.
#[cfg(windows)]
unsafe impl Send for Handle {}

/// Write `data` to the client on server pipe `h`, wait until it has read it
/// all, then disconnect and close `h`. `DisconnectNamedPipe` discards unread
/// data, hence `FlushFileBuffers`, which blocks until the client reads (or
/// closes): run on its own short-lived thread so a stalled client can only
/// hold up other replies, never hook events.
#[cfg(windows)]
fn reply(h: windows_sys::Win32::Foundation::HANDLE, data: &[u8]) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, WriteFile};
    use windows_sys::Win32::System::Pipes::DisconnectNamedPipe;
    let mut rest = data;
    while !rest.is_empty() {
        let mut n = 0u32;
        let len = rest.len().min(u32::MAX as usize) as u32;
        // SAFETY: `h` is a connected server pipe handle we own; `rest` is
        // readable for `len` bytes; `n` is a valid out-pointer.
        if unsafe { WriteFile(h, rest.as_ptr(), len, &mut n, std::ptr::null_mut()) } == 0 || n == 0 {
            break;
        }
        rest = &rest[n as usize..];
    }
    // SAFETY: `h` is ours; flushed, disconnected and closed exactly once, here.
    unsafe {
        FlushFileBuffers(h);
        DisconnectNamedPipe(h);
        CloseHandle(h);
    }
}

/// An unserved listener releases the pipe name, so a daemon that fails
/// after binding (or a test) doesn't block the next bind.
#[cfg(windows)]
impl Drop for Listener {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        if self.first != INVALID_HANDLE_VALUE {
            // SAFETY: a server pipe handle we still own (serve takes it out).
            unsafe { CloseHandle(self.first) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_event_names() {
        assert!(is_control(b"__shutdown"));
        assert!(is_control(b"__anything"));
        assert!(!is_control(b"Stop"));
        assert!(!is_control(b"_x"));
        assert!(!is_control(b""));
        assert_eq!(event_name(b"Stop\n{}"), b"Stop");
        assert_eq!(event_name(b"__shutdown"), b"__shutdown");
        assert_eq!(event_name(b""), b"");
        assert_eq!(event_name(&shutdown_request()), SHUTDOWN.as_bytes());
        assert!(is_control(event_name(&shutdown_request())));
    }

    #[test]
    fn pipe_sddl_grants_only_the_user() {
        assert_eq!(pipe_sddl("S-1-5-21-1-2-3-1001"), "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)");
    }

    #[test]
    fn pipe_owner_must_be_the_user_admins_or_system() {
        let me = "S-1-5-21-1-2-3-1001";
        assert!(pipe_owner_trusted(me, me));
        assert!(pipe_owner_trusted("S-1-5-32-544", me), "elevated daemon");
        assert!(pipe_owner_trusted("S-1-5-18", me), "LocalSystem");
        assert!(!pipe_owner_trusted("S-1-5-21-1-2-3-1002", me), "another user");
        assert!(!pipe_owner_trusted("S-1-5-21-1-2-3-10011", me));
        assert!(!pipe_owner_trusted("S-1-5-21-1-2-3-100", me));
        assert!(!pipe_owner_trusted("S-1-1-0", me), "Everyone");
        assert!(!pipe_owner_trusted("S-1-5-32-545", me), "Users");
        assert!(!pipe_owner_trusted("", me));
        assert!(!pipe_owner_trusted("", ""));
    }

    /// A hook endpoint unique to this test and process.
    fn test_addr(name: &str) -> std::path::PathBuf {
        #[cfg(unix)]
        return std::env::temp_dir().join(format!("cp-ipc-{name}-{}.sock", std::process::id()));
        #[cfg(windows)]
        return std::path::PathBuf::from(format!(r"\\.\pipe\cp-test-{name}-{}", std::process::id()));
    }

    /// Serve `l` on its own thread, forwarding every message.
    fn serve(l: Listener) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || l.serve(move |m| tx.send(m).is_ok()));
        rx
    }

    #[test]
    fn roundtrip() {
        let p = test_addr("rt");
        let l = Listener::bind(&p).unwrap();
        assert_eq!(Listener::bind(&p).err().map(|e| e.kind()), Some(io::ErrorKind::AddrInUse));
        let rx = serve(l);
        send(&p, b"Stop\n{}").unwrap();
        assert!(daemon_running(&p));
        send(&p, b"PreToolUse\n{\"a\":1}").unwrap();
        assert_eq!(rx.recv().unwrap(), b"Stop\n{}");
        assert_eq!(rx.recv().unwrap(), b"PreToolUse\n{\"a\":1}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn shutdown_request_roundtrip() {
        let p = test_addr("stop");
        let rx = serve(Listener::bind(&p).unwrap());
        send(&p, &shutdown_request()).unwrap();
        let m = rx.recv().unwrap();
        assert_eq!(event_name(&m), SHUTDOWN.as_bytes());
        assert!(is_control(event_name(&m)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn state_and_reload_requests() {
        assert_eq!(state_request(), b"__state\n");
        assert_eq!(reload_request(), b"__reload\n");
        assert!(is_control(event_name(&state_request())));
        assert_eq!(event_name(&reload_request()), RELOAD.as_bytes());
    }

    #[test]
    fn state_reply_parsing() {
        // An older daemon closes without a word.
        assert_eq!(parse_reply(Vec::new()).unwrap(), None);
        assert_eq!(parse_reply(b"\n".to_vec()).unwrap(), None);
        assert_eq!(parse_reply(b"{\"v\":1}\n".to_vec()).unwrap().as_deref(), Some("{\"v\":1}"));
        // At the cap is fine, past it is refused.
        let mut max = vec![b' '; MAX_STATE - 2];
        max.splice(0..0, *b"{}");
        assert_eq!(parse_reply(max).unwrap().as_deref(), Some("{}"));
        let e = parse_reply(vec![b'x'; MAX_STATE + 1]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(parse_reply(vec![0xff, 0xfe]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn control_messages_end_at_their_first_line() {
        // A `__state` client waits for the reply instead of closing.
        assert_eq!(control_line_end(b"__state\n"), Some(8));
        assert_eq!(control_line_end(b"__shutdown\n{}"), Some(11));
        assert_eq!(control_line_end(b"__sta"), None, "not complete yet");
        assert_eq!(control_line_end(b"_"), None);
        // Hook events are read to the end, newlines and all.
        assert_eq!(control_line_end(b"Stop\n{}"), None);
        assert_eq!(control_line_end(b"Stop\n__x\n"), None);
        assert_eq!(control_line_end(b""), None);
    }

    #[test]
    fn listener_actions_from_bool() {
        assert!(matches!(Action::from(true), Action::Continue));
        assert!(matches!(Action::from(false), Action::Stop));
    }

    /// Serve `l`, answering `__state` with `reply` (`None`: like an older
    /// daemon, which ignores it) and forwarding every other message.
    fn serve_state(l: Listener, reply: Option<Vec<u8>>) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            l.serve(move |m| {
                if event_name(&m) == STATE.as_bytes()
                    && let Some(r) = &reply
                {
                    return Action::Reply(r.clone());
                }
                tx.send(m).is_ok().into()
            })
        });
        rx
    }

    #[test]
    fn state_roundtrip() {
        use std::time::Duration;
        let p = test_addr("state");
        let rx = serve_state(Listener::bind(&p).unwrap(), Some(b"{\"v\":1,\"pid\":7}\n".to_vec()));
        let t = Duration::from_secs(5);
        assert_eq!(query_state(&p, false, t).unwrap().as_deref(), Some("{\"v\":1,\"pid\":7}"));
        // Hooks still flow, before and after; repeated queries all answer.
        send(&p, b"Stop\n{}").unwrap();
        assert_eq!(rx.recv_timeout(t).unwrap(), b"Stop\n{}");
        for _ in 0..3 {
            assert_eq!(query_state(&p, false, t).unwrap().as_deref(), Some("{\"v\":1,\"pid\":7}"));
        }
        send_reload(&p, false).unwrap();
        assert_eq!(rx.recv_timeout(t).unwrap(), reload_request());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn older_daemon_ignores_state_requests() {
        use std::time::Duration;
        let p = test_addr("state-old");
        let rx = serve_state(Listener::bind(&p).unwrap(), None);
        let t = Duration::from_secs(5);
        assert_eq!(query_state(&p, false, t).unwrap(), None);
        // It saw an unknown control event and carried on.
        assert_eq!(rx.recv_timeout(t).unwrap(), state_request());
        send(&p, b"Stop\n{}").unwrap();
        assert_eq!(rx.recv_timeout(t).unwrap(), b"Stop\n{}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn oversized_state_reply_is_refused() {
        use std::time::Duration;
        let p = test_addr("state-big");
        let rx = serve_state(Listener::bind(&p).unwrap(), Some(vec![b'x'; MAX_STATE + 1]));
        let t = Duration::from_secs(5);
        assert_eq!(query_state(&p, false, t).unwrap_err().kind(), io::ErrorKind::InvalidData);
        // The listener is still serving.
        send(&p, b"Stop\n{}").unwrap();
        assert_eq!(rx.recv_timeout(t).unwrap(), b"Stop\n{}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bind_waits_for_a_stopping_daemon() {
        use std::time::{Duration, Instant};
        let p = test_addr("wait");
        let l = Listener::bind(&p).unwrap();
        // Still held after the wait: gives up with AddrInUse.
        let t = Instant::now();
        let e = bind_waiting(&p, Duration::from_millis(300)).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
        assert!(t.elapsed() >= Duration::from_millis(250));
        // Released while waiting (old daemon finished shutting down).
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            drop(l);
        });
        drop(bind_waiting(&p, Duration::from_secs(5)).unwrap());
    }

    /// Named pipe behavior that can only be checked on Windows.
    #[cfg(windows)]
    mod windows {
        use super::*;
        use std::os::windows::io::AsRawHandle;
        use std::time::Duration;
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, LocalFree};
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows_sys::Win32::Security::{
            ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION, AclSizeInformation, CreateRestrictedToken, CreateWellKnownSid,
            DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetLengthSid, GetSecurityDescriptorControl,
            GetTokenInformation, ImpersonateLoggedOnUser, OWNER_SECURITY_INFORMATION, PSID, RevertToSelf,
            SE_DACL_PROTECTED, SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
            TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_IMPERSONATE, TOKEN_MANDATORY_LABEL,
            TOKEN_OWNER, TOKEN_QUERY, TokenElevation, TokenIntegrityLevel, TokenOwner, WinBuiltinAdministratorsSid,
        };
        use windows_sys::Win32::Storage::FileSystem::{FILE_ALL_ACCESS, READ_CONTROL};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // From Win32_System_SystemServices, a feature we don't otherwise need.
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
        const SE_GROUP_INTEGRITY: u32 = 0x20;

        /// A client handle that may read the pipe's security descriptor.
        fn open_read_control(p: &Path) -> std::fs::File {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().access_mode(READ_CONTROL).open(p).unwrap()
        }

        fn process_token(access: u32) -> HANDLE {
            let mut t: HANDLE = std::ptr::null_mut();
            // SAFETY: pseudo-handle and a valid out-pointer.
            assert_ne!(unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut t) }, 0);
            t
        }

        /// Read token information class `class` into `out`, `len` bytes.
        /// Fixed-size classes (TokenElevation) insist on their exact size
        /// (ERROR_BAD_LENGTH otherwise; Wine doesn't check); variable-size
        /// ones (TokenOwner) take any buffer big enough.
        fn read_token_info(class: i32, out: *mut std::ffi::c_void, len: u32) {
            let t = process_token(TOKEN_QUERY);
            let mut got = 0u32;
            // SAFETY: `out` is writable for `len` bytes (callers); `t` is open.
            let ok = unsafe { GetTokenInformation(t, class, out, len, &mut got) };
            let err = io::Error::last_os_error();
            // SAFETY: closing the token we opened.
            unsafe { CloseHandle(t) };
            assert_ne!(ok, 0, "token information class {class}: {err}");
        }

        fn is_elevated() -> bool {
            let mut e = TOKEN_ELEVATION::default();
            let len = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
            read_token_info(TokenElevation, (&mut e as *mut TOKEN_ELEVATION).cast(), len);
            e.TokenIsElevated != 0
        }

        /// The owner new objects get: Administrators for an elevated admin.
        fn default_owner() -> String {
            // TOKEN_OWNER plus the SID it points into; usize keeps it aligned.
            let mut buf = [0usize; 32];
            read_token_info(TokenOwner, buf.as_mut_ptr().cast(), std::mem::size_of_val(&buf) as u32);
            // SAFETY: on success `buf` starts with an initialized, aligned
            // TOKEN_OWNER whose SID points into `buf`, still alive.
            unsafe { sid_string((*buf.as_ptr().cast::<TOKEN_OWNER>()).Owner) }.unwrap()
        }

        /// Impersonates a token on this thread until dropped.
        struct Impersonating;
        impl Drop for Impersonating {
            fn drop(&mut self) {
                // SAFETY: ends this thread's impersonation.
                unsafe { RevertToSelf() };
            }
        }

        /// Impersonate what a non-elevated hook of the same user looks like
        /// to an access check: our token with `BUILTIN\Administrators`
        /// deny-only, at medium integrity.
        fn impersonate_non_elevated() -> Impersonating {
            // The restricted token gets the same access; SetTokenInformation
            // needs TOKEN_ADJUST_DEFAULT.
            let own = process_token(
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
            );
            let mut admins = [0u8; SECURITY_MAX_SID_SIZE as usize];
            let mut n = admins.len() as u32;
            // SAFETY: `admins` is writable for `n` bytes.
            let ok = unsafe {
                CreateWellKnownSid(
                    WinBuiltinAdministratorsSid,
                    std::ptr::null_mut(),
                    admins.as_mut_ptr().cast(),
                    &mut n,
                )
            };
            assert_ne!(ok, 0, "{}", io::Error::last_os_error());
            let deny = SID_AND_ATTRIBUTES { Sid: admins.as_mut_ptr().cast(), Attributes: 0 };
            let mut t: HANDLE = std::ptr::null_mut();
            // SAFETY: `own` is open with TOKEN_DUPLICATE; one SID to make
            // deny-only, no privileges or restricting SIDs; valid out-pointer.
            let ok =
                unsafe { CreateRestrictedToken(own, 0, 1, &deny, 0, std::ptr::null(), 0, std::ptr::null(), &mut t) };
            // SAFETY: closing the token we opened.
            unsafe { CloseHandle(own) };
            assert_ne!(ok, 0, "{}", io::Error::last_os_error());
            let medium: Vec<u16> = "S-1-16-8192".encode_utf16().chain(Some(0)).collect();
            let mut sid: PSID = std::ptr::null_mut();
            // SAFETY: NUL-terminated string, valid out-pointer; freed below.
            assert_ne!(unsafe { ConvertStringSidToSidW(medium.as_ptr(), &mut sid) }, 0);
            let mut label = TOKEN_MANDATORY_LABEL::default();
            label.Label.Sid = sid;
            label.Label.Attributes = SE_GROUP_INTEGRITY;
            // The documented length: the label plus the SID it points to.
            // SAFETY: `sid` is a valid SID.
            let len = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + unsafe { GetLengthSid(sid) };
            // SAFETY: `t` is a token we own with TOKEN_ADJUST_DEFAULT; `label`
            // and `sid` outlive the call.
            let ok = unsafe {
                SetTokenInformation(t, TokenIntegrityLevel, (&label as *const TOKEN_MANDATORY_LABEL).cast(), len)
            };
            let err = io::Error::last_os_error();
            // SAFETY: allocated by ConvertStringSidToSidW, freed once.
            unsafe { LocalFree(sid) };
            assert_ne!(ok, 0, "TokenIntegrityLevel: {err}");
            // SAFETY: `t` is a valid token; the guard reverts.
            let ok = unsafe { ImpersonateLoggedOnUser(t) };
            // SAFETY: the impersonation token is a copy; ours can go.
            unsafe { CloseHandle(t) };
            assert_ne!(ok, 0, "{}", io::Error::last_os_error());
            Impersonating
        }

        /// Run `f` on a fresh thread as a non-elevated client.
        fn as_non_elevated<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
            std::thread::spawn(move || {
                let _imp = impersonate_non_elevated();
                f()
            })
            .join()
            .unwrap()
        }

        #[test]
        fn pipe_dacl_admits_only_the_user() {
            let p = test_addr("dacl");
            let _l = Listener::bind(&p).unwrap();
            let f = open_read_control(&p);
            let si = SecurityInfo::query(f.as_raw_handle(), DACL_SECURITY_INFORMATION).unwrap();
            let (mut control, mut rev) = (0u16, 0u32);
            // SAFETY: `si.sd` is a valid descriptor; valid out-pointers.
            assert_ne!(unsafe { GetSecurityDescriptorControl(si.sd, &mut control, &mut rev) }, 0);
            assert_ne!(control & SE_DACL_PROTECTED, 0, "DACL must not inherit");
            assert!(!si.dacl.is_null(), "a null DACL grants everyone");
            let mut size = ACL_SIZE_INFORMATION::default();
            // SAFETY: valid ACL and a buffer of the stated size.
            let ok = unsafe {
                GetAclInformation(
                    si.dacl,
                    (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            };
            assert_ne!(ok, 0);
            assert_eq!(size.AceCount, 1);
            let mut ace = std::ptr::null_mut();
            // SAFETY: index 0 exists; valid out-pointer.
            assert_ne!(unsafe { GetAce(si.dacl, 0, &mut ace) }, 0);
            // SAFETY: GetAce points into the DACL, alive with `si`.
            let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            assert_eq!(ace.Header.AceType, ACCESS_ALLOWED_ACE_TYPE);
            // GA may be stored as given or mapped to the pipe's specific rights.
            assert!(ace.Mask == GENERIC_ALL || ace.Mask == FILE_ALL_ACCESS, "mask {:#x}", ace.Mask);
            // SAFETY: the ACE's SID starts at SidStart.
            let sid = unsafe { sid_string((&ace.SidStart as *const u32).cast_mut().cast()) }.unwrap();
            assert_eq!(sid, current_user_sid().unwrap());
        }

        #[test]
        fn our_pipe_passes_the_owner_check() {
            let p = test_addr("owner");
            let rx = serve(Listener::bind(&p).unwrap());
            let f = open_read_control(&p);
            let si = SecurityInfo::query(f.as_raw_handle(), OWNER_SECURITY_INFORMATION).unwrap();
            let owner = si.owner_string().unwrap();
            assert!(pipe_owner_trusted(&owner, &current_user_sid().unwrap()), "owner {owner}");
            drop(f);
            send_to(&p, false, b"Stop\n{}").unwrap();
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), b"Stop\n{}");
            // A non-elevated hook writing to an (elevated) daemon's pipe.
            let q = p.clone();
            as_non_elevated(move || send_to(&q, false, b"Stop\n{}")).unwrap();
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), b"Stop\n{}");
        }

        #[test]
        fn elevated_daemon_accepts_a_non_elevated_hook() {
            let p = test_addr("elev");
            let rx = serve(Listener::bind(&p).unwrap());
            let q = p.clone();
            as_non_elevated(move || send(&q, b"Stop\n{}")).unwrap();
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), b"Stop\n{}");

            // Negative control: with `OW` (owner rights) an elevated daemon's
            // pipe, owned by Administrators, shuts that same client out. Only
            // meaningful when new objects really are owned by Administrators
            // (not the user, nor Wine's primary group).
            let owner = default_owner();
            let elevated = is_elevated();
            // GitHub's runners must actually run it (windows-latest is elevated);
            // elsewhere the precondition may legitimately not hold.
            let in_ci = std::env::var_os("GITHUB_ACTIONS").is_some_and(|v| v == "true");
            assert!(
                !in_ci || (elevated && owner == "S-1-5-32-544"),
                "CI must run the OW negative control: elevated={elevated}, default owner {owner} (want S-1-5-32-544)"
            );
            if elevated && owner == "S-1-5-32-544" {
                let q = test_addr("elev-ow");
                let _l = Listener::bind_sddl(&q, "D:P(A;;GA;;;OW)").unwrap();
                let e = as_non_elevated(move || send(&q, b"Stop\n{}")).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
            } else {
                eprintln!(
                    "elevated={elevated}, default owner {owner}, not elevated Administrators: skipping the OW negative control"
                );
            }
        }

        #[test]
        fn non_elevated_client_reads_the_state_reply() {
            // An elevated daemon's pipe (owned by Administrators) answering a
            // TUI that runs non-elevated.
            let p = test_addr("state-elev");
            let _rx = serve_state(Listener::bind(&p).unwrap(), Some(b"{\"v\":1}\n".to_vec()));
            let q = p.clone();
            let r = as_non_elevated(move || query_state(&q, false, Duration::from_secs(5))).unwrap();
            assert_eq!(r.as_deref(), Some("{\"v\":1}"));
        }

        #[test]
        fn older_inbound_only_pipe_reads_as_an_older_daemon() {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
            use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT};
            // What releases before `__state` created: an inbound-only pipe.
            let p = test_addr("inbound");
            let name: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: valid NUL-terminated name; default security; closed below.
            let h = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_INBOUND,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    0,
                    64 * 1024,
                    0,
                    std::ptr::null(),
                )
            };
            assert_ne!(h, INVALID_HANDLE_VALUE);
            assert_eq!(query_state(&p, false, Duration::from_secs(2)).unwrap(), None);
            // SAFETY: the handle we created, closed once.
            unsafe { CloseHandle(h) };
        }

        #[test]
        fn message_sent_before_serving_is_delivered() {
            // The client connects, writes and closes before ConnectNamedPipe,
            // which then reports ERROR_NO_DATA; the bytes are still buffered.
            let p = test_addr("nodata");
            let l = Listener::bind(&p).unwrap();
            send(&p, b"Stop\n{}").unwrap();
            let rx = serve(l);
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), b"Stop\n{}");
        }
    }

    /// Tests that need a Unix socket or POSIX permissions.
    #[cfg(unix)]
    mod unix {
        use super::*;

        #[test]
        fn private_dir_is_verified() {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
            let base = std::env::temp_dir().join(format!("cp-priv-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            // Another test's bind may have the process umask at 0o177 right now.
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();

            // Fresh: created owner-only; an existing good dir is accepted again.
            let fresh = base.join("fresh");
            ensure_private_dir(&fresh).unwrap();
            assert_eq!(mode(&fresh), 0o700);
            ensure_private_dir(&fresh).unwrap();

            // Group/other access: rejected and left untouched.
            let open = base.join("open");
            std::fs::create_dir(&open).unwrap();
            std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(ensure_private_dir(&open).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(mode(&open), 0o755);
            // ...and the daemon refuses to put its socket there.
            let sock = open.join("hook.sock");
            assert!(Listener::bind_with(&sock, true).is_err());
            assert!(!sock.exists());
            // Without the private-dir requirement (XDG_RUNTIME_DIR etc.) it binds.
            drop(Listener::bind_with(&sock, false).unwrap());

            // A symlink, even to a good dir, and a plain file are rejected.
            let link = base.join("link");
            std::os::unix::fs::symlink(&fresh, &link).unwrap();
            assert!(ensure_private_dir(&link).is_err());
            let file = base.join("file");
            std::fs::write(&file, "").unwrap();
            assert!(ensure_private_dir(&file).is_err());

            std::fs::remove_dir_all(&base).unwrap();
        }

        #[test]
        fn private_dir_rules() {
            // Someone else's dir.
            assert!(check_private(true, 1, 0o40700, 2).is_err());
            assert!(check_private(true, 2, 0o40700, 2).is_ok());
            assert!(check_private(false, 2, 0o700, 2).is_err());
            assert!(check_private(true, 2, 0o40701, 2).is_err());
            // Owner must have rwx (a concurrent umask can't leave it unusable).
            assert!(check_private(true, 2, 0o40500, 2).is_err());
            assert!(check_private(true, 2, 0o40600, 2).is_err());
        }

        #[test]
        fn parent_must_not_let_others_swap_the_dir() {
            // Ours or root's, and sticky or not writable by others.
            assert!(check_parent(true, 0, 0o41777, 5).is_ok()); // /tmp
            assert!(check_parent(true, 5, 0o40755, 5).is_ok()); // ~/tmp
            assert!(check_parent(true, 5, 0o40700, 5).is_ok());
            assert!(check_parent(true, 5, 0o40777, 5).is_err()); // world-writable, not sticky
            assert!(check_parent(true, 0, 0o40777, 5).is_err());
            assert!(check_parent(true, 5, 0o40775, 5).is_err()); // group-writable
            assert!(check_parent(true, 7, 0o41777, 5).is_err()); // another user's: they can rename anyway
            assert!(check_parent(false, 5, 0o100700, 5).is_err());
        }

        #[test]
        fn non_sticky_world_writable_parent_is_refused() {
            use std::os::unix::fs::PermissionsExt;
            let base = std::env::temp_dir().join(format!("cp-ww-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            // Another test's bind may have the process umask at 0o177 right now.
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
            let ww = base.join("ww");
            std::fs::create_dir(&ww).unwrap();
            let good = ww.join("good");
            std::fs::create_dir(&good).unwrap();
            std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o700)).unwrap();
            let l = Listener::bind_with(&good.join("s"), false).unwrap();
            std::thread::spawn(move || l.serve(|_| true));

            std::fs::set_permissions(&ww, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(ensure_private_dir(&good).is_err(), "daemon side");
            assert!(send_to(&good.join("s"), true, b"Stop\n{}").is_err(), "client side");
            std::fs::set_permissions(&ww, std::fs::Permissions::from_mode(0o1777)).unwrap();
            ensure_private_dir(&good).unwrap();
            send_to(&good.join("s"), true, b"Stop\n{}").unwrap();

            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn legacy_socket_rules() {
            assert!(check_legacy(true, 5, 5).is_ok());
            assert!(check_legacy(true, 6, 5).is_err(), "another user's socket");
            assert!(check_legacy(true, 0, 5).is_err(), "root's socket");
            assert!(check_legacy(false, 5, 5).is_err(), "not a socket");
        }

        #[test]
        fn hook_falls_back_to_a_legacy_daemon() {
            use std::os::unix::fs::PermissionsExt;
            use std::time::Duration;
            let base = std::env::temp_dir().join(format!("cp-legacy-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            // Another test's bind may have the process umask at 0o177 right now.
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
            let old = base.join("old.sock");
            let rx = serve(Listener::bind_with(&old, false).unwrap());
            let legacy = || Some(old.clone());

            // Upgraded hook, old daemon: the private dir doesn't exist yet.
            let current = base.join("priv").join("hook.sock");
            send_hook(&current, true, legacy, b"Stop\n{}").unwrap();
            assert_eq!(rx.recv().unwrap(), b"Stop\n{}");
            // Nothing listens at today's path (stale socket file).
            let stale = base.join("stale.sock");
            drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
            send_hook(&stale, false, legacy, b"Stop\n{}").unwrap();
            assert_eq!(rx.recv().unwrap(), b"Stop\n{}");

            // Today's daemon wins; the legacy path isn't even computed.
            let new = base.join("new.sock");
            let rx_new = serve(Listener::bind_with(&new, false).unwrap());
            send_hook(&new, false, || panic!("legacy looked up"), b"Stop\n{}").unwrap();
            assert_eq!(rx_new.recv().unwrap(), b"Stop\n{}");

            // A squatter's private dir is refused outright, not bypassed.
            let open = base.join("open");
            std::fs::create_dir(&open).unwrap();
            std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(send_hook(&open.join("hook.sock"), true, legacy, b"Stop\n{}").is_err());
            // Only a real socket: not a symlink to one, not a plain file.
            let link = base.join("link.sock");
            std::os::unix::fs::symlink(&old, &link).unwrap();
            assert!(send_hook(&current, true, || Some(link.clone()), b"Stop\n{}").is_err());
            let file = base.join("file.sock");
            std::fs::write(&file, "").unwrap();
            assert!(send_hook(&current, true, || Some(file.clone()), b"Stop\n{}").is_err());
            assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
            // Nor in a dir where others could swap it (world-writable, not sticky).
            let ww = base.join("ww");
            std::fs::create_dir(&ww).unwrap();
            let ww_sock = ww.join("old.sock");
            let rx_ww = serve(Listener::bind_with(&ww_sock, false).unwrap());
            std::fs::set_permissions(&ww, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(send_hook(&current, true, || Some(ww_sock.clone()), b"Stop\n{}").is_err());
            assert!(rx_ww.recv_timeout(Duration::from_millis(200)).is_err());
            std::fs::set_permissions(&ww, std::fs::Permissions::from_mode(0o1777)).unwrap();
            send_hook(&current, true, || Some(ww_sock.clone()), b"Stop\n{}").unwrap();
            assert_eq!(rx_ww.recv().unwrap(), b"Stop\n{}");

            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn client_refuses_unverified_fallback_dir() {
            use std::os::unix::fs::PermissionsExt;
            use std::time::Duration;
            let base = std::env::temp_dir().join(format!("cp-cli-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            // Another test's bind may have the process umask at 0o177 right now.
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
            let serve = |sock: &Path| {
                let l = Listener::bind_with(sock, false).unwrap();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || l.serve(move |m| tx.send(m).is_ok()));
                rx
            };

            // A squatter's open dir with a live listener: nothing is sent.
            let open = base.join("open");
            std::fs::create_dir(&open).unwrap();
            std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
            let rx = serve(&open.join("s"));
            assert!(send_to(&open.join("s"), true, b"Stop\n{}").is_err());
            // Nor a state query (whose reply the TUI trusts) or a reload.
            let t = Duration::from_secs(2);
            assert_eq!(query_state(&open.join("s"), true, t).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert!(send_reload(&open.join("s"), true).is_err());
            assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
            // Outside the fallback dir no check is made.
            send_to(&open.join("s"), false, b"Stop\n{}").unwrap();
            assert_eq!(rx.recv().unwrap(), b"Stop\n{}");

            // A good dir works; a symlink to it is refused.
            let good = base.join("good");
            ensure_private_dir(&good).unwrap();
            let rx = serve(&good.join("s"));
            send_to(&good.join("s"), true, b"Stop\n{}").unwrap();
            assert_eq!(rx.recv().unwrap(), b"Stop\n{}");
            let link = base.join("link");
            std::os::unix::fs::symlink(&good, &link).unwrap();
            assert!(send_to(&link.join("s"), true, b"Stop\n{}").is_err());
            assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());

            let _ = std::fs::remove_dir_all(&base);
        }
    }
}
