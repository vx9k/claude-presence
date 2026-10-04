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

Requires the Discord **desktop** app (or an arRPC bridge such as Vesktop). Discord in a
web browser cannot be reached.

```sh
cargo install --git https://github.com/vx9k/claude-presence
claude-presence install
```

This installs two programs: `claude-presence` (the command you type, also called by the
hooks) and `claude-presenced` (the background daemon). `install` then:

1. writes a default config (if none exists),
2. adds hooks for 9 events to `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`).
   A one-time `settings.json.bak` backup is kept; your other settings and hooks are untouched,
3. registers and starts `claude-presenced` as a per-user background service
   (see [Background service](#background-service)).

It ends with `daemon is running` when everything worked. Then just use Claude Code.
Check on it any time with `claude-presence status`.

Re-running `install` is safe (e.g. after upgrading): it stops the old service, rewrites
the hooks and service file, and starts the new binary.

### Commands

| Command | What it does |
|---|---|
| `claude-presence install [--init <kind>] [--no-service] [--no-hooks]` | Set everything up (see above) |
| `claude-presence uninstall [--purge]` | Remove hooks and service; `--purge` also deletes config and lifetime stats |
| `claude-presence status` | Daemon state, file locations, today/lifetime stats. Exit code 3 if the daemon is not running |
| `claude-presence daemon` | Run the daemon in the foreground (for debugging) |
| `claude-presence config` | Print the config file path |
| `claude-presence hook <Event>` | Used by Claude Code; you never run this yourself |
| `claude-presence --version` / `help` | Version / usage |

`--init` picks the service manager instead of auto-detecting it:
`systemd`, `openrc`, `dinit`, `xdg-autostart`, `launchd`, `schtasks`, `run-key` or `none`.
`--no-service` skips step 3, `--no-hooks` skips step 2.

## Background service

`install` picks one of these automatically. The detected one is printed during install.

| Platform | What `install` creates | Auto-detected when |
|---|---|---|
| Linux, systemd | `~/.config/systemd/user/claude-presence.service` | `/run/systemd/system` exists |
| Linux, dinit | `~/.config/dinit.d/claude-presence` | PID 1 is dinit, or `dinitctl` is installed without `rc-service` |
| Linux, OpenRC ≥ 0.60 | `~/.config/rc/init.d/claude-presence` | `/run/openrc` exists or `openrc` is installed |
| Linux, anything else | `~/.config/autostart/claude-presence.desktop` | fallback |
| macOS | `~/Library/LaunchAgents/io.github.vx9k.claude-presence.plist` | always |
| Windows | scheduled task `claude-presence` (fallback: HKCU `Run` key) | always |

On Linux, `~/.config` means `$XDG_CONFIG_HOME` if you set it.

In the commands below, the service is always called `claude-presence`; the daemon
process is `claude-presenced`. The easiest check on every platform is
`claude-presence status` → `daemon:   running`.

### systemd (most Linux distributions)

`install` writes the unit, then runs `systemctl --user daemon-reload`,
`systemctl --user enable claude-presence.service` and
`systemctl --user restart claude-presence.service`. It starts when you log in and
restarts 5 s after a crash.

```sh
systemctl --user status claude-presence          # is it running? look for "active (running)"
journalctl --user -u claude-presence -f          # live logs (Ctrl+C to quit)
systemctl --user restart claude-presence         # restart
systemctl --user kill -s HUP claude-presence     # reload config.toml without restarting
systemctl --user stop claude-presence            # stop until next login
systemctl --user disable --now claude-presence   # stop and don't start at login
```

Remove manually:

```sh
systemctl --user disable --now claude-presence
rm ~/.config/systemd/user/claude-presence.service
systemctl --user daemon-reload
```

### OpenRC (Gentoo, Alpine, Artix, …)

Needs **OpenRC 0.60 or newer** with user services set up (your user runlevel must be
started at login). `install` writes an `openrc-run` script that runs the daemon under
`supervise-daemon` (respawns after 5 s), makes it executable, then runs
`rc-update --user add claude-presence default` and
`rc-service --user claude-presence restart` (or `start`). If that fails, install prints
the commands to run yourself.

```sh
rc-service --user claude-presence status         # is it running? look for "started"
rc-service --user claude-presence restart        # restart
rc-service --user claude-presence stop           # stop
rc-update --user del claude-presence default     # don't start at login
```

Logs: the script does not redirect output to a file; use the
[foreground debug run](#troubleshooting).

Remove manually:

```sh
rc-service --user claude-presence stop
rc-update --user del claude-presence default
rm ~/.config/rc/init.d/claude-presence
```

### dinit (Chimera, Artix-dinit, …)

Needs a **user dinit instance** running (a `dinit` started as your user, usually from
your session). `install` writes a `type = process` service that restarts after 5 s,
then runs `dinitctl enable claude-presence` (or `dinitctl start claude-presence`).

```sh
dinitctl status claude-presence                  # is it running? look for "STARTED"
dinitctl restart claude-presence                 # restart
dinitctl stop claude-presence                    # stop
dinitctl disable claude-presence                 # don't start with your user instance
```

Logs: the service file sets no log file; use the [foreground debug run](#troubleshooting).

Remove manually:

```sh
dinitctl disable claude-presence
dinitctl stop claude-presence
rm ~/.config/dinit.d/claude-presence
```

### XDG autostart (any Linux desktop without the above)

`install` writes a hidden `.desktop` entry that your desktop environment (GNOME, KDE,
Xfce, …) launches at login, and starts the daemon right away. There is no supervisor:
if it crashes it stays down until next login, and output is discarded.

```sh
pgrep -a claude-presenced                        # is it running?
pkill -x claude-presenced                        # stop
pkill -HUP -x claude-presenced                   # reload config.toml
nohup claude-presenced >/dev/null 2>&1 &         # start again (if it's on your PATH)
```

Remove manually: `rm ~/.config/autostart/claude-presence.desktop`, then
`pkill -x claude-presenced`. (`uninstall` deletes the file but does **not** stop the
running process; it ends at logout or with `pkill`.)

### macOS (launchd)

`install` writes a LaunchAgent and loads it with
`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.vx9k.claude-presence.plist`.
It starts at login (`RunAtLoad`) and is restarted if it crashes (`KeepAlive` →
`SuccessfulExit = false`, at most every 10 s). Errors go to
`~/Library/Logs/claude-presence.log`.

```sh
launchctl print gui/$(id -u)/io.github.vx9k.claude-presence | grep state   # "state = running"
tail -f ~/Library/Logs/claude-presence.log                                 # logs (or open it in Console.app)
launchctl kickstart -k gui/$(id -u)/io.github.vx9k.claude-presence         # restart
launchctl kill SIGHUP gui/$(id -u)/io.github.vx9k.claude-presence          # reload config.toml
launchctl bootout gui/$(id -u)/io.github.vx9k.claude-presence              # stop and unload until next login
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.vx9k.claude-presence.plist  # load again
```

Remove manually:

```sh
launchctl bootout gui/$(id -u)/io.github.vx9k.claude-presence
rm ~/Library/LaunchAgents/io.github.vx9k.claude-presence.plist
```

### Windows (Task Scheduler)

`install` first copies `claude-presence.exe` and `claude-presenced.exe` to
`%LOCALAPPDATA%\Programs\claude-presence` (hooks and the task point there, so you can
delete the download). It then registers a hidden scheduled task named
**`claude-presence`** that runs `claude-presenced.exe` at your logon, with no time limit,
on battery too, and restarts it every minute on failure (up to 999 times); then it starts
the task. The daemon has no window. Logs go to `%LOCALAPPDATA%\claude-presence\daemon.log`
(truncated when it passes 1 MB).

You can also see the task in the **Task Scheduler** app (Task Scheduler Library →
`claude-presence`). From `cmd` or PowerShell:

```bat
schtasks /Query /TN claude-presence /V /FO LIST          :: task status ("Running" / "Ready")
tasklist /FI "IMAGENAME eq claude-presenced.exe"         :: is the process running?
type %LOCALAPPDATA%\claude-presence\daemon.log           :: logs (cmd; in PowerShell: Get-Content $env:LOCALAPPDATA\claude-presence\daemon.log -Wait)
schtasks /End /TN claude-presence                        :: stop
schtasks /Run /TN claude-presence                        :: start (also use after /End to restart, e.g. after editing config)
schtasks /Change /TN claude-presence /DISABLE            :: don't start at logon (/ENABLE to undo)
```

Remove manually:

```bat
schtasks /End /TN claude-presence
schtasks /Delete /TN claude-presence /F
```

#### Fallback: HKCU `Run` key

If the task cannot be registered (or you chose `--init run-key`), `install` adds a value
`claude-presence` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` pointing at
`claude-presenced.exe`, and starts the daemon. Windows launches it at logon; there is no
restart on crash. It shows up in Task Manager → **Startup apps**, where you can disable it.

```bat
reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v claude-presence   :: is it registered?
tasklist /FI "IMAGENAME eq claude-presenced.exe"                                  :: is it running?
taskkill /IM claude-presenced.exe /F                                              :: stop
start "" "%LOCALAPPDATA%\Programs\claude-presence\claude-presenced.exe"           :: start
```

Remove manually:

```bat
reg delete HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v claude-presence /f
taskkill /IM claude-presenced.exe /F
```

(`uninstall` removes the value but does not stop an already running daemon; it ends at
sign-out or with `taskkill`.)

## Uninstall

```sh
claude-presence uninstall          # remove hooks + service (all flavors listed above)
claude-presence uninstall --purge  # also delete config and lifetime stats
```

`uninstall` doesn't delete the binaries: `cargo uninstall claude-presence`, and on Windows
also delete `%LOCALAPPDATA%\Programs\claude-presence`.

## Configuration

`claude-presence config` prints the path:

- Linux: `~/.config/claude-presence/config.toml` (`$XDG_CONFIG_HOME` is honored)
- macOS: `~/Library/Application Support/claude-presence/config.toml`
- Windows: `%APPDATA%\claude-presence\config.toml`

Every key is optional; the generated file documents them all. Highlights:

```toml
client_id = "..."            # your own Discord application (its name = "Playing <name>")
idle_timeout = 900           # clear an idle card after 15 min without activity (0 = never)
github_button = false        # "View on GitHub" button (off: would leak private repos)
hidden_projects = ["secret-client", "~/work/nda"]

[status.working]
details = "Working in {project}"
state = "{tool} · {file} · {tokens} tokens"
```

Other keys: `activity_type`, `status_display`, `show_elapsed`, `rotation_interval`,
`hidden_project_name`, `scan_history`, `rescan_interval`, `buttons`, `[assets]`, and
`[status.thinking|compacting|notification|idle]` (each with `details`, `state`,
optional `rotation`).

Templates are `·`-separated segments; a segment whose variable is empty is dropped.
Rotation frames (e.g. the idle stats carousel) are skipped while any variable is empty or zero.
Variables: `{project} {branch} {model} {tool} {file} {tokens} {tokens_in} {tokens_out}
{prompts} {tools} {session_time} {status} {today_time} {today_tokens} {today_prompts}
{total_time} {total_tokens} {total_sessions} {total_prompts} {streak}`.

**Applying changes:** on Linux/macOS send `SIGHUP` to the daemon (commands per service
manager above) or restart the service. On Windows, restart it (`schtasks /End` then
`/Run`). A config file with a syntax error is reported in the log and the defaults are
used.

> **Defaults borrowed from claude-rpc:** the default `client_id` is claude-rpc's public
> Discord application and the default images are its hosted gifs. Create your own
> application at <https://discord.com/developers/applications> and point `client_id` /
> `[assets]` at it if you prefer not to depend on them.

## Troubleshooting

1. **Is Discord running?** The *desktop* app (Stable, PTB, Canary; Flatpak and Snap
   are found too) must be open, and Settings → Activity Privacy → *Share your detected
   activities* must be on.
2. **Is the daemon running?** `claude-presence status` should say `daemon:   running`.
   If not, check its service (see [Background service](#background-service)) or re-run
   `claude-presence install`.
3. **Is the session local?** Only Claude Code running on *this* computer can be shown:
   the `claude` CLI, or a session in the desktop app's Code tab that uses your local
   machine. Cloud sessions (claude.ai/code, or a cloud environment picked in the desktop
   app) run their hooks on a remote container, so nothing reaches the daemon. Plain chat
   in the Claude app has no hooks at all. Quick test that bypasses Claude Code:
   `echo '{"session_id":"test","cwd":"/"}' | claude-presence hook UserPromptSubmit`
   should make a card appear.
4. **Are the hooks installed?** `~/.claude/settings.json` should contain commands ending
   in `hook SessionStart`, `hook PreToolUse`, etc. Restart Claude Code after installing.
5. **Watch it live.** Stop the service first (only one daemon can run; a second one just
   says *another claude-presence daemon is already running* and exits), then:
   ```sh
   CLAUDE_PRESENCE_LOG=debug claude-presence daemon
   ```
   On Windows (PowerShell): `$env:CLAUDE_PRESENCE_LOG="debug"; claude-presence daemon`.
   You should see `connected to Discord` once a Claude Code session is active
   (`Discord refused the handshake … (check client_id)` means a bad `client_id`).
   Press Ctrl+C to quit, then start the service again.
6. **Log files.** Windows: `%LOCALAPPDATA%\claude-presence\daemon.log`.
   macOS: `~/Library/Logs/claude-presence.log`. systemd: `journalctl --user -u claude-presence`.
   Levels via `CLAUDE_PRESENCE_LOG`: `error`, `warn`, `info` (default), `debug`.
7. **Vesktop / arRPC.** Bridges are detected (log says `connected to Discord (arRPC bridge)`);
   the card is re-sent more often because bridges drop it when they reload.
   The bridge must be running before the card can appear.
8. **Card disappeared?** That's expected after `idle_timeout` seconds (default 900) with
   no activity, or immediately when Claude Code exits. A session that is mid-task is kept
   for at least an hour. Set `idle_timeout = 0` to keep it until Claude Code exits.

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

## Building

```sh
cargo build --release   # target/release/claude-presence{,d}
cargo test
```

## Contributing

The repo ships four Claude Code sub-agents in `.claude/agents/` (workflow in
[CLAUDE.md](CLAUDE.md); general agent guide in [AGENTS.md](AGENTS.md)):

- **advisor** (Sonnet) — read-only design second opinion before non-trivial decisions.
- **auditor** — read-only review of Rust changes (correctness, `unsafe`/FFI, IPC
  permissions, cross-platform pitfalls); reports findings, never edits.
- **developer** — implements fixes (e.g. auditor findings) with tests; must pass
  `cargo fmt --check`, `clippy` for Linux/Windows/macOS targets, and `cargo test`.
- **docs-writer** — maintains this README, checking every documented command, path and
  option against the source.

## License

Apache-2.0
