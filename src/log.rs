//! Minimal leveled logger. Writes to stderr (systemd/launchd/OpenRC/dinit
//! capture it), or to a size-capped file for the Windows background daemon,
//! which has no console to write to.

use std::fs::{File, OpenOptions};
use std::io::Write;
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
}

#[inline]
pub fn enabled(l: Level) -> bool {
    l as u8 <= LEVEL.load(Ordering::Relaxed)
}

pub fn write(l: Level, args: std::fmt::Arguments<'_>) {
    let tag = match l {
        Level::Error => "error",
        Level::Warn => "warn",
        Level::Info => "info",
        Level::Debug => "debug",
    };
    if let Some(f) = FILE.get() {
        if let Ok(mut f) = f.lock() {
            let secs = crate::timeutil::now_ms() / 1000;
            let _ = writeln!(f, "{secs} {tag}: {args}");
        }
    } else {
        let _ = writeln!(std::io::stderr().lock(), "{tag}: {args}");
    }
}

#[macro_export]
macro_rules! error { ($($t:tt)*) => { if $crate::log::enabled($crate::log::Level::Error) { $crate::log::write($crate::log::Level::Error, format_args!($($t)*)) } } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { if $crate::log::enabled($crate::log::Level::Warn) { $crate::log::write($crate::log::Level::Warn, format_args!($($t)*)) } } }
#[macro_export]
macro_rules! info { ($($t:tt)*) => { if $crate::log::enabled($crate::log::Level::Info) { $crate::log::write($crate::log::Level::Info, format_args!($($t)*)) } } }
#[macro_export]
macro_rules! debug { ($($t:tt)*) => { if $crate::log::enabled($crate::log::Level::Debug) { $crate::log::write($crate::log::Level::Debug, format_args!($($t)*)) } } }
