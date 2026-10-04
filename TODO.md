# TODO / handoff

State of the project after the hardening PRs #3 (core fixes), #4 (IPC and
lifecycle) and #5 (docs).
Work items are ordered by priority. Delete an entry once its fix is merged.

## Status

- CI (fmt, clippy, test, release build on Linux/macOS/Windows) is green.
  Unit tests: 81 on Linux, 77 on Windows (socket and POSIX permission tests
  are Unix-only; named pipe tests Windows-only).
- **Verified on Windows by hand:** `install` (Task Scheduler), `status`, the
  hook named pipe, connecting to Discord and setting an activity (via a
  hand-fed `UserPromptSubmit` hook).
- **Verified on Windows by tests** (CI, `windows-latest`, elevated): the pipe
  DACL read back, an elevated daemon accepting a non-elevated (restricted
  token) hook and `OW` rejecting it (that negative control runs only when
  the runner's default owner is Administrators; the test logs a skip),
  the hook-side pipe-owner check,
  `ERROR_NO_DATA`, bind retry vs `FILE_FLAG_FIRST_PIPE_INSTANCE`, the
  `__shutdown` round trip, cancelling a Discord worker blocked in pipe I/O.
  Still by hand only: the locked-`.exe` copy during a real reinstall
  (`replace_binary`; its retry/rename logic is unit-tested).
- Under Wine (see docs/development.md) all tests pass except two Wine
  quirks: `counts_incrementally_and_dedups` (Wine reports ctime as the
  creation time, so `file_ident` changes on append) and
  `our_pipe_passes_the_owner_check` (Wine's default owner is its primary
  group `S-1-5-21-0-0-0-513`).
- **Not yet verified anywhere real:** a full local Claude Code session
  driving the card end-to-end on Windows/macOS; OpenRC and dinit services;
  launchd; the Run-key fallback.
- Only **local** Claude Code sessions can be shown (CLI, or the desktop app's
  Code tab using the local machine). Cloud sessions run hooks in a remote
  container; nothing reaches the daemon. Decided: no cloud/desktop-app
  detection fallback (it would need polling and can't be accurate).

## Open audit findings

Line numbers are approximate.

### Low / unverified

12. **`PostToolUseFailure` is handled but never wired** (`src/daemon.rs`
    vs `install::HOOK_EVENTS`). Either add it to `HOOK_EVENTS` (check the
    event exists in current Claude Code first) or drop the handler.

## Nice to have

- Optionally add the Windows install dir to the user `PATH` on install.
