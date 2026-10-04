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
//!   of seen message ids (and prompt uuids) keeps those from double counting
//!   in the totals, while each file's own stats count all it contains.
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
/// `FileState::schema` of the current per-file counting rules.
const SCHEMA: u8 = 1;
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
        self.input.saturating_add(self.output).saturating_add(self.cache_read).saturating_add(self.cache_write)
    }
    /// Saturating: a corrupt transcript with absurd counts must not wrap
    /// totals around (they only ever grow) or panic in debug builds.
    #[inline]
    fn add(&mut self, o: &Usage) {
        self.input = self.input.saturating_add(o.input);
        self.output = self.output.saturating_add(o.output);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
        self.cache_write = self.cache_write.saturating_add(o.cache_write);
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
        self.tokens = self.tokens.saturating_add(o.tokens);
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

/// Per-transcript progress and the file's own view of its conversation:
/// `usage`/`prompts`/`turns` count everything the file contains, including
/// history copied from another transcript (which the totals count once).
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
    /// Counting rules the per-file stats were built with (0: before
    /// `SCHEMA`); an older state is re-read once from the start.
    #[serde(default)]
    schema: u8,
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

/// Stable identity of an open file, to notice a transcript replaced in place.
/// `meta` is the file's metadata, already fetched by the caller.
#[cfg(unix)]
fn file_ident(_file: &File, meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino() ^ meta.dev().rotate_left(32)
}

/// Stable identity of an open file: volume serial + 128-bit file id (not the
/// creation time, which NTFS tunneling carries over to a file recreated under
/// the same name). 0 if the file system can't tell.
#[cfg(windows)]
fn file_ident(file: &File, _meta: &fs::Metadata) -> u64 {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx};
    // SAFETY: FILE_ID_INFO is plain data; all-zero is a valid value.
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the lifetime of `file`, and the buffer is
    // a FILE_ID_INFO of exactly the size passed, as FileIdInfo requires.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return 0;
    }
    fold_ident(info.VolumeSerialNumber, info.FileId.Identifier)
}

/// Fold a volume serial and a 128-bit file id into a never-zero u64 (0 is
/// what a failed lookup stores).
#[cfg(any(windows, test))]
fn fold_ident(vol: u64, id: [u8; 16]) -> u64 {
    let mut b = [0u8; 24];
    b[..8].copy_from_slice(&vol.to_le_bytes());
    b[8..].copy_from_slice(&id);
    id_hash(&b).max(1)
}

struct Ctx<'a> {
    off: i64,
    now: i64,
    pending: Vec<Pending>,
    delta: &'a mut Delta,
    /// The line was already counted before the file was re-read: it marks no
    /// minutes, and lines without an id (no dedup possible) only rebuild the
    /// file's own view.
    file_only: bool,
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
        // A re-read line's minutes are already marked, possibly under another
        // UTC offset: marking them again could add minutes or even days.
        let from = if st.last_ts > 0 && t >= st.last_ts && t - st.last_ts < ACTIVE_GAP_MS { st.last_ts } else { t };
        if !cx.file_only {
            cx.delta.mark_span(from, t, cx.off);
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
                file_turn(st, &u);
                if !cx.file_only {
                    total_turn(cx.delta, &u, day);
                }
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
                            let d = cx.delta.day(day);
                            d.tokens = d.tokens.saturating_add(g.total());
                        }
                    }
                    // Counted in another file: only this file's view grows.
                    Seen::Dup => st.usage.add(&Usage::growth(&e.usage, &u)),
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
                None => {
                    st.prompts += 1;
                    if !cx.file_only {
                        total_prompt(cx.delta, day);
                    }
                }
            }
        }
        _ => {}
    }
}

/// A turn in this file's own view (copied history included).
fn file_turn(st: &mut FileState, u: &Usage) {
    st.usage.add(u);
    st.turns += 1;
}

/// A turn in the lifetime and per-day counters.
fn total_turn(d: &mut Delta, u: &Usage, day: i32) {
    d.totals.usage.add(u);
    d.totals.turns += 1;
    let day = d.day(day);
    day.tokens = day.tokens.saturating_add(u.total());
    day.turns += 1;
}

/// A prompt in the lifetime and per-day counters.
fn total_prompt(d: &mut Delta, day: i32) {
    d.totals.prompts += 1;
    d.day(day).prompts += 1;
}

/// Settle pending ids against the global set: every one counts in this
/// file's view, only the fresh ones in the totals.
fn settle(st: &mut FileState, cx: &mut Ctx<'_>, resolve: &mut dyn FnMut(&mut [Pending])) {
    if cx.pending.is_empty() {
        return;
    }
    resolve(&mut cx.pending);
    for p in &cx.pending {
        if p.prompt {
            st.prompts += 1;
        } else {
            file_turn(st, &p.usage);
        }
        if p.fresh {
            cx.delta.new_ids.push(p.id);
            if p.prompt {
                total_prompt(cx.delta, p.day);
            } else {
                total_turn(cx.delta, &p.usage, p.day);
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
    let ident = file_ident(&file, &meta);
    // Replaced, truncated, or counted under older per-file rules: start over.
    // Lines below the old offset are already in the totals; their ids are
    // deduped globally, and lines without one only rebuild this file's view
    // (undercounts if the content really is new, never double counts).
    let mut reread_end = 0;
    if st.ident != ident || meta.len() < st.offset || st.schema < SCHEMA {
        reread_end = st.offset;
        *st = FileState { ident, schema: SCHEMA, ..FileState::default() };
    }
    if meta.len() == st.offset {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(st.offset))?;
    buf.clear();
    let mut pos = st.offset;
    let mut cx = Ctx { off, now, pending: Vec::new(), delta, file_only: false };
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
            cx.file_only = pos + (start as u64) < reread_end;
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

/// Truncate `seen.bin` to a whole number of 8-byte ids.
fn realign_seen(path: &Path) -> io::Result<()> {
    let f = fs::OpenOptions::new().write(true).open(path)?;
    let len = f.metadata()?.len();
    f.set_len(len & !7)
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
                    if b.len() % 8 != 0 {
                        // A torn append: drop the partial id so later appends stay aligned.
                        if let Err(e) = realign_seen(&l.seen_path) {
                            crate::warn!("{}: {e}", l.seen_path.display());
                        }
                    }
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
        self.ingest_at(key, timeutil::local_offset_secs(), timeutil::now_ms())
    }

    fn ingest_at(&mut self, key: &str, off: i64, now: i64) -> bool {
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

    /// Persist if anything changed. New seen ids are appended to `seen.bin`
    /// and synced *before* `ledger.json` is atomically replaced: a crash in
    /// between (or a ledger write that keeps failing) leaves ids marked seen
    /// whose counts were never saved: everything since the last successful
    /// ledger write is undercounted, never double counted.
    pub fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(dir) = self.ledger_path.parent() {
            fs::create_dir_all(dir)?;
        }
        self.append_seen()?;
        self.write_ledger()?;
        self.dirty = false;
        Ok(())
    }

    /// Append and sync the ids counted since the last save.
    fn append_seen(&mut self) -> io::Result<()> {
        if self.unsaved_ids.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(self.unsaved_ids.len() * 8);
        for id in &self.unsaved_ids {
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        // Realign through a separate write handle: on Windows an append-only
        // handle lacks the FILE_WRITE_DATA access that truncation needs.
        if fs::metadata(&self.seen_path).is_ok_and(|m| m.len() % 8 != 0) {
            realign_seen(&self.seen_path)?;
        }
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&self.seen_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        self.unsaved_ids.clear();
        self.unsaved_ids.shrink_to(64);
        Ok(())
    }

    /// Atomically replace `ledger.json` (temp file + rename).
    fn write_ledger(&self) -> io::Result<()> {
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
        fs::rename(&tmp, &self.ledger_path)
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

    /// A ledger over one transcript `name` in a fresh temp dir.
    fn one_file(name: &str, content: &[u8]) -> (PathBuf, PathBuf, String, Ledger) {
        let dir = tmpdir(name);
        let f = dir.join("t.jsonl");
        fs::write(&f, content).unwrap();
        let key = key_for(&f).unwrap();
        let l = Ledger::load(dir.join("ledger.json"), dir.join("seen.bin"));
        (dir, f, key, l)
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let mut content = Vec::new();
        for l in [
            "",
            "{",
            "not json at all",
            "[]",
            "42",
            "null",
            r#"{"type":"assistant"}"#,
            r#"{"type":"assistant","message":{"id":"x"}}"#,
            r#"{"type":"assistant","message":{"id":"neg","usage":{"input_tokens":-5}}}"#,
            r#"{"type":"assistant","message":{"id":"str","usage":{"input_tokens":"5"}}}"#,
            r#"{"type":"user","message":{"content":42}}"#,
            r#"{"type":"user","uuid":"bad-ts","timestamp":"yesterday","message":{"content":"hi"}}"#,
        ] {
            content.extend_from_slice(l.as_bytes());
            content.push(b'\n');
        }
        content.extend_from_slice(b"\xff\xfe{\"type\":\"user\"}\n");
        content.extend_from_slice(asst("ok", "2026-10-04T10:00:00Z", 3, 4).as_bytes());
        content.extend_from_slice(b"\r\n");
        let (dir, _, key, mut l) = one_file("malformed", &content);
        assert!(l.ingest(&key));
        assert_eq!(l.totals.turns, 1, "only the valid assistant line counts");
        assert_eq!(l.totals.usage.input, 3);
        assert_eq!(l.totals.prompts, 1, "a bad timestamp doesn't drop the prompt");
        assert_eq!(l.file(&key).unwrap().offset, content.len() as u64);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trailing_partial_line_waits() {
        let full = asst("m1", "2026-10-04T10:00:00Z", 1, 1) + "\n";
        let partial = asst("m2", "2026-10-04T10:00:01Z", 1, 1);
        let (dir, _, key, mut l) = one_file("partial", (full.clone() + &partial).as_bytes());
        l.ingest(&key);
        l.ingest(&key);
        assert_eq!(l.totals.turns, 1);
        assert_eq!(l.file(&key).unwrap().offset, full.len() as u64);
        // A file with no newline at all is never consumed.
        let (dir2, _, key2, mut l2) = one_file("partial-only", partial.as_bytes());
        l2.ingest(&key2);
        assert_eq!((l2.totals.turns, l2.file(&key2).unwrap().offset), (0, 0));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&dir2);
    }

    #[test]
    fn duplicates_count_once() {
        let mut lines = vec![user("p1", "2026-10-04T10:00:00Z", "hi"), user("p1", "2026-10-04T10:00:01Z", "hi")];
        lines.push(asst("m1", "2026-10-04T10:00:02Z", 10, 1));
        // More distinct messages than the per-file ring remembers...
        for i in 0..2 * RING {
            lines.push(asst(&format!("f{i}"), "2026-10-04T10:00:03Z", 1, 1));
        }
        // ...then the first id again, as a resumed session would copy it.
        lines.push(asst("m1", "2026-10-04T10:00:04Z", 10, 1));
        let (dir, f, key, mut l) = one_file("dups", (lines.join("\n") + "\n").as_bytes());
        l.ingest(&key);
        assert_eq!(l.totals.prompts, 1);
        assert_eq!(l.totals.turns, 1 + 2 * RING as u64);
        assert_eq!(l.totals.usage.input, 10 + 2 * RING as u64);
        // The same ids appended later, in a separate read, still don't count.
        let mut fh = fs::OpenOptions::new().append(true).open(&f).unwrap();
        writeln!(fh, "{}\n{}", user("p1", "2026-10-04T10:01:00Z", "hi"), asst("m1", "2026-10-04T10:01:00Z", 10, 1))
            .unwrap();
        l.ingest(&key);
        assert_eq!((l.totals.prompts, l.totals.turns), (1, 1 + 2 * RING as u64));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaced_transcript_is_reread_without_double_counting() {
        let a = asst("m1", "2026-10-04T10:00:00Z", 5, 5);
        let (dir, f, key, mut l) = one_file("replaced", (a.clone() + "\n" + &a + "\n").as_bytes());
        l.ingest(&key);
        assert_eq!(l.totals.turns, 1);
        // Truncated and rewritten shorter: start over, but known ids stay counted.
        fs::write(&f, format!("{a}\n")).unwrap();
        l.ingest(&key);
        assert_eq!(l.totals.turns, 1);
        // (Shorter again, so the rewrite is noticed: same-inode rewrites are
        // detected by length, transcripts being append-only.)
        fs::write(&f, format!("{}\n", asst("m", "2026-10-04T10:00:00Z", 1, 1))).unwrap();
        l.ingest(&key);
        assert_eq!(l.totals.turns, 2);
        // Replaced by a different file (new inode / file id) of the same length.
        let tmp = dir.join("t.tmp");
        fs::write(&tmp, format!("{}\n", asst("n", "2026-10-04T10:00:00Z", 1, 1))).unwrap();
        fs::rename(&tmp, &f).unwrap();
        l.ingest(&key);
        assert_eq!(l.totals.turns, 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn huge_token_counts_saturate() {
        let max = u64::MAX;
        let lines = [
            asst("big1", "2026-10-04T10:00:00Z", max, max),
            asst("big2", "2026-10-04T10:00:01Z", max, 1),
            // Usage growth on an already counted message.
            asst("big2", "2026-10-04T10:00:02Z", max, max),
        ];
        let (dir, _, key, mut l) = one_file("huge", (lines.join("\n") + "\n").as_bytes());
        l.ingest(&key);
        assert_eq!(l.totals.turns, 2);
        assert_eq!(l.totals.usage.total(), max);
        assert_eq!(l.file(&key).unwrap().usage.input, max);
        assert_eq!(l.days.values().map(|d| d.tokens).max(), Some(max));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_timestamps_mark_no_minutes() {
        let lines = [asst("old", "2019-12-31T23:59:59Z", 1, 1), asst("future", "2999-01-01T00:00:00Z", 1, 1)];
        let (dir, _, key, mut l) = one_file("ts-range", (lines.join("\n") + "\n").as_bytes());
        l.ingest(&key);
        assert_eq!(l.totals.turns, 2, "tokens still count");
        assert!(l.days.values().all(|d| d.active_minutes() == 0));
        let _ = fs::remove_dir_all(&dir);
    }

    fn seen_len(dir: &Path) -> u64 {
        fs::metadata(dir.join("seen.bin")).unwrap().len()
    }

    fn append_bytes(p: &Path, b: &[u8]) {
        fs::OpenOptions::new().append(true).open(p).unwrap().write_all(b).unwrap();
    }

    #[test]
    fn torn_seen_bin_is_realigned() {
        let a = asst("m1", "2026-10-04T10:00:00Z", 1, 1);
        let (dir, f, key, mut l) = one_file("torn", format!("{a}\n").as_bytes());
        l.ingest(&key);
        l.save().unwrap();
        assert_eq!(seen_len(&dir), 8);

        // A torn append left 3 stray bytes: load ignores and truncates them.
        append_bytes(&dir.join("seen.bin"), &[1, 2, 3]);
        let mut l = Ledger::load(dir.join("ledger.json"), dir.join("seen.bin"));
        assert_eq!(seen_len(&dir), 8);
        assert!(l.seen.contains(&id_hash(b"m1")));

        // Torn again while running: the next append realigns first.
        append_bytes(&dir.join("seen.bin"), &[4, 5]);
        append_bytes(&f, format!("{}\n", asst("m2", "2026-10-04T10:00:01Z", 1, 1)).as_bytes());
        l.ingest(&key);
        l.save().unwrap();
        assert_eq!(seen_len(&dir), 16);
        let l = Ledger::load(dir.join("ledger.json"), dir.join("seen.bin"));
        assert_eq!(l.seen.len(), 2);
        assert!(l.seen.contains(&id_hash(b"m1")) && l.seen.contains(&id_hash(b"m2")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_between_seen_and_ledger_never_double_counts() {
        let m1 = asst("m1", "2026-10-04T10:00:00Z", 1, 1);
        let m2 = asst("m2", "2026-10-04T10:00:01Z", 1, 1);
        let (dir, f, key, mut l) = one_file("crash", format!("{m1}\n").as_bytes());
        l.ingest(&key);
        l.save().unwrap();
        let saved = l.totals.clone();

        // m2 arrives; the daemon dies after seen.bin is synced but before
        // ledger.json is replaced.
        append_bytes(&f, format!("{m2}\n").as_bytes());
        l.ingest(&key);
        assert_eq!(l.totals.turns, 2);
        l.append_seen().unwrap();
        drop(l);

        // Restart, then a resumed session copies m2 into a new transcript.
        let mut l = Ledger::load(dir.join("ledger.json"), dir.join("seen.bin"));
        assert_eq!(l.totals, saved);
        fs::write(dir.join("resumed.jsonl"), format!("{m2}\n")).unwrap();
        l.scan(std::slice::from_ref(&dir));
        assert_eq!(l.totals.turns, saved.turns, "m2 may be lost, never counted twice");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seen_ids_persist_even_if_ledger_write_fails() {
        let (dir, _, key, mut l) =
            one_file("order", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 1, 1)).as_bytes());
        l.ingest(&key);
        // A directory in the way makes the ledger rename fail.
        fs::create_dir_all(dir.join("ledger.json/x")).unwrap();
        assert!(l.save().is_err());
        assert_eq!(seen_len(&dir), 8, "seen.bin must be written before ledger.json");
        assert!(l.is_dirty(), "the ledger write is retried on the next save");
        fs::remove_dir_all(dir.join("ledger.json")).unwrap();
        l.save().unwrap();
        assert_eq!(seen_len(&dir), 8, "ids are appended once");
        assert!(!l.is_dirty());
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

    #[test]
    fn fold_ident_is_deterministic_and_never_zero() {
        let id = |b: u8| {
            let mut a = [0u8; 16];
            a[0] = b;
            a
        };
        assert_eq!(fold_ident(7, id(1)), fold_ident(7, id(1)));
        assert_ne!(fold_ident(7, id(1)), fold_ident(7, id(2)), "file id matters");
        assert_ne!(fold_ident(7, id(1)), fold_ident(8, id(1)), "volume matters");
        let mut hi = [0u8; 16];
        hi[15] = 1;
        assert_ne!(fold_ident(7, id(0)), fold_ident(7, hi), "every id byte matters");
        assert_ne!(fold_ident(0, [0; 16]), 0);
        // 0 is what a failed lookup stores; no real identity maps to it.
        for v in 0..10_000u64 {
            assert_ne!(fold_ident(v, id((v % 251) as u8)), 0);
        }
    }

    #[test]
    fn ident_change_rereads_without_double_counting() {
        // An upgrade that changes how identities are computed makes every
        // stored ident mismatch once: the file is re-read from offset 0 and
        // the global id dedup keeps the totals unchanged.
        let lines = [
            user("p1", "2026-10-04T10:00:00Z", "hello"),
            asst("m1", "2026-10-04T10:00:05Z", 100, 1),
            asst("m1", "2026-10-04T10:00:06Z", 100, 50),
            asst("m2", "2026-10-04T10:02:00Z", 200, 20),
        ];
        let content = lines.join("\n") + "\n";
        let (dir, _, key, mut l) = one_file("ident-change", content.as_bytes());
        l.ingest(&key);
        let before = l.totals.clone();
        let days_before = l.days.clone();
        assert_eq!((before.prompts, before.turns, before.usage.total()), (1, 2, 400));
        l.files.get_mut(&key).unwrap().ident ^= 0x5a5a;
        assert!(l.ingest(&key), "a mismatched ident re-reads the file");
        assert_eq!(l.file(&key).unwrap().offset, content.len() as u64);
        assert_eq!(l.totals, before);
        assert_eq!(l.days, days_before);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A prompt with no `uuid` and a turn with no `message.id`: counted every
    /// time they are read.
    fn no_id_lines(ts: &str) -> String {
        format!(
            "{}\n{}\n",
            r#"{"type":"user","timestamp":"TS","message":{"role":"user","content":"no uuid"}}"#.replace("TS", ts),
            r#"{"type":"assistant","timestamp":"TS","message":{"usage":{"input_tokens":3,"output_tokens":4}}}"#
                .replace("TS", ts)
        )
    }

    fn file_view(st: &FileState) -> (u32, u32, Usage) {
        (st.prompts, st.turns, st.usage)
    }

    #[test]
    fn copied_history_counts_in_the_new_file_not_in_totals() {
        // `claude --resume` writes a new transcript that copies the earlier
        // conversation (same uuids and message ids).
        let lines = [
            user("p1", "2026-10-04T10:00:00Z", "hello"),
            asst("m1", "2026-10-04T10:00:05Z", 100, 1),
            asst("m1", "2026-10-04T10:00:06Z", 100, 50),
            user("p2", "2026-10-04T10:01:00Z", "more"),
            asst("m2", "2026-10-04T10:02:00Z", 200, 20),
        ];
        let content = lines.join("\n") + "\n";
        let (dir, _, a, mut l) = one_file("copied", content.as_bytes());
        l.ingest(&a);
        let (totals, days) = (l.totals.clone(), l.days.clone());
        let fb = dir.join("b.jsonl");
        fs::write(&fb, &content).unwrap();
        let b = key_for(&fb).unwrap();
        l.ingest(&b);
        assert_eq!(l.totals.usage, totals.usage);
        assert_eq!((l.totals.prompts, l.totals.turns), (totals.prompts, totals.turns));
        assert_eq!(l.days, days);
        let view = file_view(l.file(&a).unwrap());
        assert_eq!(view, (2, 2, Usage { input: 300, output: 70, cache_read: 20, cache_write: 10 }));
        assert_eq!(file_view(l.file(&b).unwrap()), view, "the copy shows the conversation's own stats");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn known_id_streamed_again_counts_once_per_file() {
        let (dir, _, a, mut l) =
            one_file("stream-dup", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 10, 50)).as_bytes());
        l.ingest(&a);
        let (totals, days) = (l.totals.clone(), l.days.clone());
        // A second file streams the same message: partial usage, then final.
        let fb = dir.join("b.jsonl");
        fs::write(
            &fb,
            format!("{}\n{}\n", asst("m1", "2026-10-04T10:00:00Z", 10, 1), asst("m1", "2026-10-04T10:00:01Z", 10, 50)),
        )
        .unwrap();
        let b = key_for(&fb).unwrap();
        l.ingest(&b);
        assert_eq!(file_view(l.file(&b).unwrap()), file_view(l.file(&a).unwrap()));
        assert_eq!(l.file(&b).unwrap().turns, 1);
        assert_eq!((l.totals.usage, l.totals.turns), (totals.usage, totals.turns));
        // Usage growth on a message already counted in another file, across
        // separate reads: the file's view grows, the totals don't.
        let fc = dir.join("c.jsonl");
        fs::write(&fc, format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 10, 1))).unwrap();
        let c = key_for(&fc).unwrap();
        l.ingest(&c);
        assert_eq!(l.file(&c).unwrap().usage.output, 1);
        append_bytes(&fc, format!("{}\n", asst("m1", "2026-10-04T10:00:01Z", 10, 50)).as_bytes());
        l.ingest(&c);
        assert_eq!(file_view(l.file(&c).unwrap()), file_view(l.file(&a).unwrap()));
        assert_eq!((l.totals.usage, l.totals.turns), (totals.usage, totals.turns));
        assert_eq!(l.days, days);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn known_prompt_counts_in_the_file_only() {
        let p = user("p1", "2026-10-04T10:00:00Z", "hello");
        let (dir, _, a, mut l) = one_file("known-prompt", format!("{p}\n").as_bytes());
        l.ingest(&a);
        let fb = dir.join("b.jsonl");
        fs::write(&fb, format!("{p}\n")).unwrap();
        let b = key_for(&fb).unwrap();
        l.ingest(&b);
        assert_eq!(l.file(&b).unwrap().prompts, 1);
        assert_eq!(l.totals.prompts, 1);
        assert_eq!(l.days.values().map(|d| d.prompts).sum::<u32>(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_schema_file_state_is_reread_once_for_its_own_view() {
        // Before schema 1, a file's own stats only counted globally fresh ids.
        let content = [user("p1", "2026-10-04T10:00:00Z", "hello"), asst("m1", "2026-10-04T10:00:05Z", 100, 50)]
            .join("\n")
            + "\n"
            + &no_id_lines("2026-10-04T10:00:06Z");
        let (dir, _, key, mut l) = one_file("schema", content.as_bytes());
        l.ingest(&key);
        assert_eq!(l.file(&key).unwrap().schema, SCHEMA, "new files start at the current schema");
        let view = file_view(l.file(&key).unwrap());
        assert_eq!((view.0, view.1), (2, 2));
        let (totals, days) = (l.totals.clone(), l.days.clone());
        // What an older daemon stored for a resumed copy: nothing of its own.
        let st = l.files.get_mut(&key).unwrap();
        (st.schema, st.prompts, st.turns, st.usage) = (0, 0, 0, Usage::default());
        assert!(l.ingest(&key), "an old schema re-reads the file");
        assert_eq!(file_view(l.file(&key).unwrap()), view);
        assert_eq!(l.file(&key).unwrap().schema, SCHEMA);
        assert_eq!(l.file(&key).unwrap().offset, content.len() as u64);
        assert_eq!(l.totals, totals, "no-id lines are not counted again");
        assert_eq!(l.days, days);
        // And only once.
        assert!(!l.ingest(&key));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reread_after_an_offset_change_keeps_days() {
        // Minutes already marked under one UTC offset must not be marked
        // again under another (DST, travel) when the file is re-read.
        let content = [
            user("p1", "2026-10-04T23:30:00Z", "hello"),
            asst("m1", "2026-10-04T23:31:00Z", 5, 5),
            asst("m2", "2026-10-04T23:58:00Z", 5, 5),
        ]
        .join("\n")
            + "\n"
            + &no_id_lines("2026-10-04T23:59:00Z");
        let (dir, _, key, mut l) = one_file("reread-off", content.as_bytes());
        let now = timeutil::parse_rfc3339_ms("2026-10-05T12:00:00Z").unwrap();
        l.ingest_at(&key, 0, now);
        let (totals, days) = (l.totals.clone(), l.days.clone());
        let st = l.files.get_mut(&key).unwrap();
        (st.schema, st.prompts, st.turns, st.usage) = (0, 0, 0, Usage::default());
        assert!(l.ingest_at(&key, 3600, now));
        assert_eq!(l.totals, totals);
        assert_eq!(l.days, days, "no minutes, days or counters added by the re-read");
        let view = file_view(l.file(&key).unwrap());
        assert_eq!((view.0, view.1), (2, 3));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reread_of_a_known_file_does_not_recount_no_id_lines() {
        let content = asst("m1", "2026-10-04T10:00:00Z", 5, 5) + "\n" + &no_id_lines("2026-10-04T10:00:01Z");
        let (dir, f, key, mut l) = one_file("reread-noid", content.as_bytes());
        l.ingest(&key);
        let (totals, days) = (l.totals.clone(), l.days.clone());
        assert_eq!((totals.prompts, totals.turns), (1, 2));
        // Ident mismatch (e.g. how identities are computed changed).
        l.files.get_mut(&key).unwrap().ident ^= 0x5a5a;
        assert!(l.ingest(&key));
        assert_eq!((l.totals.clone(), l.days.clone()), (totals.clone(), days.clone()));
        assert_eq!(file_view(l.file(&key).unwrap()).1, 2, "the file's own view is rebuilt");
        // Rewritten shorter: also re-read from 0 without recounting.
        let shorter = no_id_lines("2026-10-04T10:00:01Z");
        fs::write(&f, &shorter).unwrap();
        assert!(l.ingest(&key));
        assert_eq!(l.totals, totals);
        assert_eq!((l.file(&key).unwrap().prompts, l.file(&key).unwrap().turns), (1, 1));
        // New content appended afterwards counts normally.
        append_bytes(&f, no_id_lines("2026-10-04T10:00:02Z").as_bytes());
        l.ingest(&key);
        assert_eq!((l.totals.prompts, l.totals.turns), (totals.prompts + 1, totals.turns + 1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_ident_survives_append_not_recreate() {
        let dir = tmpdir("ident-win");
        let f = dir.join("t.jsonl");
        let ident = |p: &Path| {
            let f = File::open(p).unwrap();
            file_ident(&f, &f.metadata().unwrap())
        };
        fs::write(&f, b"a\n").unwrap();
        let first = ident(&f);
        assert_ne!(first, 0);
        append_bytes(&f, b"b\n");
        assert_eq!(ident(&f), first, "an append keeps the identity");
        // Recreated under the same name right away: NTFS tunneling carries the
        // creation time over, the file id does not.
        fs::remove_file(&f).unwrap();
        fs::write(&f, b"a\n").unwrap();
        assert_ne!(ident(&f), first, "a recreated file is a new identity");
        let _ = fs::remove_dir_all(&dir);
    }
}
