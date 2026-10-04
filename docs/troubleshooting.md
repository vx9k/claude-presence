# Troubleshooting

See also: [services.md](services.md) (commands per platform), [configuration.md](configuration.md),
[ipc-and-security.md](ipc-and-security.md), [README](../README.md#troubleshooting).

## First checks

1. `claude-presence status`: first line should be `daemon:   running` (exit code 3 means not running). It also prints the socket, config and stats paths.
2. Is the Discord desktop app open (Stable, PTB, Canary, Flatpak, Snap, or an arRPC bridge such as Vesktop), with Settings, Activity Privacy, "Share your detected activities" on? A browser tab cannot be reached.
3. Is the Claude Code session local? See below.
4. Look at the logs ([services.md](services.md#start-stop-logs)). The default level `info` already shows the key lines; set `CLAUDE_PRESENCE_LOG=debug` for more (levels `error`, `warn`, `info`, `debug`; `trace` is accepted and means `debug`; only when running the daemon in the foreground, or in the service environment).

## Feed a hook by hand

Bypasses Claude Code and proves daemon, socket and Discord work:

```sh
echo '{"session_id":"t","cwd":"/"}' | claude-presence hook UserPromptSubmit
```

A card should appear within a few seconds. With `cwd` set to `/` the project name is empty, so the first line (`Thinking in {project}`) is dropped and only the second line shows, for example `Claude · 1 prompt · 0 tokens`. To see the full card, use a real project directory as `cwd`, for example `{"session_id":"t","cwd":"/path/to/your/project"}` (the card then reads `Thinking in project`). Remove the card with:

```sh
echo '{"session_id":"t"}' | claude-presence hook SessionEnd
```

The hook command prints nothing and always exits 0, even when the daemon is down, and `status` does not show whether a hook arrived. Look at the card in Discord, or at the `first hook received (<event>)` log line (logged once per daemon run).

## Symptom, cause, fix

| Symptom | Likely cause | Fix |
|---|---|---|
| `status` says `daemon:   not running` | Service not started, crashed, or not installed | Check the service ([services.md](services.md)); re-run `claude-presence install`; run `claude-presence daemon` in a terminal to see errors |
| Log: `info: another claude-presence daemon is already running` (daemon exits 0) | A second daemon started while one holds the socket or pipe (for example the service plus a foreground run). On Windows a pipe name pre-created by another user also looks like this; your hooks are not sent to it ([ipc-and-security.md](ipc-and-security.md#windows-pipe-squatting)). | Stop the existing one first (service manager), then start yours. Reinstall already waits up to 5 s for the old one. If no daemon of yours is running on Windows, check Task Manager for another user's `claude-presenced.exe`. |
| Log: `error: cannot listen on <path>: <path>: owned by another user` (or `accessible by group/other`, `not a directory (or a symlink)`, `not rwx for its owner`) | Fallback socket directory `<tmp>/claude-presence-<uid>` exists but fails the checks. Only used when there is no `$XDG_RUNTIME_DIR` and no `/run/user/<uid>`. Daemon exits 1; hooks send nothing. | If it is yours: `chmod 700` it or `rm -r` it. If it belongs to someone else, set `XDG_RUNTIME_DIR` or `TMPDIR` to a directory you own for the daemon and for Claude Code. |
| Log: `cannot listen on ...: parent writable by others and not sticky` (or `parent owned by another user`) | The directory containing it (normally `/tmp`) is not sticky and is world/group-writable, or owned by a non-root other user | Fix the parent permissions, or set `TMPDIR` to a safe directory |
| Log: `error: Discord refused the handshake: ... (check client_id)` | Invalid `client_id` | Fix `client_id` in `config.toml` and reload; retry happens every 300 s otherwise |
| No `connected to Discord` line | Discord not running, running only in a browser, or its socket is not found. Not-reachable is only logged at `debug`; retries back off 2 s up to 60 s. | Start the desktop app; check the debug log line `Discord not reachable`. Flatpak and Snap locations are searched. |
| Log: `info: connected to Discord (arRPC bridge)` then the card vanishes occasionally | Bridges drop the activity when their renderer reloads | Expected; the card is re-sent every 20 s. The bridge must be running before the card can appear. |
| Log: `warn: Discord rejected the activity: <message>` | Discord refused the payload (for example a bad button URL or an `activity_type` it does not accept) | Fix the offending `config.toml` value; the card is not retried until the activity changes |
| Log: `info: Discord connection lost: ...` | Discord restarted | Nothing; it reconnects and resends |
| Card never appears; no `first hook received (<event>)` line | Hooks are not reaching the daemon | Local session? Hooks wired? Daemon running? See next rows |
| Cloud session (claude.ai/code, or a cloud environment in the desktop app) shows nothing | The hooks run in a remote container, not on your machine. There is no fallback detection (decided in TODO.md). | Use a local session: the `claude` CLI, or a session in the desktop app's Code tab that uses your local machine. Plain chat in the Claude app has no hooks. |
| Hooks missing | `install` was run with `--no-hooks`, or Claude Code was open during install | Check `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`) for commands ending in `hook SessionStart`, `hook PreToolUse`, and so on (10 events, including `hook PostToolUseFailure`; `install` prints `wired 10 hook events into <path>`); run `claude-presence install --no-service`; restart Claude Code |
| Hooks stopped working after moving or deleting the binary | Hook commands hold the absolute path of the executable used at install time | Re-run `claude-presence install` from the new location |
| `install` fails with `failed to update Claude Code settings: settings.json: ...` | `settings.json` is not valid JSON or not an object, or `hooks`/`hooks.<Event>` has an unexpected type | Fix the file by hand (the first install keeps `settings.json.bak`), then re-run |
| `install` ends with `daemon not reachable yet` | Service manager is slow or the service failed | Wait a few seconds and run `status`; if still down, see the service logs. Note the printed commands for OpenRC, dinit, systemd or launchd when it says `wrote <file> — enable it with ...` (launchd: `wrote <file> — load it with ...`) |
| `install` prints `note: installing from a debug build directory` | Running `target/debug/claude-presence` | `cargo install --path .` first, so hooks do not point into `target/` |
| Card disappears after a while | Expected: `idle_timeout` (default 900 s) with no activity and no transcript writes. Sessions that are Thinking, Working or Compacting (mid-task) are kept at least one hour; a session waiting on you (Notification) uses the plain `idle_timeout`. A closed Claude Code clears it immediately via `SessionEnd`. | `idle_timeout = 0` to keep it until Claude Code exits |
| Card shows the wrong session with several open | The card sticks to the shown session while it is as active as any other; it switches when it goes idle and another is working | Expected ([architecture.md](architecture.md#which-session-is-shown-sticky-choice)) |
| Config edit has no effect | Not reloaded, or file failed to parse (`error: <path>: ...; using defaults`) | Send SIGHUP or restart ([configuration.md](configuration.md#applying-changes)); fix the syntax error; a wrong-typed value discards the whole file |
| Warning `idle_timeout = N is below the minimum; using 60` | Value clamped | Use `0` or `60..=604800` |
| Lifetime stats are zero or low | `scan_history = false`, transcripts deleted before first run, or the ledger was rebuilt (`warn: <path> unreadable; rebuilding stats`) | See [ledger.md](ledger.md). Look for `info: scanned N transcripts` in the log |
| Warning `<path>/seen.bin: ...; previously counted ids are forgotten` | `seen.bin` deleted or unreadable while `ledger.json` survived | Totals stay; history a `--resume` copies into a new transcript may count again ([ledger.md](ledger.md#save-order-and-crash-consistency)). Deleting both files rebuilds cleanly |
| `error: saving stats: ...` | Data dir not writable or disk full | Fix permissions; the save retries every 60 s while dirty |
| `{project}` shows an unexpected name | It is the git root's directory name, else the cwd's name | Or the project matches `hidden_projects` |
| Windows: hooks do nothing | Daemon not running, pipe busy, or the pipe is owned by someone other than you, Administrators or SYSTEM (hooks then send nothing) | `status`; read `%LOCALAPPDATA%\claude-presence\daemon.log` |
| Windows log line `error: pipe: ...; no longer accepting hook events` | Could not create a new pipe instance | Restart the task: `schtasks /End /TN claude-presence` then `/Run` |
| `warn: the running daemon has not stopped after 2 s (still busy, or a version without __shutdown)` | Old daemon is slow or predates `__shutdown` | Stop it with your service manager or `pkill -x claude-presenced` / `taskkill /IM claude-presenced.exe /F`, then re-run install |
| `warn: Discord worker did not stop in time; leaving it` | Discord client frozen during shutdown or a `client_id` reload | Harmless at exit. On Windows the daemon first cancels the stuck pipe I/O, so this should be rare there. |
| Windows: `install` prints `cannot copy binaries: ...` | A `.exe` stayed locked after the daemon was stopped, retried for 5 s and moved aside to `.old` ([services.md](services.md#windows)); the old binaries are restored | Stop the daemon (`taskkill /IM claude-presenced.exe /F`), close anything using the exe, re-run `claude-presence install` |

## Reading the log

| Line (level) | Meaning |
|---|---|
| `listening on <path>` (info) | Daemon bound its socket or pipe |
| `scanned N transcripts (X changed, Y pruned) in T` (info) | A history scan finished |
| `first hook received (<event>)` (info) | First hook since the daemon started; proves hooks work |
| `connected to Discord` (info) | Handshake succeeded |
| `configuration reloaded` (info) | SIGHUP processed |
| `session <id> expired` (info) | Idle session dropped |
| `shutdown requested` / `shutting down` (info) | Stop via `__shutdown` / final save |
| `presence → {...}` (debug) | The activity JSON handed to the Discord worker, or `(cleared)` |
| `hook <event> session=<id>` (debug) | Every hook |
| `unparseable <event> payload` (debug) | JSON the daemon could not parse |

Windows file lines look like `1760000000 info: listening on ...` (unix seconds first).
