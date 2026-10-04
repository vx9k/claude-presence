//! Hook → daemon channel. A hook invocation connects, writes
//! `<EventName>\n<raw JSON from Claude Code>`, and disconnects. The daemon
//! does all parsing, so the hook process stays a few hundred microseconds of
//! work. Unix domain socket (mode 0600) on Linux/macOS, a local-only named
//! pipe on Windows.

use std::io;
use std::path::Path;

/// Largest message the daemon accepts (Write tool payloads carry file contents).
pub const MAX_MSG: usize = 16 << 20;

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

#[cfg(unix)]
impl Listener {
    pub fn bind(addr: &Path) -> io::Result<Listener> {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::{UnixListener, UnixStream};
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
            let Ok(mut s) = conn else { continue };
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

#[cfg(windows)]
pub struct Listener {
    name: Vec<u16>,
    first: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: a pipe HANDLE is just a kernel object reference.
#[cfg(windows)]
unsafe impl Send for Listener {}

#[cfg(windows)]
impl Listener {
    fn create(name: &[u16], first: bool) -> io::Result<windows_sys::Win32::Foundation::HANDLE> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
        use windows_sys::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
            PIPE_WAIT,
        };
        let flags = PIPE_ACCESS_INBOUND | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        // SAFETY: valid NUL-terminated name; default security descriptor
        // (owner full control, everyone else read-only — i.e. cannot send).
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                0,
                64 * 1024,
                0,
                std::ptr::null(),
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
        let first = Self::create(&name, true)?;
        Ok(Listener { name, first })
    }

    pub fn serve(self, mut on_msg: impl FnMut(Vec<u8>) -> bool) {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, GetLastError};
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, DisconnectNamedPipe};
        let mut next = self.first;
        loop {
            // SAFETY: `next` is a valid server pipe handle we own.
            let ok = unsafe { ConnectNamedPipe(next, std::ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            let cur = next;
            // Keep an instance listening while we read this one, so the pipe
            // name can never be grabbed by another process in between.
            next = match Self::create(&self.name, false) {
                Ok(h) => h,
                Err(e) => {
                    crate::error!("pipe: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    // SAFETY: closing our own handle.
                    unsafe {
                        DisconnectNamedPipe(cur);
                        CloseHandle(cur);
                    }
                    match Self::create(&self.name, false) {
                        Ok(h) => h,
                        Err(_) => return,
                    }
                }
            };
            if ok {
                let mut buf = Vec::with_capacity(4096);
                let mut chunk = [0u8; 16 * 1024];
                loop {
                    let mut n = 0u32;
                    // SAFETY: chunk is a valid writable buffer of the given length.
                    let r =
                        unsafe { ReadFile(cur, chunk.as_mut_ptr(), chunk.len() as u32, &mut n, std::ptr::null_mut()) };
                    if r == 0 || n == 0 {
                        break; // ERROR_BROKEN_PIPE: client finished writing
                    }
                    buf.extend_from_slice(&chunk[..n as usize]);
                    if buf.len() > MAX_MSG {
                        buf.clear();
                        break;
                    }
                }
                if !buf.is_empty() && !on_msg(buf) {
                    return;
                }
            }
            // SAFETY: closing our own handle.
            unsafe {
                DisconnectNamedPipe(cur);
                CloseHandle(cur);
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
}
