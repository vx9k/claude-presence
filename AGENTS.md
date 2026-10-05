# AGENTS.md

Guide for AI coding agents (and humans) working on **claude-presence**: Discord
Rich Presence for Claude Code, written in Rust, for Linux, macOS and Windows.
A lean alternative to the Node.js [claude-rpc](https://github.com/rar-file/claude-rpc).

## Goals (in priority order)

1. **Low resource usage** — the daemon runs all day. No async runtime, no
   polling loops, no needless threads or allocations on hot paths. Target:
   single-digit MB RSS, near-zero CPU while nothing happens (the daemon only
   wakes for hooks, signals and deadline-driven timers: background rescan,
   card rotation and Discord keepalive while a card is shown, transcript
   tail while a session is active, pending ledger save).
2. **Performance** — SIMD JSON (`sonic-rs`), SIMD line splitting (`memchr`),
   incremental transcript reads (only newly appended bytes).
3. **Never break the user's Claude Code session** — hooks must always exit 0
   quickly, even if the daemon is down.
4. **Small dependency set** — prefer `std`. Current deps: `sonic-rs`, `memchr`,
   `serde`, `toml`, `serde_json` (install-time `settings.json` editing only,
   for key-order preservation), `rusqlite` (`bundled`, no default features;
   ledger persistence; extensions trimmed via `LIBSQLITE3_FLAGS` in
   `.cargo/config.toml`), `libc` (unix), `windows-sys` (windows), `ratatui` (optional `tui` feature,
   default on; only the `tui` command uses it — the daemon binary stays the
   same size).

## Layout

| Path | Purpose |
|---|---|
| `src/main.rs` | CLI: `install`, `uninstall`, `status`, `daemon`, `hook`, `config` |
| `src/bin/claude-presenced.rs` | Daemon entry used by services (Windows GUI subsystem → no console window) |
| `src/daemon.rs` | Event loop, session state machine, card rendering, signals |
| `src/discord.rs` | Discord IPC client + worker thread with rate limiting/coalescing |
| `src/ipc.rs` | Hook → daemon channel: Unix socket (0600) / local named pipe |
| `src/ledger.rs` | Incremental transcript parsing, lifetime stats, delta saves to SQLite `ledger.db`, one-time legacy `ledger.json` import |
| `src/presence.rs` | Template rendering and the activity payload |
| `src/config.rs` | `config.toml` schema, defaults and `DEFAULT_TOML` (keep in sync — a test enforces it) |
| `src/install.rs` | `settings.json` hook wiring and per-user services |
| `src/git.rs` | Branch / GitHub origin read directly from `.git` |
| `src/paths.rs` | Per-OS directories and socket/pipe names |
| `src/timeutil.rs` | Time helpers (RFC 3339 parsing, local offset, formatting) |
| `src/log.rs` | Minimal leveled logger (`CLAUDE_PRESENCE_LOG`) |
| `src/state.rs` | `__state` snapshot (JSON, ≤ 64 KiB) the daemon builds for the TUI |
| `src/tui/` | `claude-presence tui`: pure `app` (state/keys) and `ui` (drawing, `TestBackend` tests); `run` owns the terminal and the poll thread |
| `docs/` | In-depth docs: architecture, IPC/security, ledger, config, services, troubleshooting, development |
| `TODO.md` | Handoff: open audit findings and verification status |

## Current state

Open work (audit findings, unverified platforms, decisions already made) is
tracked in [`TODO.md`](TODO.md). Read it before starting; update it when you
fix or discover something. How the pieces fit together is in
[`docs/architecture.md`](docs/architecture.md); the reasoning behind the IPC
checks and the ledger's crash rules is in
[`docs/ipc-and-security.md`](docs/ipc-and-security.md) and
[`docs/ledger.md`](docs/ledger.md).

## Invariants — don't break these

- **Hook wire format:** `<EventName>\n<raw JSON from Claude Code>`. The hook
  process never parses JSON. Event names starting with `__` are reserved
  control messages sent only by claude-presence itself (`__shutdown`, used
  by `install`/`uninstall` to stop a running daemon); `hook` never forwards
  them and the daemon ignores unknown ones.
- **Hook endpoint:** Unix socket `0600` in an OS-private runtime dir, or else
  in `<tmp>/claude-presence-<uid>/`, which the daemon creates `0700` and
  refuses to use unless it is a real directory owned by it with no
  group/other bits and owner rwx (never chmod/remove it), and whose parent
  is ours or root's and sticky or not writable by others. Hook clients run
  the same checks before connecting (only on this fallback path) and
  silently skip sending if they fail. Until a couple of releases after the
  private dir shipped, a hook that finds nothing at today's path (`NotFound`
  / `ConnectionRefused`, never after a failed check) falls back to
  `paths::legacy_hook_socket()`, only if `lstat` shows a socket owned by
  the euid in a parent that passes the same parent check. Windows: named pipe whose DACL grants only the current user's
  SID (not `OW`, which breaks under elevation) and
  `FILE_FLAG_FIRST_PIPE_INSTANCE`; hook clients connect with
  `SECURITY_IDENTIFICATION` and write only if the pipe's owner is the
  user, `BUILTIN\Administrators` (elevated daemon) or LocalSystem
  (`pipe_owner_trusted`), else silently skip. A starting daemon retries a
  busy endpoint for 5 s so a reinstall can replace a daemon that is still
  shutting down.
- **Ledger** (SQLite `ledger.db`): totals only grow; tokens counted once
  per `message.id`, prompts once per `uuid`, globally. State lives in
  memory; the database is only the durable store, opened per load/save and
  closed afterwards (no idle handle). A save writes only changed rows (new
  seen ids, totals, dirty days, dirty files, pruned files) in one
  `BEGIN IMMEDIATE` transaction with `synchronous=FULL`, so ids, totals and
  file offsets commit together: a crash or failed save loses nothing and
  never double counts (dirty sets are cleared only after `COMMIT`). A db
  that can't be loaded (busy, unreadable, a legacy file that can't be read;
  only `NotFound` means "nothing there") is never written, nor is one that
  appeared after a start without one; the daemon retries the load every
  60 s. A corrupt db is moved to `ledger.db.corrupt`; a
  `user_version` above ours is never written. A legacy `ledger.json` +
  `seen.bin` is imported once (same transaction that sets `user_version`)
  and renamed `*.bak`. u64s are stored bit-cast to i64. Token arithmetic
  saturates. Bump `DB_VERSION` in `src/ledger.rs` on incompatible changes.
- **Discord rate limit:** ≤ 4 `SET_ACTIVITY` per 20 s, ≥ 4 s apart; bursts
  coalesce to the latest state. All Discord I/O stays on its worker thread.
  On Windows it connects with `SECURITY_IDENTIFICATION` (no impersonation
  by a fake Discord pipe), and a worker still blocked at shutdown has its
  I/O cancelled (`CancelSynchronousIo`).
- **`settings.json`:** only touch hook entries whose command contains
  `claude-presence` and ` hook `; preserve everything else and key order.
- **Config:** every key optional (a partial `[status.*]` table falls back per
  key to that status's defaults); `Config::default()` must equal
  `DEFAULT_TOML`; `idle_timeout` and `rotation_interval` are clamped on load
  (`Config::sanitized`).
- **Tests never touch the real world:** no real Discord (use
  `Presenter::inert` / `Daemon::with_presenter`), no real `settings.json`,
  services or user dirs; temp paths are unique per test and per pid.

## Commands

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
# Optional: type-check platform code you can't run locally. Bundled SQLite
# compiles sqlite3.c even under clippy, so these need a C cross toolchain
# (mingw `gcc` for windows-gnu; a cross `cc` + macOS SDK, e.g. zig or
# osxcross, for darwin). Without one, skip them and rely on CI.
rustup target add x86_64-pc-windows-gnu aarch64-apple-darwin
cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
cargo clippy --target aarch64-apple-darwin  --all-targets -- -D warnings
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, test and a release build on
Linux, macOS and Windows; its native clippy on each OS is the authoritative
platform check. Say in the PR which platforms were only checked by CI.

Manual end-to-end run: `CLAUDE_PRESENCE_LOG=debug cargo run -- daemon`, then
pipe hook JSON into `cargo run -- hook UserPromptSubmit`. Keep socket paths
short (AF_UNIX limit ~100 bytes) when overriding `XDG_RUNTIME_DIR`.

## Conventions

- **Simplest thing that works** (the repo enables the `ponytail` plugin):
  before writing code, check whether it needs to exist, then reuse what is
  already here (`timeutil`, `presence::clamp`, `paths`, existing deps), then
  `std`, then a few lines of your own. No abstractions with one user, no
  config for values that never change, no scaffolding for later. Mark a
  deliberate shortcut with a known ceiling with a `ponytail:` comment naming
  the ceiling and the upgrade path. Never simplify away the invariants
  below, endpoint checks, or error handling that prevents data loss.
- Match surrounding style; `rustfmt.toml` sets width 120.
- Every `unsafe` block gets a `// SAFETY:` comment.
- **Test-driven:** write the failing unit test first, run it and confirm it
  fails for the intended reason, then fix. Tests live in each module. Factor
  decisions out of FFI/IO into pure functions so they are testable on Linux
  (e.g. `check_private`, `pipe_sddl`, `Wire::record`, `read_frame_from`).
  Bugs found while adding tests are fixed in the same change. See
  [`docs/development.md`](docs/development.md).
- Platform code is gated with `cfg(unix)`, `cfg(windows)`,
  `cfg(target_os = "macos")`, `cfg(all(unix, not(target_os = "macos")))`.
- Commits and PR titles: strictly [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/)
  (`<type>(<scope>)!: <description>`; types, scopes and breaking rules in
  [`docs/development.md`](docs/development.md#commit-messages)); add a
  `Sub-agent: <name>` trailer only when a sub-agent did most of the work
  (see CLAUDE.md).

## Sub-agents

Definitions live in `.claude/agents/`:

| Agent | Model | Role |
|---|---|---|
| `advisor` | sonnet | Read-only design second opinion before non-trivial decisions |
| `auditor` | inherit | Read-only review: correctness, unsafe/FFI, platform pitfalls, resources |
| `developer` | inherit | Implements changes/fixes with tests; runs all checks |
| `docs-writer` | sonnet | README and docs, verified against the source |
