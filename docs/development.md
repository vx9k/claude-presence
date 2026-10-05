# Development

Authoritative rules live in [AGENTS.md](../AGENTS.md) and [CLAUDE.md](../CLAUDE.md); open work is in
[TODO.md](../TODO.md). See also [architecture.md](architecture.md) and the [README](../README.md).

## Build

```sh
cargo build --release   # target/release/claude-presence and claude-presenced
cargo install --path .  # installs both binaries; `install` needs them side by side
```

Rust edition 2024, `rust-version = 1.85`. Release profile: `opt-level = 3`, fat LTO, one codegen unit, `panic = "abort"`, stripped.

Manual end-to-end run, no service involved:

```sh
CLAUDE_PRESENCE_LOG=debug cargo run -- daemon
echo '{"session_id":"t","cwd":"/"}' | cargo run -- hook UserPromptSubmit
```

Keep socket paths short (AF_UNIX limit is about 100 bytes) when overriding `XDG_RUNTIME_DIR`. Cloud Claude Code sessions never reach a local daemon, so feed hooks by hand when testing.

## Checks

Run the first three before every commit; the two cross clippy targets are optional locally:

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

(The last two are the Windows and macOS clippy targets; `rustup target add` is a one-time setup.) CI's native clippy on each OS is the authoritative platform check; say in the PR which platforms were only checked by CI. `rustfmt.toml` sets width 120.

## Test-driven workflow

1. Write a failing unit test first, in the `#[cfg(test)] mod tests` of the module you are changing.
2. Run it and confirm it fails for the intended reason (the assertion you expect, not a compile error or a typo). For example `cargo test crash_before_commit`.
3. Make the fix.
4. Keep the test.

Conventions the existing tests follow:

| Pattern | Example |
|---|---|
| Temp paths unique per process: `std::env::temp_dir().join(format!("cp-<name>-{}", std::process::id()))`, removed at the end | `cp-ipc-{pid}.sock` in `src/ipc.rs`, `cp-test-<name>-{pid}` in `src/ledger.rs` |
| No real Discord: `Presenter::inert()` (no worker thread) plus `Presenter::wanted()` to read what was set; a fake Discord server on a `UnixListener` for protocol tests | `talks_to_fake_discord`, `tick_pushes_to_presenter_only` |
| No real services or settings: render service files and settings as strings (`systemd_unit`, `wire_hooks(&str, &Path)`) instead of running `systemctl` or touching `~/.claude` | `service_files_render`, `wires_and_unwires_hooks_preserving_settings` |
| Daemon without a Discord client: `Daemon::with_presenter(cfg, ledger, Presenter::inert())` and `handle_hook(...)`, `render(...)`, `pick()`, `tick()`, `expire(...)` called directly | `state_machine_and_render`, `sticky_session_choice` |
| Pure logic split out of FFI so it runs on Linux: `check_private` and `check_parent` take plain uid/mode numbers; `pipe_sddl(sid)` is a string function; `next_allowed` is the rate-limit logic and `Wire::record` the send-outcome and keepalive bookkeeping; `route` and `expiry_secs` are free functions | `private_dir_is_verified`, `pipe_sddl_grants_only_the_user`, `throttle_window` |
| Output generic over `Write` so failures can be injected | `write_status` with a `ClosedPipe` writer in `src/main.rs` |
| Regression tests named for the property | `crash_before_commit_counts_exactly_once`, `live_transcript_does_not_spin_expiry` |
| Shift time instead of sleeping where possible | `d.rotation.since -= ...`, `sessions.get_mut(..).last_activity = ...`, then `tick()` |
| Unix-only tests are gated `#[cfg(all(test, unix))]` or sit in an inner `#[cfg(unix)] mod unix`; Windows runtime tests in `#[cfg(windows)] mod windows` | `src/ipc.rs`, `src/discord.rs` |

Rules that tests must respect: never talk to a real Discord client, never run or modify real services, never touch the user's `settings.json` or config; do not rely on environment variables other tests may change.

When you add a config key, update `Config::default()` and `DEFAULT_TOML` together; the test `default_toml_matches_defaults` enforces they match.

Code conventions: match surrounding style, every `unsafe` block gets a `// SAFETY:` comment, platform code is gated with `cfg(unix)`, `cfg(windows)`, `cfg(target_os = "macos")` or `cfg(all(unix, not(target_os = "macos")))`.

## Platform code you cannot run

macOS-only code and Windows service code (Task Scheduler, Run key) cannot be run on a Linux dev box; Windows named-pipe code can run under Wine (below).

- Make them compile cleanly: run the two cross clippy commands above if you have a C cross toolchain (optional), and otherwise rely on CI's native clippy on each OS and say so in the PR.
- Factor decisions out of the FFI calls into pure functions and test those on Linux (see the table).
- Put Windows runtime tests in a `#[cfg(windows)]` module of the module's tests; Windows CI runs them.
- Reason carefully about the rest, and say in the PR which behavior is only type-checked and unverified.
- Add the item to TODO.md "Low / unverified" and remove it once verified.

### Windows tests under Wine

With `wine` and the `x86_64-pc-windows-gnu` target installed, the Windows test build runs locally:

```sh
export WINEDEBUG=-all WINEPREFIX=/tmp/claude-0/wineprefix   # any private prefix
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine cargo test --target x86_64-pc-windows-gnu
```

Wine is a fast signal only; Windows CI is authoritative. Known Wine-only failures (Wine 9.0):

| Test | Why it fails only under Wine |
|---|---|
| `ledger::tests::counts_incrementally_and_dedups` | Seen when the Windows `file_ident` was the creation time: Wine reports the Unix ctime as the creation time, so it changed on every append. `file_ident` now uses `FileIdInfo`; whether Wine reports a stable file id (and passes `windows_file_ident_survives_append_not_recreate`) has not been rechecked. |
| `ipc::tests::windows::our_pipe_passes_the_owner_check` | Wine's token default owner is its primary group `S-1-5-21-0-0-0-513`, so pipes are owned by that group rather than the user or Administrators. |
| `discord::tests::a_fake_discord_cannot_impersonate_us` | Wine hands a pipe server an impersonation-level token whatever impersonation level the client asked for (`SECURITY_IDENTIFICATION`). |

Wine also ignores `FILE_FLAG_FIRST_PIPE_INSTANCE` (it only sets `ERROR_ALREADY_EXISTS`); `Listener::create` treats that as `AddrInUse`, so the single-instance tests are meaningful under Wine too. The `OW` negative control in `elevated_daemon_accepts_a_non_elevated_hook` is skipped unless the process is elevated and new objects are owned by `BUILTIN\Administrators`, so it is skipped under Wine; when `CI` or `GITHUB_ACTIONS` is set, the test fails instead of skipping, so CI can't silently skip it.

Invariants to keep (details in AGENTS.md): hooks never parse JSON, never print, always exit 0 and stay fast; ledger saves are one SQLite transaction (ids, totals, days and offsets commit together; a busy or newer database is never overwritten); Discord limit of 4 `SET_ACTIVITY` per 20 s with at least 4 s between, all Discord I/O on its worker thread; only touch `settings.json` hook entries whose command contains `claude-presence` and ` hook `; every config key optional.

## Sub-agent workflow

Definitions are in `.claude/agents/`.

| Agent | Model | Role |
|---|---|---|
| `advisor` | sonnet | Read-only design second opinion |
| `developer` | inherit | Implements change and tests, runs every check |
| `auditor` | inherit | Read-only review of the diff |
| `docs-writer` | sonnet | README and docs, verified against the source |

Sub-agents start cold and re-read the repo, so the main thread does most work itself (see CLAUDE.md):

1. Implement with tests; run all checks once the change is done. The two cross clippy targets are optional (they need a C cross toolchain), so say in the PR which platforms were only checked by CI.
2. `auditor`: one review of the finished diff; fix high/medium findings.
3. `advisor` only for genuinely open decisions (a dependency, thread or timer; the hook wire format, ledger format, config schema or service installation); `developer` / `docs-writer` only for large self-contained work.
4. Commit with a [Conventional Commits](#commit-messages) message, push, check CI.

Start new work from [TODO.md](../TODO.md); update it when you fix or discover something.

### Attribution

When a sub-agent did most of the work, commits and PR descriptions credit it; when the main thread did, they carry no credit line:

- commit: a `Sub-agent: <name>` trailer (see [Commit messages](#commit-messages));
- PR description: end with `🤖 Written by the <name> sub-agent`.

Keep any other trailers your harness requires.

### Commit messages

Every commit, and every PR title, strictly follows
[Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/):

```
<type>(<scope>)!: <description>

<body>

<footers>
```

- **type** (required), one of:

  | Type | Use for |
  |---|---|
  | `feat` | A new user-visible feature (command, flag, config key, template variable, card behavior) |
  | `fix` | A bug fix |
  | `perf` | A change that only improves speed or resource usage |
  | `refactor` | A code change that neither fixes a bug nor adds a feature |
  | `test` | Adding or correcting tests only |
  | `docs` | README, `docs/`, `SECURITY.md`, `AGENTS.md`, `CLAUDE.md`, `TODO.md`, agent definitions |
  | `build` | `Cargo.toml`, `Cargo.lock`, `.cargo/`, dependency changes |
  | `ci` | `.github/` |
  | `style` | Formatting only (`cargo fmt`) |
  | `chore` | Anything else that touches no source or docs |
  | `revert` | Reverting a commit; the body says `This reverts commit <sha>.` |

  A commit that fixes a bug and adds its test is `fix`, not `test`.
- **scope** (optional): the module or area, lowercase: `daemon`, `discord`,
  `ipc`, `ledger`, `presence`, `config`, `install`, `git`, `paths`, `log`,
  `timeutil`, `state`, `tui`, `cli` (`src/main.rs`), `deps`, `agents`. Omit it when a change
  spans many areas.
- **`!`** after the type/scope, plus a `BREAKING CHANGE: <what and how to
  migrate>` footer, when a change breaks users: the hook wire format, the
  ledger or config format without automatic migration, a removed command,
  flag or config key, or changed service names.
- **description**: imperative mood, lowercase first letter, no trailing
  period, the whole header at most 72 characters. `fix(ipc): reject pipes
  owned by other users`, not `Fixed pipe owner check.`
- **body** (optional, after a blank line): what and why, wrapped at 72.
- **footers** (after a blank line), in this order: `BREAKING CHANGE:`, `Refs: #<n>` /
  `Closes: #<n>`, `Sub-agent: <name>`, then any trailers your harness
  requires (e.g. `Co-Authored-By:`).

One logical change per commit; split mixed work (e.g. `fix(ledger): ...` and
`docs: ...`) rather than picking one type. PRs are squash-merged, so the PR
title becomes the commit on `main` and must follow the same rules.

Example:

```
feat(ledger)!: store lifetime stats in sqlite

Ids, totals and file offsets now commit in one transaction, so a crash
can no longer undercount.

BREAKING CHANGE: ledger.json and seen.bin are imported once into
ledger.db and renamed to *.bak; older versions cannot read ledger.db.
Sub-agent: developer
```

## CI

`.github/workflows/ci.yml` runs on pushes to `main` and on pull requests, on `ubuntu-latest`, `macos-latest` and `windows-latest` (not fail-fast): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build --release`. It uses the stable toolchain with clippy and rustfmt, and `Swatinem/rust-cache` (cache saved only on `main`). The cross clippy targets are not in CI; they are optional locally (they need a C cross toolchain because bundled SQLite is compiled even under clippy), and the native clippy run on each OS is the authoritative platform check. Each test module compiles on every platform; only tests that need a real Unix socket or POSIX permissions sit in an inner `#[cfg(unix)] mod unix` (or are gated per test). Pure tests such as `pipe_sddl_grants_only_the_user`, `pipe_owner_must_be_the_user_admins_or_system`, `copy_retries_only_while_locked` and the presenter shutdown tests therefore also run on Windows CI. Put new pure tests in the outer module. The hook round trip, `__shutdown` round trip, bind retry and `stops_a_running_daemon` tests run against a Unix socket or a named pipe (`test_addr`). Windows CI (`windows-latest`, which runs elevated) additionally verifies in `#[cfg(windows)]` tests: the pipe's DACL read back (protected, one `ACCESS_ALLOWED` ACE for the user's SID); an elevated daemon accepting a non-elevated hook (a restricted token with Administrators deny-only at medium integrity, impersonated on a client thread), with an `OW` pipe rejecting the same client as a negative control; the hook-side pipe-owner check; that a pipe server only gets an identification-level token from the Discord client; a message written and closed before `serve` (`ERROR_NO_DATA`); and `Presenter::shutdown` cancelling a worker blocked in `ReadFile` (`CancelSynchronousIo`). Copying over a locked `.exe` during a real reinstall is not covered (only `copy_with_retry` and the rename fallback, with closures).

## Release build

`cargo build --release` produces `target/release/claude-presence` and `claude-presenced` (`.exe` on Windows). On Windows `claude-presenced` is a GUI-subsystem binary so no console window appears at logon; its log goes to `%LOCALAPPDATA%\claude-presence\daemon.log`. Distribution is currently `cargo install --git https://github.com/vx9k/claude-presence`; there is no release automation in the repository.

SQLite is compiled in (rusqlite `bundled`), with its optional extensions switched off through `LIBSQLITE3_FLAGS` in `.cargo/config.toml` (about 650 KB saved per binary). Cargo reads that file only for builds started inside the checkout (`cargo build`, `cargo install --path .`); `cargo install --git` ignores it and builds the untrimmed SQLite, which works the same but is about 650 KB larger per binary. Windows MSVC release sizes: `claude-presence.exe` 1.51 MB before SQLite, 2.54 MB after; `claude-presenced.exe` 1.29 MB, 2.32 MB. The trim flags have only been verified with MSVC; Linux and macOS builds are checked by CI.
