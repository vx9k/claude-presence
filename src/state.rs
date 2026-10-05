//! The daemon's `__state` reply: a snapshot of what it is doing, for
//! `claude-presence tui`. Built only when asked; pull-only. One JSON line of
//! at most `ipc::MAX_STATE` bytes. Every field defaults, so readers and
//! daemons of other versions still understand each other (bump
//! `STATE_VERSION` only for changes an old reader would misread).
//!
//! Private by construction: project directory names (masked for hidden
//! projects), never a full path, prompt text, tool input or transcript path.

use crate::ledger::Stats;
use serde::{Deserialize, Serialize};

/// `StateSnapshot::v` of this layout.
pub const STATE_VERSION: u32 = 1;

/// Most sessions a snapshot lists (the most recently active ones).
pub const MAX_SESSIONS: usize = 32;

/// The reply while the daemon's loop is busy (its startup scan, say).
pub const BUSY_REPLY: &str = "{\"v\":1,\"busy\":true}\n";

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct StateSnapshot {
    pub v: u32,
    /// The daemon didn't get to it in time; nothing else is filled in.
    pub busy: bool,
    /// The daemon's `CARGO_PKG_VERSION`.
    pub version: String,
    pub pid: u32,
    /// The daemon's clock when it took the snapshot (epoch ms).
    pub now_ms: i64,
    /// `connected`, `bridge` (an arRPC-style bridge), `disconnected` or
    /// `refused` (Discord rejected the handshake: check `client_id`).
    pub discord: String,
    /// What the daemon last asked Discord to show; `None` if cleared.
    pub card: Option<Card>,
    /// Most recently active first, at most `MAX_SESSIONS`.
    pub sessions: Vec<SessionInfo>,
    /// Sessions the daemon tracks, including any left out of `sessions`.
    pub sessions_total: u32,
    /// Lifetime stats; `None` while the daemon couldn't load them (a busy
    /// database).
    pub stats: Option<Stats>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Card {
    pub details: String,
    pub state: String,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SessionInfo {
    /// The first 8 characters of Claude Code's session id.
    pub id: String,
    /// Project directory name, or `hidden_project_name` for hidden projects.
    pub project: String,
    /// Git branch; empty if none or hidden.
    pub branch: String,
    /// `idle`, `thinking`, `working`, `compacting` or `notification`.
    pub status: String,
    /// Display name, e.g. `Opus 5.5`.
    pub model: String,
    /// The running tool's display name, if any (never its input).
    pub tool: String,
    pub started_ms: i64,
    pub last_event_ms: i64,
    pub prompts: u32,
    pub tools: u32,
    pub tokens: u64,
    /// The session the Discord card shows.
    pub shown: bool,
}

impl StateSnapshot {
    /// The reply line: JSON plus a newline, within `ipc::MAX_STATE`. If too
    /// big, the least recently active sessions go first, then the days, then
    /// the card. (Strings are clamped when the snapshot is built, so this is
    /// only a backstop.)
    pub fn encode(&mut self) -> String {
        loop {
            // Plain structs of strings and numbers: serializing can't fail.
            let Ok(mut line) = sonic_rs::to_string(self) else { return BUSY_REPLY.to_owned() };
            line.push('\n');
            if line.len() <= crate::ipc::MAX_STATE {
                return line;
            }
            if self.sessions.pop().is_some() {
                continue;
            }
            if let Some(st) = self.stats.as_mut().filter(|st| !st.days.is_empty()) {
                st.days.clear();
                continue;
            }
            if self.card.take().is_none() {
                // Nothing big is left; unreachable in practice.
                return BUSY_REPLY.to_owned();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::MAX_STATE;
    use crate::ledger::DayStats;

    fn session(i: usize, name_len: usize) -> SessionInfo {
        SessionInfo {
            id: format!("{i:08}"),
            project: "p".repeat(name_len),
            branch: "b".repeat(name_len),
            status: "working".into(),
            model: "Opus 5.5".into(),
            last_event_ms: 1000 - i as i64,
            ..SessionInfo::default()
        }
    }

    #[test]
    fn small_snapshot_round_trips_as_one_line() {
        let mut s = StateSnapshot {
            v: STATE_VERSION,
            version: "0.2.0".into(),
            pid: 42,
            discord: "connected".into(),
            card: Some(Card { details: "Working in demo".into(), state: "Edit · 3 tokens".into() }),
            sessions: vec![session(0, 4)],
            sessions_total: 1,
            stats: Some(Stats {
                prompts: 3,
                days: vec![DayStats { day: 20_000, tokens: 9, ..DayStats::default() }],
                ..Stats::default()
            }),
            ..StateSnapshot::default()
        };
        let line = s.encode();
        assert!(line.ends_with('\n'));
        assert_eq!(memchr::memchr(b'\n', line.as_bytes()), Some(line.len() - 1), "one line");
        let back: StateSnapshot = sonic_rs::from_str(&line).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn oversized_snapshot_drops_old_sessions_first() {
        // 32 sessions with long names: well past 64 KiB.
        let mut s = StateSnapshot {
            v: STATE_VERSION,
            sessions: (0..MAX_SESSIONS).map(|i| session(i, 2000)).collect(),
            sessions_total: 40,
            stats: Some(Stats { days: vec![DayStats::default(); 60], ..Stats::default() }),
            ..StateSnapshot::default()
        };
        let line = s.encode();
        assert!(line.len() <= MAX_STATE, "{} bytes", line.len());
        let back: StateSnapshot = sonic_rs::from_str(&line).unwrap();
        assert!(!back.sessions.is_empty() && back.sessions.len() < MAX_SESSIONS);
        // The most recent ones are kept, in order; the days and the count too.
        assert!(back.sessions.iter().enumerate().all(|(i, x)| x.id == format!("{i:08}")));
        assert_eq!(back.sessions_total, 40);
        assert_eq!(back.stats.unwrap().days.len(), 60);
    }

    #[test]
    fn days_go_when_sessions_are_not_enough() {
        let big = DayStats { day: i32::MIN, tokens: u64::MAX, ..DayStats::default() };
        let mut s = StateSnapshot {
            card: Some(Card { details: "d".repeat(MAX_STATE), state: String::new() }),
            stats: Some(Stats { days: vec![big; 60], ..Stats::default() }),
            ..StateSnapshot::default()
        };
        let line = s.encode();
        assert!(line.len() <= MAX_STATE, "{} bytes", line.len());
        let back: StateSnapshot = sonic_rs::from_str(&line).unwrap();
        assert!(back.stats.is_some(), "the totals stay");
    }

    #[test]
    fn busy_and_other_versions_parse() {
        let busy: StateSnapshot = sonic_rs::from_str(BUSY_REPLY).unwrap();
        assert!(busy.busy);
        assert_eq!(busy.v, 1);
        // A newer daemon's extra fields are ignored; missing ones default.
        let s: StateSnapshot =
            sonic_rs::from_str(r#"{"v":1,"pid":3,"future":{"x":[1]},"sessions":[{"id":"a","new":1}]}"#).unwrap();
        assert_eq!((s.pid, s.sessions.len(), s.sessions[0].id.as_str()), (3, 1, "a"));
        assert_eq!(s.stats, None);
    }
}
