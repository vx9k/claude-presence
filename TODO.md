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

Line numbers are approximate.

### Medium

9. **Windows pipe name can be pre-created by another user** (`src/ipc.rs`).
   Our pipe now has an owner-only DACL, but a squatter who creates
   `\\.\pipe\claude-presence-<user>` first still receives hooks; the daemon
   then logs "already running". Fix: client-side
   `GetNamedPipeServerProcessId` + compare the server's token user SID.

### Low / unverified

10. **Windows changes are type-checked only**: the `ERROR_NO_DATA` handling
    and error-path read in `Listener::serve`, the user-SID DACL
    (`D:P(A;;GA;;;<user SID>)` from `TokenUser`; check an elevated daemon
    accepts non-elevated hooks), the bind retry while the old daemon exits,
    and the `__shutdown` reinstall path (copying over the `.exe` once the old
    daemon exits). Verify by hand.
11. **A wedged Discord worker is abandoned, not killed**
    (`src/discord.rs` `Presenter::shutdown`). After 1 s it is left running;
    fine at exit, but a `client_id` reload leaves the old thread (and its
    pipe) behind until its I/O returns.

## Nice to have

- Optionally add the Windows install dir to the user `PATH` on install.
- PR #1 description footer: replace "Generated with Claude Code" with
  `🤖 Written by the <sub-agent> sub-agent` (see CLAUDE.md).
