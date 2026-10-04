# Services

`claude-presence install` registers `claude-presenced` as a per-user background service. Nothing needs root. See also: [README](../README.md#background-service) (short per-platform summary), [troubleshooting.md](troubleshooting.md), [ipc-and-security.md](ipc-and-security.md), [architecture.md](architecture.md).

The service name is `claude-presence` everywhere; the process is `claude-presenced`. On every platform, `claude-presence status` printing `daemon:   running` is the quickest check (exit code 3 when not running).

## What install creates

`~/.config` below means `$XDG_CONFIG_HOME` if set (Linux). The service file points at `claude-presenced` next to the `claude-presence` you ran (on Windows, the copy in `%LOCALAPPDATA%\Programs\claude-presence`).

| `--init` value | Platform | Created | Start at login | Restart on crash | Logs |
|---|---|---|---|---|---|
| `systemd` | Linux | `~/.config/systemd/user/claude-presence.service` | `WantedBy=default.target` | `Restart=on-failure`, 5 s | `journalctl --user -u claude-presence` |
| `openrc` | Linux | `~/.config/rc/init.d/claude-presence` (mode 755) | `rc-update --user add ... default` | `supervise-daemon`, 5 s, unlimited | none (stderr not redirected) |
| `dinit` | Linux | `~/.config/dinit.d/claude-presence` | `dinitctl enable` | `restart = true`, 5 s | none (no log file set) |
| `xdg-autostart` (aliases `xdg`, `autostart`) | Linux | `~/.config/autostart/claude-presence.desktop` | desktop environment runs it | no supervisor | discarded |
| `launchd` | macOS | `~/Library/LaunchAgents/io.github.vx9k.claude-presence.plist` | `RunAtLoad` | `KeepAlive` on non-zero exit, `ThrottleInterval` 10 s | `~/Library/Logs/claude-presence.log` (stderr) |
| `schtasks` (alias `task-scheduler`) | Windows | scheduled task `claude-presence` | logon trigger | `RestartOnFailure` every 1 min, up to 999 | `%LOCALAPPDATA%\claude-presence\daemon.log` |
| `run-key` (alias `registry`) | Windows | value `claude-presence` in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` | Windows runs it at logon | none | `%LOCALAPPDATA%\claude-presence\daemon.log` |
| `none` | any | nothing | - | - | - |

`--no-service` skips this step; `--no-hooks` skips the `settings.json` step.

### Auto-detection (when `--init` is not given)

| Platform | Rule, in order |
|---|---|
| macOS | always `launchd` |
| Windows | always `schtasks` (falls back to `run-key` if registration fails) |
| Linux | 1. `/run/systemd/system` exists: `systemd`. 2. PID 1 is `dinit`, or `dinitctl` is on `PATH` and `rc-service` is not: `dinit`. 3. `/run/openrc` exists or `openrc` is on `PATH`: `openrc`. 4. otherwise `xdg-autostart`. |

An `--init` for another OS (for example `launchd` on Linux) fails with `... is not available on this platform`.

### Verification status

From [TODO.md](../TODO.md); update both when this changes.

| Service | Status |
|---|---|
| Windows Task Scheduler | `install`, `status`, hook pipe and Discord activity verified by hand on Windows. A full local Claude Code session on Windows is not yet verified. Bind retry and `__shutdown` reinstall on Windows are type-checked only. |
| Run key | not verified on a real system |
| launchd | not verified on a real system (CI compiles and tests it; macOS clippy target type-checks) |
| OpenRC | not verified on a real system |
| dinit | not verified on a real system |
| systemd, XDG autostart | TODO.md does not list them as unverified |

## Generated files

### systemd

```ini
[Unit]
Description=Discord Rich Presence for Claude Code
Documentation=https://github.com/vx9k/claude-presence

[Service]
Type=simple
ExecStart=<claude-presenced path>
Restart=on-failure
RestartSec=5
Nice=10
IOSchedulingClass=idle
NoNewPrivileges=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
SystemCallArchitectures=native

[Install]
WantedBy=default.target
```

Paths containing a space, `"` or `\` are quoted in `ExecStart`. Install runs `systemctl --user daemon-reload`, `systemctl --user enable claude-presence.service`, `systemctl --user restart claude-presence.service` (restart, not start, so a new binary is picked up).

### OpenRC (needs OpenRC 0.60 or newer with user services)

```sh
#!/sbin/openrc-run
description="Discord Rich Presence for Claude Code"
supervisor=supervise-daemon
command="<claude-presenced path>"
respawn_delay=5
respawn_max=0
```

Install runs `rc-update --user add claude-presence default`, then `rc-service --user claude-presence restart` (or `start` if restart fails). On failure it prints the commands to run by hand.

### dinit (needs a running user dinit instance)

```
type = process
command = <claude-presenced path>
restart = true
restart-delay = 5
smooth-recovery = true
```

Install runs `dinitctl stop claude-presence` (ignoring failure), then `dinitctl enable claude-presence`, or `dinitctl start claude-presence` if enable fails.

### XDG autostart

```ini
[Desktop Entry]
Type=Application
Name=claude-presence
Comment=Discord Rich Presence for Claude Code
Exec="<claude-presenced path>"
Terminal=false
NoDisplay=true
X-GNOME-Autostart-enabled=true
```

Install also starts the daemon right away, detached (own process group, stdio to null).

### launchd

Plist keys: `Label` = `io.github.vx9k.claude-presence`, `ProgramArguments` = the daemon, `RunAtLoad` true, `KeepAlive` = `{SuccessfulExit: false}`, `ThrottleInterval` 10, `ProcessType` `Background`, `LowPriorityIO` true, `Nice` 10, `StandardErrorPath` `~/Library/Logs/claude-presence.log`. Install runs `launchctl bootout gui/<uid>/io.github.vx9k.claude-presence` (ignoring failure), then `launchctl bootstrap gui/<uid> <plist>`.

### Windows

1. Install copies `claude-presence.exe` and `claude-presenced.exe` from where you ran them to `%LOCALAPPDATA%\Programs\claude-presence` (unless already there) and points hooks and the task at that copy. Only add that folder to `PATH` yourself if you want to type `claude-presence` anywhere (a PATH step is listed as nice-to-have in TODO.md).
2. Task XML (UTF-16, written to `%TEMP%\claude-presence-task.xml`, deleted after): logon trigger for your account, `InteractiveToken`, `LeastPrivilege`, `MultipleInstancesPolicy` `IgnoreNew`, runs on battery, no execution time limit (`PT0S`), hidden, `RestartOnFailure` `PT1M` x 999, action = the daemon exe.
3. `schtasks /End /TN claude-presence` (quiet), `schtasks /Create /TN claude-presence /XML <file> /F`. If Create fails, it prints `Task Scheduler registration failed; using the Run registry key instead` and uses the Run key. On success it removes any old Run value, then `schtasks /Run /TN claude-presence` (or starts the daemon detached if that fails).
4. Run key: `reg add HKCU\...\Run /v claude-presence /t REG_SZ /d "<exe>" /f`, then the daemon is started detached.

The daemon is a GUI-subsystem binary (no console window). Its log lines are `<unix seconds> <level>: <message>`; the file is truncated at start when larger than 1 MiB.

## Start, stop, logs

| Platform | Status | Stop | Start / restart | Logs |
|---|---|---|---|---|
| systemd | `systemctl --user status claude-presence` | `systemctl --user stop claude-presence` | `systemctl --user restart claude-presence` | `journalctl --user -u claude-presence -f` |
| OpenRC | `rc-service --user claude-presence status` | `rc-service --user claude-presence stop` | `rc-service --user claude-presence restart` | none; run in the foreground |
| dinit | `dinitctl status claude-presence` | `dinitctl stop claude-presence` | `dinitctl restart claude-presence` | none; run in the foreground |
| XDG autostart | `pgrep -a claude-presenced` | `pkill -x claude-presenced` | `nohup claude-presenced >/dev/null 2>&1 &` | none |
| launchd | `launchctl print gui/$(id -u)/io.github.vx9k.claude-presence \| grep state` | `launchctl bootout gui/$(id -u)/io.github.vx9k.claude-presence` | `launchctl kickstart -k gui/$(id -u)/io.github.vx9k.claude-presence` (after bootout: `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.vx9k.claude-presence.plist`) | `tail -f ~/Library/Logs/claude-presence.log` |
| Task Scheduler | `schtasks /Query /TN claude-presence /V /FO LIST` | `schtasks /End /TN claude-presence` | `schtasks /Run /TN claude-presence` | `type %LOCALAPPDATA%\claude-presence\daemon.log` |
| Run key | `tasklist /FI "IMAGENAME eq claude-presenced.exe"` | `taskkill /IM claude-presenced.exe /F` | `start "" "%LOCALAPPDATA%\Programs\claude-presence\claude-presenced.exe"` | same `daemon.log` |

There is no stop-only command in `claude-presence`; use the table. A service that respawns (OpenRC, dinit) must be stopped through its manager, not killed.

Foreground debugging: stop the service, then `CLAUDE_PRESENCE_LOG=debug claude-presence daemon` (Windows PowerShell: `$env:CLAUDE_PRESENCE_LOG="debug"; claude-presence daemon`). Levels: `error`, `warn`, `info` (default), `debug` (also `trace`). Logs go to stderr.

## Remove manually

Stop the daemon first (the manual steps below do not ask it to stop; `claude-presence uninstall` does).

| Platform | Commands |
|---|---|
| systemd | `systemctl --user disable --now claude-presence`; `rm ~/.config/systemd/user/claude-presence.service`; `systemctl --user daemon-reload` |
| OpenRC | `rc-service --user claude-presence stop`; `rc-update --user del claude-presence default`; `rm ~/.config/rc/init.d/claude-presence` |
| dinit | `dinitctl disable claude-presence`; `dinitctl stop claude-presence`; `rm ~/.config/dinit.d/claude-presence` |
| XDG autostart | `rm ~/.config/autostart/claude-presence.desktop`; `pkill -x claude-presenced` |
| launchd | `launchctl bootout gui/$(id -u)/io.github.vx9k.claude-presence`; `rm ~/Library/LaunchAgents/io.github.vx9k.claude-presence.plist` |
| Task Scheduler | `schtasks /End /TN claude-presence`; `schtasks /Delete /TN claude-presence /F` |
| Run key | `reg delete HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v claude-presence /f`; `taskkill /IM claude-presenced.exe /F` |

## Extra platform notes

| Platform | Note |
|---|---|
| systemd | Disable at login but keep the unit: `systemctl --user disable --now claude-presence`. Reload config: `systemctl --user kill -s HUP claude-presence`. |
| OpenRC | Don't start at login: `rc-update --user del claude-presence default`. Your user runlevel must be started at login. |
| dinit | Don't start with your user instance: `dinitctl disable claude-presence`. |
| XDG autostart | Reload config: `pkill -HUP -x claude-presenced`. If it crashes it stays down until next login. |
| launchd | Reload config: `launchctl kill SIGHUP gui/$(id -u)/io.github.vx9k.claude-presence`. Logs can also be opened in Console.app. |
| Task Scheduler | Also visible in the Task Scheduler app (Task Scheduler Library, `claude-presence`). Task status is `Running` or `Ready`. Don't start at logon: `schtasks /Change /TN claude-presence /DISABLE` (`/ENABLE` to undo). PowerShell log follow: `Get-Content $env:LOCALAPPDATA\claude-presence\daemon.log -Wait`. |
| Run key | Appears in Task Manager, Startup apps, where it can be disabled. Check registration: `reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v claude-presence`. |

## Reinstall and upgrade

Re-running `claude-presence install` is safe. For the service step it:

1. Prints `Installing claude-presence <version>`.
2. Calls the same cleanup as `uninstall` (`uninstall_service`): sends `__shutdown` to the running daemon at the current socket or pipe, and also at the legacy socket path if different and present. It then polls for up to 2 s (every 50 ms). Output: `stopped the running daemon (reinstalling)`. If it has not stopped: `warn: the running daemon has not stopped after 2 s (still busy, or a version without __shutdown)`. Then it stops and removes every service flavor it finds (see Uninstall).
3. On Windows, copies the binaries (now replaceable because the old daemon exited).
4. Keeps an existing `config.toml` (`keeping existing config <path>`); otherwise writes the default.
5. Rewrites the hooks (idempotent; first run saves `settings.json.bak` next to `settings.json` if it did not exist).
6. Writes the service file and starts the new binary.
7. Waits up to 5 s (20 x 250 ms) for the daemon: `daemon is running`, or `daemon not reachable yet - check "<exe>" status in a moment`.

A daemon that starts while the old one is still shutting down retries binding for up to 5 s (`bind_waiting`) before logging `another claude-presence daemon is already running` and exiting 0.

`--no-service` skips steps 2, 6 and 7 (on Windows the binaries are still copied, which fails if the old daemon is still running).

Legacy socket path: older versions listened at `<tmp>/claude-presence-<uid>.sock` (or `<tmp>/claude-presence.sock` when `<tmp>` is not `/tmp`) when no per-user runtime directory exists. Only in that case is the legacy path also asked to stop.

## Uninstall

```sh
claude-presence uninstall           # hooks and service
claude-presence uninstall --purge   # also config dir and data dir
```

Order: remove our hooks from `settings.json` (`removed hooks from <path>` or `no hooks to remove`); ask the daemon to stop (`stopped the running daemon`); then remove, if present:

| Platform | Removed |
|---|---|
| Linux | systemd: `systemctl --user disable --now`, delete unit, `daemon-reload`. OpenRC: `rc-service --user ... stop`, `rc-update --user del ... default`, delete script. dinit: `dinitctl disable`, `dinitctl stop`, delete file. XDG: delete `.desktop` file. (All four are checked regardless of which one you used.) |
| macOS | `launchctl bootout gui/<uid>/io.github.vx9k.claude-presence`, delete the plist |
| Windows | `schtasks /End` and `/Delete /F` for the task, delete the Run value |

Each removed item prints `removed <path>` (or `removed scheduled task "claude-presence"` / `removed Run registry key`). Only hook entries whose command contains `claude-presence` and ` hook ` are removed from `settings.json`; everything else stays. `--purge` deletes the config dir and the data dir ([ledger.md](ledger.md) lists them). Binaries are never deleted: `cargo uninstall claude-presence`, and on Windows remove `%LOCALAPPDATA%\Programs\claude-presence`.

To undo by hand, see [Remove manually](#remove-manually).
