//! `claude-presence tui`: a live dashboard of the daemon. Pull-only: the TUI
//! asks the daemon for a `__state` snapshot about once a second; the daemon
//! never pushes. With the daemon down it shows the stored stats and the
//! config file instead.
//!
//! Everything but `run` is pure (state in `app`, drawing in `ui`, tested on
//! ratatui's `TestBackend`); `run` owns the terminal and the poll thread.

pub mod app;
pub mod format;
pub mod run;
pub mod theme;
pub mod toml_hl;
pub mod ui;

pub use run::run;
