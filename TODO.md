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
10. **Config clamp ranges in README** (`README.md`). `idle_timeout`
    (0, or 60..604800) and `rotation_interval` (5..86400) are now clamped at
    load (`Config::sanitized`) and documented in `DEFAULT_TOML`; the README
    still needs the ranges (docs-writer).

## Nice to have

- Optionally add the Windows install dir to the user `PATH` on install.
- PR #1 description footer: replace "Generated with Claude Code" with
  `🤖 Written by the <sub-agent> sub-agent` (see CLAUDE.md).
