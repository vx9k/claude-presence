//! The long-running process. One thread accepts hook messages, one owns the
//! Discord socket, and the main thread runs an event loop that sleeps until
//! the next hook or the next timer deadline — no polling, no async runtime.

use crate::config::{Config, Template};
use crate::discord::Presenter;
use crate::git::{self, GitInfo};
use crate::ledger::{self, Ledger};
use crate::presence::{self, Activity, Vars};
use crate::state::{self, SessionInfo, StateSnapshot};
use crate::timeutil::{self, fmt_count, fmt_duration_ms, fmt_hours_ms};
use crate::{ipc, paths};
use serde::Deserialize;
use sonic_rs::{JsonValueTrait, LazyValue};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender};
use std::time::{Duration, Instant};

pub enum Msg {
    Hook(Vec<u8>),
    Reload,
    Shutdown,
    /// `__state`: send the snapshot reply line back, unless the listener has
    /// stopped waiting for it (the deadline passed).
    State(SyncSender<String>, Instant),
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
    /// Stable machine name, for `__state`.
    fn key(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Thinking => "thinking",
            Status::Working => "working",
            Status::Compacting => "compacting",
            Status::Notification => "notification",
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
    /// The transcript's prompt count when the running turn's prompt was
    /// submitted; `None` outside a turn (cleared by `Stop`).
    prompt_at_submit: Option<u32>,
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
            prompt_at_submit: None,
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
            if after.first().is_some_and(|c| *c == b'-' || *c == b'.')
                && let Some((minor, _)) = num(&after[1..])
            {
                name.push('.');
                name.push_str(&minor);
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

/// The model a session shows: its transcript's, else the hook's hint.
fn model_name(s: &Session, fs: Option<&ledger::FileState>) -> String {
    fs.and_then(|f| f.model.as_deref()).or(s.model_hint.as_deref()).map(pretty_model).unwrap_or_else(|| "Claude".into())
}

/// The prompt count a session shows. The transcript counts the whole
/// conversation (resumed history included) but not custom slash commands,
/// which also fire `UserPromptSubmit`. Until `Stop`, the running turn's
/// prompt adds one while the transcript hasn't passed its count at submit
/// (the prompt isn't written yet).
fn shown_prompts(s: &Session, fs: Option<&ledger::FileState>) -> u32 {
    match fs {
        Some(f) => f.prompts.saturating_add(u32::from(s.prompt_at_submit.is_some_and(|at| f.prompts <= at))),
        None => s.prompts,
    }
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
/// How long the hook listener waits for the loop to answer `__state`.
const STATE_WAIT: Duration = Duration::from_millis(300);
/// Longest text field (bytes) per session in a `__state` reply.
const STATE_FIELD_MAX: usize = 128;

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
            if let Some(s) = self.sessions.remove(&sid)
                && let Some(k) = &s.transcript
            {
                self.ledger.ingest(k);
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
        if let Some(c) = input.cwd.as_deref()
            && s.cwd.as_os_str() != c
        {
            s.cwd = PathBuf::from(c);
        }
        if let Some(t) = input.transcript_path.as_deref()
            && s.transcript_path.as_deref().map(Path::as_os_str) != Some(t.as_ref())
        {
            s.transcript_path = Some(PathBuf::from(t));
            s.transcript = None;
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
                    s.prompt_at_submit = None;
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
                let key = s.transcript.clone().or_else(|| s.transcript_path.as_deref().and_then(ledger::key_for));
                s.prompt_at_submit = Some(key.and_then(|k| self.ledger.file(&k)).map_or(0, |f| f.prompts));
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
                s.prompt_at_submit = None;
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
                        if p.extension().is_some_and(|x| x == "jsonl")
                            && let Some(k) = ledger::key_for(&p)
                        {
                            self.ledger.ingest(&k);
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
        if let Some(d) = &self.displayed
            && let Some(s) = self.sessions.get(d)
            && tier(s) >= best.1
        {
            return Some(d.clone());
        }
        self.displayed = Some(best.0.clone());
        Some(best.0)
    }

    fn git_info(&mut self, cwd: &Path) -> GitInfo {
        if let Some((at, info)) = self.git.get(cwd)
            && at.elapsed() < GIT_TTL
        {
            return info.clone();
        }
        if self.git.len() > 64 {
            self.git.clear();
        }
        let info = git::inspect(cwd);
        self.git.insert(cwd.to_path_buf(), (Instant::now(), info.clone()));
        info
    }

    /// Git info for `cwd`, the project name shown for it (masked for a
    /// hidden project) and whether it is hidden.
    fn project(&mut self, cwd: &Path) -> (GitInfo, String, bool) {
        let gi = self.git_info(cwd);
        let base = gi.root.as_deref().unwrap_or(cwd);
        let name = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let hidden = self.is_hidden(&name, cwd);
        let name = if hidden { self.cfg.hidden_project_name.clone() } else { name };
        (gi, name, hidden)
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
        let (gi, name, hidden) = self.project(&cwd);
        let s = self.sessions.get(&id)?;
        let fs = s.transcript.as_deref().and_then(|k| self.ledger.file(k));

        let off = timeutil::local_offset_secs();
        let snap = self.ledger.snapshot(now, off);
        let model = model_name(s, fs);
        let usage = fs.map(|f| f.usage).unwrap_or_default();
        let prompts = shown_prompts(s, fs);

        let mut v = Vars::default();
        v.set("project", name);
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
        if self.cfg.github_button
            && !hidden
            && let Some(url) = gi.github
        {
            buttons.push(presence::Button { label: "View on GitHub".into(), url });
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
        // Stats that couldn't load (e.g. a busy database) are loaded before
        // any save: saving the in-memory rebuild would replace them.
        if self.ledger.needs_load() {
            if !self.ledger.retry_load() {
                self.last_save = Instant::now();
                return;
            }
            crate::info!("stats loaded");
            if self.cfg.scan_history {
                self.rescan();
            }
        }
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
        if let Some(k) = &active_key
            && now_i.duration_since(self.last_tail) >= TAIL_EVERY
        {
            self.ledger.ingest(k);
            self.last_tail = now_i;
        }
        if self.cfg.rescan_interval > 0
            && now_i.duration_since(self.last_rescan) >= Duration::from_secs(self.cfg.rescan_interval)
        {
            self.rescan();
        }
        // Unloaded stats are retried on the same deadline, hooks or not.
        let save_due = self.ledger.is_dirty() || self.ledger.needs_load();
        if save_due && now_i.duration_since(self.last_save) >= SAVE_EVERY {
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
        if self.ledger.is_dirty() || self.ledger.needs_load() {
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

    /// The `__state` snapshot, built only on request: the most recently
    /// active sessions as the card would show them (hidden projects masked,
    /// no paths), the card last handed to Discord, and the lifetime stats
    /// unless they couldn't be loaded (a partial rebuild would look like lost
    /// stats).
    fn state(&mut self, now: i64) -> StateSnapshot {
        // Most recent first; among equals, the newer session.
        let mut recent: Vec<(i64, usize, String)> = self
            .sessions
            .iter()
            .map(|(id, s)| (s.last_activity, self.ids.get(id).copied().unwrap_or(0), id.clone()))
            .collect();
        recent.sort_unstable_by(|a, b| b.cmp(a));
        recent.truncate(state::MAX_SESSIONS);
        let mut sessions = Vec::with_capacity(recent.len());
        for (_, _, id) in recent {
            let Some(cwd) = self.sessions.get(&id).map(|s| s.cwd.clone()) else { continue };
            let (gi, project, hidden) = self.project(&cwd);
            let Some(s) = self.sessions.get(&id) else { continue };
            let fs = s.transcript.as_deref().and_then(|k| self.ledger.file(k));
            sessions.push(SessionInfo {
                id: id.chars().take(8).collect(),
                project: presence::clamp(project, STATE_FIELD_MAX),
                branch: if hidden { String::new() } else { presence::clamp(gi.branch, STATE_FIELD_MAX) },
                status: s.status.key().into(),
                model: presence::clamp(model_name(s, fs), STATE_FIELD_MAX),
                tool: presence::clamp(s.tool.clone().unwrap_or_default(), STATE_FIELD_MAX),
                started_ms: s.started,
                last_event_ms: s.last_activity,
                prompts: shown_prompts(s, fs),
                tools: s.tools,
                tokens: fs.map(|f| f.usage.total()).unwrap_or(0),
                shown: self.displayed.as_deref() == Some(id.as_str()),
            });
        }
        let card = match &self.last_given {
            Some(Some(activity)) => sonic_rs::from_str::<state::Card>(activity).ok(),
            _ => None,
        };
        let stats = (!self.ledger.needs_load()).then(|| self.ledger.stats(now, timeutil::local_offset_secs()));
        StateSnapshot {
            v: state::STATE_VERSION,
            busy: false,
            version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
            now_ms: now,
            discord: self.presenter.status().into(),
            card,
            sessions,
            sessions_total: self.sessions.len().try_into().unwrap_or(u32::MAX),
            stats,
        }
    }

    /// The `__state` reply line.
    fn state_reply(&mut self) -> String {
        self.state(timeutil::now_ms()).encode()
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
        if let Some(tx) = TX.get()
            && let Ok(tx) = tx.lock()
        {
            let _ = tx.send(Msg::Shutdown);
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
/// stops the daemon like SIGTERM (sent by `install`/`uninstall`), `__reload`
/// reloads the config like SIGHUP; other reserved `__` events are ignored,
/// so newer clients can't confuse older daemons. (`__state` is answered by
/// `on_message`.)
fn route(m: Vec<u8>) -> Option<Msg> {
    let event = ipc::event_name(&m);
    if !ipc::is_control(event) {
        return Some(Msg::Hook(m));
    }
    if event == ipc::SHUTDOWN.as_bytes() {
        crate::info!("shutdown requested");
        return Some(Msg::Shutdown);
    }
    if event == ipc::RELOAD.as_bytes() {
        return Some(Msg::Reload);
    }
    crate::debug!("ignoring control event {}", String::from_utf8_lossy(event));
    None
}

/// The hook listener's handler: queue messages for the loop (`route`), and
/// answer `__state` with the loop's snapshot. The listener never touches
/// daemon state itself; if the loop doesn't answer within `wait` (busy with
/// the startup scan, say) the reply says so, rather than holding up hooks.
fn on_message(m: Vec<u8>, tx: &Sender<Msg>, wait: Duration) -> ipc::Action {
    if ipc::event_name(&m) == ipc::STATE.as_bytes() {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if tx.send(Msg::State(reply_tx, Instant::now() + wait)).is_err() {
            return ipc::Action::Stop;
        }
        let line = reply_rx.recv_timeout(wait).unwrap_or_else(|_| state::BUSY_REPLY.to_owned());
        return ipc::Action::Reply(line.into_bytes());
    }
    route(m).is_none_or(|msg| tx.send(msg).is_ok()).into()
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
        .spawn(move || listener.serve(move |m| on_message(m, &htx, STATE_WAIT)))
        .expect("spawn hook listener");
    drop(tx);

    let cfg = Config::load(&paths::config_file());
    crate::info!("listening on {}", addr.display());
    let ledger = Ledger::load(paths::ledger_file());
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
                // Skip requests the listener gave up on (queued behind the
                // startup scan): nobody would read the snapshot.
                Msg::State(reply, deadline) => {
                    if Instant::now() < deadline {
                        let _ = reply.send(d.state_reply());
                    }
                }
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
        let ledger = Ledger::load(dir.join("l.db"));
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
        // `__reload` does what SIGHUP does (Windows has no SIGHUP).
        assert!(matches!(route(ipc::reload_request()), Some(Msg::Reload)));
        // Other reserved names are dropped without error.
        assert!(route(b"__reloadx\n".to_vec()).is_none());
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
    fn in_flight_prompt_counts_once_until_stop() {
        let mut d = daemon();
        let prompt = |u: &str| {
            format!(r#"{{"type":"user","timestamp":"2026-10-04T10:00:00Z","uuid":"{u}","message":{{"content":"go"}}}}"#)
        };
        // A resumed conversation: the transcript has more prompts than hooks.
        let (tp, tp_s) = transcript("inflight", &format!("{}\n{}\n", prompt("if1"), prompt("if2")));
        let tp_field = format!(",\"transcript_path\":\"{tp_s}\"");
        hook(&mut d, "SessionStart", "if", &format!(",\"source\":\"resume\"{tp_field}"));
        let state = |d: &mut Daemon| activity(d)["state"].as_str().unwrap_or_default().to_owned();
        hook(&mut d, "UserPromptSubmit", "if", &tp_field);
        assert!(state(&mut d).contains("3 prompts"), "the new prompt counts before it is written");
        hook(&mut d, "Notification", "if", &tp_field);
        assert!(state(&mut d).contains("3 prompts"), "still in flight during a notification");
        hook(&mut d, "PreCompact", "if", &tp_field);
        // The default compacting card has no `{prompts}`; the TUI's view does.
        assert_eq!(d.state(timeutil::now_ms()).sessions[0].prompts, 3, "and while compacting");
        hook(&mut d, "Stop", "if", &tp_field);
        let prompts = d.state(timeutil::now_ms()).sessions[0].prompts;
        assert_eq!(prompts, 2, "the turn ended without it being written");
        d.presenter.shutdown();
        let _ = std::fs::remove_file(&tp);
    }

    #[test]
    fn slash_command_hooks_do_not_inflate_prompts() {
        // Custom slash commands fire UserPromptSubmit but are not real
        // prompts in the transcript.
        let mut d = daemon();
        let real = r#"{"type":"user","timestamp":"2026-10-04T10:00:00Z","uuid":"sl1","message":{"content":"go"}}"#;
        let cmd = r#"{"type":"user","timestamp":"2026-10-04T10:00:01Z","uuid":"slc","message":{"content":"<command-name>/x</command-name>"}}"#;
        let (tp, tp_s) = transcript("slash", &format!("{real}\n{cmd}\n{cmd}\n{cmd}\n"));
        let tp_field = format!(",\"transcript_path\":\"{tp_s}\"");
        for _ in 0..4 {
            hook(&mut d, "UserPromptSubmit", "sl", &tp_field);
        }
        // Mid-turn: at most the one prompt that may not be written yet.
        assert_eq!(activity(&mut d)["state"].as_str(), Some("Claude · 2 prompts · 0 tokens"));
        hook(&mut d, "Stop", "sl", &tp_field);
        hook(&mut d, "Notification", "sl", &tp_field);
        assert_eq!(activity(&mut d)["state"].as_str(), Some("Claude · 1 prompt"));
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

    #[test]
    fn busy_stats_load_on_a_later_save() {
        let dir = std::env::temp_dir().join(format!("cp-daemon-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let line = r#"{"type":"user","timestamp":"2026-10-04T10:00:00Z","uuid":"p1","message":{"content":"hi"}}"#;
        let tp = dir.join("t.jsonl");
        std::fs::write(&tp, format!("{line}\n")).unwrap();
        let db = dir.join("ledger.db");
        let mut l = Ledger::load(db.clone());
        l.ingest(&ledger::key_for(&tp).unwrap());
        l.save().unwrap();

        let lock = rusqlite::Connection::open(&db).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        // No history scan: tests must not read the real ~/.claude.
        let cfg = Config { client_id: "0".into(), scan_history: false, ..Config::default() };
        let mut d = Daemon::with_presenter(cfg, Ledger::load(db.clone()), Presenter::inert());
        assert!(d.ledger.needs_load());
        d.save();
        assert!(d.ledger.needs_load(), "still busy: nothing saved");
        lock.execute_batch("ROLLBACK").unwrap();
        drop(lock);
        d.save();
        assert!(!d.ledger.needs_load());
        assert_eq!(d.ledger.totals.prompts, 1, "the stored stats are loaded");
        d.presenter.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unloaded_stats_retry_on_the_save_deadline_without_hooks() {
        let dir = std::env::temp_dir().join(format!("cp-daemon-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tp = dir.join("t.jsonl");
        let line = r#"{"type":"user","timestamp":"2026-10-04T10:00:00Z","uuid":"p1","message":{"content":"hi"}}"#;
        std::fs::write(&tp, format!("{line}\n")).unwrap();
        let db = dir.join("ledger.db");
        let mut l = Ledger::load(db.clone());
        l.ingest(&ledger::key_for(&tp).unwrap());
        l.save().unwrap();

        let lock = rusqlite::Connection::open(&db).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        // No scans: tests must not read the real ~/.claude.
        let cfg = Config { client_id: "0".into(), scan_history: false, rescan_interval: 0, ..Config::default() };
        let mut d = Daemon::with_presenter(cfg, Ledger::load(db.clone()), Presenter::inert());
        assert!(d.ledger.needs_load() && !d.ledger.is_dirty());
        assert!(d.tick() <= SAVE_EVERY, "a retry is scheduled even with nothing to save");
        lock.execute_batch("ROLLBACK").unwrap();
        drop(lock);
        d.last_save = Instant::now().checked_sub(SAVE_EVERY).unwrap();
        d.tick();
        assert!(!d.ledger.needs_load());
        assert_eq!(d.ledger.totals.prompts, 1);
        d.presenter.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_requests_wait_briefly_for_the_loop() {
        let wait = Duration::from_millis(300);
        // The loop answers.
        let (tx, rx) = mpsc::channel::<Msg>();
        let t = std::thread::spawn(move || {
            for m in rx {
                if let Msg::State(r, _) = m {
                    r.send("{\"v\":1,\"pid\":1}\n".into()).unwrap();
                }
            }
        });
        let ipc::Action::Reply(r) = on_message(ipc::state_request(), &tx, wait) else { panic!("no reply") };
        assert_eq!(r, b"{\"v\":1,\"pid\":1}\n");
        drop(tx);
        t.join().unwrap();

        // The loop is busy (the startup scan runs before it): say so, soon.
        let (tx, rx) = mpsc::channel::<Msg>();
        let started = Instant::now();
        let ipc::Action::Reply(r) = on_message(ipc::state_request(), &tx, wait) else { panic!("no reply") };
        assert_eq!(r, crate::state::BUSY_REPLY.as_bytes());
        assert!(started.elapsed() >= wait && started.elapsed() < Duration::from_secs(2));
        // The stale request is still queued, past its deadline: the loop skips it.
        let Ok(Msg::State(late, deadline)) = rx.try_recv() else { panic!("not queued") };
        assert!(Instant::now() >= deadline);
        assert!(late.send(String::new()).is_err(), "nobody waits for it any more");

        // Everything else is queued as before.
        assert!(matches!(on_message(b"Stop\n{}".to_vec(), &tx, wait), ipc::Action::Continue));
        assert!(matches!(rx.try_recv(), Ok(Msg::Hook(_))));
        assert!(matches!(on_message(ipc::reload_request(), &tx, wait), ipc::Action::Continue));
        assert!(matches!(rx.try_recv(), Ok(Msg::Reload)));
        assert!(matches!(on_message(b"__nope\n".to_vec(), &tx, wait), ipc::Action::Continue));
        assert!(rx.try_recv().is_err());
        drop(rx);
        assert!(matches!(on_message(b"Stop\n{}".to_vec(), &tx, wait), ipc::Action::Stop), "loop gone");
    }

    #[test]
    fn state_snapshot_is_private_and_capped() {
        use crate::state::{MAX_SESSIONS, StateSnapshot};
        let mut d = daemon();
        let line = r#"{"type":"assistant","timestamp":"2026-10-04T10:00:00Z","message":{"id":"st1","model":"claude-opus-5-5","usage":{"input_tokens":5,"output_tokens":7}}}"#;
        let (tp, tp_s) = transcript("state-snap", &format!("{line}\n"));
        let tp_field = format!(",\"transcript_path\":\"{tp_s}\"");
        for i in 0..40 {
            hook(&mut d, "Stop", &format!("old-session-{i:02}"), "");
        }
        for s in d.sessions.values_mut() {
            s.last_activity -= 60_000;
        }
        hook(&mut d, "UserPromptSubmit", "secret-proj", "");
        d.sessions.get_mut("secret-proj").unwrap().last_activity -= 1000;
        d.cfg.hidden_projects = vec!["secret-proj".into()];
        hook(&mut d, "UserPromptSubmit", "visible-session-id", &tp_field);
        hook(
            &mut d,
            "PreToolUse",
            "visible-session-id",
            &format!(r#","tool_name":"Edit","tool_input":{{"file_path":"/x/private-file.rs"}}{tp_field}"#),
        );
        d.tick();
        let reply = d.state_reply();
        assert!(reply.len() <= ipc::MAX_STATE && reply.ends_with('\n'));
        // No paths (cwd, tool input, transcript), and no hidden project name.
        for leak in ["/p/", "/x/", "secret-proj", "cp-state-snap"] {
            assert!(!reply.contains(leak), "{leak} leaked: {reply}");
        }
        let s: StateSnapshot = sonic_rs::from_str(&reply).unwrap();
        // Sessions never carry tool input; only the card, which Discord
        // shows anyway, has the file name its template asks for.
        let listed = sonic_rs::to_string(&s.sessions).unwrap();
        assert!(!listed.contains("private-file"), "{listed}");
        // The cap keeps the newest of the equally quiet sessions.
        assert!(listed.contains("old-session-39") && !listed.contains("old-session-00"));
        assert_eq!((s.v, s.busy, s.pid), (crate::state::STATE_VERSION, false, std::process::id()));
        assert_eq!(s.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(s.discord, "disconnected");
        assert_eq!((s.sessions.len(), s.sessions_total), (MAX_SESSIONS, 42));
        // Most recent first: the tool user, then the hidden project.
        let v = &s.sessions[0];
        assert_eq!(
            (v.id.as_str(), v.project.as_str(), v.status.as_str()),
            ("visible-", "visible-session-id", "working")
        );
        assert_eq!((v.model.as_str(), v.tool.as_str(), v.tokens, v.prompts, v.tools), ("Opus 5.5", "Edit", 12, 1, 1));
        assert!(v.shown && v.started_ms > 0 && v.last_event_ms >= v.started_ms);
        let h = &s.sessions[1];
        assert_eq!((h.project.as_str(), h.branch.as_str(), h.shown), ("a private project", "", false));
        assert!(s.sessions[2..].iter().all(|x| x.status == "idle"));
        // The card as last handed to Discord.
        let card = s.card.expect("card");
        assert_eq!(card.details, "Working in visible-session-id");
        assert!(card.state.starts_with("Edit"), "{}", card.state);
        let stats = s.stats.expect("stats");
        assert_eq!(stats.usage.output, 7);
        d.presenter.shutdown();
        let _ = std::fs::remove_file(&tp);
    }

    #[test]
    fn unloaded_stats_are_left_out_of_the_snapshot() {
        let dir = std::env::temp_dir().join(format!("cp-daemon-state-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("ledger.db");
        Ledger::load(db.clone()).save().unwrap();
        let lock = rusqlite::Connection::open(&db).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let cfg = Config { client_id: "0".into(), scan_history: false, ..Config::default() };
        let mut d = Daemon::with_presenter(cfg, Ledger::load(db.clone()), Presenter::inert());
        assert!(d.ledger.needs_load());
        let s: crate::state::StateSnapshot = sonic_rs::from_str(&d.state_reply()).unwrap();
        assert_eq!(s.stats, None, "a partial rebuild would look like lost stats");
        assert_eq!(s.card, None);
        lock.execute_batch("ROLLBACK").unwrap();
        d.presenter.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_round_trip_over_the_hook_endpoint() {
        #[cfg(unix)]
        let addr = std::env::temp_dir().join(format!("cp-daemon-state-{}.sock", std::process::id()));
        #[cfg(windows)]
        let addr = PathBuf::from(format!(r"\\.\pipe\cp-test-daemon-state-{}", std::process::id()));
        let listener = ipc::Listener::bind(&addr).unwrap();
        let (tx, rx) = mpsc::channel::<Msg>();
        std::thread::spawn(move || listener.serve(move |m| on_message(m, &tx, STATE_WAIT)));
        // The loop, as `run` drives it.
        let main = std::thread::spawn(move || {
            let mut d = daemon();
            for m in rx {
                match m {
                    Msg::Hook(b) => d.handle_hook(&b),
                    Msg::State(r, _) => {
                        let _ = r.send(d.state_reply());
                    }
                    Msg::Reload => {}
                    Msg::Shutdown => break,
                }
            }
            d.presenter.shutdown();
        });
        let t = Duration::from_secs(5);
        ipc::send(&addr, b"UserPromptSubmit\n{\"session_id\":\"rt-1\",\"cwd\":\"/p/roundtrip\"}").unwrap();
        let reply = ipc::query_state(&addr, false, t).unwrap().expect("a reply");
        let s: crate::state::StateSnapshot = sonic_rs::from_str(&reply).unwrap();
        assert_eq!(s.sessions.len(), 1);
        assert_eq!((s.sessions[0].id.as_str(), s.sessions[0].project.as_str()), ("rt-1", "roundtrip"));
        assert_eq!(s.sessions[0].status, "thinking");
        ipc::send(&addr, &ipc::shutdown_request()).unwrap();
        main.join().unwrap();
        let _ = std::fs::remove_file(&addr);
    }
}
