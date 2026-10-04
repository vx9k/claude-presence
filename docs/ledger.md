# Ledger

Lifetime statistics (`src/ledger.rs`). See also: [architecture.md](architecture.md),
[configuration.md](configuration.md) (`scan_history`, `rescan_interval`), [README](../README.md).

## Files

In the data dir (`claude-presence status` prints `stats:` path):

| OS | Directory |
|---|---|
| Linux | `$XDG_DATA_HOME/claude-presence`, else `~/.local/share/claude-presence` |
| macOS | `~/Library/Application Support/claude-presence` |
| Windows | `%LOCALAPPDATA%\claude-presence` |

| File | Format | Written |
|---|---|---|
| `ledger.json` | JSON, replaced atomically (`ledger.json.tmp`, fsync, rename) | on save |
| `seen.bin` | append-only array of little-endian `u64`, 8 bytes per id, no header | before `ledger.json`, fsynced |

`claude-presence uninstall --purge` deletes the data dir. Deleting only the two files is safe: the next daemon start rebuilds from the transcripts still on disk (older, deleted transcripts are lost).

## `ledger.json`

`VERSION` is `1`. A file with another version, or one that does not parse, is treated as missing: a warning `<path> unreadable; rebuilding stats` is logged (only if the file existed) and `seen.bin` is deleted. Bump `VERSION` on any incompatible change.

```
{
  "version": 1,
  "totals": { "usage": {input, output, cache_read, cache_write}, "prompts": u64, "turns": u64, "sessions": u64 },
  "days":   [ [day_number, Day], ... ],
  "files":  { "<canonical transcript path>": FileState, ... }
}
```

| Field | Type | Meaning |
|---|---|---|
| `usage.*` | u64 each | Token sums: `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens` (stored as `cache_write`) |
| `totals.turns` | u64 | Assistant messages counted |
| `totals.prompts` | u64 | Real typed prompts counted |
| `totals.sessions` | u64 | Transcripts first seen, excluding subagent files (path contains `/subagents/` or `\subagents\`) |
| `day_number` | i32 | Days since the Unix epoch in the local UTC offset at the time of ingestion |
| `Day.minutes` | 23 x u64 | Bitmap of 1440 minutes (bit `m % 64` of word `m / 64`); a set bit is an active minute |
| `Day.tokens`, `.prompts`, `.turns` | u64, u32, u32 | Per-day counters |

`FileState` (per transcript):

| Field | Meaning |
|---|---|
| `offset` | Bytes already consumed; always the end of a complete line |
| `ident` | File identity: inode xor rotated device on Unix; on Windows a never-zero hash of the volume serial number and the 128-bit file id (`FileIdInfo`), not the creation time, which NTFS tunneling carries over to a file recreated under the same name. 0 if it can't be read |
| `last_ts` | Latest valid timestamp seen (ms), for active-time gaps |
| `usage`, `prompts`, `turns` | Totals attributed to this file |
| `model` | Latest assistant model id (ignores ids starting with `<`) |
| `ring` | Up to 8 recent message-id entries `{id, usage, seen}`; `seen` is `"Counted"`, `"Dup"` or `{"Pending": n}` |

Derived values (`Ledger::snapshot`): `total_time` is active minutes summed over all days; `today_*` use today's `Day`; `streak` counts consecutive active days ending today, or yesterday if today has no activity yet. A day is active if it has any turn, prompt or minute.

## Reading transcripts

Source: `<claude home>/projects/**/*.jsonl` (`$CLAUDE_CONFIG_DIR` or `~/.claude`).

- Incremental: each file is opened, seeked to `offset`, and read in 1 MiB chunks. Only complete lines (up to the last `\n`) are consumed; a partial trailing line waits for the writer. A file with no newline is never consumed.
- Replaced files: if `ident` changed or the file is shorter than `offset`, that file's state resets and it is read again from 0 (global dedup keeps totals correct). The same happens once per file when an upgrade changes how `ident` is computed (Windows moved from creation time to file id without a `VERSION` bump): totals are unchanged, but that file's own `usage`/`prompts`/`turns` restart from what is still new to the global set.
- Malformed lines (invalid JSON, wrong types, negative numbers) are skipped.
- Live sessions: `ingest(key)` on each hook and every 5 s while the displayed session is active. Background `scan` of everything at startup (if `scan_history`) and every `rescan_interval` seconds; scan uses up to 8 threads when there are 32 or more files.
- `scan` forgets files that no longer exist (`pruned`); their totals stay.

### Counting rules

| What | Rule |
|---|---|
| Tokens | Once per `message.id` (assistant lines), globally via `seen.bin`. Claude Code repeats `usage` on each content-block line; within one message the maximum per field wins, and later growth on an already counted message is added. |
| Messages with no `id` | Counted every time (no dedup possible). |
| Prompts | Type `user` lines with real text, once per `uuid` globally. Excluded: `isMeta`, `isCompactSummary`, tool results, and text starting with `<local-command`, `<command-`, `<system-reminder`, `<bash-`. A prompt with no `uuid` is counted every time. |
| Active time | Gap between consecutive timestamps under 5 minutes marks every minute between them; otherwise only the record's own minute. Timestamps before 2020-01-01 or more than 48 h in the future are ignored for time (tokens still count). |
| Resumed or copied history | The id is already in the global set, so it is not counted again. |

Ids are stored as 64-bit hashes (FNV-1a with a splitmix finalizer) of the `message.id` or `uuid` string.

## Save order and crash consistency

`Ledger::save`, when dirty (the daemon calls it every 60 s while dirty, after the startup scan, and on shutdown):

1. create the data dir;
2. append the ids counted since the last save to `seen.bin` and fsync (first realigning it to a multiple of 8 bytes if needed);
3. write `ledger.json.tmp`, fsync, rename over `ledger.json`;
4. clear the dirty flag.

| Failure | Outcome |
|---|---|
| Crash after step 2, before step 3 | Ids are marked seen but their counts were not saved. Everything since the last good `ledger.json` is undercounted, never double counted. Verified by the test `crash_between_seen_and_ledger_never_double_counts`. |
| `ledger.json` write fails | `seen.bin` already has the ids and is not appended again; the ledger write retries on the next save. |
| Torn append to `seen.bin` (size not a multiple of 8) | On load the partial id is truncated; if it happens while running, the next append truncates first. |
| Crash before step 2 | Transcript offsets in memory are lost; the next start reads from the saved offsets and recounts only what was not persisted. |

## Arithmetic

Token sums use `saturating_add` and growth uses `saturating_sub`, so a corrupt transcript with absurd values (even `u64::MAX`) cannot wrap or panic; totals only grow. Per-day `prompts`/`turns` are `u32`. Rendering uses saturating sums too (`{tokens}`, `{tokens_in}`).
