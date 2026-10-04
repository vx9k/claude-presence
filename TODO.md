# TODO / handoff

State of the project after the hardening PRs #3 (core fixes), #4 (IPC and
lifecycle) and #5 (docs).
Work items are ordered by priority. Delete an entry once its fix is merged.

## Status

- CI (fmt, clippy, test, release build on Linux/macOS/Windows) is green.
  Unit tests: 127 on Linux, 126 on Windows (socket and POSIX permission tests
  are Unix-only; named pipe tests Windows-only).
- **Verified on Windows by hand:** `install` (Task Scheduler), `status`, the
  hook named pipe, connecting to Discord and setting an activity (via a
  hand-fed `UserPromptSubmit` hook).
- **Verified on Windows by tests** (CI, `windows-latest`, elevated): the pipe
  DACL read back, an elevated daemon accepting a non-elevated (restricted
  token) hook and `OW` rejecting it (that negative control needs an
  elevated process whose default owner is Administrators; when `CI` or
  `GITHUB_ACTIONS` is set the test fails instead of skipping),
  the hook-side pipe-owner check,
  `ERROR_NO_DATA`, bind retry vs `FILE_FLAG_FIRST_PIPE_INSTANCE`, the
  `__shutdown` round trip, cancelling a Discord worker blocked in pipe I/O.
  Still by hand only: the locked-`.exe` copy during a real reinstall
  (`replace_binary`; its retry/rename logic is unit-tested).
- Under Wine (see docs/development.md) all tests pass except three Wine
  quirks: `counts_incrementally_and_dedups` (not rechecked since the Windows
  `file_ident` moved to the file id),
  `our_pipe_passes_the_owner_check` and `a_fake_discord_cannot_impersonate_us`.
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

15. **Windows user `PATH` edit is only type-checked** (`src/install.rs`,
    `edit_user_path`, `broadcast_environment_change`). The string logic
    (`add_path_entry`, `remove_path_entry` with quote stripping and an
    injected `%var%` expander, the growth-only 2047-character limit, the
    empty-value check) and `expand_env` are unit-tested; the
    `HKCU\Environment` read/write/delete and the `WM_SETTINGCHANGE`
    broadcast have not run against a real registry. To verify by hand:
    `install` adds the folder once (re-run says "already in your user
    PATH", also when the entry is written as `%LOCALAPPDATA%\...` or
    quoted), a new terminal finds `claude-presence`, the value type
    (`REG_EXPAND_SZ`) and `%vars%` survive, `uninstall` removes only that
    entry (and deletes `Path` if it was the only one), `--no-path` leaves
    `Path` alone.
16. **Windows `file_ident` fallback is only type-checked**
    (`src/ledger.rs`): when `FileIdInfo` fails, the identity comes from
    `GetFileInformationByHandle` (`fold_index`, unit-tested). Not run on a
    file system without `FileIdInfo` (FAT, some network shares).
17. **`{prompts}` in-flight +1 is approximate** (`src/daemon.rs` `render`).
    The +1 applies while Thinking/Working whenever the hook count is ahead
    of the transcript: after a custom slash command in a fresh session it
    stays +1 for every later turn; in a resumed session (hook count below
    the transcript's) it never applies; it drops during Notification and
    Compacting. Fix: record the transcript's count at `UserPromptSubmit`
    (`prompts_at_submit`) and add 1 only while the transcript hasn't passed
    it, until `Stop`. Then tighten the wording in docs/configuration.md.
21. **Drop the legacy ledger import** (`src/ledger.rs` `import_legacy`,
    `read_legacy`, `Stored`, `LEGACY_*`, and `load_stats`' legacy
    fallback): a couple of releases after `ledger.db` shipped, stop reading
    `ledger.json`/`seen.bin` (users who skipped those releases rebuild from
    the transcripts still on disk). The `*.bak` files can then be ignored
    or removed by `uninstall --purge` (already covered: it deletes the
    whole data dir).
22. **SQLite trim flags verified on Windows MSVC only**
    (`.cargo/config.toml` `LIBSQLITE3_FLAGS`): `-U` after
    libsqlite3-sys's `-D`s works with cl.exe (tests pass, ~650 KB
    smaller); gcc/clang process `-D`/`-U` in order too, but the Linux and
    macOS builds are only checked by CI. Also unverified: the ledger on a
    network/FUSE data dir (SQLite locking), and the daemon's busy-db retry
    against a real second process (unit-tested with a second connection),
    and the Unix (`ENOTDIR`) branch of `unreadable_db_path_is_retried`
    (only the Windows `ERROR_INVALID_NAME` branch was run locally).
    Known, accepted: a lasting load error (read-only data dir, invalid
    path) makes the daemon retry the load once a minute forever (logged at
    `debug` after the first warning). A backoff can be added later.
