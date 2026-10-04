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

Run all five before every commit:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
# Platform code you can't run locally must at least type-check:
rustup target add x86_64-pc-windows-gnu aarch64-apple-darwin
cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
cargo clippy --target aarch64-apple-darwin  --all-targets -- -D warnings
```

(The last two are the Windows and macOS clippy targets; `rustup target add` is a one-time setup.) `rustfmt.toml` sets width 120.

## Test-driven workflow

1. Write a failing unit test first, in the `#[cfg(test)] mod tests` of the module you are changing.
2. Run it and confirm it fails for the intended reason (the assertion you expect, not a compile error or a typo). For example `cargo test crash_between_seen`.
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
| Regression tests named for the property | `crash_between_seen_and_ledger_never_double_counts`, `live_transcript_does_not_spin_expiry` |
| Shift time instead of sleeping where possible | `d.rotation.since -= ...`, `sessions.get_mut(..).last_activity = ...`, then `tick()` |
| Unix-only tests are gated `#[cfg(all(test, unix))]` or sit in an inner `#[cfg(unix)] mod unix`; Windows runtime tests in `#[cfg(windows)] mod windows` | `src/ipc.rs`, `src/discord.rs` |

Rules that tests must respect: never talk to a real Discord client, never run or modify real services, never touch the user's `settings.json` or config; do not rely on environment variables other tests may change.

When you add a config key, update `Config::default()` and `DEFAULT_TOML` together; the test `default_toml_matches_defaults` enforces they match.

Code conventions: match surrounding style, every `unsafe` block gets a `// SAFETY:` comment, platform code is gated with `cfg(unix)`, `cfg(windows)`, `cfg(target_os = "macos")` or `cfg(all(unix, not(target_os = "macos")))`.

## Platform code you cannot run

macOS-only code and Windows service code (Task Scheduler, Run key) cannot be run on a Linux dev box; Windows named-pipe code can run under Wine (below).

- Make them compile cleanly with the two cross clippy commands above.
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
| `ledger::tests::counts_incrementally_and_dedups` | Wine reports the Unix ctime as the creation time, so the Windows `file_ident` (creation time) changes on every append and the file is re-read from the start. NTFS creation times don't change on append. |
| `ipc::tests::windows::our_pipe_passes_the_owner_check` | Wine's token default owner is its primary group `S-1-5-21-0-0-0-513`, so pipes are owned by that group rather than the user or Administrators. |
| `discord::tests::a_fake_discord_cannot_impersonate_us` | Wine hands a pipe server an impersonation-level token whatever impersonation level the client asked for (`SECURITY_IDENTIFICATION`). |

Wine also ignores `FILE_FLAG_FIRST_PIPE_INSTANCE` (it only sets `ERROR_ALREADY_EXISTS`); `Listener::create` treats that as `AddrInUse`, so the single-instance tests are meaningful under Wine too. The `OW` negative control in `elevated_daemon_accepts_a_non_elevated_hook` is skipped unless the process is elevated and new objects are owned by `BUILTIN\Administrators`, so it is skipped under Wine; when `CI` or `GITHUB_ACTIONS` is set, the test fails instead of skipping, so CI can't silently skip it.

Invariants to keep (details in AGENTS.md): hooks never parse JSON, never print, always exit 0 and stay fast; ledger write order (`seen.bin` synced before `ledger.json` is replaced); Discord limit of 4 `SET_ACTIVITY` per 20 s with at least 4 s between, all Discord I/O on its worker thread; only touch `settings.json` hook entries whose command contains `claude-presence` and ` hook `; every config key optional.

## Sub-agent workflow

Definitions are in `.claude/agents/`.

| Agent | Model | Role |
|---|---|---|
| `advisor` | sonnet | Read-only design second opinion |
| `developer` | inherit | Implements change and tests, runs every check |
| `auditor` | inherit | Read-only review of the diff |
| `docs-writer` | sonnet | README and docs, verified against the source |

Loop for a change:

1. `advisor`, if the change adds a dependency, a thread or timer, or touches the hook wire format, the ledger format, the config schema or service installation.
2. `developer`: implement with tests; run all checks including the two cross clippy targets.
3. `auditor`: review the diff; hand findings back to `developer` until it reports no high or medium findings.
4. `docs-writer`: update README and `docs/` when commands, config keys, template variables, paths or service behavior changed. This and the audit can run in parallel.
5. Commit, push, check CI.

Start new work from [TODO.md](../TODO.md); update it when you fix or discover something.

### Attribution

Commits and PR descriptions are credited to one sub-agent picked at random from `advisor`, `auditor`, `developer`, `docs-writer`:

- commit: imperative subject and a `Sub-agent: <name>` trailer;
- PR description: end with `🤖 Written by the <name> sub-agent`.

Keep any other trailers your harness requires.

## CI

`.github/workflows/ci.yml` runs on pushes to `main` and on pull requests, on `ubuntu-latest`, `macos-latest` and `windows-latest` (not fail-fast): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build --release`. It uses the stable toolchain with clippy and rustfmt, and `Swatinem/rust-cache` (cache saved only on `main`). The cross clippy targets are not in CI; run them locally. Each test module compiles on every platform; only tests that need a real Unix socket or POSIX permissions sit in an inner `#[cfg(unix)] mod unix` (or are gated per test). Pure tests such as `pipe_sddl_grants_only_the_user`, `pipe_owner_must_be_the_user_admins_or_system`, `copy_retries_only_while_locked` and the presenter shutdown tests therefore also run on Windows CI. Put new pure tests in the outer module. The hook round trip, `__shutdown` round trip, bind retry and `stops_a_running_daemon` tests run against a Unix socket or a named pipe (`test_addr`). Windows CI (`windows-latest`, which runs elevated) additionally verifies in `#[cfg(windows)]` tests: the pipe's DACL read back (protected, one `ACCESS_ALLOWED` ACE for the user's SID); an elevated daemon accepting a non-elevated hook (a restricted token with Administrators deny-only at medium integrity, impersonated on a client thread), with an `OW` pipe rejecting the same client as a negative control; the hook-side pipe-owner check; that a pipe server only gets an identification-level token from the Discord client; a message written and closed before `serve` (`ERROR_NO_DATA`); and `Presenter::shutdown` cancelling a worker blocked in `ReadFile` (`CancelSynchronousIo`). Copying over a locked `.exe` during a real reinstall is not covered (only `copy_with_retry` and the rename fallback, with closures).

## Release build

`cargo build --release` produces `target/release/claude-presence` and `claude-presenced` (`.exe` on Windows). On Windows `claude-presenced` is a GUI-subsystem binary so no console window appears at logon; its log goes to `%LOCALAPPDATA%\claude-presence\daemon.log`. Distribution is currently `cargo install --git https://github.com/vx9k/claude-presence`; there is no release automation in the repository.
