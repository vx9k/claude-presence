//! Discord Rich Presence over the local IPC socket — just the slice we need:
//! handshake, `SET_ACTIVITY`, and clearing it.
//!
//! Wire format: `<op: u32 LE> <len: u32 LE> <len bytes of JSON>`.
//! op 0 = handshake, 1 = frame, 2 = close, 3 = ping, 4 = pong.
//!
//! All socket I/O happens on one worker thread that owns the connection, so a
//! wedged Discord client can never stall hook processing. The worker also
//! enforces Discord's activity rate limit (≤ 4 updates / 20 s, ≥ 4 s apart):
//! rapid changes coalesce to the latest one.

use serde::Deserialize;
use sonic_rs::{JsonValueTrait, LazyValue};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;
const MAX_FRAME: usize = 1 << 20;

/// arRPC (Vesktop, Equibop, web-client bridges) answers READY with this mock
/// user. Bridges drop the activity when their renderer reloads, so we
/// re-assert more often against them.
const ARRPC_USER_ID: &str = "1045800378228281345";

const MIN_GAP: Duration = Duration::from_secs(4);
const WINDOW: Duration = Duration::from_secs(20);
const MAX_PER_WINDOW: usize = 4;
const KEEPALIVE: Duration = Duration::from_secs(60);
const KEEPALIVE_BRIDGE: Duration = Duration::from_secs(20);

/// Possible Discord IPC endpoints, in preference order.
pub fn candidate_paths() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        (0..10).map(|i| PathBuf::from(format!(r"\\.\pipe\discord-ipc-{i}"))).collect()
    }
    #[cfg(unix)]
    {
        let mut bases: Vec<PathBuf> = Vec::new();
        #[cfg(target_os = "macos")]
        if let Some(p) = crate::paths::darwin_user_temp_dir() {
            bases.push(p);
        }
        for key in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
            if let Some(v) = std::env::var_os(key).filter(|v| !v.is_empty()) {
                bases.push(PathBuf::from(v));
            }
        }
        #[cfg(target_os = "linux")]
        bases.push(crate::paths::runtime_dir());
        bases.push(PathBuf::from("/tmp"));
        let mut dirs = Vec::new();
        for b in &bases {
            dirs.push(b.clone());
            #[cfg(target_os = "linux")]
            for sub in [
                "app/com.discordapp.Discord",
                "app/com.discordapp.DiscordCanary",
                "app/com.discordapp.DiscordPTB",
                "app/dev.vencord.Vesktop",
                "app/io.github.equicord.equibop",
                ".flatpak/dev.vencord.Vesktop/xdg-run",
                "snap.discord",
                "snap.discord-canary",
            ] {
                dirs.push(b.join(sub));
            }
        }
        let mut out: Vec<PathBuf> = Vec::new();
        for d in dirs.iter().filter(|d| d.is_dir()) {
            for i in 0..10 {
                let p = d.join(format!("discord-ipc-{i}"));
                if !out.contains(&p) && p.exists() {
                    out.push(p);
                }
            }
        }
        out
    }
}

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Stream = std::fs::File;

pub struct Conn {
    s: Stream,
    pub bridge: bool,
    nonce: u64,
    buf: Vec<u8>,
}

fn open(p: &std::path::Path) -> io::Result<Stream> {
    #[cfg(unix)]
    {
        let s = Stream::connect(p)?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(s)
    }
    #[cfg(windows)]
    {
        std::fs::OpenOptions::new().read(true).write(true).open(p)
    }
}

impl Conn {
    pub fn connect(client_id: &str, paths: &[PathBuf]) -> io::Result<Conn> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, "Discord is not running");
        for p in paths {
            let s = match open(p) {
                Ok(s) => s,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            let mut c = Conn { s, bridge: false, nonce: 0, buf: Vec::with_capacity(4096) };
            match c.handshake(client_id) {
                Ok(()) => return Ok(c),
                // A bad client id is fatal for every socket; stop probing.
                Err(e) if e.kind() == io::ErrorKind::InvalidInput => return Err(e),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn handshake(&mut self, client_id: &str) -> io::Result<()> {
        let body = format!(r#"{{"v":1,"client_id":{}}}"#, sonic_rs::to_string(client_id).map_err(io::Error::other)?);
        self.write_frame(OP_HANDSHAKE, body.as_bytes())?;
        loop {
            match self.read_frame()? {
                OP_FRAME => {
                    let Ok(r) = sonic_rs::from_slice::<Reply<'_>>(&self.buf) else { continue };
                    if r.evt.as_deref() == Some("READY") {
                        self.bridge = r
                            .data
                            .as_ref()
                            .and_then(|d| d.pointer(["user", "id"]))
                            .is_some_and(|id| id.as_str() == Some(ARRPC_USER_ID));
                        return Ok(());
                    }
                }
                OP_CLOSE => {
                    let msg = String::from_utf8_lossy(&self.buf).into_owned();
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
                }
                OP_PING => self.pong()?,
                _ => {}
            }
        }
    }

    fn write_frame(&mut self, op: u32, body: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(body);
        self.s.write_all(&frame)?;
        self.s.flush()
    }

    /// Read one frame into `self.buf`, returning its opcode.
    fn read_frame(&mut self) -> io::Result<u32> {
        read_frame_from(&mut self.s, &mut self.buf)
    }

    fn pong(&mut self) -> io::Result<()> {
        let body = std::mem::take(&mut self.buf);
        let r = self.write_frame(OP_PONG, &body);
        self.buf = body;
        r
    }

    /// Set (or with `None`, clear) the activity. `activity` is a JSON object.
    /// Returns `InvalidData` when Discord rejected the payload but the
    /// connection itself is fine.
    pub fn set_activity(&mut self, activity: Option<&str>) -> io::Result<()> {
        self.nonce += 1;
        let pid = std::process::id();
        let nonce = self.nonce;
        let body = match activity {
            Some(a) => format!(r#"{{"cmd":"SET_ACTIVITY","args":{{"pid":{pid},"activity":{a}}},"nonce":"{nonce}"}}"#),
            None => format!(r#"{{"cmd":"SET_ACTIVITY","args":{{"pid":{pid}}},"nonce":"{nonce}"}}"#),
        };
        self.write_frame(OP_FRAME, body.as_bytes())?;
        let nonce = nonce.to_string();
        loop {
            match self.read_frame()? {
                OP_FRAME => {
                    let Ok(r) = sonic_rs::from_slice::<Reply<'_>>(&self.buf) else { continue };
                    if r.nonce.as_deref() != Some(nonce.as_str()) {
                        continue;
                    }
                    if r.evt.as_deref() == Some("ERROR") {
                        let msg = r
                            .data
                            .as_ref()
                            .and_then(|d| d.get("message").and_then(|m| m.as_str().map(str::to_owned)))
                            .unwrap_or_else(|| String::from_utf8_lossy(&self.buf).into_owned());
                        return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
                    }
                    return Ok(());
                }
                OP_CLOSE => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "closed by Discord")),
                OP_PING => self.pong()?,
                _ => {}
            }
        }
    }
}

/// Read one frame from `r` into `buf`, returning its opcode.
fn read_frame_from(r: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<u32> {
    let mut hdr = [0u8; 8];
    r.read_exact(&mut hdr)?;
    let op = u32::from_le_bytes(hdr[..4].try_into().unwrap());
    let len = u32::from_le_bytes(hdr[4..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        // Not InvalidData: that means "activity rejected, connection fine".
        return Err(io::Error::other("oversized frame"));
    }
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf)?;
    Ok(op)
}

/// The fields of a Discord IPC reply we look at.
#[derive(Deserialize)]
struct Reply<'a> {
    #[serde(borrow, default)]
    evt: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    nonce: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    data: Option<LazyValue<'a>>,
}

// ------------------------------------------------------------ worker thread --

#[derive(Default)]
struct Want {
    activity: Option<String>,
    generation: u64,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    want: Mutex<Want>,
    cv: Condvar,
}

/// How long `Presenter::shutdown` waits for the worker to clear the
/// activity. A worker stuck in blocking I/O (Windows pipes have no timeout)
/// is left behind; process exit ends it.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

/// Handle to the Discord worker thread.
pub struct Presenter {
    shared: Arc<Shared>,
    /// The worker and a channel it signals right before exiting.
    thread: Option<(JoinHandle<()>, Receiver<()>)>,
}

impl Presenter {
    pub fn spawn(client_id: String) -> Presenter {
        Presenter::spawn_with(move |s| worker(s, &client_id))
    }

    fn spawn_with(work: impl FnOnce(&Shared) + Send + 'static) -> Presenter {
        let shared = Arc::new(Shared::default());
        let s2 = shared.clone();
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("discord".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                work(&s2);
                let _ = done_tx.send(());
            })
            .expect("spawn discord thread");
        Presenter { shared, thread: Some((thread, done_rx)) }
    }

    /// A presenter without a worker thread, so tests never reach Discord.
    #[cfg(test)]
    pub fn inert() -> Presenter {
        Presenter { shared: Arc::new(Shared::default()), thread: None }
    }

    /// The activity last handed to `set`.
    #[cfg(test)]
    pub fn wanted(&self) -> Option<String> {
        self.shared.want.lock().unwrap_or_else(|e| e.into_inner()).activity.clone()
    }

    /// Replace the desired activity (JSON object), or clear it with `None`.
    pub fn set(&self, activity: Option<String>) {
        let mut w = self.shared.want.lock().unwrap_or_else(|e| e.into_inner());
        w.activity = activity;
        w.generation += 1;
        self.shared.cv.notify_one();
    }

    /// Clear the activity (if connected) and stop the worker.
    pub fn shutdown(self) {
        self.shutdown_within(SHUTDOWN_WAIT);
    }

    /// Like `shutdown`, but give up on the worker after `wait`.
    fn shutdown_within(mut self, wait: Duration) {
        {
            let mut w = self.shared.want.lock().unwrap_or_else(|e| e.into_inner());
            w.stop = true;
            self.shared.cv.notify_one();
        }
        if let Some((t, done)) = self.thread.take() {
            // Disconnected means the worker is gone too (it panicked).
            match done.recv_timeout(wait) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    let _ = t.join();
                }
                Err(RecvTimeoutError::Timeout) => crate::warn!("Discord worker did not stop in time; leaving it"),
            }
        }
    }
}

fn next_allowed(window: &VecDeque<Instant>, last: Option<Instant>) -> Option<Instant> {
    let mut t = last.map(|l| l + MIN_GAP);
    if window.len() >= MAX_PER_WINDOW {
        let w = window[window.len() - MAX_PER_WINDOW] + WINDOW;
        t = Some(t.map_or(w, |t| t.max(w)));
    }
    t
}

/// What the worker last got onto the wire.
#[derive(Default)]
struct Wire {
    /// What Discord currently shows (and keepalives re-assert).
    on_wire: Option<String>,
    /// Generation of the last activity Discord answered for.
    sent_gen: u64,
    /// Resend the wanted activity even if its generation was already sent.
    resend: bool,
}

impl Wire {
    /// Record the outcome of sending `want` (generation `generation`).
    /// Returns `false` when the connection is lost and must be dropped.
    fn record(&mut self, res: &io::Result<()>, want: Option<String>, generation: u64) -> bool {
        match res {
            Ok(()) => {
                crate::debug!("activity {}", if want.is_some() { "set" } else { "cleared" });
                self.on_wire = want;
                self.sent_gen = generation;
                self.resend = false;
                true
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                crate::warn!("Discord rejected the activity: {e}");
                // Not shown, so nothing to keep alive: re-sending a payload
                // Discord refused would only fail again every keepalive.
                self.on_wire = None;
                self.sent_gen = generation;
                self.resend = false;
                true
            }
            Err(e) => {
                crate::info!("Discord connection lost: {e}");
                self.on_wire = None;
                self.resend = true;
                false
            }
        }
    }
}

fn worker(shared: &Shared, client_id: &str) {
    let mut conn: Option<Conn> = None;
    let mut wire = Wire::default();
    let mut window: VecDeque<Instant> = VecDeque::with_capacity(MAX_PER_WINDOW + 1);
    let mut last_send: Option<Instant> = None;
    let mut retry_at = Instant::now();
    let mut backoff = Duration::from_secs(2);

    loop {
        let (want, generation) = {
            let mut g = shared.want.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if g.stop {
                    drop(g);
                    if let (Some(c), Some(_)) = (conn.as_mut(), wire.on_wire.as_ref()) {
                        let _ = c.set_activity(None);
                    }
                    return;
                }
                let now = Instant::now();
                let keepalive = conn.as_ref().map(|c| if c.bridge { KEEPALIVE_BRIDGE } else { KEEPALIVE });
                let keepalive_at = match (keepalive, &wire.on_wire, last_send) {
                    (Some(k), Some(_), Some(l)) => Some(l + k),
                    _ => None,
                };
                let dirty = g.generation != wire.sent_gen || wire.resend;
                let mut wake = None;
                if dirty || keepalive_at.is_some_and(|t| t <= now) {
                    if conn.is_none() && g.activity.is_none() {
                        // Nothing to clear on a connection we don't have.
                        wire.sent_gen = g.generation;
                        wire.resend = false;
                        continue;
                    }
                    let mut at = next_allowed(&window, last_send).unwrap_or(now);
                    if conn.is_none() {
                        at = at.max(retry_at);
                    }
                    if at <= now {
                        break (g.activity.clone(), g.generation);
                    }
                    wake = Some(at);
                } else if let Some(t) = keepalive_at {
                    wake = Some(t);
                }
                g = match wake {
                    Some(t) => {
                        shared.cv.wait_timeout(g, t.saturating_duration_since(now)).unwrap_or_else(|e| e.into_inner()).0
                    }
                    None => shared.cv.wait(g).unwrap_or_else(|e| e.into_inner()),
                };
            }
        };

        if conn.is_none() {
            match Conn::connect(client_id, &candidate_paths()) {
                Ok(c) => {
                    crate::info!("connected to Discord{}", if c.bridge { " (arRPC bridge)" } else { "" });
                    conn = Some(c);
                    backoff = Duration::from_secs(2);
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::InvalidInput {
                        crate::error!("Discord refused the handshake: {e} (check client_id)");
                        retry_at = Instant::now() + Duration::from_secs(300);
                    } else {
                        crate::debug!("Discord not reachable: {e}");
                        retry_at = Instant::now() + backoff;
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                    }
                    continue;
                }
            }
        }

        let c = conn.as_mut().expect("connected");
        let res = c.set_activity(want.as_deref());
        let now = Instant::now();
        last_send = Some(now);
        window.push_back(now);
        while window.len() > MAX_PER_WINDOW || window.front().is_some_and(|&t| now - t >= WINDOW) {
            window.pop_front();
        }
        if !wire.record(&res, want, generation) {
            conn = None;
            retry_at = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_activity_is_not_kept_alive() {
        let mut w = Wire::default();
        assert!(w.record(&Ok(()), Some("a".into()), 1));
        assert_eq!((w.on_wire.as_deref(), w.sent_gen, w.resend), (Some("a"), 1, false));

        // Discord refused it: don't resend (keepalive only runs while on_wire is set).
        let rejected = Err(io::Error::new(io::ErrorKind::InvalidData, "bad"));
        assert!(w.record(&rejected, Some("b".into()), 2));
        assert_eq!((w.on_wire.as_deref(), w.sent_gen, w.resend), (None, 2, false));

        // A broken connection: forget what's shown and resend after reconnecting.
        w.record(&Ok(()), Some("c".into()), 3);
        let lost = Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone"));
        assert!(!w.record(&lost, Some("d".into()), 4));
        assert_eq!((w.on_wire.as_deref(), w.sent_gen, w.resend), (None, 3, true));
    }

    #[test]
    fn oversized_frame_drops_the_connection() {
        let mut hdr = OP_FRAME.to_le_bytes().to_vec();
        hdr.extend_from_slice(&(MAX_FRAME as u32 + 1).to_le_bytes());
        let mut buf = Vec::new();
        let e = read_frame_from(&mut hdr.as_slice(), &mut buf).unwrap_err();
        assert_ne!(e.kind(), io::ErrorKind::InvalidData, "InvalidData means \"rejected, keep the connection\"");
        assert!(!Wire::default().record(&Err(e), Some("a".into()), 1));

        let mut ok = OP_PING.to_le_bytes().to_vec();
        ok.extend_from_slice(&2u32.to_le_bytes());
        ok.extend_from_slice(b"{}");
        assert_eq!(read_frame_from(&mut ok.as_slice(), &mut buf).unwrap(), OP_PING);
        assert_eq!(buf, b"{}");
        // Truncated body.
        assert!(read_frame_from(&mut &ok[..9], &mut buf).is_err());
    }

    #[test]
    fn inert_presenter_spawns_nothing() {
        let p = Presenter::inert();
        assert!(p.thread.is_none());
        p.set(Some("x".into()));
        assert_eq!(p.wanted().as_deref(), Some("x"));
        p.shutdown();
    }

    #[test]
    fn shutdown_does_not_wait_forever() {
        // A worker stuck in blocking I/O (e.g. a frozen Discord pipe on Windows).
        let p = Presenter::spawn_with(|_| std::thread::sleep(Duration::from_secs(5)));
        let t = Instant::now();
        p.shutdown_within(Duration::from_millis(100));
        assert!(t.elapsed() < Duration::from_secs(2), "shutdown hung: {:?}", t.elapsed());
    }

    #[test]
    fn shutdown_joins_a_finishing_worker() {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = finished.clone();
        let p = Presenter::spawn_with(move |shared| {
            let mut g = shared.want.lock().unwrap();
            while !g.stop {
                g = shared.cv.wait(g).unwrap();
            }
            std::thread::sleep(Duration::from_millis(50));
            f2.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        p.shutdown_within(Duration::from_secs(5));
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn throttle_window() {
        let t0 = Instant::now();
        let mut w = VecDeque::new();
        assert_eq!(next_allowed(&w, None), None);
        for i in 0..4 {
            w.push_back(t0 + MIN_GAP * i);
        }
        let last = *w.back().unwrap();
        assert_eq!(next_allowed(&w, Some(last)), Some(t0 + WINDOW));
    }

    /// Tests that need a real socket.
    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::net::{UnixListener, UnixStream};

        fn read_frame(s: &mut UnixStream) -> (u32, String) {
            let mut h = [0u8; 8];
            s.read_exact(&mut h).unwrap();
            let op = u32::from_le_bytes(h[..4].try_into().unwrap());
            let len = u32::from_le_bytes(h[4..].try_into().unwrap()) as usize;
            let mut b = vec![0; len];
            s.read_exact(&mut b).unwrap();
            (op, String::from_utf8(b).unwrap())
        }

        fn write_frame(s: &mut UnixStream, op: u32, body: &str) {
            let mut f = op.to_le_bytes().to_vec();
            f.extend_from_slice(&(body.len() as u32).to_le_bytes());
            f.extend_from_slice(body.as_bytes());
            s.write_all(&f).unwrap();
        }

        #[test]
        fn talks_to_fake_discord() {
            let p = std::env::temp_dir().join(format!("cp-discord-{}", std::process::id()));
            let _ = std::fs::remove_file(&p);
            let l = UnixListener::bind(&p).unwrap();
            let server = std::thread::spawn(move || {
                let (mut s, _) = l.accept().unwrap();
                let (op, body) = read_frame(&mut s);
                assert_eq!(op, OP_HANDSHAKE);
                assert!(body.contains(r#""client_id":"123""#));
                write_frame(
                    &mut s,
                    OP_FRAME,
                    r#"{"cmd":"DISPATCH","evt":"READY","data":{"user":{"id":"1045800378228281345"}}}"#,
                );
                let (op, body) = read_frame(&mut s);
                assert_eq!(op, OP_FRAME);
                assert!(body.contains(r#""activity":{"details":"x"}"#));
                write_frame(&mut s, OP_PING, "{}");
                assert_eq!(read_frame(&mut s).0, OP_PONG);
                write_frame(&mut s, OP_FRAME, r#"{"cmd": "SET_ACTIVITY", "nonce": "0", "data": {}}"#);
                write_frame(&mut s, OP_FRAME, r#"{"cmd": "SET_ACTIVITY", "nonce": "1", "data": {}}"#);
                let (_, body) = read_frame(&mut s);
                assert!(!body.contains("activity"));
                write_frame(
                    &mut s,
                    OP_FRAME,
                    r#"{"cmd":"SET_ACTIVITY","evt":"ERROR","nonce":"2","data":{"message":"bad"}}"#,
                );
            });
            let mut c = Conn::connect("123", std::slice::from_ref(&p)).unwrap();
            assert!(c.bridge);
            c.set_activity(Some(r#"{"details":"x"}"#)).unwrap();
            assert_eq!(c.set_activity(None).unwrap_err().kind(), io::ErrorKind::InvalidData);
            server.join().unwrap();
            let _ = std::fs::remove_file(&p);
        }
    }
}
