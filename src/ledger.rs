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
//! * Everything is kept in memory; `ledger.db` (SQLite) is only the durable
//!   store. A save writes the rows changed since the last one in a single
//!   transaction, so counted ids, totals and file offsets stay consistent.

use crate::timeutil::{self, MINUTE_MS};
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sonic_rs::{JsonValueTrait, LazyValue};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// `FileState::schema` of the current per-file counting rules.
const SCHEMA: u8 = 1;
/// `FileState::ident_v` of the current `file_ident` rule.
const IDENT_V: u8 = 1;
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
    #[serde(default)]
    ident_v: u8,
    /// End of the region already counted in the totals before a re-read
    /// from 0; persisted so a re-read cut short by an I/O error doesn't
    /// count it again when it resumes. 0 once the re-read has passed it.
    #[serde(default, skip_serializing_if = "is_zero")]
    counted_to: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
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
    /// Read from the region already counted before a re-read (`file_only`).
    counted: bool,
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
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    };
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
    if ok != 0 {
        return fold_ident(info.VolumeSerialNumber, info.FileId.Identifier);
    }
    // File systems without FileIdInfo (e.g. some FAT or network volumes):
    // the 32-bit volume serial and 64-bit file index.
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data; all-zero is valid.
    let mut bh: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the lifetime of `file` and `bh` is a valid out pointer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut bh) } == 0 {
        return 0;
    }
    fold_index(bh.dwVolumeSerialNumber, bh.nFileIndexHigh, bh.nFileIndexLow)
}

/// [`fold_ident`] of a 32-bit volume serial and a 64-bit file index, laid
/// out as the 128-bit id NTFS reports for the same index (zero-extended).
#[cfg(any(windows, test))]
fn fold_index(vol: u32, high: u32, low: u32) -> u64 {
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&((u64::from(high) << 32) | u64::from(low)).to_le_bytes());
    fold_ident(u64::from(vol), id)
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
    /// minutes and adds nothing to the totals or days, only to the file's own
    /// view (its ids still go into the global set, healing forgotten ones).
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
            if let Some(m) = msg.model.as_deref()
                && !m.starts_with('<')
                && st.model.as_deref() != Some(m)
            {
                st.model = Some(m.to_owned());
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
                        st.usage.add(&g);
                        if g.total() > 0 && !cx.file_only {
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
            cx.pending.push(Pending { id: h, usage: u, day, prompt: false, fresh: false, counted: cx.file_only });
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
                    counted: cx.file_only,
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
/// file's view, only the fresh ones in the totals, and not those read from
/// an already counted region (fresh there means they were forgotten: they
/// are only recorded again).
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
        }
        if p.fresh && !p.counted {
            if p.prompt {
                total_prompt(cx.delta, p.day);
            } else {
                total_turn(cx.delta, &p.usage, p.day);
            }
        }
        if !p.prompt
            && let Some(e) = st.ring.iter_mut().find(|e| e.id == p.id)
        {
            // A healed id (fresh in an already counted region) may be a
            // copy counted elsewhere: its later growth stays out of the totals.
            e.seen = if p.fresh && !p.counted { Seen::Counted } else { Seen::Dup };
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
    // Lines below the old offset are already in the totals: they only rebuild
    // this file's view and record their ids (undercounts if the content
    // really is new, never double counts, even with the ids forgotten).
    // An identity stored under an older `file_ident` rule can't be compared:
    // adopt today's unless the file shrank (then it was rewritten).
    let adopted = st.ident_v < IDENT_V && meta.len() >= st.offset;
    if adopted {
        (st.ident, st.ident_v) = (ident, IDENT_V);
    }
    let reset = st.ident != ident || meta.len() < st.offset || st.schema < SCHEMA;
    // A known file's reset state must be saved even if nothing is read.
    let reset_known = reset && (st.ident != 0 || st.offset != 0);
    if reset {
        // Persisted, so a re-read cut short by an error doesn't recount it.
        let counted_to = st.counted_to.max(st.offset).min(meta.len());
        *st = FileState { ident, schema: SCHEMA, ident_v: IDENT_V, counted_to, ..FileState::default() };
    }
    if meta.len() == st.offset {
        return Ok(adopted || reset_known);
    }
    file.seek(SeekFrom::Start(st.offset))?;
    buf.clear();
    let mut pos = st.offset;
    let mut cx = Ctx { off, now, pending: Vec::new(), delta, file_only: false };
    loop {
        let filled = buf.len();
        // `read_to_end` grows `buf` as needed without zero-filling it.
        let n = match (&mut file).take(READ_CHUNK as u64).read_to_end(buf) {
            Ok(n) => n,
            Err(e) => {
                buf.truncate(filled);
                settle(st, &mut cx, resolve);
                st.offset = pos;
                return Err(e);
            }
        };
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
            cx.file_only = pos + (start as u64) < st.counted_to;
            process_line(&buf[start..nl], st, &mut cx);
            start = nl + 1;
        }
        settle(st, &mut cx, resolve);
        pos += (last_nl + 1) as u64;
        buf.drain(..=last_nl);
    }
    st.offset = pos;
    if st.offset >= st.counted_to {
        st.counted_to = 0;
    }
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
        } else if ft.is_file()
            && p.extension().is_some_and(|x| x == "jsonl")
            && let Some(s) = p.to_str()
        {
            out.push(s.to_owned());
        }
    }
}

/// Key under which a transcript is tracked (canonical path).
pub fn key_for(path: &Path) -> Option<String> {
    fs::canonicalize(path).ok()?.into_os_string().into_string().ok()
}

// ------------------------------------------------------------------ ledger --

/// Whether the database may be written.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
enum Store {
    #[default]
    Ready,
    /// Not loaded (busy, or unreadable for now): saving would replace the
    /// stored totals with whatever was rebuilt in memory. Load is retried.
    Unavailable,
    /// Written by a newer release: never touched.
    Newer,
}

#[derive(Default)]
pub struct Ledger {
    pub totals: Totals,
    pub days: BTreeMap<i32, Day>,
    files: HashMap<String, FileState>,
    seen: IdSet,
    // What changed since the last save: saves only write these rows.
    unsaved_ids: Vec<u64>,
    dirty_days: BTreeSet<i32>,
    dirty_files: HashSet<String>,
    deleted_files: HashSet<String>,
    totals_dirty: bool,
    buf: Vec<u8>,
    db_path: PathBuf,
    store: Store,
    /// The state was loaded from, or saved to, the database: saves may
    /// write only what changed.
    from_db: bool,
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

/// How many days, today included, [`Stats::days`] covers.
pub const STATS_DAYS: i32 = 60;

/// Lifetime totals and recent days, as the daemon's `__state` reply and
/// [`read_only_stats`] give them to `claude-presence tui`. Every field
/// defaults, so readers of other versions still parse it.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Stats {
    /// Local day number (days since the Unix epoch) the stats were taken on.
    pub today: i32,
    pub usage: Usage,
    pub prompts: u64,
    pub turns: u64,
    pub sessions: u64,
    /// Lifetime active time.
    pub active_ms: i64,
    pub streak: u32,
    /// Days with activity in the last [`STATS_DAYS`], oldest first.
    pub days: Vec<DayStats>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct DayStats {
    pub day: i32,
    pub active_minutes: u32,
    pub tokens: u64,
    pub prompts: u32,
    pub turns: u32,
}

#[derive(Default, Debug)]
pub struct ScanReport {
    pub files: usize,
    pub changed: usize,
    pub pruned: usize,
}

impl Ledger {
    /// Load from `db_path`. Anything missing starts a fresh ledger (the next
    /// scan rebuilds it from the transcripts still on disk); a corrupt
    /// database is moved aside first. A busy database leaves the ledger
    /// unloaded: it works in memory and refuses to save until
    /// [`Ledger::retry_load`] succeeds.
    pub fn load(db_path: PathBuf) -> Ledger {
        Ledger::load_with(db_path, false)
    }

    /// [`Ledger::load`]; a `repeat` failure is logged at debug level only.
    fn load_with(db_path: PathBuf, repeat: bool) -> Ledger {
        let mut l = Ledger { db_path, ..Ledger::default() };
        let mut res = l.read_db();
        if let Err(DbError::Sql(e)) = &res
            && is_corrupt(e)
        {
            crate::warn!("{}: {e}; moved aside, rebuilding stats", l.db_path.display());
            l.reset();
            move_aside(&l.db_path);
            res = l.read_db();
        }
        match res {
            Ok(()) => {}
            Err(DbError::Newer(v)) => {
                crate::error!(
                    "{} has version {v}, from a newer claude-presence; stats will not be saved",
                    l.db_path.display()
                );
                l.reset();
                l.store = Store::Newer;
            }
            Err(e) => {
                let e = e.describe(&l.db_path);
                // Retried on every save: a lasting cause is reported once.
                if repeat {
                    crate::debug!("{e}; stats will load later");
                } else {
                    crate::warn!("{e}; stats will load later");
                }
                l.reset();
                l.store = Store::Unavailable;
            }
        }
        l
    }

    /// Whether a load failed for a reason that may pass (e.g. a busy
    /// database), or a database appeared that this ledger didn't load.
    pub fn needs_load(&self) -> bool {
        self.store == Store::Unavailable
    }

    /// Try loading again after [`Ledger::needs_load`]; on success the stored
    /// state replaces the one rebuilt in memory meanwhile.
    pub fn retry_load(&mut self) -> bool {
        let l = Ledger::load_with(self.db_path.clone(), true);
        if l.needs_load() {
            return false;
        }
        *self = l;
        true
    }

    /// Forget a partially read state.
    fn reset(&mut self) {
        let db_path = std::mem::take(&mut self.db_path);
        *self = Ledger { db_path, ..Ledger::default() };
    }

    fn read_db(&mut self) -> Result<(), DbError> {
        // Only "not found" means there is nothing to load.
        if !exists(&self.db_path)? {
            return Ok(());
        }
        let mut conn = open(&self.db_path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE)?;
        // A write lock up front: a first load creates the schema.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.from_db = true;
        match user_version(&tx)? {
            0 => {
                tx.execute_batch(SCHEMA_SQL)?;
                self.write_rows(&tx, true)?;
                // Last: until this commits, the next load starts over.
                tx.pragma_update(None, "user_version", DB_VERSION)?;
                tx.commit()?;
            }
            DB_VERSION => {
                read_stats(&tx, self)?;
                read_files(&tx, &mut self.files)?;
                let n: i64 = tx.query_row("SELECT count(*) FROM seen", [], |r| r.get(0))?;
                self.seen.reserve(n.max(0) as usize);
                let mut q = tx.prepare("SELECT id FROM seen")?;
                let mut rows = q.query([])?;
                while let Some(r) = rows.next()? {
                    self.seen.insert(r.get::<_, i64>(0)? as u64);
                }
            }
            v => return Err(DbError::Newer(v)),
        }
        Ok(())
    }

    pub fn file(&self, key: &str) -> Option<&FileState> {
        self.files.get(key)
    }

    fn mark_file(&mut self, key: &str) {
        self.deleted_files.remove(key);
        if !self.dirty_files.contains(key) {
            self.dirty_files.insert(key.to_owned());
        }
    }

    fn apply(&mut self, d: Delta) {
        if d.totals != Totals::default() {
            self.totals.usage.add(&d.totals.usage);
            self.totals.prompts += d.totals.prompts;
            self.totals.turns += d.totals.turns;
            self.totals.sessions += d.totals.sessions;
            self.totals_dirty = true;
        }
        for (k, v) in d.days {
            self.days.entry(k).or_default().merge(&v);
            self.dirty_days.insert(k);
        }
        self.unsaved_ids.extend_from_slice(&d.new_ids);
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
        // Keep a small buffer between ingests; a big first read must not pin MiBs.
        if buf.capacity() > 64 << 10 {
            buf = Vec::new();
        }
        self.buf = buf;
        match res {
            Ok(changed) => {
                if is_new && !is_subagent(key) {
                    delta.totals.sessions += 1;
                }
                if changed || is_new {
                    self.mark_file(key);
                }
                self.files.insert(key.to_owned(), st);
                self.apply(delta);
                changed
            }
            Err(_) => {
                // Possibly read partway: the offset is saved with the counts.
                if !is_new || st.offset > 0 {
                    self.mark_file(key);
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
        let (dirty, deleted) = (&mut self.dirty_files, &mut self.deleted_files);
        self.files.retain(|k, _| {
            let keep = present.contains(k.as_str()) || Path::new(k).exists();
            if !keep {
                dirty.remove(k);
                deleted.insert(k.clone());
            }
            keep
        });
        report.pruned = before - self.files.len();

        // (path, state, is_new, changed)
        let mut work: Vec<(String, FileState, bool, bool)> = paths
            .iter()
            .map(|p| match self.files.remove(p) {
                Some(st) => (p.clone(), st, false, false),
                None => (p.clone(), FileState::default(), true, false),
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
                        for (path, st, is_new, dirty) in chunk.iter_mut() {
                            let r = ingest_file(Path::new(path), st, &mut buf, &mut delta, off, now, &mut resolve);
                            if matches!(r, Ok(true)) {
                                changed += 1;
                            }
                            if *is_new && !is_subagent(path) && r.is_ok() {
                                delta.totals.sessions += 1;
                            }
                            // An error may come after a partial read.
                            *dirty = *is_new || !matches!(r, Ok(false));
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
        for (p, st, _, dirty) in work {
            if dirty {
                self.mark_file(&p);
            }
            self.files.insert(p, st);
        }
        self.apply(total);
        report
    }

    /// Persist what changed since the last save, in one transaction: the
    /// counted ids, the totals and days they went into, and the file offsets
    /// past them are stored together, so a crash or a failed save never
    /// loses or double counts anything (the transcripts are read again from
    /// the stored offsets). Refuses to write a database it could not load.
    pub fn save(&mut self) -> io::Result<()> {
        self.save_with(|| Ok(()))
    }

    /// [`Ledger::save`], running `before_commit` (a test failpoint) last.
    fn save_with(&mut self, before_commit: impl FnOnce() -> rusqlite::Result<()>) -> io::Result<()> {
        match self.store {
            Store::Ready => {}
            Store::Unavailable => {
                return Err(io::Error::other(format!("{} is not loaded; not saving", self.db_path.display())));
            }
            Store::Newer => {
                self.clear_dirty();
                return Err(io::Error::other(format!(
                    "{} is from a newer claude-presence; not saving",
                    self.db_path.display()
                )));
            }
        }
        if !self.is_dirty() {
            return Ok(());
        }
        if let Some(dir) = self.db_path.parent() {
            fs::create_dir_all(dir)?;
        }
        match self.write_db(before_commit) {
            Ok(()) => {
                self.clear_dirty();
                self.from_db = true;
                Ok(())
            }
            Err(e @ DbError::Newer(_)) => {
                self.store = Store::Newer;
                self.clear_dirty();
                Err(io::Error::other(format!("{}; not saving", e.describe(&self.db_path))))
            }
            // Loaded (replacing the in-memory state) before the next save.
            Err(e @ DbError::Appeared) => {
                self.store = Store::Unavailable;
                Err(io::Error::other(format!("{}; loading it instead of saving", e.describe(&self.db_path))))
            }
            Err(DbError::Sql(e)) => Err(io::Error::other(e)),
            Err(DbError::Io(_, e)) => Err(e),
        }
    }

    fn write_db(&self, before_commit: impl FnOnce() -> rusqlite::Result<()>) -> Result<(), DbError> {
        let mut conn = open(&self.db_path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A database that vanished since load is recreated in full.
        let full = match user_version(&tx)? {
            0 => {
                tx.execute_batch(SCHEMA_SQL)?;
                true
            }
            DB_VERSION if !self.from_db => return Err(DbError::Appeared),
            DB_VERSION => false,
            v => return Err(DbError::Newer(v)),
        };
        self.write_rows(&tx, full)?;
        if full {
            tx.pragma_update(None, "user_version", DB_VERSION)?;
        }
        before_commit()?;
        tx.commit()?;
        Ok(())
    }

    /// Write the changed rows, or every row if `full`.
    fn write_rows(&self, tx: &Connection, full: bool) -> rusqlite::Result<()> {
        let mut q = tx.prepare("INSERT OR IGNORE INTO seen(id) VALUES (?1)")?;
        if full {
            for &id in &self.seen {
                q.execute([id as i64])?;
            }
        } else {
            for &id in &self.unsaved_ids {
                q.execute([id as i64])?;
            }
        }
        if full || self.totals_dirty {
            let (t, u) = (&self.totals, &self.totals.usage);
            tx.execute(
                "INSERT OR REPLACE INTO totals(id, input, output, cache_read, cache_write, prompts, turns, sessions)
                 VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    u.input as i64,
                    u.output as i64,
                    u.cache_read as i64,
                    u.cache_write as i64,
                    t.prompts as i64,
                    t.turns as i64,
                    t.sessions as i64
                ],
            )?;
        }
        let mut q =
            tx.prepare("INSERT OR REPLACE INTO day(day, minutes, tokens, prompts, turns) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        let mut day = |k: i32, d: &Day| {
            let mut minutes = [0u8; DAY_WORDS * 8];
            for (c, w) in minutes.as_chunks_mut::<8>().0.iter_mut().zip(d.minutes.iter()) {
                *c = w.to_le_bytes();
            }
            q.execute(params![k, &minutes[..], d.tokens as i64, d.prompts, d.turns])
        };
        if full {
            for (&k, d) in &self.days {
                day(k, d)?;
            }
        } else {
            for &k in &self.dirty_days {
                if let Some(d) = self.days.get(&k) {
                    day(k, d)?;
                }
            }
        }
        let mut q = tx.prepare(
            "INSERT OR REPLACE INTO file(path, pos, ident, last_ts, input, output, cache_read, cache_write,
                 prompts, turns, model, ring, schema_v, ident_v, counted_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?;
        let mut file = |k: &str, st: &FileState| {
            let ring = sonic_rs::to_string(&st.ring).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
            let u = &st.usage;
            q.execute(params![
                k,
                st.offset as i64,
                st.ident as i64,
                st.last_ts,
                u.input as i64,
                u.output as i64,
                u.cache_read as i64,
                u.cache_write as i64,
                st.prompts,
                st.turns,
                st.model,
                ring,
                st.schema,
                st.ident_v,
                st.counted_to as i64
            ])
        };
        if full {
            for (k, st) in &self.files {
                file(k, st)?;
            }
        } else {
            for k in &self.dirty_files {
                if let Some(st) = self.files.get(k) {
                    file(k, st)?;
                }
            }
        }
        let mut q = tx.prepare("DELETE FROM file WHERE path = ?1")?;
        for k in &self.deleted_files {
            q.execute([k])?;
        }
        Ok(())
    }

    fn clear_dirty(&mut self) {
        self.unsaved_ids.clear();
        self.unsaved_ids.shrink_to(64);
        self.dirty_days.clear();
        self.dirty_files.clear();
        self.deleted_files.clear();
        self.totals_dirty = false;
    }

    /// Whether a save has anything to write (never, for a database from a
    /// newer release).
    pub fn is_dirty(&self) -> bool {
        self.store != Store::Newer
            && (self.totals_dirty
                || !self.unsaved_ids.is_empty()
                || !self.dirty_days.is_empty()
                || !self.dirty_files.is_empty()
                || !self.deleted_files.is_empty())
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

    /// Totals plus the last [`STATS_DAYS`] days, for `claude-presence tui`.
    pub fn stats(&self, now_ms: i64, off: i64) -> Stats {
        let snap = self.snapshot(now_ms, off);
        let today = timeutil::day_number(now_ms, off);
        let days = self
            .days
            .range(today - (STATS_DAYS - 1)..)
            .map(|(&day, d)| DayStats {
                day,
                active_minutes: d.active_minutes(),
                tokens: d.tokens,
                prompts: d.prompts,
                turns: d.turns,
            })
            .collect();
        Stats {
            today,
            usage: self.totals.usage,
            prompts: self.totals.prompts,
            turns: self.totals.turns,
            sessions: self.totals.sessions,
            active_ms: snap.total_ms,
            streak: snap.streak,
            days,
        }
    }
}

/// Totals and days only (for `status`), read without creating, migrating or
/// locking out anything. Errors if the database is busy or unreadable.
pub fn load_stats(db_path: &Path) -> io::Result<Ledger> {
    Ok(read_stored(db_path, BUSY_TIMEOUT)?.unwrap_or_default())
}

/// [`Ledger::stats`] of the stored ledger, for `claude-presence tui` while
/// the daemon is down: like [`load_stats`] (read-only, one read transaction,
/// never the seen ids, never a write, rename or migration), but waits at most
/// [`READ_ONLY_BUSY_TIMEOUT`] for a busy database. `None` if nothing is
/// stored yet.
pub fn read_only_stats(db_path: &Path, now_ms: i64, off: i64) -> io::Result<Option<Stats>> {
    Ok(read_stored(db_path, READ_ONLY_BUSY_TIMEOUT)?.map(|l| l.stats(now_ms, off)))
}

/// The stored totals and days, `None` if nothing is stored yet.
fn read_stored(db_path: &Path, busy: Duration) -> io::Result<Option<Ledger>> {
    let read = || -> Result<Option<Ledger>, DbError> {
        if !exists(db_path)? {
            return Ok(None);
        }
        let mut conn = open(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(busy)?;
        let tx = conn.transaction()?;
        match user_version(&tx)? {
            // Created but not set up yet (a first save that was rolled back).
            0 => Ok(None),
            DB_VERSION => {
                let mut l = Ledger::default();
                read_stats(&tx, &mut l)?;
                Ok(Some(l))
            }
            v => Err(DbError::Newer(v)),
        }
    };
    match read() {
        Ok(l) => Ok(l),
        Err(DbError::Sql(e)) => Err(io::Error::other(e)),
        Err(DbError::Io(_, e)) => Err(e),
        Err(e) => Err(io::Error::other(e.describe(db_path))),
    }
}

// --------------------------------------------------------------- database --

/// `PRAGMA user_version` of the current `ledger.db` schema.
const DB_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_millis(if cfg!(test) { 100 } else { 2000 });
/// A reader that refreshes on its own schedule waits less for a save.
pub const READ_ONLY_BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// u64 counters and ids are stored bit-cast to SQLite's i64.
const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS seen(id INTEGER PRIMARY KEY);
CREATE TABLE IF NOT EXISTS totals(
    id INTEGER PRIMARY KEY CHECK (id = 1),
    input INTEGER NOT NULL, output INTEGER NOT NULL, cache_read INTEGER NOT NULL, cache_write INTEGER NOT NULL,
    prompts INTEGER NOT NULL, turns INTEGER NOT NULL, sessions INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS day(
    day INTEGER PRIMARY KEY, minutes BLOB NOT NULL,
    tokens INTEGER NOT NULL, prompts INTEGER NOT NULL, turns INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS file(
    path TEXT PRIMARY KEY, pos INTEGER NOT NULL, ident INTEGER NOT NULL, last_ts INTEGER NOT NULL,
    input INTEGER NOT NULL, output INTEGER NOT NULL, cache_read INTEGER NOT NULL, cache_write INTEGER NOT NULL,
    prompts INTEGER NOT NULL, turns INTEGER NOT NULL, model TEXT, ring TEXT NOT NULL,
    schema_v INTEGER NOT NULL, ident_v INTEGER NOT NULL, counted_to INTEGER NOT NULL);
";

enum DbError {
    Sql(rusqlite::Error),
    /// A file next to the database (or its metadata) couldn't be read.
    Io(PathBuf, io::Error),
    /// `user_version` above ours.
    Newer(i64),
    /// A ledger that started without a database found a current one at
    /// save time: it must load it rather than write over it.
    Appeared,
}

impl DbError {
    /// `<path>: <what went wrong>`, `db` being the database's path.
    fn describe(&self, db: &Path) -> String {
        match self {
            DbError::Sql(e) => format!("{}: {e}", db.display()),
            DbError::Io(p, e) => format!("{}: {e}", p.display()),
            DbError::Newer(v) => format!("{} has version {v}, from a newer claude-presence", db.display()),
            DbError::Appeared => format!("{} was created by someone else", db.display()),
        }
    }
}

/// Whether `path` exists; errors other than "not found" are not a no.
fn exists(path: &Path) -> Result<bool, DbError> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(DbError::Io(path.to_owned(), e)),
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> DbError {
        DbError::Sql(e)
    }
}

/// A connection for one load or save (none is kept open between them).
fn open(path: &Path, flags: OpenFlags) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA cache_size = -512;")?;
    Ok(conn)
}

fn user_version(c: &Connection) -> rusqlite::Result<i64> {
    c.query_row("PRAGMA user_version", [], |r| r.get(0))
}

fn is_corrupt(e: &rusqlite::Error) -> bool {
    matches!(e.sqlite_error_code(), Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt))
}

/// `path` with `suffix` appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Move a corrupt database out of the way as `<db>.corrupt`, with its
/// journal (which must not be rolled back into a fresh database) as the
/// journal of the moved file.
fn move_aside(db: &Path) {
    for (from, to) in [
        (with_suffix(db, "-journal"), with_suffix(db, ".corrupt-journal")),
        (db.to_owned(), with_suffix(db, ".corrupt")),
    ] {
        match fs::rename(&from, &to) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => crate::warn!("{}: {e}", from.display()),
        }
    }
}

/// Read the totals and days into `l`.
fn read_stats(c: &Connection, l: &mut Ledger) -> rusqlite::Result<()> {
    let t = c
        .query_row(
            "SELECT input, output, cache_read, cache_write, prompts, turns, sessions FROM totals WHERE id = 1",
            [],
            |r| {
                Ok(Totals {
                    usage: Usage {
                        input: r.get::<_, i64>(0)? as u64,
                        output: r.get::<_, i64>(1)? as u64,
                        cache_read: r.get::<_, i64>(2)? as u64,
                        cache_write: r.get::<_, i64>(3)? as u64,
                    },
                    prompts: r.get::<_, i64>(4)? as u64,
                    turns: r.get::<_, i64>(5)? as u64,
                    sessions: r.get::<_, i64>(6)? as u64,
                })
            },
        )
        .optional()?;
    l.totals = t.unwrap_or_default();
    let mut q = c.prepare("SELECT day, minutes, tokens, prompts, turns FROM day")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let mut d = Day { tokens: r.get::<_, i64>(2)? as u64, prompts: r.get(3)?, turns: r.get(4)?, ..Day::default() };
        let blob = r.get_ref(1)?.as_blob()?;
        for (w, c) in d.minutes.iter_mut().zip(blob.as_chunks::<8>().0) {
            *w = u64::from_le_bytes(*c);
        }
        l.days.insert(r.get(0)?, d);
    }
    Ok(())
}

fn read_files(c: &Connection, files: &mut HashMap<String, FileState>) -> rusqlite::Result<()> {
    let mut q = c.prepare(
        "SELECT path, pos, ident, last_ts, input, output, cache_read, cache_write, prompts, turns, model, ring,
                schema_v, ident_v, counted_to FROM file",
    )?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        // An unreadable ring only loses the in-file dedup of the last few
        // messages' usage growth.
        let ring = r.get_ref(11)?.as_str().ok().and_then(|s| sonic_rs::from_str(s).ok()).unwrap_or_default();
        let st = FileState {
            offset: r.get::<_, i64>(1)? as u64,
            ident: r.get::<_, i64>(2)? as u64,
            last_ts: r.get(3)?,
            usage: Usage {
                input: r.get::<_, i64>(4)? as u64,
                output: r.get::<_, i64>(5)? as u64,
                cache_read: r.get::<_, i64>(6)? as u64,
                cache_write: r.get::<_, i64>(7)? as u64,
            },
            prompts: r.get(8)?,
            turns: r.get(9)?,
            model: r.get(10)?,
            ring,
            schema: r.get(12)?,
            ident_v: r.get(13)?,
            counted_to: r.get::<_, i64>(14)? as u64,
        };
        files.insert(r.get(0)?, st);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
    fn stats_cover_totals_and_recent_days() {
        let totals = Totals {
            usage: Usage { input: 1, output: 2, cache_read: 3, cache_write: 4 },
            prompts: 5,
            turns: 6,
            sessions: 7,
        };
        let mut l = Ledger { totals, ..Ledger::default() };
        let now = 20_000 * 86_400_000 + 12 * 3_600_000;
        let today = timeutil::day_number(now, 0);
        let day = |minutes: u16, tokens: u64| {
            let mut d = Day { tokens, prompts: 1, turns: 2, ..Day::default() };
            d.mark(0, minutes - 1);
            d
        };
        // Today, yesterday, the oldest day still in the window and one past it.
        for (n, d) in [(today, day(3, 30)), (today - 1, day(2, 20)), (today - 59, day(1, 10)), (today - 60, day(1, 1))]
        {
            l.days.insert(n, d);
        }
        let s = l.stats(now, 0);
        assert_eq!(s.today, today);
        assert_eq!(s.usage, l.totals.usage);
        assert_eq!((s.prompts, s.turns, s.sessions), (5, 6, 7));
        assert_eq!(s.active_ms, 7 * MINUTE_MS, "every day counts toward the lifetime");
        assert_eq!(s.streak, 2);
        let days: Vec<_> = s.days.iter().map(|d| (d.day, d.active_minutes, d.tokens, d.prompts, d.turns)).collect();
        assert_eq!(days, [(today - 59, 1, 10, 1, 2), (today - 1, 2, 20, 1, 2), (today, 3, 30, 1, 2)]);
        // A JSON round trip keeps it whole (the TUI reads it from `__state`).
        let back: Stats = sonic_rs::from_str(&sonic_rs::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn read_only_stats_never_create_or_write() {
        let dir = tmpdir("ro-stats");
        let off = 0;
        let now = timeutil::now_ms();
        // Nothing stored: nothing is created either.
        assert_eq!(read_only_stats(&db(&dir), now, off).unwrap(), None);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "no database or journal appeared");

        let f = dir.join("t.jsonl");
        fs::write(
            &f,
            format!("{}\n{}\n", user("p1", "2026-10-04T10:00:00Z", "hi"), asst("m1", "2026-10-04T10:00:05Z", 7, 3)),
        )
        .unwrap();
        let mut l = Ledger::load(db(&dir));
        l.ingest(&key_for(&f).unwrap());
        l.save().unwrap();
        // The seen-ids table is never read: the stats load without it.
        raw(&dir).execute_batch("DROP TABLE seen").unwrap();
        let before = fs::read(db(&dir)).unwrap();
        let entries = fs::read_dir(&dir).unwrap().count();

        let s = read_only_stats(&db(&dir), now, off).unwrap().expect("stored stats");
        assert_eq!(s, l.stats(now, off));
        assert_eq!((s.prompts, s.turns, s.usage.input), (1, 1, 7));
        assert_eq!(fs::read(db(&dir)).unwrap(), before, "not written");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), entries, "no journal or other file left behind");

        // Busy: an error, soon, rather than zeros or a wait.
        let lock = raw(&dir);
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let t = std::time::Instant::now();
        assert!(read_only_stats(&db(&dir), now, off).is_err());
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        lock.execute_batch("ROLLBACK").unwrap();
        drop(lock);

        // A newer release's database is not read as ours, and not renamed.
        raw(&dir).pragma_update(None, "user_version", DB_VERSION + 1).unwrap();
        assert!(read_only_stats(&db(&dir), now, off).is_err());
        // Neither is a corrupt one moved aside.
        fs::write(db(&dir), b"not a database at all, just text").unwrap();
        assert!(read_only_stats(&db(&dir), now, off).is_err());
        assert!(db(&dir).exists() && !with_suffix(&db(&dir), ".corrupt").exists());
        let _ = fs::remove_dir_all(&dir);
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

        let mut l = Ledger::load(dir.join("ledger.db"));
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
        let mut l2 = Ledger::load(dir.join("ledger.db"));
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
        let l = Ledger::load(dir.join("ledger.db"));
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
        // detected by length, transcripts being append-only.) Lines below the
        // old offset count as already counted: a new id there is undercounted
        // (it could also be a known one that `seen.bin` lost).
        let m = asst("m", "2026-10-04T10:00:00Z", 1, 1);
        fs::write(&f, format!("{m}\n")).unwrap();
        l.ingest(&key);
        assert_eq!((l.totals.turns, l.file(&key).unwrap().turns), (1, 1));
        append_bytes(&f, format!("{}\n", asst("m2", "2026-10-04T10:00:01Z", 1, 1)).as_bytes());
        l.ingest(&key);
        assert_eq!(l.totals.turns, 2, "content past the old offset counts");
        // Replaced by a different file (new inode / file id) of the same length.
        let tmp = dir.join("t.tmp");
        fs::write(&tmp, format!("{m}\n{}\n", asst("n2", "2026-10-04T10:00:01Z", 1, 1))).unwrap();
        fs::rename(&tmp, &f).unwrap();
        l.ingest(&key);
        assert_eq!((l.totals.turns, l.file(&key).unwrap().turns), (2, 2));
        append_bytes(&f, format!("{}\n", asst("n3", "2026-10-04T10:00:02Z", 1, 1)).as_bytes());
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

    fn append_bytes(p: &Path, b: &[u8]) {
        fs::OpenOptions::new().append(true).open(p).unwrap().write_all(b).unwrap();
    }

    fn db(dir: &Path) -> PathBuf {
        dir.join("ledger.db")
    }

    /// A raw connection, for tampering with or inspecting the database.
    fn raw(dir: &Path) -> rusqlite::Connection {
        rusqlite::Connection::open(db(dir)).unwrap()
    }

    fn row_count(dir: &Path, table: &str) -> i64 {
        raw(dir).query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    fn fail() -> rusqlite::Result<()> {
        Err(rusqlite::Error::ExecuteReturnedResults)
    }

    /// Everything persisted, compared field by field.
    fn assert_same(a: &Ledger, b: &Ledger) {
        assert_eq!(a.totals, b.totals);
        assert_eq!(a.days, b.days);
        assert_eq!(a.seen, b.seen);
        let mut ka: Vec<_> = a.files.keys().collect();
        let mut kb: Vec<_> = b.files.keys().collect();
        ka.sort();
        kb.sort();
        assert_eq!(ka, kb);
        for (k, x) in &a.files {
            let y = &b.files[k];
            assert_eq!(
                (x.offset, x.ident, x.last_ts, file_view(x), &x.model, &x.ring, x.schema, x.ident_v, x.counted_to),
                (y.offset, y.ident, y.last_ts, file_view(y), &y.model, &y.ring, y.schema, y.ident_v, y.counted_to),
                "{k}"
            );
        }
    }

    #[test]
    fn crash_before_commit_counts_exactly_once() {
        let first = [user("p1", "2026-10-04T10:00:00Z", "hello"), asst("m1", "2026-10-04T10:00:05Z", 100, 1)]
            .join("\n")
            + "\n"
            + &no_id_lines("2026-10-04T10:00:06Z");
        let (dir, f, key, mut l) = one_file("crash", first.as_bytes());
        l.scan(std::slice::from_ref(&dir));
        l.save().unwrap();

        // More arrives (id-less lines included); the daemon dies mid-save.
        let more = asst("m2", "2026-10-04T10:01:00Z", 7, 7) + "\n" + &no_id_lines("2026-10-04T10:01:01Z");
        append_bytes(&f, more.as_bytes());
        l.ingest(&key);
        assert!(l.save_with(fail).is_err());
        drop(l);

        // Restart, plus a resumed session copying m2 into a new transcript.
        fs::write(dir.join("resumed.jsonl"), format!("{}\n", asst("m2", "2026-10-04T10:01:00Z", 7, 7))).unwrap();
        let mut l = Ledger::load(db(&dir));
        assert!(!l.needs_load());
        l.scan(std::slice::from_ref(&dir));
        let mut clean = Ledger::default();
        clean.scan(std::slice::from_ref(&dir));
        assert_eq!(l.totals, clean.totals, "counted exactly once");
        assert_eq!(l.days, clean.days);
        assert_eq!((l.totals.prompts, l.totals.turns, l.totals.sessions), (3, 4, 2));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_save_is_retried() {
        let (dir, _, key, mut l) =
            one_file("save-retry", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 1, 1)).as_bytes());
        l.ingest(&key);
        assert!(l.save_with(fail).is_err());
        assert!(l.is_dirty(), "nothing is forgotten after a failed save");
        assert_eq!(load_stats(&db(&dir)).unwrap().totals, Totals::default(), "rolled back");
        l.save().unwrap();
        assert!(!l.is_dirty());
        assert_same(&Ledger::load(db(&dir)), &l);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_db_is_moved_aside() {
        let (dir, _, key, _) =
            one_file("garbage", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 1, 1)).as_bytes());
        let junk = vec![0x5au8; 4096];
        fs::write(db(&dir), &junk).unwrap();
        fs::write(dir.join("ledger.db-journal"), b"stale").unwrap();
        let mut l = Ledger::load(db(&dir));
        assert!(!l.needs_load());
        assert_eq!(l.totals, Totals::default());
        assert_eq!(fs::read(dir.join("ledger.db.corrupt")).unwrap(), junk, "kept for inspection");
        // (SQLite discards an invalid journal itself; a valid one is moved
        // along with the database.)
        assert!(!dir.join("ledger.db-journal").exists(), "no stale journal next to the fresh database");
        l.ingest(&key);
        l.save().unwrap();
        assert_eq!(Ledger::load(db(&dir)).totals.turns, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_page_is_moved_aside() {
        // A valid header with a damaged table page: SQLITE_CORRUPT, not NOTADB.
        let (dir, _, key, mut l) =
            one_file("corrupt-page", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 1, 1)).as_bytes());
        l.ingest(&key);
        l.save().unwrap();
        let mut b = fs::read(db(&dir)).unwrap();
        let page: usize = raw(&dir).query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0)).unwrap() as usize;
        assert!(b.len() >= 3 * page);
        b[page..].fill(0xa5);
        fs::write(db(&dir), &b).unwrap();
        let l = Ledger::load(db(&dir));
        assert!(!l.needs_load());
        assert_eq!(l.totals, Totals::default());
        assert!(dir.join("ledger.db.corrupt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn db_created_meanwhile_is_loaded_not_overwritten() {
        // Started with no database (or couldn't see it); another writer, or a
        // restored backup, puts one there before our first save.
        let (dir, _, key, mut l) =
            one_file("appeared", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 1, 1)).as_bytes());
        let mut other = Ledger::load(db(&dir));
        other.totals.prompts = 50;
        other.totals_dirty = true;
        other.save().unwrap();
        l.ingest(&key);
        assert!(l.save().is_err(), "an in-memory rebuild is not merged over stored stats");
        assert!(l.needs_load());
        assert_eq!(load_stats(&db(&dir)).unwrap().totals.prompts, 50);
        assert!(l.retry_load());
        assert_eq!(l.totals.prompts, 50);
        // Once loaded (or created by us), later saves go through.
        l.ingest(&key);
        l.save().unwrap();
        l.ingest(&key);
        assert_eq!(Ledger::load(db(&dir)).totals.turns, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_db_path_is_retried() {
        // A metadata error other than NotFound must not look like "no database".
        let dir = tmpdir("db-path-err");
        #[cfg(unix)]
        let bad = {
            fs::write(dir.join("f"), b"").unwrap();
            dir.join("f").join("ledger.db") // ENOTDIR
        };
        #[cfg(windows)]
        let bad = dir.join("bad<name").join("ledger.db"); // ERROR_INVALID_NAME
        let mut l = Ledger::load(bad);
        assert!(l.needs_load());
        assert!(l.save().is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn busy_db_is_never_clobbered() {
        let (dir, f, key, mut l) =
            one_file("busy", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 1, 1)).as_bytes());
        l.ingest(&key);
        l.save().unwrap();
        let saved = l.totals.clone();

        let lock = raw(&dir);
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let mut l = Ledger::load(db(&dir));
        assert!(l.needs_load());
        assert!(load_stats(&db(&dir)).is_err(), "status reports busy, not zeros");
        append_bytes(&f, format!("{}\n", asst("m2", "2026-10-04T10:00:01Z", 1, 1)).as_bytes());
        l.ingest(&key);
        assert!(l.save().is_err(), "an unloaded ledger must not overwrite the stored one");
        assert!(!l.retry_load());
        assert_eq!(l.totals.turns, 2, "rebuilt in memory meanwhile");
        lock.execute_batch("ROLLBACK").unwrap();
        drop(lock);

        assert_eq!(load_stats(&db(&dir)).unwrap().totals, saved, "not clobbered");
        assert!(l.retry_load());
        assert!(!l.needs_load());
        assert_eq!(l.totals, saved, "the stored state replaces the in-memory one");
        l.ingest(&key);
        assert_eq!(l.totals.turns, 2);
        l.save().unwrap();
        assert_eq!(Ledger::load(db(&dir)).totals.turns, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_db_is_never_written() {
        let (dir, f, key, mut l) =
            one_file("newer", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 1, 1)).as_bytes());
        l.ingest(&key);
        l.save().unwrap();
        raw(&dir).pragma_update(None, "user_version", DB_VERSION + 1).unwrap();
        let mut l = Ledger::load(db(&dir));
        assert!(!l.needs_load(), "not retried either");
        append_bytes(&f, format!("{}\n", asst("m2", "2026-10-04T10:00:01Z", 1, 1)).as_bytes());
        l.ingest(&key);
        assert!(l.save().is_err());
        assert!(!l.is_dirty(), "the daemon doesn't keep trying");
        assert!(load_stats(&db(&dir)).is_err());
        let v: i64 = raw(&dir).query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, DB_VERSION + 1);
        assert_eq!(row_count(&dir, "seen"), 1, "untouched");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extreme_values_round_trip() {
        let lines =
            [asst("big1", "2026-10-04T10:00:00Z", u64::MAX, u64::MAX), asst("big2", "2026-10-04T10:00:01Z", 1, 1)];
        let (dir, _, key, mut l) = one_file("extreme", (lines.join("\n") + "\n").as_bytes());
        l.ingest(&key);
        for id in [u64::MAX, 1 << 63, 0] {
            l.seen.insert(id);
            l.unsaved_ids.push(id);
        }
        l.files.get_mut(&key).unwrap().counted_to = u64::MAX;
        l.files.get_mut(&key).unwrap().ident = u64::MAX;
        l.save().unwrap();
        assert_same(&Ledger::load(db(&dir)), &l);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruned_files_are_deleted() {
        let (dir, _, _, mut l) =
            one_file("prune", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 1, 1)).as_bytes());
        let b = dir.join("b.jsonl");
        fs::write(&b, format!("{}\n", asst("m2", "2026-10-04T10:00:00Z", 1, 1))).unwrap();
        l.scan(std::slice::from_ref(&dir));
        l.save().unwrap();
        assert_eq!(row_count(&dir, "file"), 2);
        fs::remove_file(&b).unwrap();
        assert_eq!(l.scan(std::slice::from_ref(&dir)).pruned, 1);
        l.save().unwrap();
        assert_eq!(row_count(&dir, "file"), 1);
        let l2 = Ledger::load(db(&dir));
        assert_eq!(l2.files.len(), 1);
        assert_eq!(l2.totals.turns, 2, "totals keep what was counted");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_write_only_dirty_rows() {
        let (dir, _, a, mut l) =
            one_file("delta", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 1, 1)).as_bytes());
        l.ingest(&a);
        l.save().unwrap();
        assert!(!l.is_dirty());
        assert!(l.save().is_ok(), "nothing to do");
        // Tamper with the rows a later save has no reason to touch.
        let day = *l.days.keys().next().unwrap();
        let c = raw(&dir);
        c.execute("UPDATE day SET tokens = 12345 WHERE day = ?1", [day]).unwrap();
        c.execute("UPDATE file SET prompts = 77 WHERE path = ?1", [&a]).unwrap();
        drop(c);
        let fb = dir.join("b.jsonl");
        fs::write(&fb, format!("{}\n", asst("m2", "2026-10-06T10:00:00Z", 1, 1))).unwrap();
        l.ingest(&key_for(&fb).unwrap());
        l.save().unwrap();
        let l2 = Ledger::load(db(&dir));
        assert_eq!(l2.days[&day].tokens, 12345, "a clean day is not rewritten");
        assert_eq!(l2.file(&a).unwrap().prompts, 77, "a clean file is not rewritten");
        assert_eq!(l2.totals, l.totals);
        assert_eq!(l2.files.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn emptied_transcript_reset_is_saved() {
        let (dir, f, key, mut l) =
            one_file("emptied", format!("{}\n", asst("m1", "2026-10-04T10:00:00Z", 1, 1)).as_bytes());
        l.ingest(&key);
        l.save().unwrap();
        fs::write(&f, b"").unwrap();
        assert!(l.ingest(&key), "the reset is a change");
        l.save().unwrap();
        let st = Ledger::load(db(&dir)).files.remove(&key).unwrap();
        assert_eq!((st.offset, st.turns, st.counted_to), (0, 0, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_of_a_missing_db_are_empty_and_create_nothing() {
        let dir = tmpdir("stats-missing");
        let l = load_stats(&db(&dir)).unwrap();
        assert_eq!(l.totals, Totals::default());
        assert!(!db(&dir).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leftover_legacy_ledger_is_ignored() {
        let dir = tmpdir("legacy-ignored");
        let json = r#"{"version":1,"totals":{"prompts":9},"days":[],"files":{}}"#;
        fs::write(dir.join("ledger.json"), json).unwrap();
        assert_eq!(load_stats(&db(&dir)).unwrap().totals, Totals::default());
        let mut l = Ledger::load(db(&dir));
        assert!(!l.needs_load());
        assert_eq!(l.totals, Totals::default());
        l.save().unwrap();
        assert!(dir.join("ledger.json").exists(), "left alone");
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
    fn fold_index_matches_the_zero_extended_file_id() {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&0x0000_0002_0000_0001u64.to_le_bytes());
        assert_eq!(fold_index(7, 2, 1), fold_ident(7, id));
        assert_ne!(fold_index(7, 2, 1), fold_index(7, 1, 2), "high and low are not interchangeable");
        assert_ne!(fold_index(7, 2, 1), fold_index(8, 2, 1));
        assert_ne!(fold_index(0, 0, 0), 0);
    }

    #[test]
    fn ident_change_rereads_without_double_counting() {
        // A file replaced under the same name (a different identity under the
        // current rule) is re-read from offset 0 and the global id dedup keeps
        // the totals unchanged.
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
    fn old_ident_rule_is_adopted_without_rereading() {
        // An upgrade that changes how identities are computed (Windows:
        // creation time → file id) must not re-read every transcript.
        let content = asst("m1", "2026-10-04T23:30:00Z", 5, 5) + "\n" + &no_id_lines("2026-10-04T23:59:00Z");
        let (dir, f, key, mut l) = one_file("ident-v", content.as_bytes());
        let now = timeutil::parse_rfc3339_ms("2026-10-05T12:00:00Z").unwrap();
        l.ingest_at(&key, 0, now);
        assert_eq!(l.file(&key).unwrap().ident_v, IDENT_V, "new files start at the current rule");
        let real = l.file(&key).unwrap().ident;
        let (totals, days) = (l.totals.clone(), l.days.clone());
        let st = l.files.get_mut(&key).unwrap();
        (st.ident_v, st.ident, st.prompts) = (0, real ^ 0x5a5a, 77);
        l.ingest_at(&key, 3600, now);
        let st = l.file(&key).unwrap();
        assert_eq!((st.ident, st.ident_v, st.offset), (real, IDENT_V, content.len() as u64));
        assert_eq!(st.prompts, 77, "not re-read");
        assert_eq!((l.totals.clone(), l.days.clone()), (totals.clone(), days.clone()));
        // A current-rule mismatch still means a replaced file.
        l.files.get_mut(&key).unwrap().ident ^= 0x5a5a;
        assert!(l.ingest_at(&key, 3600, now));
        assert_eq!(l.file(&key).unwrap().prompts, 1);
        assert_eq!((l.totals.clone(), l.days.clone()), (totals.clone(), days.clone()));
        // An old rule on a file that shrank: it was rewritten, so re-read.
        let st = l.files.get_mut(&key).unwrap();
        (st.ident_v, st.prompts) = (0, 77);
        fs::write(&f, no_id_lines("2026-10-04T23:59:00Z")).unwrap();
        assert!(l.ingest_at(&key, 3600, now));
        assert_eq!((l.file(&key).unwrap().prompts, l.file(&key).unwrap().ident_v), (1, IDENT_V));
        assert_eq!(l.totals, totals);
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

    #[test]
    fn interrupted_reread_resumes_without_recounting() {
        // A re-read from 0 that stopped partway (a read error) leaves the
        // offset below the region the totals already counted.
        let first = asst("m1", "2026-10-04T10:00:00Z", 5, 5) + "\n";
        let content = first.clone() + &no_id_lines("2026-10-04T10:00:01Z");
        let (dir, f, key, mut l) = one_file("reread-cut", content.as_bytes());
        l.ingest(&key);
        let (totals, days) = (l.totals.clone(), l.days.clone());
        let st = l.files.get_mut(&key).unwrap();
        (st.offset, st.counted_to, st.prompts, st.turns) = (first.len() as u64, content.len() as u64, 0, 1);
        assert!(l.ingest(&key));
        assert_eq!((l.totals.clone(), l.days.clone()), (totals.clone(), days.clone()));
        let st = l.file(&key).unwrap();
        assert_eq!((st.prompts, st.turns, st.offset), (1, 2, content.len() as u64), "the file's view is rebuilt");
        assert_eq!(st.counted_to, 0, "cleared once passed");
        // Content appended afterwards counts normally.
        append_bytes(&f, no_id_lines("2026-10-04T10:00:02Z").as_bytes());
        l.ingest(&key);
        assert_eq!((l.totals.prompts, l.totals.turns), (totals.prompts + 1, totals.turns + 1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reread_with_forgotten_ids_heals_them_without_recounting() {
        // A long line between two lines of m1 puts them in separate reads, so
        // the second one is usage growth on an already settled message.
        let pad = format!(r#"{{"type":"x","pad":"{}"}}"#, "a".repeat(READ_CHUNK + 1024));
        let content = [
            user("p1", "2026-10-04T10:00:00Z", "hello"),
            asst("m1", "2026-10-04T10:00:05Z", 100, 1),
            pad,
            asst("m1", "2026-10-04T10:00:06Z", 100, 50),
        ]
        .join("\n")
            + "\n";
        let (dir, _, key, mut l) = one_file("no-seen", content.as_bytes());
        l.ingest(&key);
        l.save().unwrap();
        let (totals, days) = (l.totals.clone(), l.days.clone());
        // Ids forgotten (a legacy ledger imported without `seen.bin`), then a
        // schema migration.
        raw(&dir).execute("DELETE FROM seen", []).unwrap();
        let mut l = Ledger::load(db(&dir));
        assert!(l.seen.is_empty());
        l.files.get_mut(&key).unwrap().schema = 0;
        assert!(l.ingest(&key));
        assert_eq!(l.totals, totals, "already counted ids are not counted again");
        assert_eq!(l.days, days);
        assert!(l.seen.contains(&id_hash(b"m1")) && l.seen.contains(&id_hash(b"p1")));
        assert_eq!(file_view(l.file(&key).unwrap()).0, 1);
        assert_eq!(l.file(&key).unwrap().usage.output, 50);
        l.save().unwrap();
        assert_eq!(row_count(&dir, "seen"), 2, "the ids are stored again");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn healed_id_growth_does_not_recount() {
        // m1 is counted, then its id is forgotten (`seen.bin` lost) and the
        // file re-read: the healed id must not let later
        // usage growth on m1 into the totals, as it may be a copy counted
        // elsewhere. The pad puts the growth line in a separate read.
        let pad = format!(r#"{{"type":"x","pad":"{}"}}"#, "a".repeat(READ_CHUNK + 1024));
        let content = asst("m1", "2026-10-04T10:00:05Z", 100, 1) + "\n" + &pad + "\n";
        let (dir, f, key, mut l) = one_file("healed-growth", content.as_bytes());
        l.ingest(&key);
        let totals = l.totals.clone();
        l.seen.clear();
        l.files.get_mut(&key).unwrap().schema = 0;
        append_bytes(&f, format!("{}\n", asst("m1", "2026-10-04T10:00:06Z", 100, 50)).as_bytes());
        assert!(l.ingest(&key));
        assert!(l.seen.contains(&id_hash(b"m1")), "the id is recorded again");
        assert_eq!(l.file(&key).unwrap().usage.output, 50, "the file's own view grows");
        assert_eq!(l.totals, totals, "a healed id's growth is not counted");
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
