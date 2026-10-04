//! Hook → daemon channel. A hook invocation connects, writes
//! `<EventName>\n<raw JSON from Claude Code>`, and disconnects. The daemon
//! does all parsing, so the hook process stays a few hundred microseconds of
//! work. Unix domain socket (mode 0600, in a directory only we can enter) on
//! Linux/macOS, a local-only named pipe with an owner-only DACL on Windows.
//!
//! Event names starting with `__` are reserved for control messages sent by
//! claude-presence itself (`__shutdown`); the `hook` command never forwards
//! them.

use std::io;
use std::path::Path;

/// Largest message the daemon accepts (Write tool payloads carry file contents).
pub const MAX_MSG: usize = 16 << 20;

/// Control event: stop the daemon cleanly, like SIGTERM.
pub const SHUTDOWN: &str = "__shutdown";

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
    let mut m = SHUTDOWN.as_bytes().to_vec();
    m.push(b'\n');
    m
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
/// `paths::private_socket_dir()` (`in_private_dir`), first check with one
/// `lstat` that the directory is ours: a squatter who pre-created it must not
/// receive hook payloads. Once it passes, nobody else can swap it: it is
/// owned by us with mode 0700, and in the usual sticky `/tmp` only its owner
/// may rename or remove it. OS-provided runtime dirs cost nothing extra.
#[cfg(unix)]
pub fn send_to(addr: &Path, in_private_dir: bool, msg: &[u8]) -> io::Result<()> {
    if let (true, Some(dir)) = (in_private_dir, addr.parent()) {
        verify_private_dir(dir)?;
    }
    send(addr, msg)
}

/// `send` for hook events; named pipes carry their own DACL on Windows.
#[cfg(windows)]
pub fn send_to(addr: &Path, _in_private_dir: bool, msg: &[u8]) -> io::Result<()> {
    send(addr, msg)
}

#[cfg(windows)]
pub fn send(addr: &Path, msg: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
    let wide: Vec<u16> = addr.as_os_str().encode_wide().chain(Some(0)).collect();
    for _ in 0..10 {
        match std::fs::OpenOptions::new().write(true).open(addr) {
            Ok(mut f) => return f.write_all(msg),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                // SAFETY: NUL-terminated wide string.
                unsafe { WaitNamedPipeW(wide.as_ptr(), 250) };
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "daemon pipe busy"))
}

/// True if a daemon is accepting connections at `addr`.
pub fn daemon_running(addr: &Path) -> bool {
    send(addr, b"").is_ok()
}

// ------------------------------------------------------------------ server --

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
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    verify_private_dir(dir)
}

/// `lstat` `dir` and apply `check_private`, without creating anything.
#[cfg(unix)]
fn verify_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(dir)?;
    // SAFETY: geteuid never fails.
    check_private(m.file_type().is_dir(), m.uid(), m.mode(), unsafe { libc::geteuid() })
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", dir.display())))
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
            std::fs::remove_file(addr)?;
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
    /// (which returns `false` to stop).
    pub fn serve(self, mut on_msg: impl FnMut(Vec<u8>) -> bool) {
        use std::io::Read;
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
            if (&mut s).take(MAX_MSG as u64).read_to_end(&mut buf).is_ok() && !buf.is_empty() && !on_msg(buf) {
                return;
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

/// An owner-only security descriptor, `D:P(A;;GA;;;OW)`: a protected DACL
/// whose single ACE grants the object's owner full access. Other users can
/// neither connect to our pipe nor add instances to it.
#[cfg(windows)]
struct OwnerOnly(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl OwnerOnly {
    fn new() -> io::Result<OwnerOnly> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let sddl: Vec<u16> = "D:P(A;;GA;;;OW)".encode_utf16().chain(Some(0)).collect();
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
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
        use windows_sys::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
            PIPE_WAIT,
        };
        let flags = PIPE_ACCESS_INBOUND | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let sa = sd.attributes();
        // SAFETY: valid NUL-terminated name; `sa` and the descriptor it points
        // to outlive the call.
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                0,
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
        Ok(h)
    }

    pub fn bind(addr: &Path) -> io::Result<Listener> {
        use std::os::windows::ffi::OsStrExt;
        let name: Vec<u16> = addr.as_os_str().encode_wide().chain(Some(0)).collect();
        let sd = OwnerOnly::new()?;
        let first = Self::create(&name, &sd, true)?;
        Ok(Listener { name, sd, first })
    }

    /// Read one client's message until it closes its end (`ERROR_BROKEN_PIPE`).
    /// Empty if it sent nothing or more than `MAX_MSG`.
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
            if buf.len() > MAX_MSG {
                buf.clear();
                return buf;
            }
        }
    }

    pub fn serve(self, mut on_msg: impl FnMut(Vec<u8>) -> bool) {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, GetLastError, HANDLE};
        use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, DisconnectNamedPipe};
        let close = |h: HANDLE| {
            // SAFETY: closing a server pipe handle we own, exactly once.
            unsafe {
                DisconnectNamedPipe(h);
                CloseHandle(h);
            }
        };
        let mut next = self.first;
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
            if connected {
                let buf = Self::read_msg(cur);
                keep = buf.is_empty() || on_msg(buf);
            }
            close(cur);
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let p = std::env::temp_dir().join(format!("cp-ipc-{}.sock", std::process::id()));
        let l = Listener::bind(&p).unwrap();
        assert_eq!(Listener::bind(&p).err().map(|e| e.kind()), Some(io::ErrorKind::AddrInUse));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || l.serve(move |m| tx.send(m).is_ok()));
        send(&p, b"Stop\n{}").unwrap();
        assert!(daemon_running(&p));
        send(&p, b"PreToolUse\n{\"a\":1}").unwrap();
        assert_eq!(rx.recv().unwrap(), b"Stop\n{}");
        assert_eq!(rx.recv().unwrap(), b"PreToolUse\n{\"a\":1}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn shutdown_request_roundtrip() {
        let p = std::env::temp_dir().join(format!("cp-ipc-stop-{}.sock", std::process::id()));
        let l = Listener::bind(&p).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || l.serve(move |m| tx.send(m).is_ok()));
        send(&p, &shutdown_request()).unwrap();
        let m = rx.recv().unwrap();
        assert_eq!(event_name(&m), SHUTDOWN.as_bytes());
        assert!(is_control(event_name(&m)));
        let _ = std::fs::remove_file(&p);
    }

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
    }

    #[test]
    fn private_dir_is_verified() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let base = std::env::temp_dir().join(format!("cp-priv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

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

        // Someone else's dir.
        assert!(check_private(true, 1, 0o40700, 2).is_err());
        assert!(check_private(true, 2, 0o40700, 2).is_ok());
        assert!(check_private(false, 2, 0o700, 2).is_err());
        assert!(check_private(true, 2, 0o40701, 2).is_err());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn client_refuses_unverified_fallback_dir() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;
        let base = std::env::temp_dir().join(format!("cp-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
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
