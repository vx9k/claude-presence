# Configuration reference

File: `config.toml`. Print its path with `claude-presence config`.

| OS | Path |
|---|---|
| Linux | `$XDG_CONFIG_HOME/claude-presence/config.toml`, else `~/.config/claude-presence/config.toml` |
| macOS | `~/Library/Application Support/claude-presence/config.toml` |
| Windows | `%APPDATA%\claude-presence\config.toml` |

`claude-presence install` writes a fully commented default file only if none exists (`DEFAULT_TOML`); an existing file is kept. See also [services.md](services.md), [ipc-and-security.md](ipc-and-security.md) (privacy), [README](../README.md).

Rules:

- Every key is optional; missing keys use the default below.
- Unknown keys and sections are ignored.
- A value with the wrong type (for example `idle_timeout = "900"`, `idle_timeout = -1`, `activity_type = 300`, a button without `url`) or a TOML syntax error makes the whole file fall back to defaults, with `error: <path>: <message>; using defaults` in the log. An unreadable file does the same.
- Numeric ranges are clamped, with `warn: <key> = <v> is below the minimum; using <n>` (or `above the maximum`).

> **Defaults borrowed from claude-rpc:** the default `client_id` is claude-rpc's public Discord application and the default images are its hosted gifs (`https://cdn.qualit.ly/...`). Create your own application at <https://discord.com/developers/applications> and point `client_id` and `[assets]` at it if you prefer not to depend on them.

Highlights: `client_id` (your own application), `idle_timeout`, `github_button`, `hidden_projects`, and the `[status.*]` templates. Everything else is below.

## Top-level keys

| Key | Type | Default | Notes |
|---|---|---|---|
| `client_id` | string | `"1506443909406920948"` | Discord application ID; its name is shown as "Playing <name>". Default is claude-rpc's public application. Changing it on reload restarts the Discord worker. |
| `activity_type` | integer 0..255 | `0` | `0` Playing, `2` Listening to, `3` Watching, `5` Competing in. Other values are sent as given; Discord may reject them. |
| `status_display` | string | `"name"` | What the member list shows next to your name: `"name"` (app name), `"state"` or `"details"`. Any other value behaves like `"name"`. |
| `show_elapsed` | bool | `true` | Elapsed timer counting from session start. |
| `idle_timeout` | integer seconds | `900` | `0` = never expire (kept until `SessionEnd`); otherwise clamped to `60..=604800`. Sessions in Thinking, Working or Compacting use at least 3600. |
| `rotation_interval` | integer seconds | `15` | Clamped to `5..=86400`. Applies to status sections that define `rotation`. |
| `github_button` | bool | `false` | "View on GitHub" button when origin is a github.com repository. Off because it exposes private repository URLs. |
| `hidden_projects` | array of strings | `[]` | Project directory names or absolute path prefixes (`~/` expands to home) to anonymize. |
| `hidden_project_name` | string | `"a private project"` | What `{project}` becomes for hidden projects. |
| `scan_history` | bool | `true` | Import lifetime stats from existing transcripts at start. See [ledger.md](ledger.md). |
| `rescan_interval` | integer seconds | `1800` | Background rescan of `~/.claude/projects` (or `$CLAUDE_CONFIG_DIR/projects`). `0` = never. |
| `buttons` | array of `{ label, url }` | `[]` | At most two buttons in total (including the GitHub one, which comes first). `url` must start with `http`, `label` must be non-empty and is cut to 32 bytes. |

## `[assets]`

Image values are an asset key you uploaded to your Discord application or an `https://` URL. Empty means none. Keys are cut to 300 bytes; tooltips to 128 bytes.

| Key | Default | Templated |
|---|---|---|
| `large_text` | `"{model} · {total_time} on Claude"` | yes (tooltip) |
| `small_image` | `""` | no |
| `small_text` | `""` | yes |
| `working` | `https://cdn.qualit.ly/clawd-working-building.gif` | no (large image while Working) |
| `thinking` | `https://cdn.qualit.ly/clawd-working-typing.gif` | no |
| `compacting` | `https://cdn.qualit.ly/clawd-working-typing.gif` | no |
| `notification` | `https://cdn.qualit.ly/clawd-notification.gif` | no |
| `idle` | `https://cdn.qualit.ly/clawd-sleeping.gif` | no |

If both `large_image` (the status one) and `small_image` are empty, the card has no assets block.

## `[status.<name>]` sections

One per status: `working`, `thinking`, `compacting`, `notification`, `idle`. Each has:

| Key | Type | Meaning |
|---|---|---|
| `details` | template string | First line of the card (cut to 128 bytes) |
| `state` | template string | Second line (cut to 128 bytes) |
| `rotation` | array of `{ details, state }` | Extra frames cycled after the base frame, every `rotation_interval` s |

Defaults:

| Section | `details` | `state` |
|---|---|---|
| `working` | `Working in {project}` | `{tool} · {file} · {tokens} tokens` |
| `thinking` | `Thinking in {project}` | `{model} · {prompts} prompts · {tokens} tokens` |
| `compacting` | `Compacting context in {project}` | `{model} · {tokens} tokens` |
| `notification` | `Waiting on you · {project}` | `{model} · {prompts} prompts` |
| `idle` | `Idle in {project}` | `{model} · {today_time} today` |

Only `[status.idle]` has default `rotation` frames:

| details | state |
|---|---|
| `Today · {today_time}` | `{today_prompts} prompts · {today_tokens} tokens` |
| `{total_time} on Claude` | `{total_sessions} sessions · {total_prompts} prompts` |
| `Lifetime · {total_tokens} tokens` | `{streak} day streak` |

Missing keys fall back per key to that status's built-in default from the tables above. An explicit value always wins, including an empty string (`state = ""` omits that line from the card) and `rotation = []` (no rotation). So `[status.idle]` with only `details = "zzz"` keeps the default `state` and the default idle carousel. Statuses you do not mention keep all their defaults. `[assets]` works the same way per key.

## Template syntax

- `{name}` is replaced by the variable's value. An unknown name renders empty. An unterminated `{` is left as text. Values are inserted verbatim (never re-expanded).
- The template is split into segments on `·` (U+00B7); segments are joined with ` · `. A segment containing a variable whose value is empty is dropped entirely; empty segments are dropped.
- Rotation frames are skipped while any variable they use is empty or `0`. The base `details`/`state` are always shown.
- A plural word after the value `1` is made singular: `{prompts} prompts` renders `1 prompt`.
- Fields shorter than 2 characters get an invisible padding character (Discord requires 2).

### Variables

| Group | Variable | Value |
|---|---|---|
| Session | `{project}` | Git root directory name, else the cwd's name; or `hidden_project_name` |
| | `{branch}` | Git branch; empty when not a repo or hidden |
| | `{model}` | Prettified model (`Opus 5.5`, `Sonnet 4.5`), else the SessionStart model hint, else `Claude` |
| | `{tool}` | Current tool (`mcp__server__tool` shown as `server:tool`); empty unless Working |
| | `{file}` | File name of the tool's `file_path`, `notebook_path` or `path`; empty when hidden |
| | `{tokens}` | Session total tokens (input + output + cache read + cache write), compact (`12.3k`) |
| | `{tokens_in}` | input + cache read + cache write |
| | `{tokens_out}` | output |
| | `{prompts}` | Session prompts (from the transcript, else counted from hooks) |
| | `{tools}` | Tool calls this session (from hooks) |
| | `{session_time}` | Time since session start (`3h 12m`) |
| | `{status}` | `Idle`, `Thinking`, `Working`, `Compacting` or `Waiting for input` |
| Today | `{today_time}` | Active time today; empty when 0 |
| | `{today_tokens}` | Tokens today |
| | `{today_prompts}` | Prompts today |
| Lifetime | `{total_time}` | Active time; empty when 0 |
| | `{total_tokens}` | Tokens |
| | `{total_sessions}` | Sessions |
| | `{total_prompts}` | Prompts |
| | `{streak}` | Consecutive active days |

## Examples

Own Discord application, keep the card longer, hide client work:

```toml
client_id = "123456789012345678"
idle_timeout = 3600
hidden_projects = ["secret-client", "~/work/nda"]
hidden_project_name = "client work"
```

Show branch and a website button:

```toml
buttons = [{ label = "My website", url = "https://example.com" }]

[status.working]
details = "Working in {project} · {branch}"
state = "{tool} · {file}"
```

Turn off the idle carousel and the elapsed timer:

```toml
show_elapsed = false

[status.idle]
rotation = []
```

Never expire sessions and never rescan:

```toml
idle_timeout = 0
rescan_interval = 0
```

## Applying changes

| Platform | How |
|---|---|
| Linux, macOS | Send `SIGHUP`: `systemctl --user kill -s HUP claude-presence` (systemd), `pkill -HUP -x claude-presenced`, `launchctl kill SIGHUP gui/$(id -u)/io.github.vx9k.claude-presence` (macOS). Log: `info: configuration reloaded`. |
| Any | Restart the service ([services.md](services.md)). |
| Windows | No reload signal: `schtasks /End /TN claude-presence` then `schtasks /Run /TN claude-presence`. |

A config file with a syntax error is reported in the log and the defaults are used.

Reload re-reads the file, applies it to existing sessions immediately, and restarts the Discord worker only when `client_id` changed. `scan_history` is only read at start.
