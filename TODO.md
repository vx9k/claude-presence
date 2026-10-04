# TODO / handoff

State of the project as of PR #1 (`claude/rust-claude-presence-p1vb5y`).
Work items are ordered by priority. Delete an entry once its fix is merged.

## Status

- CI (fmt, clippy, test, release build on Linux/macOS/Windows) is green.
- **Verified on Windows by hand:** `install` (Task Scheduler), `status`, the
  hook named pipe, connecting to Discord and setting an activity (via a
  hand-fed `UserPromptSubmit` hook).
- **Not yet verified anywhere real:** a full local Claude Code session
  driving the card end-to-end on Windows/macOS; OpenRC and dinit services;
  launchd; the Run-key fallback.
- Only **local** Claude Code sessions can be shown (CLI, or the desktop app's
  Code tab using the local machine). Cloud sessions run hooks in a remote
  container; nothing reaches the daemon. Decided: no cloud/desktop-app
  detection fallback (it would need polling and can't be accurate).

## Open audit findings

From the auditor sub-agent's review of the initial implementation. Line
numbers are approximate.

### Medium

1. **Rotation spin, 20 Hz wakeups** (`src/daemon.rs` `next_rotation` /
   `render` / `tick`). `render` only advances `rotation.since` when more than
   one rotation frame is eligible; with ≤ 1 eligible frame (e.g. a new user
   with no stats, idle card) `next_rotation` stays in the past and `tick`
   returns the 50 ms floor forever. Reproduced. Fix: advance
   `rotation.since = now` when `frames.len() <= 1`, or return `None`.
   Add a unit test.
2. **Expiry spin, 20 Hz wakeups** (`daemon.rs` `expire` / `next_expiry`).
   A recent transcript mtime keeps a session alive but `last_activity` is
   not updated, so `next_expiry` is in the past. Reproduced. Fix:
   `s.last_activity = max(s.last_activity, mtime)`.
3. **Windows hook pipe drops messages** (`src/ipc.rs` Windows `serve`).
   `ConnectNamedPipe` returning `ERROR_NO_DATA` (client wrote and closed
   already) is treated as failure and the buffered data is discarded; the
   error path also closes `cur` without reading it. Fix: treat
   `ERROR_NO_DATA` like `ERROR_PIPE_CONNECTED` and read until
   `ERROR_BROKEN_PIPE`.
4. **Socket/pipe squatting** (`src/paths.rs`, `src/ipc.rs`). Without
   `XDG_RUNTIME_DIR` / `/run/user/<uid>` the socket lands in `/tmp` (or a
   shared `TMPDIR`), where another local user can pre-create it and receive
   every hook payload. Fix: per-user `0700` dir (e.g.
   `/tmp/claude-presence-<uid>/`, verify owner/mode/not-symlink with
   `lstat`), plus a client-side peer check (`SO_PEERCRED` on Linux,
   `getpeereid` on macOS). Windows: pipe can be pre-created by another user;
   create it with an owner-only DACL and/or verify the server's SID
   (`GetNamedPipeServerProcessId`). Consult the advisor first (IPC change).

### Low / low-medium

5. **Ledger save order** (`src/ledger.rs` `save`). `ledger.json` is renamed
   before `seen.bin` is appended, so a crash in between can double-count a
   message copied into a resumed transcript. Fix: append + `sync_all`
   `seen.bin` first, then the ledger, **and update the ledger invariant in
   AGENTS.md accordingly**. Also: a torn `seen.bin` append leaves the length
   not a multiple of 8 and misaligns every later id — truncate to a multiple
   of 8 on load and before appending.
6. **Windows Discord pipe has no timeout** (`src/discord.rs` Windows
   `open`). A frozen Discord blocks the worker forever, and
   `Presenter::shutdown`'s `join` hangs exit. Fix: overlapped I/O with a
   timeout, or `CancelSynchronousIo` from a watchdog; don't join forever.
7. **Reinstall doesn't stop the running daemon** for XDG autostart and the
   Windows Run key / spawn fallback (`src/install.rs` `uninstall_service`,
   `src/main.rs`). Linux: the new daemon exits with "already running", the
   old version keeps running. Windows: copying over the locked `.exe` fails.
   Fix: a shutdown request over the hook IPC (e.g. event name `__shutdown`
   → `Msg::Shutdown`), sent by `uninstall_service`, then wait ≤ 2 s for
   `daemon_running` to turn false. Touches the wire format → advisor first.
8. **Rejected activity resent forever** (`discord.rs` ~400): set
   `on_wire = None` after Discord rejects an activity.
9. **Unix accept error busy loop** (`ipc.rs` Unix `serve`): sleep ~100 ms on
   `incoming()` errors (EMFILE/ENFILE).
10. **Config overflow** (`daemon.rs` uses of `idle_timeout`,
    `rotation_interval`): clamp at config load so `as i64 * 1000` can't
    overflow. Document the ranges in README and `DEFAULT_TOML`.

## Nice to have

- Log the first received hook at `info` so a silent daemon is easier to
  diagnose (users saw only "listening…" and assumed it was broken).
- Optionally add the Windows install dir to the user `PATH` on install.
- PR #1 description footer: replace "Generated with Claude Code" with
  `🤖 Written by the <sub-agent> sub-agent` (see CLAUDE.md).
