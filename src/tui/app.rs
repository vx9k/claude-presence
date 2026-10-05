//! The dashboard's state and key handling; no terminal, no I/O.

use crate::ledger::Stats;
use crate::state::StateSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Sessions,
    Stats,
    Config,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Overview, Tab::Sessions, Tab::Stats, Tab::Config];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Sessions => "Sessions",
            Tab::Stats => "Stats",
            Tab::Config => "Config",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Where the daemon stands, as the last poll saw it.
#[derive(Clone, Debug, PartialEq)]
pub enum Daemon {
    /// No answer yet.
    Unknown,
    /// Answered with a snapshot.
    Live,
    /// Answered `busy`; the last snapshot (if any) is stale.
    Busy,
    /// Listening but closed without a reply: a version before `__state`.
    Old,
    /// Not reachable; the reason.
    Down(String),
}

/// What the poll thread reports.
#[derive(Debug)]
pub enum Update {
    Live(Box<StateSnapshot>),
    Old(Result<Option<Stats>, String>),
    /// The daemon is unreachable: why, and the stats read from disk.
    Down(String, Result<Option<Stats>, String>),
    /// `config.toml`'s text (`None`: no file) and the result of a reload.
    Config(Option<String>),
    /// A one-line message for the footer.
    Note(String),
}

/// What a key asks the outside world to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    None,
    Quit,
    /// Poll the daemon now.
    Refresh,
    /// Re-read `config.toml` and ask the daemon to reload it.
    Reload,
}

/// A key, already decoded from the terminal's events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Tab,
    BackTab,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Esc,
    CtrlC,
}

pub struct App {
    pub tab: Tab,
    pub daemon: Daemon,
    /// The last snapshot the daemon sent.
    pub snapshot: Option<StateSnapshot>,
    /// Stats read from disk while the daemon is down.
    pub offline_stats: Result<Option<Stats>, String>,
    pub config: Option<String>,
    /// Parse errors and clamp notes for `config`.
    pub config_notes: Vec<String>,
    /// Scroll offset of the Sessions and Config tabs.
    pub scroll: u16,
    pub help: bool,
    pub paused: bool,
    pub note: Option<String>,
}

impl Default for App {
    fn default() -> App {
        App {
            tab: Tab::Overview,
            daemon: Daemon::Unknown,
            snapshot: None,
            offline_stats: Ok(None),
            config: None,
            config_notes: Vec::new(),
            scroll: 0,
            help: false,
            paused: false,
            note: None,
        }
    }
}

impl App {
    /// Lifetime stats from the daemon, or from disk while it is down.
    pub fn stats(&self) -> Option<&Stats> {
        match &self.daemon {
            Daemon::Down(_) | Daemon::Old | Daemon::Unknown => self.offline_stats.as_ref().ok()?.as_ref(),
            Daemon::Live | Daemon::Busy => self.snapshot.as_ref()?.stats.as_ref(),
        }
    }

    pub fn apply(&mut self, u: Update) {
        match u {
            Update::Live(s) if s.busy => self.daemon = Daemon::Busy,
            Update::Live(s) => {
                self.daemon = Daemon::Live;
                self.snapshot = Some(*s);
            }
            Update::Old(stats) => {
                self.daemon = Daemon::Old;
                self.snapshot = None;
                self.offline_stats = stats;
            }
            Update::Down(why, stats) => {
                self.daemon = Daemon::Down(why);
                self.snapshot = None;
                self.offline_stats = stats;
            }
            Update::Config(text) => {
                self.config_notes = match text.as_deref().map(crate::config::Config::parse) {
                    Some(Ok((_, notes))) => notes,
                    Some(Err(e)) => vec![format!("invalid config: {}", e.message())],
                    None => vec!["no config file; defaults in effect".into()],
                };
                self.config = text;
            }
            Update::Note(n) => self.note = Some(n),
        }
    }

    pub fn key(&mut self, k: Key) -> Command {
        if self.help {
            self.help = false;
            return if matches!(k, Key::CtrlC | Key::Char('q')) { Command::Quit } else { Command::None };
        }
        self.note = None;
        match k {
            Key::CtrlC | Key::Char('q') => return Command::Quit,
            Key::Esc => {}
            Key::Char('?') => self.help = true,
            Key::Tab | Key::Char('l') | Key::Right => self.switch(self.tab.index() + 1),
            Key::BackTab | Key::Char('h') | Key::Left => self.switch(self.tab.index() + Tab::ALL.len() - 1),
            Key::Char(c @ '1'..='4') => self.switch(c as usize - '1' as usize),
            Key::Char('j') | Key::Down => self.scroll = self.scroll.saturating_add(1),
            Key::Char('k') | Key::Up => self.scroll = self.scroll.saturating_sub(1),
            Key::PageDown => self.scroll = self.scroll.saturating_add(10),
            Key::PageUp => self.scroll = self.scroll.saturating_sub(10),
            Key::Home | Key::Char('g') => self.scroll = 0,
            Key::End | Key::Char('G') => self.scroll = u16::MAX,
            Key::Char('p') => {
                self.paused = !self.paused;
                return if self.paused { Command::None } else { Command::Refresh };
            }
            Key::Char('r') if self.tab == Tab::Config => return Command::Reload,
            Key::Char('r') => return Command::Refresh,
            Key::Char(_) => {}
        }
        Command::None
    }

    fn switch(&mut self, i: usize) {
        let t = Tab::ALL[i % Tab::ALL.len()];
        if t != self.tab {
            self.tab = t;
            self.scroll = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_cycle_and_reset_scroll() {
        let mut a = App { scroll: 5, ..App::default() };
        a.key(Key::Tab);
        assert_eq!((a.tab, a.scroll), (Tab::Sessions, 0));
        a.key(Key::BackTab);
        a.key(Key::BackTab);
        assert_eq!(a.tab, Tab::Config);
        a.key(Key::Char('3'));
        assert_eq!(a.tab, Tab::Stats);
        a.key(Key::Char('9'));
        assert_eq!(a.tab, Tab::Stats);
    }

    #[test]
    fn commands() {
        let mut a = App::default();
        assert_eq!(a.key(Key::Char('r')), Command::Refresh);
        a.key(Key::Char('4'));
        assert_eq!(a.key(Key::Char('r')), Command::Reload);
        assert_eq!(a.key(Key::Char('p')), Command::None);
        assert!(a.paused);
        assert_eq!(a.key(Key::Char('p')), Command::Refresh);
        assert_eq!(a.key(Key::CtrlC), Command::Quit);
        assert_eq!(a.key(Key::Char('q')), Command::Quit);
    }

    #[test]
    fn help_swallows_one_key() {
        let mut a = App::default();
        a.key(Key::Char('?'));
        assert!(a.help);
        assert_eq!(a.key(Key::Tab), Command::None);
        assert_eq!((a.help, a.tab), (false, Tab::Overview));
        a.key(Key::Char('?'));
        assert_eq!(a.key(Key::Char('q')), Command::Quit);
    }

    #[test]
    fn updates() {
        let mut a = App::default();
        let snap =
            StateSnapshot { pid: 7, stats: Some(Stats { prompts: 3, ..Stats::default() }), ..Default::default() };
        a.apply(Update::Live(Box::new(snap)));
        assert_eq!((a.daemon.clone(), a.stats().unwrap().prompts), (Daemon::Live, 3));
        // Busy keeps the last snapshot.
        a.apply(Update::Live(Box::new(StateSnapshot { busy: true, ..Default::default() })));
        assert_eq!((a.daemon.clone(), a.snapshot.as_ref().unwrap().pid), (Daemon::Busy, 7));
        // Down switches to the stats on disk.
        let disk = Stats { prompts: 9, ..Stats::default() };
        a.apply(Update::Down("refused".into(), Ok(Some(disk))));
        assert!(a.snapshot.is_none());
        assert_eq!(a.stats().unwrap().prompts, 9);
    }

    #[test]
    fn config_notes() {
        let mut a = App::default();
        a.apply(Update::Config(None));
        assert_eq!(a.config_notes, ["no config file; defaults in effect"]);
        a.apply(Update::Config(Some("idle_timeout = 1\n".into())));
        assert_eq!(a.config_notes, ["idle_timeout = 1 is below the minimum; using 60"]);
        a.apply(Update::Config(Some("nope".into())));
        assert!(a.config_notes[0].starts_with("invalid config"));
    }
}
