# AGENTS.md

Guide for AI coding agents (and humans) working on **claude-presence**: Discord
Rich Presence for Claude Code, written in Rust, for Linux, macOS and Windows.
A lean alternative to the Node.js [claude-rpc](https://github.com/rar-file/claude-rpc).

## Goals (in priority order)

1. **Low resource usage** — the daemon runs all day. No async runtime, no
   polling loops, no needless threads or allocations on hot paths. Target:
   single-digit MB RSS, zero CPU while nothing happens.
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

## Invariants — don't break these

- **Hook wire format:** `<EventName>\n<raw JSON from Claude Code>`. The hook
  process never parses JSON.
- **Ledger** (`ledger.json` + append-only `seen.bin`): totals only grow;
  tokens counted once per `message.id`, prompts once per `uuid`, globally.
  `ledger.json` is written atomically *before* `seen.bin` is appended. Bump
  `VERSION` in `src/ledger.rs` on incompatible changes.
- **Discord rate limit:** ≤ 4 `SET_ACTIVITY` per 20 s, ≥ 4 s apart; bursts
  coalesce to the latest state. All Discord I/O stays on its worker thread.
- **`settings.json`:** only touch hook entries whose command contains
  `claude-presence` and ` hook `; preserve everything else and key order.
- **Config:** every key optional; `Config::default()` must equal
  `DEFAULT_TOML`.

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
- Testable fixes come with a unit test (tests live in each module).
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
