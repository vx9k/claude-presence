//! Lifetime statistics from Claude Code transcripts.
//!
//! Transcripts (`~/.claude/projects/**/*.jsonl`) are append-only JSONL. For
//! each file we remember the byte offset we have consumed, so every later
//! read only parses newly appended lines. Lines are split with `memchr`
//! (SIMD) and parsed with `sonic-rs` (SIMD), deserializing only the handful
//! of fields we need and skipping everything else at memory bandwidth.
//!
//! Correctness notes:
//! * Claude Code writes one line per content block and repeats the message's
//!   `usage` on each, so tokens are counted once per `message.id`.
//! * Resumed/forked sessions can copy history into a new file; a global set
//!   of seen message ids (and prompt uuids) keeps those from double counting.
//! * Active time is stored as a per-day bitmap of active minutes, so parallel
//!   sessions and copied history can't inflate "hours on Claude".
//! * Totals only ever grow: Claude Code deletes old transcripts after a while,
//!   but the ledger keeps what it already counted.

use crate::timeutil::{self, MINUTE_MS};
use serde::{Deserialize, Serialize};
use sonic_rs::{JsonValueTrait, LazyValue};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const VERSION: u32 = 1;
/// Gaps between consecutive records shorter than this count as active time.
const ACTIVE_GAP_MS: i64 = 5 * MINUTE_MS;
const READ_CHUNK: usize = 1 << 20;
const RING: usize = 8;
const DAY_WORDS: usize = 23; // 1440 minutes / 64 bits, rounded up
/// Ignore timestamps before Claude Code existed or far in the future.
const TS_FLOOR_MS: i64 = 1_577_836_800_000; // 2020-01-01
const TS_SKEW_MS: i64 = 48 * 3_600_000;

// ---------------------------------------------------------------- records --

#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

impl Usage {
    #[inline]
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
    #[inline]
    fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
    #[inline]
    fn max(&self, o: &Usage) -> Usage {
        Usage {
            input: self.input.max(o.input),
            output: self.output.max(o.output),
            cache_read: self.cache_read.max(o.cache_read),
            cache_write: self.cache_write.max(o.cache_write),
        }
    }
    /// Field-wise growth from `old` to `new` (never negative).
    #[inline]
    fn growth(old: &Usage, new: &Usage) -> Usage {
        Usage {
            input: new.input.saturating_sub(old.input),
            output: new.output.saturating_sub(old.output),
            cache_read: new.cache_read.saturating_sub(old.cache_read),
            cache_write: new.cache_write.saturating_sub(old.cache_write),
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Day {
    pub minutes: [u64; DAY_WORDS],
    pub tokens: u64,
    pub prompts: u32,
    pub turns: u32,
}

impl Day {
    #[inline]
    pub fn active_minutes(&self) -> u32 {
        self.minutes.iter().map(|w| w.count_ones()).sum()
    }
    #[inline]
    fn is_active(&self) -> bool {
        self.turns > 0 || self.prompts > 0 || self.minutes.iter().any(|&w| w != 0)
    }
    fn merge(&mut self, o: &Day) {
        for (a, b) in self.minutes.iter_mut().zip(o.minutes.iter()) {
            *a |= *b;
        }
        self.tokens += o.tokens;
        self.prompts += o.prompts;
        self.turns += o.turns;
    }
    /// Set minutes `from..=to` (both minute-of-day indices).
    fn mark(&mut self, from: u16, to: u16) {
        for m in from..=to.min(1439) {
            self.minutes[(m / 64) as usize] |= 1u64 << (m % 64);
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Totals {
    pub usage: Usage,
    pub prompts: u64,
    pub turns: u64,
    pub sessions: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
enum Seen {
    Pending(u32),
    Counted,
    Dup,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
struct RingEntry {
    id: u64,
    usage: Usage,
    seen: Seen,
}

/// Per-transcript progress and totals.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct FileState {
    pub offset: u64,
    ident: u64,
    last_ts: i64,
    pub usage: Usage,
    pub prompts: u32,
    pub turns: u32,
    pub model: Option<String>,
    #[serde(default)]
    ring: Vec<RingEntry>,
}

// ------------------------------------------------------------- id hashing --

/// FNV-1a + splitmix finalizer: fast, and well mixed in every bit so it can
/// be used directly as a hash-table key.
#[inline]
fn id_hash(s: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

#[derive(Default)]
struct IdHasher(u64);
impl Hasher for IdHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | b as u64;
        }
    }
    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}
type IdSet = HashSet<u64, BuildHasherDefault<IdHasher>>;

// ---------------------------------------------------------- line parsing --

#[derive(Deserialize)]
struct Rec<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    uuid: Option<Cow<'a, str>>,
    #[serde(rename = "isMeta", default)]
    is_meta: Option<bool>,
    #[serde(rename = "isCompactSummary", default)]
    is_compact_summary: Option<bool>,
    #[serde(borrow, default)]
    message: Option<Msg<'a>>,
}

#[derive(Deserialize)]
struct Msg<'a> {
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    model: Option<Cow<'a, str>>,
    #[serde(default)]
    usage: Option<RawUsage>,
    #[serde(borrow, default)]
    content: Option<LazyValue<'a>>,
}

#[derive(Deserialize, Default)]
struct RawUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

impl RawUsage {
    fn usage(&self) -> Usage {
        Usage {
            input: self.input_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
            cache_read: self.cache_read_input_tokens.unwrap_or(0),
            cache_write: self.cache_creation_input_tokens.unwrap_or(0),
        }
    }
}

/// A typed prompt, as opposed to tool results, slash-command echoes and
/// injected reminders that are also stored as `type: "user"`.
fn is_real_prompt(rec: &Rec<'_>, content: &LazyValue<'_>) -> bool {
    if rec.is_meta == Some(true) || rec.is_compact_summary == Some(true) {
        return false;
    }
    if let Some(s) = content.as_str() {
        let s = s.trim_start();
        return !s.is_empty()
            && !s.starts_with("<local-command")
            && !s.starts_with("<command-")
            && !s.starts_with("<system-reminder")
            && !s.starts_with("<bash-");
    }
    let raw = content.as_raw_str();
    // Tool results are by far the most common user records; reject them with
    // a SIMD substring search before touching the structure.
    if memchr::memmem::find(raw.as_bytes(), br#""tool_result""#).is_some() {
        return false;
    }
    let Ok(items) = sonic_rs::from_str::<Vec<LazyValue<'_>>>(raw) else {
        return false;
    };
    items.iter().any(|it| {
        it.get("type").is_some_and(|t| t.as_str() == Some("text"))
            && it.get("text").is_some_and(|t| t.as_str().is_some_and(|s| !s.trim().is_empty()))
    })
}

// --------------------------------------------------------------- ingestion --

#[derive(Clone, Copy)]
struct Pending {
    id: u64,
    usage: Usage,
    day: i32,
    prompt: bool,
    fresh: bool,
}

/// Counters accumulated by one ingestion pass, merged into the ledger later.
#[derive(Default)]
struct Delta {
    totals: Totals,
    days: BTreeMap<i32, Day>,
    new_ids: Vec<u64>,
}

impl Delta {
    fn day(&mut self, d: i32) -> &mut Day {
        self.days.entry(d).or_default()
    }

    fn mark_span(&mut self, from_ms: i64, to_ms: i64, off: i64) {
        let (mut d, end_d) = (timeutil::day_number(from_ms, off), timeutil::day_number(to_ms, off));
        let mut from = timeutil::minute_of_day(from_ms, off);
        while d < end_d {
            self.day(d).mark(from, 1439);
            d += 1;
            from = 0;
        }
        let to = timeutil::minute_of_day(to_ms, off);
        self.day(d).mark(from, to);
    }

    fn merge(&mut self, o: Delta) {
        self.totals.usage.add(&o.totals.usage);
        self.totals.prompts += o.totals.prompts;
        self.totals.turns += o.totals.turns;
        self.totals.sessions += o.totals.sessions;
        for (k, v) in o.days {
            self.day(k).merge(&v);
        }
        self.new_ids.extend(o.new_ids);
    }
}

/// Stable identity of a file, to notice a transcript replaced in place.
fn file_ident(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.ino() ^ meta.dev().rotate_left(32)
    }
    #[cfg(not(unix))]
    {
        meta.created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}

struct Ctx<'a> {
    off: i64,
    now: i64,
    pending: Vec<Pending>,
    delta: &'a mut Delta,
}

fn process_line(line: &[u8], st: &mut FileState, cx: &mut Ctx<'_>) {
    if line.len() < 2 {
        return;
    }
    let Ok(rec) = sonic_rs::from_slice::<Rec<'_>>(line) else {
        return;
    };
    let ts = rec
        .timestamp
        .as_deref()
        .and_then(timeutil::parse_rfc3339_ms)
        .filter(|&t| t >= TS_FLOOR_MS && t <= cx.now + TS_SKEW_MS);
    if let Some(t) = ts {
        if st.last_ts > 0 && t >= st.last_ts && t - st.last_ts < ACTIVE_GAP_MS {
            cx.delta.mark_span(st.last_ts, t, cx.off);
        } else {
            cx.delta.mark_span(t, t, cx.off);
        }
        st.last_ts = st.last_ts.max(t);
    }
    let day = timeutil::day_number(ts.unwrap_or(if st.last_ts > 0 { st.last_ts } else { cx.now }), cx.off);
    let Some(msg) = rec.message.as_ref() else {
        return;
    };
    match rec.kind.as_deref() {
        Some("assistant") => {
            if let Some(m) = msg.model.as_deref() {
                if !m.starts_with('<') && st.model.as_deref() != Some(m) {
                    st.model = Some(m.to_owned());
                }
            }
            let Some(raw) = msg.usage.as_ref() else {
                return;
            };
            let u = raw.usage();
            let Some(id) = msg.id.as_deref() else {
                count_turn(st, cx.delta, &u, day);
                return;
            };
            let h = id_hash(id.as_bytes());
            if let Some(e) = st.ring.iter_mut().find(|e| e.id == h) {
                match e.seen {
                    Seen::Pending(i) => {
                        let p = &mut cx.pending[i as usize];
                        p.usage = p.usage.max(&u);
                    }
                    Seen::Counted => {
                        let g = Usage::growth(&e.usage, &u);
                        if g.total() > 0 {
                            st.usage.add(&g);
                            cx.delta.totals.usage.add(&g);
                            cx.delta.day(day).tokens += g.total();
                        }
                    }
                    Seen::Dup => {}
                }
                e.usage = e.usage.max(&u);
                return;
            }
            if st.ring.len() >= RING {
                st.ring.remove(0);
            }
            st.ring.push(RingEntry { id: h, usage: u, seen: Seen::Pending(cx.pending.len() as u32) });
            cx.pending.push(Pending { id: h, usage: u, day, prompt: false, fresh: false });
        }
        Some("user") => {
            let Some(content) = msg.content.as_ref() else {
                return;
            };
            if !is_real_prompt(&rec, content) {
                return;
            }
            match rec.uuid.as_deref() {
                Some(uuid) => cx.pending.push(Pending {
                    id: id_hash(uuid.as_bytes()),
                    usage: Usage::default(),
                    day,
                    prompt: true,
                    fresh: false,
                }),
                None => count_prompt(st, cx.delta, day),
            }
        }
        _ => {}
    }
}

fn count_turn(st: &mut FileState, d: &mut Delta, u: &Usage, day: i32) {
    st.usage.add(u);
    st.turns += 1;
    d.totals.usage.add(u);
    d.totals.turns += 1;
    let day = d.day(day);
    day.tokens += u.total();
    day.turns += 1;
}

fn count_prompt(st: &mut FileState, d: &mut Delta, day: i32) {
    st.prompts += 1;
    d.totals.prompts += 1;
    d.day(day).prompts += 1;
}

/// Settle pending ids against the global set and count the fresh ones.
fn settle(st: &mut FileState, cx: &mut Ctx<'_>, resolve: &mut dyn FnMut(&mut [Pending])) {
    if cx.pending.is_empty() {
        return;
    }
    resolve(&mut cx.pending);
    for p in &cx.pending {
        if p.fresh {
            cx.delta.new_ids.push(p.id);
            if p.prompt {
                count_prompt(st, cx.delta, p.day);
            } else {
                count_turn(st, cx.delta, &p.usage, p.day);
            }
        }
        if !p.prompt {
            if let Some(e) = st.ring.iter_mut().find(|e| e.id == p.id) {
                e.seen = if p.fresh { Seen::Counted } else { Seen::Dup };
            }
        }
    }
    cx.pending.clear();
}

/// Read whatever was appended to `path` since `st.offset`.
fn ingest_file(
    path: &Path,
    st: &mut FileState,
    buf: &mut Vec<u8>,
    delta: &mut Delta,
    off: i64,
    now: i64,
    resolve: &mut dyn FnMut(&mut [Pending]),
) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let ident = file_ident(&meta);
    if st.ident != ident || meta.len() < st.offset {
        *st = FileState { ident, ..FileState::default() };
    }
    if meta.len() == st.offset {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(st.offset))?;
    buf.clear();
    let mut pos = st.offset;
    let mut cx = Ctx { off, now, pending: Vec::new(), delta };
    loop {
        let filled = buf.len();
        buf.resize(filled + READ_CHUNK, 0);
        let n = match file.read(&mut buf[filled..]) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                buf.truncate(filled);
                continue;
            }
            Err(e) => {
                buf.truncate(filled);
                settle(st, &mut cx, resolve);
                st.offset = pos;
                return Err(e);
            }
        };
        buf.truncate(filled + n);
        if n == 0 {
            break;
        }
        // Only consume complete lines; a partial trailing line waits for the
        // writer to finish it.
        let Some(last_nl) = memchr::memrchr(b'\n', &buf[filled..]).map(|i| i + filled) else {
            continue;
        };
        let mut start = 0;
        for nl in memchr::memchr_iter(b'\n', &buf[..=last_nl]) {
            process_line(&buf[start..nl], st, &mut cx);
            start = nl + 1;
        }
        settle(st, &mut cx, resolve);
        pos += (last_nl + 1) as u64;
        buf.drain(..=last_nl);
    }
    st.offset = pos;
    Ok(true)
}

fn is_subagent(path: &str) -> bool {
    path.contains("/subagents/") || path.contains("\\subagents\\")
}

/// Recursively list `*.jsonl` files (canonical paths) under `root`.
fn walk(root: &Path, out: &mut Vec<String>) {
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            walk(&p, out);
        } else if ft.is_file() && p.extension().is_some_and(|x| x == "jsonl") {
            if let Some(s) = p.to_str() {
                out.push(s.to_owned());
            }
        }
    }
}

/// Key under which a transcript is tracked (canonical path).
pub fn key_for(path: &Path) -> Option<String> {
    fs::canonicalize(path).ok()?.into_os_string().into_string().ok()
}

// ------------------------------------------------------------------ ledger --

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    version: u32,
    totals: Totals,
    days: Vec<(i32, Day)>,
    files: HashMap<String, FileState>,
}

#[derive(Default)]
pub struct Ledger {
    pub totals: Totals,
    pub days: BTreeMap<i32, Day>,
    files: HashMap<String, FileState>,
    seen: IdSet,
    unsaved_ids: Vec<u64>,
    buf: Vec<u8>,
    dirty: bool,
    ledger_path: PathBuf,
    seen_path: PathBuf,
}

/// Aggregates ready for templating.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub today_ms: i64,
    pub today_tokens: u64,
    pub today_prompts: u64,
    pub total_ms: i64,
    pub total_tokens: u64,
    pub total_prompts: u64,
    pub total_sessions: u64,
    pub streak: u32,
}

#[derive(Default, Debug)]
pub struct ScanReport {
    pub files: usize,
    pub changed: usize,
    pub pruned: usize,
}

impl Ledger {
    /// Load from disk; anything missing or unreadable starts a fresh ledger
    /// (the next scan rebuilds it from the transcripts still on disk).
    pub fn load(ledger_path: PathBuf, seen_path: PathBuf) -> Ledger {
        let mut l = Ledger { ledger_path, seen_path, ..Ledger::default() };
        let stored = fs::read(&l.ledger_path)
            .ok()
            .and_then(|b| sonic_rs::from_slice::<Stored>(&b).ok())
            .filter(|s| s.version == VERSION);
        match stored {
            Some(s) => {
                l.totals = s.totals;
                l.days = s.days.into_iter().collect();
                l.files = s.files;
                if let Ok(b) = fs::read(&l.seen_path) {
                    l.seen.reserve(b.len() / 8);
                    l.seen.extend(b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())));
                }
            }
            None => {
                if l.ledger_path.exists() {
                    crate::warn!("{} unreadable; rebuilding stats", l.ledger_path.display());
                }
                let _ = fs::remove_file(&l.seen_path);
            }
        }
        l
    }

    pub fn file(&self, key: &str) -> Option<&FileState> {
        self.files.get(key)
    }

    fn apply(&mut self, d: Delta) {
        if d.totals == Totals::default() && d.days.is_empty() && d.new_ids.is_empty() {
            return;
        }
        self.totals.usage.add(&d.totals.usage);
        self.totals.prompts += d.totals.prompts;
        self.totals.turns += d.totals.turns;
        self.totals.sessions += d.totals.sessions;
        for (k, v) in d.days {
            self.days.entry(k).or_default().merge(&v);
        }
        self.unsaved_ids.extend_from_slice(&d.new_ids);
        self.dirty = true;
    }

    /// Ingest one transcript incrementally (used for live sessions).
    pub fn ingest(&mut self, key: &str) -> bool {
        let now = timeutil::now_ms();
        let off = timeutil::local_offset_secs();
        let mut delta = Delta::default();
        let is_new = !self.files.contains_key(key);
        let mut st = self.files.remove(key).unwrap_or_default();
        let seen = &mut self.seen;
        let mut resolve = |ps: &mut [Pending]| {
            for p in ps {
                p.fresh = seen.insert(p.id);
            }
        };
        let mut buf = std::mem::take(&mut self.buf);
        let res = ingest_file(Path::new(key), &mut st, &mut buf, &mut delta, off, now, &mut resolve);
        // Don't let one huge line pin memory in a long-running daemon.
        if buf.capacity() > 4 * READ_CHUNK {
            buf = Vec::new();
        }
        self.buf = buf;
        match res {
            Ok(changed) => {
                if is_new && !is_subagent(key) {
                    delta.totals.sessions += 1;
                }
                if changed || is_new {
                    self.dirty = true;
                }
                self.files.insert(key.to_owned(), st);
                self.apply(delta);
                changed
            }
            Err(_) => {
                if !is_new || st.offset > 0 {
                    self.files.insert(key.to_owned(), st);
                }
                self.apply(delta);
                false
            }
        }
    }

    /// Scan every transcript under `roots` (in parallel), and forget files
    /// that no longer exist.
    pub fn scan(&mut self, roots: &[PathBuf]) -> ScanReport {
        let now = timeutil::now_ms();
        let off = timeutil::local_offset_secs();
        let mut paths = Vec::new();
        for r in roots {
            if let Ok(c) = fs::canonicalize(r) {
                walk(&c, &mut paths);
            }
        }
        paths.sort_unstable();
        paths.dedup();
        let mut report = ScanReport { files: paths.len(), ..ScanReport::default() };

        let present: HashSet<&str> = paths.iter().map(String::as_str).collect();
        let before = self.files.len();
        self.files.retain(|k, _| present.contains(k.as_str()) || Path::new(k).exists());
        report.pruned = before - self.files.len();
        if report.pruned > 0 {
            self.dirty = true;
        }

        let mut work: Vec<(String, FileState, bool)> = paths
            .iter()
            .map(|p| match self.files.remove(p) {
                Some(st) => (p.clone(), st, false),
                None => (p.clone(), FileState::default(), true),
            })
            .collect();
        drop(present);

        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8);
        let threads = if work.len() < 32 { 1 } else { threads };
        let per = work.len().div_ceil(threads.max(1)).max(1);
        let seen = Mutex::new(std::mem::take(&mut self.seen));

        let results: Vec<(Delta, usize)> = std::thread::scope(|s| {
            let handles: Vec<_> = work
                .chunks_mut(per)
                .map(|chunk| {
                    let seen = &seen;
                    s.spawn(move || {
                        let mut delta = Delta::default();
                        let mut buf = Vec::new();
                        let mut changed = 0;
                        let mut resolve = |ps: &mut [Pending]| {
                            let mut set = seen.lock().unwrap_or_else(|e| e.into_inner());
                            for p in ps {
                                p.fresh = set.insert(p.id);
                            }
                        };
                        for (path, st, is_new) in chunk.iter_mut() {
                            let r = ingest_file(Path::new(path), st, &mut buf, &mut delta, off, now, &mut resolve);
                            if matches!(r, Ok(true)) {
                                changed += 1;
                            }
                            if *is_new && !is_subagent(path) && r.is_ok() {
                                delta.totals.sessions += 1;
                            }
                        }
                        (delta, changed)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });

        self.seen = seen.into_inner().unwrap_or_else(|e| e.into_inner());
        let mut total = Delta::default();
        for (d, c) in results {
            total.merge(d);
            report.changed += c;
        }
        let any_new = work.iter().any(|(_, _, n)| *n);
        for (p, st, _) in work {
            self.files.insert(p, st);
        }
        if report.changed > 0 || any_new {
            self.dirty = true;
        }
        self.apply(total);
        report
    }

    /// Persist if anything changed. The ledger is written atomically before
    /// the seen-id log is appended, so a crash in between can only make us
    /// skip (never double count) a duplicate later.
    pub fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(dir) = self.ledger_path.parent() {
            fs::create_dir_all(dir)?;
        }
        let stored = StoredRef {
            version: VERSION,
            totals: &self.totals,
            days: self.days.iter().map(|(k, v)| (*k, v)).collect(),
            files: &self.files,
        };
        let json = sonic_rs::to_vec(&stored).map_err(io::Error::other)?;
        let tmp = self.ledger_path.with_extension("json.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.ledger_path)?;
        if !self.unsaved_ids.is_empty() {
            let mut bytes = Vec::with_capacity(self.unsaved_ids.len() * 8);
            for id in &self.unsaved_ids {
                bytes.extend_from_slice(&id.to_le_bytes());
            }
            let mut f = fs::OpenOptions::new().create(true).append(true).open(&self.seen_path)?;
            f.write_all(&bytes)?;
            self.unsaved_ids.clear();
            self.unsaved_ids.shrink_to(64);
        }
        self.dirty = false;
        Ok(())
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn snapshot(&self, now_ms: i64, off: i64) -> Snapshot {
        let today = timeutil::day_number(now_ms, off);
        let mut s = Snapshot {
            total_tokens: self.totals.usage.total(),
            total_prompts: self.totals.prompts,
            total_sessions: self.totals.sessions,
            ..Snapshot::default()
        };
        for d in self.days.values() {
            s.total_ms += d.active_minutes() as i64 * MINUTE_MS;
        }
        if let Some(d) = self.days.get(&today) {
            s.today_ms = d.active_minutes() as i64 * MINUTE_MS;
            s.today_tokens = d.tokens;
            s.today_prompts = d.prompts as u64;
        }
        // A streak survives until a whole day passes without activity.
        let active = |d: i32| self.days.get(&d).is_some_and(Day::is_active);
        let mut d = if active(today) { today } else { today - 1 };
        while active(d) {
            s.streak += 1;
            d -= 1;
        }
        s
    }
}

#[derive(Serialize)]
struct StoredRef<'a> {
    version: u32,
    totals: &'a Totals,
    days: Vec<(i32, &'a Day)>,
    files: &'a HashMap<String, FileState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cp-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn asst(id: &str, ts: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","uuid":"u-{id}-{output}","message":{{"id":"{id}","model":"claude-opus-5-5","role":"assistant","content":[{{"type":"text","text":"hi"}}],"usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":10,"cache_creation_input_tokens":5}}}}}}"#
        )
    }

    fn user(uuid: &str, ts: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","uuid":"{uuid}","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn tool_result(uuid: &str, ts: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","uuid":"{uuid}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"x","content":"ok"}}]}}}}"#
        )
    }

    #[test]
    fn counts_incrementally_and_dedups() {
        let dir = tmpdir("ledger");
        let proj = dir.join("projects/-tmp-demo");
        fs::create_dir_all(&proj).unwrap();
        let f = proj.join("s1.jsonl");
        let lines = [
            user("p1", "2026-10-04T10:00:00Z", "hello"),
            asst("m1", "2026-10-04T10:00:05Z", 100, 1),
            asst("m1", "2026-10-04T10:00:06Z", 100, 50), // same message, final usage
            tool_result("t1", "2026-10-04T10:00:07Z"),
            asst("m2", "2026-10-04T10:02:00Z", 200, 20),
        ];
        fs::write(&f, lines.join("\n") + "\n").unwrap();

        let mut l = Ledger::load(dir.join("ledger.json"), dir.join("seen.bin"));
        let r = l.scan(&[dir.join("projects")]);
        assert_eq!(r.files, 1);
        assert_eq!(l.totals.sessions, 1);
        assert_eq!(l.totals.prompts, 1);
        assert_eq!(l.totals.turns, 2);
        assert_eq!(l.totals.usage.input, 300);
        assert_eq!(l.totals.usage.output, 70);
        assert_eq!(l.totals.usage.cache_read, 20);

        // Append a partial line: not consumed until it is complete.
        let key = key_for(&f).unwrap();
        let mut fh = fs::OpenOptions::new().append(true).open(&f).unwrap();
        let next = asst("m3", "2026-10-04T10:03:00Z", 1, 1);
        fh.write_all(&next.as_bytes()[..20]).unwrap();
        l.ingest(&key);
        assert_eq!(l.totals.turns, 2);
        fh.write_all(&next.as_bytes()[20..]).unwrap();
        fh.write_all(b"\n").unwrap();
        l.ingest(&key);
        assert_eq!(l.totals.turns, 3);
        assert_eq!(l.file(&key).unwrap().turns, 3);

        // A resumed session copying history into a new file doesn't double count.
        let f2 = proj.join("s2.jsonl");
        fs::write(&f2, format!("{}\n{}\n{}\n", lines[0], lines[1], asst("m9", "2026-10-04T11:00:00Z", 7, 7))).unwrap();
        l.scan(&[dir.join("projects")]);
        assert_eq!(l.totals.sessions, 2);
        assert_eq!(l.totals.turns, 4);
        assert_eq!(l.totals.prompts, 1);

        // Persist, reload, rescan: nothing changes.
        l.save().unwrap();
        let mut l2 = Ledger::load(dir.join("ledger.json"), dir.join("seen.bin"));
        assert_eq!(l2.totals, l.totals);
        let r = l2.scan(&[dir.join("projects")]);
        assert_eq!(r.changed, 0);
        assert_eq!(l2.totals, l.totals);

        // Deleted transcripts are forgotten, but their totals stay.
        fs::remove_file(&f2).unwrap();
        let r = l2.scan(&[dir.join("projects")]);
        assert_eq!(r.pruned, 1);
        assert_eq!(l2.totals, l.totals);

        // Active minutes: 10:00..10:03 continuous (gaps < 5 min) plus 11:00.
        let off = 0;
        let day = timeutil::day_number(timeutil::parse_rfc3339_ms("2026-10-04T10:00:00Z").unwrap(), off);
        assert_eq!(l2.days[&day].active_minutes(), 5);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_filter() {
        let parse = |s: &str| {
            let rec: Rec<'_> = sonic_rs::from_str(s).unwrap();
            let c = rec.message.as_ref().unwrap().content.clone().unwrap();
            is_real_prompt(&rec, &c)
        };
        assert!(parse(&user("a", "2026-10-04T10:00:00Z", "fix the bug")));
        assert!(!parse(&user("a", "2026-10-04T10:00:00Z", "<command-name>/clear</command-name>")));
        assert!(!parse(&tool_result("a", "2026-10-04T10:00:00Z")));
        assert!(parse(r#"{"type":"user","message":{"content":[{"type":"text","text":"yo"},{"type":"image"}]}}"#));
        assert!(!parse(r#"{"type":"user","isMeta":true,"message":{"content":"x"}}"#));
    }

    #[test]
    fn streak() {
        let mut l = Ledger::default();
        let now = timeutil::parse_rfc3339_ms("2026-10-04T12:00:00Z").unwrap();
        let today = timeutil::day_number(now, 0);
        for d in [today - 1, today - 2, today - 4] {
            l.days.entry(d).or_default().turns = 1;
        }
        assert_eq!(l.snapshot(now, 0).streak, 2);
        l.days.entry(today).or_default().prompts = 1;
        assert_eq!(l.snapshot(now, 0).streak, 3);
    }
}
