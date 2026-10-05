//! The terminal side: raw mode and the alternate screen (restored on exit
//! and, via ratatui's panic hook, on panic), key events, and a poll thread
//! that asks the daemon for a snapshot every `POLL` so a slow daemon never
//! freezes the keys.

use super::app::{App, Command, Key, Update};
use super::theme::Palette;
use super::ui;
use crate::state::StateSnapshot;
use crate::{ipc, ledger, paths, timeutil};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// How often the daemon is asked for a snapshot.
const POLL: Duration = Duration::from_secs(1);
/// How long one snapshot request may take.
const QUERY_TIMEOUT: Duration = Duration::from_secs(1);
/// How long the UI waits for a key before checking for new snapshots.
const TICK: Duration = Duration::from_millis(100);

enum Cmd {
    Refresh,
    Reload,
}

pub fn run() -> io::Result<()> {
    let pal = Palette::detect(|k| std::env::var(k).ok());
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (up_tx, up_rx) = mpsc::channel();
    let paused = Arc::new(AtomicBool::new(false));
    let p = paused.clone();
    // Detached: it ends with the process (or when the UI drops `cmd_tx`).
    std::thread::Builder::new().name("poll".into()).spawn(move || poller(&cmd_rx, &up_tx, &p))?;

    let mut term = ratatui::try_init()?;
    let res = ui_loop(&mut term, &pal, &cmd_tx, &up_rx, &paused);
    ratatui::try_restore()?;
    res
}

fn ui_loop(
    term: &mut ratatui::DefaultTerminal,
    pal: &Palette,
    cmd: &Sender<Cmd>,
    updates: &Receiver<Update>,
    paused: &AtomicBool,
) -> io::Result<()> {
    let mut app = App::default();
    let mut dirty = true;
    loop {
        while let Ok(u) = updates.try_recv() {
            app.apply(u);
            dirty = true;
        }
        if dirty {
            term.draw(|f| ui::draw(f, &app, pal))?;
            dirty = false;
        }
        if !event::poll(TICK)? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => decode(k.code, k.modifiers),
            Event::Resize(..) => {
                dirty = true;
                None
            }
            _ => None,
        };
        let Some(key) = key else { continue };
        dirty = true;
        let c = app.key(key);
        paused.store(app.paused, Ordering::Relaxed);
        match c {
            Command::Quit => return Ok(()),
            Command::Refresh => drop(cmd.send(Cmd::Refresh)),
            Command::Reload => drop(cmd.send(Cmd::Reload)),
            Command::None => {}
        }
    }
}

fn decode(code: KeyCode, m: KeyModifiers) -> Option<Key> {
    Some(match code {
        KeyCode::Char('c') if m.contains(KeyModifiers::CONTROL) => Key::CtrlC,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Esc => Key::Esc,
        _ => return None,
    })
}

/// The poll thread: a snapshot every `POLL` (unless paused), or at once on
/// `Refresh`; `Reload` re-reads the config and asks the daemon to reload.
/// Ends when the UI is gone.
fn poller(cmds: &Receiver<Cmd>, out: &Sender<Update>, paused: &AtomicBool) {
    let _ = out.send(Update::Config(read_config()));
    let send = |u| out.send(u).is_ok();
    loop {
        if !paused.load(Ordering::Relaxed) && !send(poll()) {
            return;
        }
        match cmds.recv_timeout(POLL) {
            Ok(Cmd::Refresh) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Cmd::Reload) => {
                let (addr, private) = paths::hook_endpoint();
                let note = match ipc::send_reload(&addr, private) {
                    Ok(()) => "asked the daemon to reload config.toml",
                    Err(_) => "daemon not running; config.toml applies when it starts",
                };
                if !send(Update::Config(read_config())) || !send(Update::Note(note.into())) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn poll() -> Update {
    let (addr, private) = paths::hook_endpoint();
    match ipc::query_state(&addr, private, QUERY_TIMEOUT) {
        Ok(Some(line)) => match sonic_rs::from_str::<StateSnapshot>(&line) {
            Ok(s) => Update::Live(Box::new(s)),
            Err(e) => Update::Down(format!("unreadable reply: {e}"), disk_stats()),
        },
        Ok(None) => Update::Old(disk_stats()),
        Err(e) => Update::Down(e.to_string(), disk_stats()),
    }
}

fn disk_stats() -> Result<Option<ledger::Stats>, String> {
    ledger::read_only_stats(&paths::ledger_file(), timeutil::now_ms(), timeutil::local_offset_secs())
        .map_err(|e| e.to_string())
}

fn read_config() -> Option<String> {
    match std::fs::read_to_string(paths::config_file()) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("# could not read config.toml: {e}\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_decode() {
        assert_eq!(decode(KeyCode::Char('c'), KeyModifiers::CONTROL), Some(Key::CtrlC));
        assert_eq!(decode(KeyCode::Char('c'), KeyModifiers::NONE), Some(Key::Char('c')));
        assert_eq!(decode(KeyCode::BackTab, KeyModifiers::SHIFT), Some(Key::BackTab));
        assert_eq!(decode(KeyCode::F(1), KeyModifiers::NONE), None);
    }
}
