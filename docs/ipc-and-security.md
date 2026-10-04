# IPC and security

See also: [architecture.md](architecture.md), [configuration.md](configuration.md),
[troubleshooting.md](troubleshooting.md), [README](../README.md).

## Wire format

One connection per message: connect, write, close.

```
<EventName>\n<raw JSON from Claude Code>
```

- `<EventName>` is everything before the first `\n` (`ipc::event_name`). The hook process never parses the JSON.
- The daemon prefers `hook_event_name` from the JSON and falls back to the first line.
- Maximum message: 16 MiB (`MAX_MSG`); anything beyond is truncated (Unix) or discarded (Windows).
- Empty messages are ignored. `claude-presence status` and `install` use an empty message as a "is the daemon alive" probe (`ipc::daemon_running`).
- Unparseable JSON is dropped (debug log `unparseable <event> payload`) but still counts for the one-time `first hook received (<event>)` log line.

### Reserved control events

Event names starting with `__` are reserved (`ipc::is_control`).

| Event | Effect | Sent by |
|---|---|---|
| `__shutdown` | Daemon stops cleanly like SIGTERM (log `shutdown requested`) | `install`, `uninstall` (`install::stop_daemon`) |
| any other `__...` | Ignored (debug log `ignoring control event`) | nobody; older daemons ignore newer controls |

`claude-presence hook __shutdown` does not forward: the hook command drains stdin and exits 0, so a `settings.json` entry cannot stop the daemon. The control check looks at the first line only, so a payload containing `"hook_event_name":"__shutdown"` is a normal hook.

## Endpoint locations

| OS | Hook endpoint | Verified before use |
|---|---|---|
| Linux | `$XDG_RUNTIME_DIR/claude-presence.sock`, else `/run/user/<uid>/claude-presence.sock` if that directory exists | no (OS-provided private dir is trusted) |
| Linux or macOS, no per-user runtime dir | `<tmp>/claude-presence-<uid>/hook.sock`; `<tmp>` is `$TMPDIR`, `$TMP`, `$TEMP`, else `/tmp` | yes, by daemon and client |
| macOS | `<per-user temp dir>/claude-presence.sock` (`confstr(_CS_DARWIN_USER_TEMP_DIR)`, then `$XDG_RUNTIME_DIR`) | no |
| Windows | `\\.\pipe\claude-presence-<USERNAME>` (characters other than ASCII letters, digits, `-`, `_` become `_`) | by DACL |

The socket file is created mode 0600 (umask `0177` during `bind`, then `chmod 600`).

### Fallback directory checks

Applies only to `<tmp>/claude-presence-<uid>`. `claude-presence status` prints the socket path in use.

Daemon (`ensure_private_dir`): create the directory mode 0700 if missing (chmod to exactly 0700 only when it was just created), then verify. It never chmods or removes an existing directory that fails.

Client (`send_to`, used by every hook): verify only, nothing is created. A failed check means nothing is sent.

| Check | Target | Failure text in the log or error |
|---|---|---|
| is a real directory, not a symlink (`lstat`) | the directory | `not a directory (or a symlink)` |
| owned by the current effective uid | the directory | `owned by another user` |
| no group/other bits (`mode & 077 == 0`) | the directory | `accessible by group/other` |
| owner has rwx | the directory | `not rwx for its owner` |
| is a directory (followed, so a `/tmp` symlink works) | parent | `not a directory` |
| owned by us or root | parent | `parent owned by another user` |
| sticky, or not writable by group/other (`mode & 022 == 0`) | parent | `parent writable by others and not sticky` |

Daemon failure logs `cannot listen on <path>: <path>: <reason>` and exits 1. Fix: if the directory is yours, `chmod 700` it or delete it; if it is not yours, someone else created it first, so leave it and set `XDG_RUNTIME_DIR` or `TMPDIR` to a directory you own.

### Windows pipe

`CreateNamedPipeW` with:

- `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first instance; if it fails with access denied, bind reports "daemon already running", and the daemon logs `another claude-presence daemon is already running` and exits 0.
- a protected DACL `D:P(A;;GA;;;<your user SID>)` built from the process token's `TokenUser`. It uses the SID, not owner rights, so an elevated daemon still accepts your non-elevated hooks.
- `PIPE_REJECT_REMOTE_CLIENTS`, inbound only, byte mode.
- a new instance is created before the current client is read, so the name is never free between clients.

## Threat model

Assumption: one trusted user per account; other local users are untrusted; the machine is not compromised.

| A local other user can | Cannot |
|---|---|
| see that `claude-presence-<uid>` or the socket exists (fallback dir, shared `/tmp`) | connect to the socket (0600, directory 0700) |
| create `<tmp>/claude-presence-<uid>` before you (shared `/tmp`) | make the daemon or hooks use it: the checks reject it and nothing is sent; the daemon refuses to start (denial of service until you choose another `TMPDIR`) |
| swap the directory after the checks | rename it away: parent is sticky or not writable by others |
| read `ledger.json` or `config.toml` if your home directory permissions allow it (the tool does not change them) | |
| | send hooks, send `__shutdown`, or read hook payloads (these include `cwd` and tool inputs, e.g. file contents for Write) |

The same user's processes can always talk to the daemon; that is by design. Hook payloads stay on the local machine: the daemon sends only the rendered card to Discord.

Not covered:

- A process running as you can stop the daemon or feed it false hooks.
- Discord sees whatever the card shows; see Privacy.

### Unverified on Windows (TODO.md items 9 and 10)

- Pipe squatting (open item 9): the DACL is owner-only, but another user who creates `\\.\pipe\claude-presence-<user>` first still receives your hooks; the daemon then logs `another claude-presence daemon is already running` and exits 0. Proposed fix (not implemented): client checks `GetNamedPipeServerProcessId` against the token user SID.
- Type-checked only, not run: the `ERROR_NO_DATA` handling and error-path read in `Listener::serve`, the user-SID DACL (including an elevated daemon accepting non-elevated hooks), the bind retry while the old daemon exits, and the `__shutdown` reinstall path.
- Verified by hand on Windows: `install` via Task Scheduler, `status`, the hook named pipe, and setting an activity from a hand-fed `UserPromptSubmit`.

## Privacy

What leaves the machine: only the Discord activity (details, state, image keys/URLs, tooltips, elapsed timestamp, buttons). No telemetry.

| Setting | Default | Effect |
|---|---|---|
| `github_button` | `false` | Adds "View on GitHub" for a github.com origin. Off because it would publish private repository URLs. Never shown for hidden projects. |
| `hidden_projects` | `[]` | Entries match the project directory name exactly, or an absolute path prefix of the cwd (`~/` is expanded to your home). A hidden project renders `{project}` as `hidden_project_name`, and `{branch}` and `{file}` as empty. |
| `hidden_project_name` | `a private project` | Replacement for `{project}`. |
| `buttons` | `[]` | Up to two, `http` URLs only; shown as written. |
| `[assets]` | claude-rpc's hosted gifs | Discord fetches these URLs; see [configuration.md](configuration.md). |

Hidden projects still count toward lifetime stats, and `{model}`, `{tool}`, token and prompt counts are still shown. If the tool name itself is sensitive, change the `state` template.

The default `client_id` and images are borrowed from claude-rpc; set your own Discord application to avoid depending on them.
