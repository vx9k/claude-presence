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

`--no-service` skips this step; `--no-hooks` skips the `settings.json` step; `--no-path` skips adding the install folder to the user `PATH` on Windows ([User PATH](#user-path-windows); accepted and ignored on other platforms).

### Auto-detection (when `--init` is not given)

| Platform | Rule, in order |
|---|---|
| macOS | always `launchd` |
| Windows | always `schtasks` (falls back to `run-key` if registration fails) |
| Linux | 1. `/run/systemd/system` exists: `systemd`. 2. PID 1 is `dinit`, or `dinitctl` is on `PATH` and `rc-service` is not: `dinit`. 3. `/run/openrc` exists or `openrc` is on `PATH`: `openrc`. 4. otherwise `xdg-autostart`. |

On Linux and Windows, an `--init` kind for another OS (for example `launchd` on Linux) fails after the hooks step with `service setup failed: <Kind> is not available on this platform`. On macOS the Linux kinds (`systemd`, `openrc`, `dinit`, `xdg-autostart`) are not rejected; they would write files nothing reads, so don't use them.

### Verification status

From [TODO.md](../TODO.md); update both when this changes. You do not need to verify anything yourself: `claude-presence install` ends with `daemon is running` when it worked.

| Service | Status |
|---|---|
| Windows Task Scheduler | `install`, `status`, hook pipe and Discord activity verified by hand on Windows. Windows CI verifies the pipe DACL, an elevated daemon accepting a non-elevated hook, the hook-side pipe owner check, `ERROR_NO_DATA` handling, the bind retry, the `__shutdown` round trip and cancelling a stuck Discord worker (see [development.md](development.md#ci)). A full local Claude Code session on Windows is not yet verified. |
| Run key | not verified on a real system |
| User `PATH` edit (Windows) | the `PATH` string logic is unit-tested; the registry read/write and the `WM_SETTINGCHANGE` broadcast are only type-checked (tests never touch the real registry) |
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

1. Install copies `claude-presence.exe` and `claude-presenced.exe` from where you ran them to `%LOCALAPPDATA%\Programs\claude-presence` (unless already there) and points hooks and the task at that copy, so you can delete the download. It also adds that folder to your user `PATH` (see [User PATH](#user-path-windows)). Windows will not overwrite a running `.exe`, so before copying:
   - A running daemon is stopped first. With the service step this is the `stopped the running daemon (reinstalling)` line. With `--no-service` the copy step stops it itself (`stopped the running daemon (to replace its binary)`), and after copying starts the new one detached (`restarted the daemon`; on failure `could not restart the daemon: <error>`), so `--no-service` never leaves you without a daemon.
   - Leftover `claude-presence.exe.old` and `claude-presenced.exe.old` from an earlier install are deleted (silently; one that is still in use stays until the next install).
   - If the copy fails because the file is still locked (access denied, sharing or lock violation), it is retried for up to 5 s (20 tries, 250 ms apart). If it is still locked, the old exe is renamed to `<name>.exe.old` (Windows allows renaming a running exe) and the new one copied in its place.
   - If that copy also fails, the old exe is moved back, so hooks and the task never lose their binary. Install then prints `cannot copy binaries: <error>` and exits 1; fix the cause (for example stop the daemon with `taskkill /IM claude-presenced.exe /F`) and re-run.
   - On success it prints `copied binaries to <folder>`.
2. Task XML (UTF-16, written to `%TEMP%\claude-presence-task.xml`, deleted after): logon trigger for your account, `InteractiveToken`, `LeastPrivilege`, `MultipleInstancesPolicy` `IgnoreNew`, runs on battery, no execution time limit (`PT0S`), hidden, `RestartOnFailure` `PT1M` x 999, action = the daemon exe.
3. `schtasks /End /TN claude-presence` (quiet), `schtasks /Create /TN claude-presence /XML <file> /F`. If Create fails, it prints `Task Scheduler registration failed; using the Run registry key instead` and uses the Run key. On success it removes any old Run value, then `schtasks /Run /TN claude-presence` (or starts the daemon detached if that fails).
4. Run key: `reg add HKCU\...\Run /v claude-presence /t REG_SZ /d "<exe>" /f`, then the daemon is started detached.

### User PATH (Windows)

After the hooks step, `install` appends `%LOCALAPPDATA%\Programs\claude-presence` (written out as an absolute path) to your user `PATH`, the `Path` value under `HKCU\Environment`, so new terminals can run `claude-presence` directly. `--no-path` skips this. The machine-wide `PATH` (HKLM) is never touched.

- Entries are compared trimmed, case-insensitively and ignoring a trailing `\` or `/`. If the folder is already there: `<folder> is already in your user PATH`, nothing is written.
- Otherwise it is appended after one `;` (a trailing `;` on the old value doesn't produce an empty entry). The value keeps its type (`REG_SZ` or `REG_EXPAND_SZ`) and `%vars%` in it are kept unexpanded. A missing `Path` value is created as `REG_EXPAND_SZ`. Output: `added <folder> to your user PATH (restart open terminals to pick it up)`.
- If the result would be longer than 2047 characters, nothing is written: `warning: not adding <folder> to your user PATH: it would exceed 2047 characters`.
- Any other failure prints `could not update your user PATH: <error>`; install carries on.
- After a write, `WM_SETTINGCHANGE` ("Environment") is broadcast so Explorer, and terminals started from it afterwards, see the new value. Terminals already open keep their old `PATH` until restarted.

`uninstall` removes only the entries naming that folder (same comparison) and leaves the rest of the value as it was: `removed <folder> from your user PATH (restart open terminals to pick it up)`, or nothing if it wasn't there.

The daemon is a GUI-subsystem binary (no console window). Its log lines are `<unix seconds> <level>: <message>`; the file is truncated at start when larger than 1 MiB.

## Start, stop, logs

| Platform | Status | Stop | Start / restart | Logs |
|---|---|---|---|---|
| systemd | `systemctl --user status claude-presence` (look for `active (running)`) | `systemctl --user stop claude-presence` | `systemctl --user restart claude-presence` | `journalctl --user -u claude-presence -f` |
| OpenRC | `rc-service --user claude-presence status` (look for `started`) | `rc-service --user claude-presence stop` | `rc-service --user claude-presence restart` | none; run in the foreground |
| dinit | `dinitctl status claude-presence` (look for `STARTED`) | `dinitctl stop claude-presence` | `dinitctl restart claude-presence` | none; run in the foreground |
| XDG autostart | `pgrep -a claude-presenced` | `pkill -x claude-presenced` | `nohup claude-presenced >/dev/null 2>&1 &` | none |
| launchd | `launchctl print gui/$(id -u)/io.github.vx9k.claude-presence \| grep state` (look for `state = running`) | `launchctl bootout gui/$(id -u)/io.github.vx9k.claude-presence` | `launchctl kickstart -k gui/$(id -u)/io.github.vx9k.claude-presence` (after bootout: `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.vx9k.claude-presence.plist`) | `tail -f ~/Library/Logs/claude-presence.log` |
| Task Scheduler | `schtasks /Query /TN claude-presence /V /FO LIST` (`Running` or `Ready`); process check: `tasklist /FI "IMAGENAME eq claude-presenced.exe"` | `schtasks /End /TN claude-presence` | `schtasks /Run /TN claude-presence` | `type %LOCALAPPDATA%\claude-presence\daemon.log` |
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
| User `PATH` (Windows) | Settings, "Edit environment variables for your account", `Path`: delete the `%LOCALAPPDATA%\Programs\claude-presence` entry |

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
3. On Windows, copies the binaries (now replaceable because the old daemon exited; see [Windows](#windows) for retry, `.old` and rollback).
4. Keeps an existing `config.toml` (`keeping existing config <path>`); otherwise writes the default.
5. Rewrites the hooks (idempotent: our old entries are removed and all 10 current events are written, so events added by a newer version appear without duplicates; first run saves `settings.json.bak` next to `settings.json` if it did not exist). Output: `wired 10 hook events into <path>`.
6. On Windows, adds the install folder to the user `PATH` unless already there or `--no-path` ([User PATH](#user-path-windows)).
7. Writes the service file and starts the new binary.
8. Waits up to 5 s (20 x 250 ms) for the daemon: `daemon is running`, or ``daemon not reachable yet — check `"<exe>" status` in a moment`` (em dash and backticks as printed).

A daemon that starts while the old one is still shutting down retries binding for up to 5 s (`bind_waiting`) before logging `another claude-presence daemon is already running` and exiting 0.

`--no-service` skips steps 2, 7 and 8. On Windows the binaries are still copied: the running daemon is stopped, the binaries replaced, and the daemon restarted detached (see [Windows](#windows)).

Legacy socket path: older versions listened at `<tmp>/claude-presence-<uid>.sock` (or `<tmp>/claude-presence.sock` when `<tmp>` is not `/tmp`) when no per-user runtime directory exists. Only in that case is the legacy path also asked to stop.

### Upgrading the binary without `install` (Linux, macOS)

If you replace the binaries (`cargo install ...`) but do not re-run `install`, a daemon from before the private socket directory may still be running at the legacy path. A hook first tries today's endpoint; if nothing listens there (not found or connection refused), it falls back to the legacy socket, but only if that path is a socket owned by you and its parent directory is safe (the same parent check as the fallback directory). Hooks therefore keep working until the next `install` or logon replaces the old daemon. Windows has no legacy endpoint. Re-run `claude-presence install` after an upgrade anyway to get new hook events and the new daemon.

## Uninstall

```sh
claude-presence uninstall           # hooks, service and (Windows) the PATH entry
claude-presence uninstall --purge   # also config dir and data dir
```

Order: remove our hooks from `settings.json` (`removed hooks from <path>` or `no hooks to remove`); ask the daemon to stop (`stopped the running daemon`); then remove, if present:

| Platform | Removed |
|---|---|
| Linux | systemd: `systemctl --user disable --now`, delete unit, `daemon-reload`. OpenRC: `rc-service --user ... stop`, `rc-update --user del ... default`, delete script. dinit: `dinitctl disable`, `dinitctl stop`, delete file. XDG: delete `.desktop` file. (All four are checked regardless of which one you used.) |
| macOS | `launchctl bootout gui/<uid>/io.github.vx9k.claude-presence`, delete the plist |
| Windows | `schtasks /End` and `/Delete /F` for the task, delete the Run value; then the install folder's entry in the user `PATH` ([User PATH](#user-path-windows)) |

Each removed item prints `removed <path>` (or `removed scheduled task "claude-presence"` / `removed Run registry key`). Only hook entries whose command contains `claude-presence` and ` hook ` are removed from `settings.json`; everything else stays. `--purge` deletes the config dir and the data dir ([ledger.md](ledger.md) lists them). Binaries are never deleted: `cargo uninstall claude-presence`, and on Windows remove `%LOCALAPPDATA%\Programs\claude-presence`.

To undo by hand, see [Remove manually](#remove-manually).
