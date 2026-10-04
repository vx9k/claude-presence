# claude-presence

Discord Rich Presence for [Claude Code](https://claude.com/claude-code), in pure Rust.
A lean, native alternative to [claude-rpc](https://github.com/rar-file/claude-rpc): no
Node.js, no web dashboard, no telemetry — just the card.

Linux, macOS and Windows. **Full documentation: [docs/](docs/README.md).**

- **Live card** driven by Claude Code hooks: *Thinking / Working / Waiting on you /
  Compacting / Idle*, current project, git-aware project name, tool + file, model,
  session tokens, elapsed time.
- **Lifetime stats** (active time, tokens, prompts, sessions, streak) imported from your
  existing transcripts and kept even after Claude Code deletes old ones, in one SQLite file
  (`ledger.db`, see `claude-presence status` for the path).
- **Small and fast**: ~3–4 MB RSS, no async runtime, no polling loop: near-zero CPU, it
  wakes only for hooks and timers. A hook invocation takes ~2 ms. JSON is parsed with SIMD
  ([sonic-rs](https://github.com/cloudwego/sonic-rs)), lines are split with SIMD
  ([memchr](https://github.com/BurntSushi/memchr)), and transcripts are read
  incrementally (only newly appended bytes), in parallel on first import.
- **Respects Discord's rate limit** (≤ 4 updates / 20 s, bursts coalesce to the latest state).
- **Per-user service** for every platform: systemd, OpenRC, dinit (or XDG autostart),
  launchd, Windows Task Scheduler.

## Install

Requires the Discord **desktop** app (or an arRPC bridge such as Vesktop). Discord in a
web browser cannot be reached.

```sh
cargo install --git https://github.com/vx9k/claude-presence
claude-presence install
```

This installs two programs: `claude-presence` (the command you type, also called by the
hooks) and `claude-presenced` (the background daemon). `install` then:

1. writes a default config (if none exists),
2. adds hooks for 10 events to `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`).
   A one-time `settings.json.bak` backup is kept; your other settings and hooks are untouched.
   Events: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
   `PostToolUseFailure`, `Notification`, `PreCompact`, `Stop`, `SubagentStop`, `SessionEnd`
   (a Claude Code version that doesn't know an event ignores it),
3. on Windows, adds `%LOCALAPPDATA%\Programs\claude-presence` (where it copies the
   binaries) to your user `PATH`, so new terminals can run `claude-presence`,
4. registers and starts `claude-presenced` as a per-user background service
   (see [Background service](#background-service)).

It ends with `daemon is running` when everything worked. Then just use Claude Code.
Check on it any time with `claude-presence status`. (Installing from `target/debug` prints a
note: run `cargo install --path .` first so hooks don't point into `target/`.)

Re-running `install` is safe (e.g. after upgrading): it asks a running daemon to stop
cleanly (it saves your stats; waits up to 2 s; prints `stopped the running daemon`),
stops the old service, rewrites the hooks and service file, and starts the new binary.
Re-running it also adds hook events a newer version wires (no duplicates). On Windows it
replaces the copied binaries even while the old daemon is shutting down. If you upgrade
the binary but don't re-run `install`, hooks on Linux and macOS still reach a daemon from
before the private socket directory that keeps running.
Details: [docs/services.md](docs/services.md#reinstall-and-upgrade).

### Commands

| Command | What it does |
|---|---|
| `claude-presence install [--init <kind>] [--no-service] [--no-hooks] [--no-path]` | Set everything up (see above) |
| `claude-presence uninstall [--purge]` | Remove hooks, service and (Windows) the `PATH` entry; `--purge` also deletes config and lifetime stats |
| `claude-presence status` | Daemon state, file locations, today/lifetime stats. Exit code 3 if the daemon is not running |
| `claude-presence daemon` | Run the daemon in the foreground (for debugging) |
| `claude-presence config` | Print the config file path |
| `claude-presence hook <Event>` | Used by Claude Code; you never run this yourself |
| `claude-presence --version` / `help` | Version / usage |

`--init` picks the service manager instead of auto-detecting it: `systemd`, `openrc`,
`dinit`, `xdg-autostart` (also `xdg`, `autostart`), `launchd`, `schtasks` (also
`task-scheduler`), `run-key` (also `registry`) or `none`. `--no-service` skips step 3,
`--no-hooks` skips step 2, `--no-path` skips step 4 (accepted and ignored outside Windows).
Details: [docs/services.md](docs/services.md#user-path-windows).

## Background service

`install` picks one automatically and prints it. In all commands the service is called
`claude-presence`; the process is `claude-presenced`. On Linux, `~/.config` means
`$XDG_CONFIG_HOME` if set.

| Platform | Created by `install` | Auto-detected when | Check | Restart |
|---|---|---|---|---|
| Linux, systemd | `~/.config/systemd/user/claude-presence.service` | `/run/systemd/system` exists | `systemctl --user status claude-presence` | `systemctl --user restart claude-presence` |
| Linux, dinit | `~/.config/dinit.d/claude-presence` | PID 1 is dinit, or `dinitctl` is installed without `rc-service` | `dinitctl status claude-presence` | `dinitctl restart claude-presence` |
| Linux, OpenRC ≥ 0.60 | `~/.config/rc/init.d/claude-presence` | `/run/openrc` exists or `openrc` is installed | `rc-service --user claude-presence status` | `rc-service --user claude-presence restart` |
| Linux, other | `~/.config/autostart/claude-presence.desktop` | fallback | `pgrep -a claude-presenced` | `pkill -x claude-presenced`, then start it again |
| macOS | `~/Library/LaunchAgents/io.github.vx9k.claude-presence.plist` | always | `launchctl print gui/$(id -u)/io.github.vx9k.claude-presence \| grep state` | `launchctl kickstart -k gui/$(id -u)/io.github.vx9k.claude-presence` |
| Windows | scheduled task `claude-presence` (fallback: HKCU `Run` key) | always | `schtasks /Query /TN claude-presence /V /FO LIST` | `schtasks /End /TN claude-presence`, then `schtasks /Run /TN claude-presence` |

The easiest check everywhere is `claude-presence status` → `daemon:   running`.
Logs, stop/disable commands, the exact generated files, per-platform requirements
(OpenRC user services, a user dinit instance), manual removal and which services are
verified on real systems: **[docs/services.md](docs/services.md)**.

## Uninstall

```sh
claude-presence uninstall          # remove hooks + service (all flavors) + PATH entry
claude-presence uninstall --purge  # also delete config and lifetime stats
```

`uninstall` first asks a running daemon to stop cleanly and prints `stopped the running
daemon`, whatever started it. It doesn't delete the binaries: `cargo uninstall
claude-presence`, and on Windows also delete `%LOCALAPPDATA%\Programs\claude-presence`.
See [docs/services.md](docs/services.md#uninstall).

## Configuration

`claude-presence config` prints the path:

- Linux: `~/.config/claude-presence/config.toml` (`$XDG_CONFIG_HOME` is honored)
- macOS: `~/Library/Application Support/claude-presence/config.toml`
- Windows: `%APPDATA%\claude-presence\config.toml`

Every key is optional; the generated file documents them all.

```toml
client_id = "..."            # your own Discord application (its name = "Playing <name>")
idle_timeout = 900           # clear an idle card after 15 min without activity (0 = never, else 60..604800 s)
github_button = false        # "View on GitHub" button (off: would leak private repos)
hidden_projects = ["secret-client", "~/work/nda"]

[status.working]
details = "Working in {project}"
state = "{tool} · {file} · {tokens} tokens"
```

Apply changes with `SIGHUP` (Linux/macOS) or by restarting the service (Windows).
Every key, template variable, clamping rule and example:
**[docs/configuration.md](docs/configuration.md)**. The default `client_id` and images are
borrowed from claude-rpc; see the note there.

## Troubleshooting

1. `claude-presence status` should say `daemon:   running`.
2. The Discord *desktop* app must be open, with Activity Privacy → *Share your detected
   activities* on.
3. Only local Claude Code sessions can be shown (not cloud sessions). Test without Claude
   Code: `echo '{"session_id":"test","cwd":"/"}' | claude-presence hook UserPromptSubmit`
   should make a card appear (with `cwd` `/` only the second line shows; use a real project
   directory to see the full card).
4. Watch it live: stop the service, then `CLAUDE_PRESENCE_LOG=debug claude-presence daemon`.
   The log line `first hook received (<event>)` proves hooks reach the daemon.

Symptom-by-symptom table, log lines explained, log file locations:
**[docs/troubleshooting.md](docs/troubleshooting.md)**.

## More documentation

| Page | Topic |
|---|---|
| [docs/architecture.md](docs/architecture.md) | Components, data flow, threads and timers, session state machine |
| [docs/ipc-and-security.md](docs/ipc-and-security.md) | Hook wire format, socket/pipe permissions, threat model, privacy |
| [docs/ledger.md](docs/ledger.md) | Lifetime stats database, dedup rules, crash consistency, migration |
| [docs/development.md](docs/development.md) | Build, checks, test-driven workflow, sub-agents, CI |

For contributors and coding agents: [AGENTS.md](AGENTS.md), [CLAUDE.md](CLAUDE.md),
[TODO.md](TODO.md). Build with `cargo build --release`; run `cargo test`.

## License

Apache-2.0
