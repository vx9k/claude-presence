# claude-presence

Discord Rich Presence for [Claude Code](https://claude.com/claude-code), in pure Rust.
A lean, native alternative to [claude-rpc](https://github.com/rar-file/claude-rpc): no
Node.js, no web dashboard, no telemetry — just the card.

Linux, macOS and Windows.

- **Live card** driven by Claude Code hooks: *Thinking / Working / Waiting on you /
  Compacting / Idle*, current project, git-aware project name, tool + file, model,
  session tokens, elapsed time.
- **Lifetime stats** (active time, tokens, prompts, sessions, streak) imported from your
  existing transcripts and kept even after Claude Code deletes old ones.
- **Small and fast**: ~3–4 MB RSS, no async runtime, sleeps until a hook or timer fires.
  A hook invocation takes ~2 ms. JSON is parsed with SIMD
  ([sonic-rs](https://github.com/cloudwego/sonic-rs)), lines are split with SIMD
  ([memchr](https://github.com/BurntSushi/memchr)), and transcripts are read
  incrementally (only newly appended bytes), in parallel on first import.
- **Respects Discord's rate limit** (≤ 4 updates / 20 s, bursts coalesce to the latest state).
- **Per-user service** for every platform: systemd, OpenRC, dinit (or XDG autostart),
  launchd, Windows Task Scheduler.

## Install

Requires the Discord **desktop** app (or an arRPC bridge such as Vesktop).

```sh
cargo install --git https://github.com/vx9k/claude-presence
claude-presence install
```

`install` does three things:

1. writes a default config (if none exists),
2. adds hooks to `~/.claude/settings.json` (a one-time `settings.json.bak` backup is kept;
   your other settings and hooks are untouched),
3. registers and starts the background daemon (`claude-presenced`) as a per-user service.

Then just use Claude Code. Check on it with `claude-presence status`.

### Service per platform

| Platform | Default | Where |
|---|---|---|
| Linux (systemd) | user unit | `~/.config/systemd/user/claude-presence.service` |
| Linux (OpenRC ≥ 0.60) | user service | `~/.config/rc/init.d/claude-presence` |
| Linux (dinit) | user service | `~/.config/dinit.d/claude-presence` |
| Linux (other) | XDG autostart | `~/.config/autostart/claude-presence.desktop` |
| macOS | LaunchAgent | `~/Library/LaunchAgents/io.github.vx9k.claude-presence.plist` |
| Windows | Task Scheduler (at logon, restart on failure; falls back to the HKCU `Run` key) | task `claude-presence` |

The init system is auto-detected; override with `--init systemd|openrc|dinit|xdg-autostart|launchd|schtasks|run-key|none`.

On **Windows**, `install` copies both executables to `%LOCALAPPDATA%\Programs\claude-presence`
so it keeps working if you delete the download. The daemon is a windowless background
process; it logs to `%LOCALAPPDATA%\claude-presence\daemon.log`.

### Manual service management

```sh
systemctl --user status claude-presence           # systemd
rc-service --user claude-presence status          # OpenRC
dinitctl status claude-presence                   # dinit
launchctl print gui/$(id -u)/io.github.vx9k.claude-presence   # macOS
schtasks /Query /TN claude-presence               # Windows
```

## Uninstall

```sh
claude-presence uninstall          # remove hooks + service
claude-presence uninstall --purge  # also delete config and lifetime stats
```

## Configuration

`claude-presence config` prints the path:

- Linux: `~/.config/claude-presence/config.toml`
- macOS: `~/Library/Application Support/claude-presence/config.toml`
- Windows: `%APPDATA%\claude-presence\config.toml`

Every key is optional; the generated file documents them all. Highlights:

```toml
client_id = "..."            # your own Discord application (its name = "Playing <name>")
idle_timeout = 900           # clear an idle card after 15 min without activity
github_button = false        # "View on GitHub" button (off: would leak private repos)
hidden_projects = ["secret-client", "~/work/nda"]

[status.working]
details = "Working in {project}"
state = "{tool} · {file} · {tokens} tokens"
```

Templates are `·`-separated segments; a segment whose variable is empty is dropped.
Rotation frames (e.g. the idle stats carousel) are skipped while any variable is empty or zero.
Variables: `{project} {branch} {model} {tool} {file} {tokens} {tokens_in} {tokens_out}
{prompts} {tools} {session_time} {status} {today_time} {today_tokens} {today_prompts}
{total_time} {total_tokens} {total_sessions} {total_prompts} {streak}`.

Send `SIGHUP` to the daemon to reload the config (or restart the service).

> **Defaults borrowed from claude-rpc:** the default `client_id` is claude-rpc's public
> Discord application and the default images are its hosted gifs. Create your own
> application at <https://discord.com/developers/applications> and point `client_id` /
> `[assets]` at it if you prefer not to depend on them.

## How it works

```
Claude Code ──hook──▶ claude-presence hook <Event>  (forwards stdin, exits)
                              │ unix socket / named pipe (owner-only)
                              ▼
                      claude-presenced ──▶ Discord IPC (discord-ipc-N)
                              │
                              └─ reads ~/.claude/projects/**/*.jsonl incrementally
```

- The hook process does no parsing: it pipes Claude Code's JSON to the daemon.
- Tokens come from the transcript (`message.usage`), counted once per `message.id`;
  copies of history in resumed sessions are de-duplicated globally.
- Active time is a per-day bitmap of active minutes, so parallel sessions don't inflate it.
- Stats live in `ledger.json` + `seen.bin` in the data dir
  (`~/.local/share/claude-presence`, `~/Library/Application Support/claude-presence`,
  `%LOCALAPPDATA%\claude-presence`).

Logs: `CLAUDE_PRESENCE_LOG=debug claude-presence daemon` runs it in the foreground.

## Building

```sh
cargo build --release   # target/release/claude-presence{,d}
cargo test
```

## License

Apache-2.0
