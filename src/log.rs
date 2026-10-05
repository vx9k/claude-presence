//! Minimal leveled logger. Writes to stderr (systemd/launchd/OpenRC/dinit
//! capture it), or to a size-capped file for the Windows background daemon,
//! which has no console to write to. On a terminal, lines get a local clock
//! and colored level tags (unless `NO_COLOR` is set or `TERM=dumb`); piped
//! stderr and the file stay plain.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static FILE: OnceLock<Mutex<File>> = OnceLock::new();
/// `TTY` and `COLOR` bits for stderr output, decided once in `init`.
static STYLE: AtomicU8 = AtomicU8::new(0);
const TTY: u8 = 1;
const COLOR: u8 = 2;

const MAX_LOG_BYTES: u64 = 1 << 20;

/// Read `CLAUDE_PRESENCE_LOG` (error|warn|info|debug) and optionally redirect
/// output to `file`.
pub fn init(file: Option<&Path>) {
    let lvl = match std::env::var("CLAUDE_PRESENCE_LOG").as_deref() {
        Ok("error") => Level::Error,
        Ok("warn") => Level::Warn,
        Ok("debug") | Ok("trace") => Level::Debug,
        _ => Level::Info,
    };
    LEVEL.store(lvl as u8, Ordering::Relaxed);
    if let Some(path) = file {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let big = std::fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false);
        let f = OpenOptions::new().create(true).append(!big).write(true).truncate(big).open(path);
        if let Ok(f) = f {
            let _ = FILE.set(Mutex::new(f));
        }
    }
    let tty = FILE.get().is_none() && std::io::stderr().is_terminal();
    let no_color = std::env::var_os("NO_COLOR");
    let term = std::env::var_os("TERM");
    // Only ask the console for escape sequences when they'd be used.
    let color = use_color(tty, FILE.get().is_some(), no_color.as_deref(), term.as_deref()) && enable_vt();
    STYLE.store(if tty { TTY } else { 0 } | if color { COLOR } else { 0 }, Ordering::Relaxed);
}

#[inline]
pub fn enabled(l: Level) -> bool {
    l as u8 <= LEVEL.load(Ordering::Relaxed)
}

pub fn write(l: Level, args: std::fmt::Arguments<'_>) {
    if let Some(f) = FILE.get() {
        if let Ok(mut f) = f.lock() {
            let secs = crate::timeutil::now_ms() / 1000;
            let _ = writeln!(f, "{secs} {}: {args}", tag(l));
        }
    } else {
        let style = STYLE.load(Ordering::Relaxed);
        let clock = (style & TTY != 0)
            .then(|| (crate::timeutil::now_ms() / 1000).saturating_add(crate::timeutil::local_offset_secs()));
        let _ = line(&mut std::io::stderr().lock(), l, style & COLOR != 0, clock, args);
    }
}

fn tag(l: Level) -> &'static str {
    match l {
        Level::Error => "error",
        Level::Warn => "warn",
        Level::Info => "info",
        Level::Debug => "debug",
    }
}

/// Whether stderr lines get ANSI colors: only on a terminal (`tty`), never
/// into a log file, and not when `NO_COLOR` is set to anything non-empty
/// (no-color.org) or `TERM` is `dumb`.
fn use_color(tty: bool, to_file: bool, no_color: Option<&OsStr>, term: Option<&OsStr>) -> bool {
    tty && !to_file && no_color.is_none_or(OsStr::is_empty) && term != Some(OsStr::new("dumb"))
}

/// One stderr log line: `<tag>: <args>`. `clock` (local seconds since the
/// epoch, given only on a terminal) adds an `HH:MM:SS` prefix; `color` adds
/// SGR escapes: error bold red, warn yellow, info green, debug dim.
fn line(
    w: &mut impl Write,
    l: Level,
    color: bool,
    clock: Option<i64>,
    args: std::fmt::Arguments<'_>,
) -> std::io::Result<()> {
    if let Some(t) = clock {
        let t = t.rem_euclid(crate::timeutil::DAY_SECS);
        let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
        if color {
            write!(w, "\x1b[2m{h:02}:{m:02}:{s:02}\x1b[0m ")?;
        } else {
            write!(w, "{h:02}:{m:02}:{s:02} ")?;
        }
    }
    let sgr = match l {
        Level::Error => "1;31",
        Level::Warn => "33",
        Level::Info => "32",
        Level::Debug => "2",
    };
    match (color, l) {
        (false, _) => writeln!(w, "{}: {args}", tag(l)),
        (true, Level::Debug) => writeln!(w, "\x1b[{sgr}m{}: {args}\x1b[0m", tag(l)),
        (true, _) => writeln!(w, "\x1b[{sgr}m{}\x1b[0m: {args}", tag(l)),
    }
}

/// Turn on escape-sequence processing for a console stderr; false if that
/// fails (an old console), so lines stay plain. A terminal that is not a
/// console is an MSYS/Cygwin pty (`is_terminal` recognizes those by name),
/// which handles escapes itself.
#[cfg(windows)]
fn enable_vt() -> bool {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, SetConsoleMode,
    };
    // SAFETY: GetStdHandle has no preconditions; the console calls take that
    // handle (failing harmlessly if it is not a console) and a valid
    // out-pointer.
    unsafe {
        let h = GetStdHandle(STD_ERROR_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(h, &mut mode) == 0 {
            return true;
        }
        mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
            || SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_vt() -> bool {
    true
}

#[macro_export]
macro_rules! error { ($($t:tt)*) => { if $crate::log::enabled($crate::log::Level::Error) { $crate::log::write($crate::log::Level::Error, format_args!($($t)*)) } } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { if $crate::log::enabled($crate::log::Level::Warn) { $crate::log::write($crate::log::Level::Warn, format_args!($($t)*)) } } }
#[macro_export]
macro_rules! info { ($($t:tt)*) => { if $crate::log::enabled($crate::log::Level::Info) { $crate::log::write($crate::log::Level::Info, format_args!($($t)*)) } } }
#[macro_export]
macro_rules! debug { ($($t:tt)*) => { if $crate::log::enabled($crate::log::Level::Debug) { $crate::log::write($crate::log::Level::Debug, format_args!($($t)*)) } } }

#[cfg(test)]
mod tests {
    use super::*;

    fn render(l: Level, color: bool, clock: Option<i64>, msg: &str) -> String {
        let mut out = Vec::new();
        line(&mut out, l, color, clock, format_args!("{msg}")).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn plain_lines_are_unchanged() {
        // Piped stderr (journald, launchd, a file): exactly the old format.
        assert_eq!(render(Level::Error, false, None, "boom"), "error: boom\n");
        assert_eq!(render(Level::Warn, false, None, "w"), "warn: w\n");
        assert_eq!(render(Level::Info, false, None, "i"), "info: i\n");
        assert_eq!(render(Level::Debug, false, None, "d"), "debug: d\n");
    }

    #[test]
    fn colored_lines() {
        assert_eq!(render(Level::Error, true, None, "boom"), "\x1b[1;31merror\x1b[0m: boom\n");
        assert_eq!(render(Level::Warn, true, None, "w"), "\x1b[33mwarn\x1b[0m: w\n");
        assert_eq!(render(Level::Info, true, None, "i"), "\x1b[32minfo\x1b[0m: i\n");
        // Debug lines are noise: dimmed whole.
        assert_eq!(render(Level::Debug, true, None, "d"), "\x1b[2mdebug: d\x1b[0m\n");
    }

    #[test]
    fn terminal_lines_get_a_clock() {
        let t = 13 * 3600 + 4 * 60 + 5;
        assert_eq!(render(Level::Info, true, Some(t), "i"), "\x1b[2m13:04:05\x1b[0m \x1b[32minfo\x1b[0m: i\n");
        // NO_COLOR on a terminal: the clock stays, without escapes.
        assert_eq!(render(Level::Warn, false, Some(t), "w"), "13:04:05 warn: w\n");
        // Wrapped into the day; negative values (clock skew) too.
        assert_eq!(render(Level::Info, false, Some(86_400 + 59), "i"), "00:00:59 info: i\n");
        assert_eq!(render(Level::Info, false, Some(-1), "i"), "23:59:59 info: i\n");
    }

    #[test]
    fn color_only_on_a_terminal_without_opt_outs() {
        let t = Some(OsStr::new("xterm-256color"));
        assert!(use_color(true, false, None, t));
        assert!(use_color(true, false, None, None), "no TERM (Windows console)");
        assert!(!use_color(false, false, None, t), "piped");
        assert!(!use_color(true, true, None, t), "log file");
        assert!(!use_color(true, false, Some(OsStr::new("1")), t), "NO_COLOR");
        assert!(use_color(true, false, Some(OsStr::new("")), t), "empty NO_COLOR is unset");
        assert!(!use_color(true, false, None, Some(OsStr::new("dumb"))));
    }
}
