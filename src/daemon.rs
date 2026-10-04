//! The long-running process. One thread accepts hook messages, one owns the
//! Discord socket, and the main thread runs an event loop that sleeps until
//! the next hook or the next timer deadline — no polling, no async runtime.

use crate::config::{Config, Template};
use crate::discord::Presenter;
use crate::git::{self, GitInfo};
use crate::ledger::{self, Ledger};
use crate::presence::{self, Activity, Vars};
use crate::timeutil::{self, fmt_count, fmt_duration_ms, fmt_hours_ms};
use crate::{ipc, paths};
use serde::Deserialize;
use sonic_rs::{JsonValueTrait, LazyValue};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

pub enum Msg {
    Hook(Vec<u8>),
    Reload,
    Shutdown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Idle,
    Thinking,
    Working,
    Compacting,
    Notification,
}

impl Status {
    fn active(self) -> bool {
        matches!(self, Status::Thinking | Status::Working | Status::Compacting)
    }
    fn name(self) -> &'static str {
        match self {
            Status::Idle => "Idle",
            Status::Thinking => "Thinking",
            Status::Working => "Working",
            Status::Compacting => "Compacting",
            Status::Notification => "Waiting for input",
        }
    }
}

#[derive(Debug)]
struct Session {
    cwd: PathBuf,
    transcript_path: Option<PathBuf>,
    /// Canonical ledger key, resolved once the transcript exists.
    transcript: Option<String>,
    model_hint: Option<String>,
    status: Status,
    tool: Option<String>,
    file: Option<String>,
    started: i64,
    last_activity: i64,
    prompts: u32,
    tools: u32,
}

impl Session {
    fn new(now: i64) -> Session {
        Session {
            cwd: PathBuf::new(),
            transcript_path: None,
            transcript: None,
            model_hint: None,
            status: Status::Idle,
            tool: None,
            file: None,
            started: now,
            last_activity: now,
            prompts: 0,
            tools: 0,
        }
    }
}

#[derive(Deserialize)]
struct HookInput<'a> {
    #[serde(borrow, default)]
    session_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    transcript_path: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    cwd: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    hook_event_name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    tool_name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    tool_input: Option<LazyValue<'a>>,
    #[serde(borrow, default)]
    source: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    model: Option<LazyValue<'a>>,
}

/// `claude-opus-5-5` → `Opus 5.5`, `claude-sonnet-4-5-20250929[1m]` → `Sonnet 4.5`.
pub fn pretty_model(id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    for fam in ["fable", "opus", "sonnet", "haiku"] {
        let Some(pos) = lower.find(fam) else { continue };
        let mut name = String::with_capacity(12);
        name.push(fam.as_bytes()[0].to_ascii_uppercase() as char);
        name.push_str(&fam[1..]);
        let rest = &lower.as_bytes()[pos + fam.len()..];
        let num = |b: &[u8]| -> Option<(String, usize)> {
            let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
            if (1..=2).contains(&n) { Some((String::from_utf8_lossy(&b[..n]).into_owned(), n)) } else { None }
        };
        let skip = rest.iter().take_while(|c| !c.is_ascii_digit() && **c != b'[').count();
        if let Some((major, n)) = num(&rest[skip..]) {
            name.push(' ');
            name.push_str(&major);
            let after = &rest[skip + n..];
            if after.first().is_some_and(|c| *c == b'-' || *c == b'.') {
                if let Some((minor, _)) = num(&after[1..]) {
                    name.push('.');
                    name.push_str(&minor);
                }
            }
        }
        return name;
    }
    if id.is_empty() { "Claude".into() } else { id.into() }
}

/// `mcp__claude_ai_Linear__list_issues` → `Linear:list_issues`.
pub fn pretty_tool(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("mcp__") {
        let parts: Vec<&str> = rest.split("__").filter(|p| !p.is_empty()).collect();
        if parts.len() >= 2 {
            let server = parts[parts.len() - 2];
            let server = server.strip_prefix("claude_ai_").unwrap_or(server);
            return format!("{server}:{}", parts[parts.len() - 1]);
        }
        return rest.replace("__", ":");
    }
    name.to_owned()
}

fn file_name(p: &str) -> String {
    p.rsplit(['/', '\\']).next().unwrap_or(p).to_owned()
}

/// Seconds of quiet before `s` expires, `None` if sessions never expire.
fn expiry_secs(idle_timeout: u64, s: &Session) -> Option<i64> {
    let t = idle_timeout as i64;
    if t == 0 {
        return None;
    }
    Some(if s.status.active() { t.max(3600) } else { t })
}

struct Rotation {
    key: (usize, Status),
    index: usize,
    since: i64,
}

pub struct Daemon {
    cfg: Config,
    ledger: Ledger,
    sessions: HashMap<String, Session>,
    order: usize,
    ids: HashMap<String, usize>,
    displayed: Option<String>,
    rotation: Rotation,
    git: HashMap<PathBuf, (Instant, GitInfo)>,
    presenter: Presenter,
    last_given: Option<Option<String>>,
    last_save: Instant,
    last_rescan: Instant,
    last_tail: Instant,
    /// Whether a hook has arrived yet (logged once, to show hooks are wired).
    got_hook: bool,
}

const SAVE_EVERY: Duration = Duration::from_secs(60);
const TAIL_EVERY: Duration = Duration::from_secs(5);
const GIT_TTL: Duration = Duration::from_secs(60);

impl Daemon {
    fn new(cfg: Config, ledger: Ledger) -> Daemon {
        let presenter = Presenter::spawn(cfg.client_id.clone());
        Daemon::with_presenter(cfg, ledger, presenter)
    }

    fn with_presenter(cfg: Config, ledger: Ledger, presenter: Presenter) -> Daemon {
        let now = Instant::now();
        Daemon {
            cfg,
            ledger,
            sessions: HashMap::new(),
            order: 0,
            ids: HashMap::new(),
            displayed: None,
            rotation: Rotation { key: (usize::MAX, Status::Idle), index: 0, since: 0 },
            git: HashMap::new(),
            presenter,
            last_given: None,
            last_save: now,
            last_rescan: now,
            last_tail: now,
            got_hook: false,
        }
    }

    fn handle_hook(&mut self, msg: &[u8]) {
        let (arg_event, payload) = match memchr::memchr(b'\n', msg) {
            Some(i) => (&msg[..i], &msg[i + 1..]),
            None => (msg, &b""[..]),
        };
        let input = sonic_rs::from_slice::<HookInput<'_>>(payload).ok();
        let event = input
            .as_ref()
            .and_then(|i| i.hook_event_name.as_deref())
            .map(str::to_owned)
            .unwrap_or_else(|| String::from_utf8_lossy(arg_event).into_owned());
        if !self.got_hook {
            self.got_hook = true;
            crate::info!("first hook received ({event})");
        }
        let Some(input) = input else {
            crate::debug!("unparseable {event} payload");
            return;
        };
        let now = timeutil::now_ms();
        let sid = input.session_id.as_deref().unwrap_or("default").to_owned();
        crate::debug!("hook {event} session={sid}");

        if event == "SessionEnd" {
            if let Some(s) = self.sessions.remove(&sid) {
                if let Some(k) = &s.transcript {
                    self.ledger.ingest(k);
                }
            }
            self.ids.remove(&sid);
            return;
        }

        if !self.sessions.contains_key(&sid) {
            self.order += 1;
            self.ids.insert(sid.clone(), self.order);
        }
        let s = self.sessions.entry(sid.clone()).or_insert_with(|| Session::new(now));
        s.last_activity = now;
        if let Some(c) = input.cwd.as_deref() {
            if s.cwd.as_os_str() != c {
                s.cwd = PathBuf::from(c);
            }
        }
        if let Some(t) = input.transcript_path.as_deref() {
            if s.transcript_path.as_deref().map(Path::as_os_str) != Some(t.as_ref()) {
                s.transcript_path = Some(PathBuf::from(t));
                s.transcript = None;
            }
        }
        match event.as_str() {
            "SessionStart" => {
                let source = input.source.as_deref();
                if source == Some("compact") {
                    s.status = Status::Thinking;
                } else {
                    // A resume continues the same conversation; startup and
                    // clear begin a new one.
                    if source != Some("resume") {
                        s.started = now;
                        s.prompts = 0;
                        s.tools = 0;
                    }
                    s.status = Status::Idle;
                    s.tool = None;
                    s.file = None;
                }
                if let Some(m) = input.model.as_ref() {
                    let id = m
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| m.get("id").and_then(|v| v.as_str().map(str::to_owned)));
                    if id.is_some() {
                        s.model_hint = id;
                    }
                }
            }
            "UserPromptSubmit" => {
                s.prompts += 1;
                s.status = Status::Thinking;
                s.tool = None;
                s.file = None;
            }
            "PreToolUse" => {
                s.tools += 1;
                s.status = Status::Working;
                s.tool = input.tool_name.as_deref().map(pretty_tool);
                s.file = input.tool_input.as_ref().and_then(|ti| {
                    ["file_path", "notebook_path", "path"]
                        .iter()
                        .find_map(|k| ti.get(*k).and_then(|v| v.as_str().map(file_name)))
                });
            }
            "PostToolUse" | "PostToolUseFailure" => {
                if s.status != Status::Working {
                    s.status = Status::Working;
                }
            }
            "Notification" => s.status = Status::Notification,
            "PreCompact" => s.status = Status::Compacting,
            "Stop" => {
                s.status = Status::Idle;
                s.tool = None;
                s.file = None;
            }
            _ => {} // SubagentStop etc.: liveness only
        }

        if s.transcript.is_none() {
            s.transcript = s.transcript_path.as_deref().and_then(ledger::key_for);
        }
        let key = s.transcript.clone();
        let tp = s.transcript_path.clone();
        if let Some(k) = key {
            self.ledger.ingest(&k);
        }
        if event == "SubagentStop" {
            // Subagent transcripts live in <dir>/<session-id>/subagents/.
            if let Some(dir) = tp.as_deref().and_then(Path::parent) {
                let sub = dir.join(&sid).join("subagents");
                if let Ok(rd) = std::fs::read_dir(sub) {
                    for e in rd.flatten() {
                        let p = e.path();
                        if p.extension().is_some_and(|x| x == "jsonl") {
                            if let Some(k) = ledger::key_for(&p) {
                                self.ledger.ingest(&k);
                            }
                        }
                    }
                }
            }
        }
    }

    fn expiry_secs(&self, s: &Session) -> Option<i64> {
        expiry_secs(self.cfg.idle_timeout, s)
    }

    /// Drop sessions that went quiet (e.g. terminal closed without SessionEnd).
    fn expire(&mut self, now: i64) {
        let mut dead = Vec::new();
        for (id, s) in &mut self.sessions {
            let Some(secs) = expiry_secs(self.cfg.idle_timeout, s) else { continue };
            if now - s.last_activity < secs * 1000 {
                continue;
            }
            let modified = s
                .transcript_path
                .as_deref()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if now - modified >= secs * 1000 {
                dead.push(id.clone());
            } else {
                // A transcript still being written counts as activity, so the
                // next expiry deadline moves forward instead of staying past.
                // A future mtime (clock skew) counts as now.
                s.last_activity = s.last_activity.max(modified.min(now));
            }
        }
        for id in dead {
            crate::info!("session {id} expired");
            self.sessions.remove(&id);
            self.ids.remove(&id);
        }
    }

    fn next_expiry(&self) -> Option<i64> {
        self.sessions.values().filter_map(|s| self.expiry_secs(s).map(|t| s.last_activity + t * 1000)).min()
    }

    /// Which session the card shows: active beats idle, then most recent —
    /// but stick with the shown one while it stays in the top tier, so the
    /// card doesn't flap between parallel sessions.
    fn pick(&mut self) -> Option<String> {
        let tier = |s: &Session| match s.status {
            Status::Notification => 2,
            st if st.active() => 2,
            _ => 1,
        };
        let best = self
            .sessions
            .iter()
            .max_by_key(|(_, s)| (tier(s), s.last_activity))
            .map(|(id, s)| (id.clone(), tier(s)))?;
        if let Some(d) = &self.displayed {
            if let Some(s) = self.sessions.get(d) {
                if tier(s) >= best.1 {
                    return Some(d.clone());
                }
            }
        }
        self.displayed = Some(best.0.clone());
        Some(best.0)
    }

    fn git_info(&mut self, cwd: &Path) -> GitInfo {
        if let Some((at, info)) = self.git.get(cwd) {
            if at.elapsed() < GIT_TTL {
                return info.clone();
            }
        }
        if self.git.len() > 64 {
            self.git.clear();
        }
        let info = git::inspect(cwd);
        self.git.insert(cwd.to_path_buf(), (Instant::now(), info.clone()));
        info
    }

    fn is_hidden(&self, name: &str, cwd: &Path) -> bool {
        self.cfg.hidden_projects.iter().any(|h| {
            if h == name {
                return true;
            }
            let p = match h.strip_prefix("~/") {
                Some(rest) => paths::home().join(rest),
                None => PathBuf::from(h),
            };
            p.is_absolute() && cwd.starts_with(&p)
        })
    }

    fn template(&self, st: Status) -> &Template {
        let t = &self.cfg.status;
        match st {
            Status::Idle => &t.idle,
            Status::Thinking => &t.thinking,
            Status::Working => &t.working,
            Status::Compacting => &t.compacting,
            Status::Notification => &t.notification,
        }
    }

    /// Build the activity JSON for the current state, or `None` to clear.
    fn render(&mut self, now: i64) -> Option<String> {
        let id = self.pick()?;
        let s = self.sessions.get(&id)?;
        let (cwd, status, started) = (s.cwd.clone(), s.status, s.started);
        let gi = self.git_info(&cwd);
        let s = self.sessions.get(&id)?;
        let fs = s.transcript.as_deref().and_then(|k| self.ledger.file(k));

        let base = gi.root.as_deref().unwrap_or(&cwd);
        let name = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let hidden = self.is_hidden(&name, &cwd);
        let off = timeutil::local_offset_secs();
        let snap = self.ledger.snapshot(now, off);
        let model = fs
            .and_then(|f| f.model.as_deref())
            .or(s.model_hint.as_deref())
            .map(pretty_model)
            .unwrap_or_else(|| "Claude".into());
        let usage = fs.map(|f| f.usage).unwrap_or_default();
        // The transcript counts the whole conversation (resumed history
        // included); the hook count covers prompts since we attached, which
        // can be one ahead while the transcript catches up with the latest.
        let prompts = fs.map_or(0, |f| f.prompts).max(s.prompts);

        let mut v = Vars::default();
        v.set("project", if hidden { self.cfg.hidden_project_name.clone() } else { name });
        v.set("branch", if hidden { String::new() } else { gi.branch.clone() });
        v.set("model", model);
        v.set("tool", s.tool.clone().unwrap_or_default());
        v.set("file", if hidden { String::new() } else { s.file.clone().unwrap_or_default() });
        v.set("tokens", fmt_count(usage.total()));
        v.set("tokens_in", fmt_count(usage.input.saturating_add(usage.cache_read).saturating_add(usage.cache_write)));
        v.set("tokens_out", fmt_count(usage.output));
        v.set("prompts", prompts.to_string());
        v.set("tools", s.tools.to_string());
        v.set("session_time", fmt_duration_ms(now - started));
        v.set("status", status.name());
        v.set("today_time", if snap.today_ms > 0 { fmt_hours_ms(snap.today_ms) } else { String::new() });
        v.set("today_tokens", fmt_count(snap.today_tokens));
        v.set("today_prompts", snap.today_prompts.to_string());
        v.set("total_time", if snap.total_ms > 0 { fmt_hours_ms(snap.total_ms) } else { String::new() });
        v.set("total_tokens", fmt_count(snap.total_tokens));
        v.set("total_sessions", snap.total_sessions.to_string());
        v.set("total_prompts", fmt_count(snap.total_prompts));
        v.set("streak", snap.streak.to_string());

        // Frame selection: the base frame, then rotation frames whose
        // variables are all present.
        let tpl = self.template(status);
        let mut frames: Vec<(String, String)> = Vec::with_capacity(1 + tpl.rotation.len());
        frames.push((presence::render(&tpl.details, &v).0, presence::render(&tpl.state, &v).0));
        for f in &tpl.rotation {
            let (d, ok1) = presence::render(&f.details, &v);
            let (st, ok2) = presence::render(&f.state, &v);
            if ok1 && ok2 {
                frames.push((d, st));
            }
        }
        let key = (self.ids.get(&id).copied().unwrap_or(0), status);
        let interval = self.cfg.rotation_interval as i64 * 1000;
        if self.rotation.key != key {
            self.rotation = Rotation { key, index: 0, since: now };
        } else if now - self.rotation.since >= interval {
            // Restart the interval even with a single eligible frame, so
            // `next_rotation` never falls into the past.
            if frames.len() > 1 {
                self.rotation.index += 1;
            }
            self.rotation.since = now;
        }
        let (details, state) = frames.swap_remove(self.rotation.index % frames.len());

        let a = &self.cfg.assets;
        let large_image = match status {
            Status::Idle => &a.idle,
            Status::Thinking => &a.thinking,
            Status::Working => &a.working,
            Status::Compacting => &a.compacting,
            Status::Notification => &a.notification,
        };
        let assets = presence::Assets {
            large_image: presence::field(large_image.clone(), 300),
            large_text: presence::field(presence::render(&a.large_text, &v).0, 128),
            small_image: presence::field(a.small_image.clone(), 300),
            small_text: presence::field(presence::render(&a.small_text, &v).0, 128),
        };
        let mut buttons = Vec::new();
        if self.cfg.github_button && !hidden {
            if let Some(url) = gi.github {
                buttons.push(presence::Button { label: "View on GitHub".into(), url });
            }
        }
        for b in &self.cfg.buttons {
            if buttons.len() < 2 && b.url.starts_with("http") && !b.label.is_empty() {
                buttons.push(presence::Button { label: presence::clamp(b.label.clone(), 32), url: b.url.clone() });
            }
        }
        let has_assets = assets.large_image.is_some() || assets.small_image.is_some();
        let activity = Activity {
            kind: self.cfg.activity_type,
            status_display_type: self.cfg.status_display_type(),
            details: presence::field(details, 128),
            state: presence::field(state, 128),
            timestamps: self.cfg.show_elapsed.then_some(presence::Timestamps { start: started }),
            assets: has_assets.then_some(assets),
            buttons,
            instance: false,
        };
        sonic_rs::to_string(&activity).ok()
    }

    fn next_rotation(&self) -> Option<i64> {
        let d = self.displayed.as_ref()?;
        let s = self.sessions.get(d)?;
        if self.template(s.status).rotation.is_empty() {
            return None;
        }
        Some(self.rotation.since + self.cfg.rotation_interval as i64 * 1000)
    }

    fn push(&mut self) {
        let now = timeutil::now_ms();
        let want = self.render(now);
        if self.last_given.as_ref() != Some(&want) {
            crate::debug!("presence → {}", want.as_deref().unwrap_or("(cleared)"));
            self.presenter.set(want.clone());
            self.last_given = Some(want);
        }
    }

    fn rescan(&mut self) {
        let t = Instant::now();
        let r = self.ledger.scan(&[paths::claude_projects()]);
        crate::info!(
            "scanned {} transcripts ({} changed, {} pruned) in {:.0?}",
            r.files,
            r.changed,
            r.pruned,
            t.elapsed()
        );
        self.last_rescan = Instant::now();
    }

    fn save(&mut self) {
        if let Err(e) = self.ledger.save() {
            crate::error!("saving stats: {e}");
        }
        self.last_save = Instant::now();
    }

    /// Periodic work; returns how long the loop may sleep.
    fn tick(&mut self) -> Duration {
        let now_i = Instant::now();
        let now = timeutil::now_ms();
        self.expire(now);

        // Keep token counts moving during long generations (no hooks fire
        // while the model streams).
        let active_key = self
            .displayed
            .as_ref()
            .and_then(|d| self.sessions.get(d))
            .filter(|s| s.status.active())
            .and_then(|s| s.transcript.clone());
        if let Some(k) = &active_key {
            if now_i.duration_since(self.last_tail) >= TAIL_EVERY {
                self.ledger.ingest(k);
                self.last_tail = now_i;
            }
        }
        if self.cfg.rescan_interval > 0
            && now_i.duration_since(self.last_rescan) >= Duration::from_secs(self.cfg.rescan_interval)
        {
            self.rescan();
        }
        if self.ledger.is_dirty() && now_i.duration_since(self.last_save) >= SAVE_EVERY {
            self.save();
        }
        self.push();

        // Next deadline.
        let mut wait = Duration::from_secs(24 * 3600);
        let mut at_ms = |t: i64| {
            let d = Duration::from_millis((t - timeutil::now_ms()).max(0) as u64);
            wait = wait.min(d);
        };
        if let Some(t) = self.next_rotation() {
            at_ms(t);
        }
        if let Some(t) = self.next_expiry() {
            at_ms(t + 1000);
        }
        if active_key.is_some() {
            wait = wait.min(TAIL_EVERY.saturating_sub(now_i.duration_since(self.last_tail)));
        }
        if self.ledger.is_dirty() {
            wait = wait.min(SAVE_EVERY.saturating_sub(now_i.duration_since(self.last_save)));
        }
        if self.cfg.rescan_interval > 0 {
            let every = Duration::from_secs(self.cfg.rescan_interval);
            wait = wait.min(every.saturating_sub(now_i.duration_since(self.last_rescan)));
        }
        wait.max(Duration::from_millis(50))
    }

    fn reload(&mut self) {
        let cfg = Config::load(&paths::config_file());
        if cfg.client_id != self.cfg.client_id {
            let old = std::mem::replace(&mut self.presenter, Presenter::spawn(cfg.client_id.clone()));
            old.shutdown();
            self.last_given = None;
        }
        self.cfg = cfg;
        crate::info!("configuration reloaded");
    }
}

#[cfg(unix)]
fn install_signals(tx: Sender<Msg>) {
    // Block the signals in every thread (inherited by threads spawned after
    // this), then receive them synchronously on a dedicated thread.
    // SAFETY: plain libc signal-mask manipulation.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        let set_copy = set;
        std::thread::Builder::new()
            .name("signals".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                loop {
                    let mut sig: libc::c_int = 0;
                    if libc::sigwait(&set_copy, &mut sig) != 0 {
                        continue;
                    }
                    let msg = if sig == libc::SIGHUP { Msg::Reload } else { Msg::Shutdown };
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn signal thread");
    }
}

#[cfg(windows)]
fn install_signals(tx: Sender<Msg>) {
    use std::sync::{Mutex, OnceLock};
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    static TX: OnceLock<Mutex<Sender<Msg>>> = OnceLock::new();
    unsafe extern "system" fn handler(_: u32) -> i32 {
        if let Some(tx) = TX.get() {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(Msg::Shutdown);
            }
        }
        // Give the main loop a moment to clear presence and save.
        std::thread::sleep(Duration::from_millis(1500));
        1
    }
    let _ = TX.set(Mutex::new(tx));
    // SAFETY: registering a static handler function.
    unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
}

/// Turn a message from the hook channel into a loop message. `__shutdown`
/// stops the daemon like SIGTERM (sent by `install`/`uninstall`); other
/// reserved `__` events are ignored, so newer clients can't confuse older
/// daemons.
fn route(m: Vec<u8>) -> Option<Msg> {
    let event = ipc::event_name(&m);
    if !ipc::is_control(event) {
        return Some(Msg::Hook(m));
    }
    if event == ipc::SHUTDOWN.as_bytes() {
        crate::info!("shutdown requested");
        return Some(Msg::Shutdown);
    }
    crate::debug!("ignoring control event {}", String::from_utf8_lossy(event));
    None
}

pub struct Options {
    pub log_file: Option<PathBuf>,
}

/// Run the daemon until SIGINT/SIGTERM or `__shutdown`. Returns the process exit code.
pub fn run(opts: Options) -> i32 {
    crate::log::init(opts.log_file.as_deref());
    let addr = paths::hook_socket();
    let (tx, rx) = mpsc::channel::<Msg>();
    install_signals(tx.clone());

    // Wait out a daemon that was just asked to stop (reinstall).
    let listener = match ipc::bind_waiting(&addr, Duration::from_secs(5)) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            crate::info!("another claude-presence daemon is already running");
            return 0;
        }
        Err(e) => {
            crate::error!("cannot listen on {}: {e}", addr.display());
            return 1;
        }
    };
    let htx = tx.clone();
    std::thread::Builder::new()
        .name("hooks".into())
        .stack_size(128 * 1024)
        .spawn(move || listener.serve(move |m| route(m).is_none_or(|msg| htx.send(msg).is_ok())))
        .expect("spawn hook listener");
    drop(tx);

    let cfg = Config::load(&paths::config_file());
    crate::info!("listening on {}", addr.display());
    let ledger = Ledger::load(paths::ledger_file(), paths::seen_file());
    let scan = cfg.scan_history;
    let mut d = Daemon::new(cfg, ledger);
    if scan {
        d.rescan();
        d.save();
    }

    loop {
        let wait = d.tick();
        let first = match rx.recv_timeout(wait) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Drain everything queued so a burst of hooks renders once.
        let mut stop = false;
        for m in first.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            match m {
                Msg::Hook(b) => d.handle_hook(&b),
                Msg::Reload => d.reload(),
                Msg::Shutdown => stop = true,
            }
        }
        if stop {
            break;
        }
    }
    crate::info!("shutting down");
    d.save();
    d.presenter.shutdown();
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models() {
        assert_eq!(pretty_model("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(pretty_model("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(pretty_model("claude-opus-4-6[1m]"), "Opus 4.6");
        assert_eq!(pretty_model("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(pretty_model("claude-3-5-sonnet-20241022"), "Sonnet");
        assert_eq!(pretty_model("claude-fable-5-1"), "Fable 5.1");
        assert_eq!(pretty_model("opus"), "Opus");
    }

    #[test]
    fn tools() {
        assert_eq!(pretty_tool("Bash"), "Bash");
        assert_eq!(pretty_tool("mcp__claude_ai_Linear__list_issues"), "Linear:list_issues");
        assert_eq!(pretty_tool("mcp__github__get_me"), "github:get_me");
    }

    fn daemon() -> Daemon {
        let dir = std::env::temp_dir().join(format!("cp-daemon-{}", std::process::id()));
        let ledger = Ledger::load(dir.join("l.json"), dir.join("s.bin"));
        let cfg = Config { client_id: "0".into(), ..Config::default() };
        // Inert presenter: tests must never talk to a real Discord client.
        Daemon::with_presenter(cfg, ledger, Presenter::inert())
    }

    fn transcript(name: &str, content: &str) -> (PathBuf, String) {
        let tp = std::env::temp_dir().join(format!("cp-{name}-{}.jsonl", std::process::id()));
        std::fs::write(&tp, content).unwrap();
        let tp_s = tp.to_string_lossy().replace('\\', "\\\\");
        (tp, tp_s)
    }

    fn activity(d: &mut Daemon) -> sonic_rs::Value {
        sonic_rs::from_str(&d.render(timeutil::now_ms()).expect("activity")).unwrap()
    }

    #[test]
    fn state_machine_and_render() {
        let mut d = daemon();
        let cwd = std::env::temp_dir().join("cp-proj-demo");
        let cwd_s = cwd.to_string_lossy().replace('\\', "\\\\");
        let ev = |name: &str, extra: &str| {
            format!(
                r#"{name}
{{"session_id":"s1","cwd":"{cwd_s}","hook_event_name":"{name}"{extra}}}"#
            )
        };
        d.handle_hook(ev("SessionStart", r#","source":"startup","model":"claude-opus-5-5""#).as_bytes());
        let a = activity(&mut d);
        assert_eq!(a["details"].as_str(), Some("Idle in cp-proj-demo"));
        assert!(a["state"].as_str().unwrap().starts_with("Opus 5.5"));

        d.handle_hook(ev("UserPromptSubmit", "").as_bytes());
        let a = activity(&mut d);
        assert_eq!(a["details"].as_str(), Some("Thinking in cp-proj-demo"));

        d.handle_hook(
            ev("PreToolUse", r#","tool_name":"Edit","tool_input":{"file_path":"/x/src/main.rs","old_string":"a"}"#)
                .as_bytes(),
        );
        let a = activity(&mut d);
        assert_eq!(a["details"].as_str(), Some("Working in cp-proj-demo"));
        assert_eq!(a["state"].as_str(), Some("Edit · main.rs · 0 tokens"));
        assert!(a["assets"]["large_image"].as_str().unwrap().contains("building"));
        assert!(a["timestamps"]["start"].as_i64().is_some());

        d.cfg.hidden_projects = vec!["cp-proj-demo".into()];
        let a = activity(&mut d);
        assert_eq!(a["details"].as_str(), Some("Working in a private project"));
        assert_eq!(a["state"].as_str(), Some("Edit · 0 tokens"));

        d.handle_hook(b"SessionEnd\n{\"session_id\":\"s1\",\"hook_event_name\":\"SessionEnd\"}");
        assert!(d.render(timeutil::now_ms()).is_none());
        d.presenter.shutdown();
    }

    #[test]
    fn failed_tool_keeps_the_session_working() {
        let mut d = daemon();
        let cwd = std::env::temp_dir().join("cp-proj-fail");
        let cwd_s = cwd.to_string_lossy().replace('\\', "\\\\");
        let ev = |name: &str, extra: &str| {
            format!(
                r#"{name}
{{"session_id":"f1","cwd":"{cwd_s}","hook_event_name":"{name}"{extra}}}"#
            )
        };
        d.handle_hook(ev("UserPromptSubmit", "").as_bytes());
        d.handle_hook(ev("PreToolUse", r#","tool_name":"Bash","tool_input":{"command":"false"}"#).as_bytes());
        d.handle_hook(ev("Notification", r#","message":"needs permission""#).as_bytes());
        assert_eq!(d.sessions["f1"].status, Status::Notification);
        // The documented payload: the tool's input plus the error.
        let failure = r#","tool_name":"Bash","tool_input":{"command":"false"},"tool_use_id":"toolu_1","tool_error":{"error_type":"exit_code","error_message":"exit 1"}"#;
        d.sessions.get_mut("f1").unwrap().last_activity -= 60_000;
        d.handle_hook(ev("PostToolUseFailure", failure).as_bytes());
        let s = &d.sessions["f1"];
        // Like PostToolUse: Claude carries on with the result.
        assert_eq!(s.status, Status::Working);
        assert_eq!((s.prompts, s.tools, s.tool.as_deref()), (1, 1, Some("Bash")));
        assert!(timeutil::now_ms() - s.last_activity < 60_000, "counts as activity");
        let a = activity(&mut d);
        assert_eq!(a["details"].as_str(), Some("Working in cp-proj-fail"));
        d.presenter.shutdown();
    }

    #[test]
    fn sticky_session_choice() {
        let mut d = daemon();
        assert!(!d.got_hook);
        d.handle_hook(b"UserPromptSubmit\n{\"session_id\":\"a\",\"cwd\":\"/p/a\"}");
        assert!(d.got_hook);
        assert_eq!(d.pick().as_deref(), Some("a"));
        d.handle_hook(b"UserPromptSubmit\n{\"session_id\":\"b\",\"cwd\":\"/p/b\"}");
        // Both active: keep showing "a".
        assert_eq!(d.pick().as_deref(), Some("a"));
        d.handle_hook(b"Stop\n{\"session_id\":\"a\"}");
        // "a" went idle while "b" is still working: switch.
        assert_eq!(d.pick().as_deref(), Some("b"));
        d.presenter.shutdown();
    }

    #[test]
    fn single_frame_rotation_does_not_spin() {
        let mut d = daemon();
        // Fresh stats: every idle rotation frame is skipped, only the base
        // frame is eligible.
        d.handle_hook(b"Stop\n{\"session_id\":\"r\",\"cwd\":\"/p/r\"}");
        d.tick();
        d.rotation.since -= 10 * d.cfg.rotation_interval as i64 * 1000;
        let wait = d.tick();
        assert!(wait > Duration::from_secs(1), "tick spins: {wait:?}");
        assert!(d.next_rotation().unwrap() > timeutil::now_ms());
        d.presenter.shutdown();
    }

    #[test]
    fn live_transcript_does_not_spin_expiry() {
        let mut d = daemon();
        let (tp, tp_s) = transcript("expiry", "");
        d.handle_hook(
            format!("Stop\n{{\"session_id\":\"e\",\"cwd\":\"/p/e\",\"transcript_path\":\"{tp_s}\"}}").as_bytes(),
        );
        let now = timeutil::now_ms();
        // No hooks for longer than idle_timeout, but the transcript was just written.
        d.sessions.get_mut("e").unwrap().last_activity = now - (d.cfg.idle_timeout as i64 + 60) * 1000;
        d.expire(now);
        assert!(d.sessions.contains_key("e"));
        assert!(d.next_expiry().unwrap() > now);
        let wait = d.tick();
        assert!(wait > Duration::from_secs(1), "tick spins: {wait:?}");
        d.presenter.shutdown();
        let _ = std::fs::remove_file(&tp);
    }

    #[test]
    fn tick_pushes_to_presenter_only() {
        let mut d = daemon();
        d.handle_hook(b"UserPromptSubmit\n{\"session_id\":\"p\",\"cwd\":\"/p/p\"}");
        d.tick();
        assert!(d.presenter.wanted().is_some_and(|a| a.contains("Thinking in p")));
        d.presenter.shutdown();
    }

    #[test]
    fn future_transcript_mtime_is_clamped() {
        let mut d = daemon();
        let (tp, tp_s) = transcript("future", "");
        let future = std::time::SystemTime::now() + Duration::from_secs(24 * 3600);
        std::fs::File::options().write(true).open(&tp).unwrap().set_modified(future).unwrap();
        d.handle_hook(format!("Stop\n{{\"session_id\":\"f\",\"transcript_path\":\"{tp_s}\"}}").as_bytes());
        let now = timeutil::now_ms();
        d.sessions.get_mut("f").unwrap().last_activity = now - (d.cfg.idle_timeout as i64 + 60) * 1000;
        d.expire(now);
        let s = &d.sessions["f"];
        assert!(s.last_activity <= now, "a skewed clock must not keep a session alive for a day");
        d.presenter.shutdown();
        let _ = std::fs::remove_file(&tp);
    }

    #[test]
    fn control_events_are_routed() {
        assert!(matches!(route(b"__shutdown\n".to_vec()), Some(Msg::Shutdown)));
        assert!(matches!(route(b"__shutdown\n{}".to_vec()), Some(Msg::Shutdown)));
        assert!(matches!(route(b"__shutdown".to_vec()), Some(Msg::Shutdown)));
        // Other reserved names are dropped without error.
        assert!(route(b"__reload\n{}".to_vec()).is_none());
        assert!(route(b"__shutdownx\n{}".to_vec()).is_none());
        // The event name is the first line only; payload contents don't count.
        assert!(matches!(route(b"Stop\n{\"hook_event_name\":\"__shutdown\"}".to_vec()), Some(Msg::Hook(_))));
        assert!(matches!(route(b"Stop\n{}".to_vec()), Some(Msg::Hook(_))));
    }

    fn hook(d: &mut Daemon, name: &str, sid: &str, extra: &str) {
        d.handle_hook(format!("{name}\n{{\"session_id\":\"{sid}\",\"cwd\":\"/p/{sid}\"{extra}}}").as_bytes());
    }

    #[test]
    fn resumed_conversation_shows_its_history_prompts() {
        use std::io::Write;
        let mut d = daemon();
        let prompt = |u: &str| {
            format!(r#"{{"type":"user","timestamp":"2026-10-04T10:00:00Z","uuid":"{u}","message":{{"content":"go"}}}}"#)
        };
        let history = format!("{}\n{}\n", prompt("rp1"), prompt("rp2"));
        // The original conversation, already in the ledger.
        let (orig, _) = transcript("resume-orig", &history);
        assert!(d.ledger.ingest(&ledger::key_for(&orig).unwrap()));
        // `--resume` copies it into a new transcript.
        let (tp, tp_s) = transcript("resume-copy", &history);
        let tp_field = format!(",\"transcript_path\":\"{tp_s}\"");
        hook(&mut d, "SessionStart", "rs", &format!(",\"source\":\"resume\"{tp_field}"));
        hook(&mut d, "Notification", "rs", &tp_field);
        assert_eq!(activity(&mut d)["state"].as_str(), Some("Claude · 2 prompts"));
        hook(&mut d, "UserPromptSubmit", "rs", &tp_field);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&tp)
            .unwrap()
            .write_all((prompt("rp3") + "\n").as_bytes())
            .unwrap();
        hook(&mut d, "Notification", "rs", &tp_field);
        assert_eq!(activity(&mut d)["state"].as_str(), Some("Claude · 3 prompts"));
        d.presenter.shutdown();
        let _ = std::fs::remove_file(&orig);
        let _ = std::fs::remove_file(&tp);
    }

    #[test]
    fn submitted_prompt_shows_before_the_transcript_has_it() {
        let mut d = daemon();
        let line = r#"{"type":"user","timestamp":"2026-10-04T10:00:00Z","uuid":"lag1","message":{"content":"go"}}"#;
        let (tp, tp_s) = transcript("prompt-lag", &format!("{line}\n"));
        let tp_field = format!(",\"transcript_path\":\"{tp_s}\"");
        hook(&mut d, "UserPromptSubmit", "lag", &tp_field);
        // The second prompt is not written yet when its hook arrives.
        hook(&mut d, "UserPromptSubmit", "lag", &tp_field);
        assert_eq!(activity(&mut d)["state"].as_str(), Some("Claude · 2 prompts · 0 tokens"));
        d.presenter.shutdown();
        let _ = std::fs::remove_file(&tp);
    }

    #[test]
    fn resume_keeps_session_counters_startup_resets_them() {
        let mut d = daemon();
        hook(&mut d, "SessionStart", "k", r#","source":"startup""#);
        hook(&mut d, "UserPromptSubmit", "k", "");
        hook(&mut d, "PreToolUse", "k", r#","tool_name":"Bash""#);
        let started = d.sessions["k"].started - 60_000;
        d.sessions.get_mut("k").unwrap().started = started;
        hook(&mut d, "SessionStart", "k", r#","source":"resume""#);
        let s = &d.sessions["k"];
        assert_eq!((s.started, s.prompts, s.tools, s.status), (started, 1, 1, Status::Idle));
        hook(&mut d, "SessionStart", "k", r#","source":"clear""#);
        let s = &d.sessions["k"];
        assert!(s.started > started);
        assert_eq!((s.prompts, s.tools), (0, 0));
        hook(&mut d, "UserPromptSubmit", "k", "");
        hook(&mut d, "SessionStart", "k", r#","source":"startup""#);
        assert_eq!(d.sessions["k"].prompts, 0);
        d.presenter.shutdown();
    }

    #[test]
    fn huge_usage_renders() {
        let mut d = daemon();
        let m = u64::MAX;
        let line = format!(
            r#"{{"type":"assistant","timestamp":"2026-10-04T10:00:00Z","message":{{"id":"h","model":"claude-opus-5-5","usage":{{"input_tokens":{m},"output_tokens":{m},"cache_read_input_tokens":{m},"cache_creation_input_tokens":{m}}}}}}}"#
        );
        let (tp, tp_s) = transcript("huge", &(line + "\n"));
        d.handle_hook(
            format!("UserPromptSubmit\n{{\"session_id\":\"h\",\"cwd\":\"/p/h\",\"transcript_path\":\"{tp_s}\"}}")
                .as_bytes(),
        );
        let a = activity(&mut d);
        assert!(a["state"].as_str().unwrap().contains("tokens"));
        let _ = d.render(timeutil::now_ms());
        d.presenter.shutdown();
        let _ = std::fs::remove_file(&tp);
    }
}
