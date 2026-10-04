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
   for key-order preservation), `libc` (unix), `windows-sys` (windows).

## Layout

| Path | Purpose |
|---|---|
| `src/main.rs` | CLI: `install`, `uninstall`, `status`, `daemon`, `hook`, `config` |
| `src/bin/claude-presenced.rs` | Daemon entry used by services (Windows GUI subsystem → no console window) |
| `src/daemon.rs` | Event loop, session state machine, card rendering, signals |
| `src/discord.rs` | Discord IPC client + worker thread with rate limiting/coalescing |
| `src/ipc.rs` | Hook → daemon channel: Unix socket (0600) / local named pipe |
| `src/ledger.rs` | Incremental transcript parsing, lifetime stats, persistence |
| `src/presence.rs` | Template rendering and the activity payload |
| `src/config.rs` | `config.toml` schema, defaults and `DEFAULT_TOML` (keep in sync — a test enforces it) |
| `src/install.rs` | `settings.json` hook wiring and per-user services |
| `src/git.rs` | Branch / GitHub origin read directly from `.git` |
| `src/paths.rs` | Per-OS directories and socket/pipe names |
| `src/timeutil.rs` | Time helpers (RFC 3339 parsing, local offset, formatting) |
| `src/log.rs` | Minimal leveled logger (`CLAUDE_PRESENCE_LOG`) |
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
  the euid. Windows: named pipe whose DACL grants only the current user's
  SID (not `OW`, which breaks under elevation) and
  `FILE_FLAG_FIRST_PIPE_INSTANCE`; hook clients connect with
  `SECURITY_IDENTIFICATION` and write only if the pipe's owner is the
  user, `BUILTIN\Administrators` (elevated daemon) or LocalSystem
  (`pipe_owner_trusted`), else silently skip. A starting daemon retries a
  busy endpoint for 5 s so a reinstall can replace a daemon that is still
  shutting down.
- **Ledger** (`ledger.json` + append-only `seen.bin`): totals only grow;
  tokens counted once per `message.id`, prompts once per `uuid`, globally.
  `seen.bin` is appended and synced before `ledger.json` is atomically
  replaced; a crash between them (or a failing ledger write) undercounts
  everything since the last successful ledger write, never double counts.
  `seen.bin` is truncated to a multiple of 8 bytes on load and before
  appending. Token arithmetic saturates. Bump `VERSION` in `src/ledger.rs` on incompatible changes.
- **Discord rate limit:** ≤ 4 `SET_ACTIVITY` per 20 s, ≥ 4 s apart; bursts
  coalesce to the latest state. All Discord I/O stays on its worker thread.
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
# Platform code you can't run locally must at least type-check:
rustup target add x86_64-pc-windows-gnu aarch64-apple-darwin
cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
cargo clippy --target aarch64-apple-darwin  --all-targets -- -D warnings
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, test and a release build on
Linux, macOS and Windows.

Manual end-to-end run: `CLAUDE_PRESENCE_LOG=debug cargo run -- daemon`, then
pipe hook JSON into `cargo run -- hook UserPromptSubmit`. Keep socket paths
short (AF_UNIX limit ~100 bytes) when overriding `XDG_RUNTIME_DIR`.

## Conventions

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
- Commits: imperative subject; end with a `Sub-agent: <name>` trailer naming
  the sub-agent credited for the change (see CLAUDE.md).

## Sub-agents

Definitions live in `.claude/agents/`:

| Agent | Model | Role |
|---|---|---|
| `advisor` | sonnet | Read-only design second opinion before non-trivial decisions |
| `auditor` | inherit | Read-only review: correctness, unsafe/FFI, platform pitfalls, resources |
| `developer` | inherit | Implements changes/fixes with tests; runs all checks |
| `docs-writer` | sonnet | README and docs, verified against the source |
