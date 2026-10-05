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

| File | What it is |
|---|---|
| `ledger.db` | The ledger: one SQLite database. `claude-presence status` prints its path on the `stats:` line |
| `ledger.db-journal` | SQLite's rollback journal; exists only while a save is in progress (or after a crash, until the next open) |
| `ledger.db.corrupt` (and `ledger.db.corrupt-journal`) | A database SQLite reported as corrupt, moved aside so stats can be rebuilt. Kept for inspection; safe to delete |
| `ledger.json(.bak)`, `seen.bin(.bak)` | Stats from releases before 0.2.0. Ignored (see [Older ledgers](#older-ledgers)); safe to delete |

The database is opened only for one load or one save and closed again; the daemon holds no connection or lock between them. It is a plain SQLite 3 file (journal mode default, `synchronous = FULL`), so any SQLite tool can read it, for example `sqlite3 ledger.db 'select * from totals'` (read-only use only while the daemon is not saving).

`claude-presence uninstall --purge` deletes the whole data dir, including all of the files above. To rebuild the stats by hand, stop the daemon, delete `ledger.db` (and any `ledger.db-journal`), and start it again: it rebuilds from the transcripts still on disk (older, deleted transcripts are lost).

## Schema

`PRAGMA user_version` is `1` (`DB_VERSION`). Bump it on any incompatible change. All `u64` counters and ids are stored bit-cast to SQLite's signed 64-bit integer.

| Table | Rows | Columns |
|---|---|---|
| `seen` | one per counted id | `id` (primary key): 64-bit hash of a `message.id` or `uuid` |
| `totals` | exactly one (`id = 1`) | `input`, `output`, `cache_read`, `cache_write`, `prompts`, `turns`, `sessions` |
| `day` | one per active day | `day` (primary key), `minutes` (BLOB, 23 little-endian `u64`), `tokens`, `prompts`, `turns` |
| `file` | one per transcript | `path` (primary key), `pos` (the `offset` below), `ident`, `last_ts`, `input`, `output`, `cache_read`, `cache_write`, `prompts`, `turns`, `model`, `ring` (JSON text), `schema_v`, `ident_v`, `counted_to` |

The fields below keep the names used in the code (`FileState`); in the `file` table `offset` is `pos` and `schema`/`ident_v` are `schema_v`/`ident_v`.

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
| `ident` | File identity: inode xor rotated device on Unix; on Windows a never-zero hash of the volume serial number and the 128-bit file id (`FileIdInfo`), not the creation time, which NTFS tunneling carries over to a file recreated under the same name. Where `FileIdInfo` is not supported, the 32-bit volume serial and 64-bit file index (`GetFileInformationByHandle`) instead. 0 if neither can be read |
| `ident_v` | Rule `ident` was computed with; currently `1`. `0` for files imported from an old `ledger.json` that had none |
| `last_ts` | Latest valid timestamp seen (ms), for active-time gaps |
| `usage`, `prompts`, `turns` | The file's own view of its conversation (what the card shows): everything the file contains, including history a resumed session copied from another transcript. Not globally deduped, so they can add up to more than the totals |
| `model` | Latest assistant model id (ignores ids starting with `<`) |
| `ring` | Up to 8 recent message-id entries `{id, usage, seen}`; `seen` is `"Counted"` (new to the global set), `"Dup"` (already in the global set) or `{"Pending": n}` |
| `schema` | Per-file counting rules the stats were built with; currently `1`. `0` for files imported from an old `ledger.json` that had none |
| `counted_to` | During a re-read from 0: the end of the region already counted in the totals (the old `offset`, at most the file's length). Persisted so a re-read cut short by a read error does not count that region again when it resumes. Omitted (`0`) once the re-read has passed it |

Derived values (`Ledger::snapshot`): `total_time` is active minutes summed over all days; `today_*` use today's `Day`; `streak` counts consecutive active days ending today, or yesterday if today has no activity yet. A day is active if it has any turn, prompt or minute.

## Reading transcripts

Source: `<claude home>/projects/**/*.jsonl` (`$CLAUDE_CONFIG_DIR` or `~/.claude`).

- Incremental: each file is opened, seeked to `offset`, and read in 1 MiB chunks. Only complete lines (up to the last `\n`) are consumed; a partial trailing line waits for the writer. A file with no newline is never consumed.
- Re-reads: if `ident` differs (both computed with the current rule), the file is shorter than `offset`, or `schema` is older than the current one, that file's state resets (keeping `counted_to`) and it is read again from 0. Lines before `counted_to` were already counted, so during the re-read they mark no active minutes (the UTC offset may have changed since, which would land them on other minutes or days) and add nothing to the totals or day counters, with or without an id: they rebuild only the file's own stats. Their ids and uuids still go into the global set, and those missing from it are inserted into `seen` on the next save, which heals ids lost from the database. If the content really is new (a replaced file), everything below the old `offset` is undercounted, never double counted; lines past it count as usual. Detection is best effort: a file rewritten in place to the same or a greater length, keeping its identity, is not noticed (transcripts are append-only).
- Identity rule migration: an `ident` stored under an older rule (`ident_v` below the current one; Windows moved from creation time to file id without a version bump) can't be compared, so it is replaced by today's identity without a re-read, unless the file is shorter than `offset` (then it is re-read as above).
- Schema migration: files stored before per-file stats counted copied history have `schema` `0`; each such file is re-read once as above, which rebuilds its `usage`/`prompts`/`turns` without changing totals or days. No `user_version` bump.
- Malformed lines (invalid JSON, wrong types, negative numbers) are skipped.
- Live sessions: `ingest(key)` on each hook and every 5 s while the displayed session is active. Background `scan` of everything at startup (if `scan_history`) and every `rescan_interval` seconds; scan uses up to 8 threads when there are 32 or more files.
- `scan` forgets files that no longer exist (`pruned`); their totals stay.

### Counting rules

| What | Rule |
|---|---|
| Tokens | Once per `message.id` (assistant lines), globally via the `seen` table. Claude Code repeats `usage` on each content-block line; within one message the maximum per field wins, and later growth on an already counted message is added. |
| Messages with no `id` | Counted every time (no dedup possible). |
| Prompts | Type `user` lines with real text, once per `uuid` globally. Excluded: `isMeta`, `isCompactSummary`, tool results, and text starting with `<local-command`, `<command-`, `<system-reminder`, `<bash-`. A prompt with no `uuid` is counted every time. |
| Active time | Gap between consecutive timestamps under 5 minutes marks every minute between them; otherwise only the record's own minute. Timestamps before 2020-01-01 or more than 48 h in the future are ignored for time (tokens still count). |
| Resumed or copied history | The id is already in the global set, so the totals and days do not count it again. The new file's own `usage`/`prompts`/`turns` do count it, so a resumed conversation shows its full history on the card. Usage growth on such a message grows only the file's own `usage`. |

Known ring limits: within one file, a message is recognized as the same message only while its id is among the file's last 8 (`ring`). If more than 8 other messages come between two lines of any message (whether counted in this file or elsewhere), the ring has evicted it, so its later line is treated as a new message:

- The file's own stats (the card) count another turn with that line's full usage: a small overcount there.
- The totals and days are not double counted: the id is already in the global `seen` set, so the line adds nothing. But any usage growth that line carries over what was counted is lost from the totals: a small undercount, never an overcount.

Ids are stored as 64-bit hashes (FNV-1a with a splitmix finalizer) of the `message.id` or `uuid` string.

## Save order and crash consistency

`Ledger::save`, when dirty (the daemon calls it every 60 s while dirty, after the startup scan, and on shutdown):

1. create the data dir;
2. open `ledger.db` and begin a write transaction (waiting up to 2 s for a lock);
3. in that one transaction, insert the ids counted since the last save into `seen`, and write only the changed rows: `totals`, the touched `day` rows, and the `file` rows (offsets and per-file stats), deleting rows of pruned files;
4. commit (fsynced), close the database, clear the dirty flag.

**Exactly-once rule:** an id, the totals and days it was counted into, and the transcript offset past it commit together or not at all. After a crash or a failed save, the database still holds the previous consistent state: the offsets are old, so the next start reads those transcript bytes again, and their ids are not yet in `seen`, so they are counted once. Nothing is lost and nothing is counted twice. This replaces the old two-file scheme, where a crash undercounted everything since the last good write. Verified by the tests `crash_before_commit_counts_exactly_once` and `failed_save_is_retried`.

| Situation | Outcome |
|---|---|
| Crash or power loss mid-save | SQLite rolls the transaction back on the next open (using `ledger.db-journal`). See the exactly-once rule. |
| Save fails (disk full, data dir not writable) | `error: saving stats: <error>` in the log. The changes stay marked dirty and the save retries every 60 s. |
| Database file is corrupt (SQLite says "not a database" or "malformed") | At load: `warn: <path>: <error>; moved aside, rebuilding stats`. The file (and its journal) become `ledger.db.corrupt` / `ledger.db.corrupt-journal`, and a fresh database is rebuilt from the transcripts still on disk. Test `garbage_db_is_moved_aside`. |
| Database is busy (another process holds the lock past 2 s) or unreadable for now (including a metadata error on its path, or a legacy file that cannot be read during the import) | At load: `warn: <path>: <error>; stats will load later` (repeated failures are logged at `debug` only). The daemon keeps working in memory but does not save. Every 60 s, whether or not anything changed, it retries the load; once it succeeds it logs `info: stats loaded`, rescans the transcripts (if `scan_history`) and then saves. The stored totals are never overwritten by the in-memory rebuild. Test `busy_db_is_never_clobbered`. |
| `user_version` above `1` (written by a newer claude-presence) | At load: `error: <path> has version N, from a newer claude-presence; stats will not be saved`. The database is never written; the daemon still shows the card, with stats rebuilt in memory only. Upgrade claude-presence or delete the file. Test `newer_db_is_never_written`. |
| `ledger.db` deleted while the daemon runs | The next save recreates it in full from memory (every row). |
| `ledger.db` appears after the daemon started without one (another writer, a restored backup) | The save refuses with `error: saving stats: <path> was created by someone else; loading it instead of saving`; the next 60 s attempt loads it as above. Test `db_created_meanwhile_is_loaded_not_overwritten`. |

`claude-presence status` reads the database read-only (it never creates, migrates or locks it for writing). If the database is busy, unreadable or from a newer version it prints `stats unavailable: <error>` instead of the stats lines, rather than showing zeros. If `ledger.db` does not exist yet, it shows zeros.

## Older ledgers

Releases before 0.2.0 kept stats in `ledger.json` and `seen.bin`, and 0.2.0 imported them once into `ledger.db` (renaming them `*.bak`). Later releases no longer read them: if `ledger.db` does not exist yet, it is rebuilt from the transcripts still on disk, so stats from deleted transcripts are lost when upgrading straight from a release before 0.2.0. Leftover files are left alone and are safe to delete.

## Arithmetic

Token sums use `saturating_add` and growth uses `saturating_sub`, so a corrupt transcript with absurd values (even `u64::MAX`) cannot wrap or panic; totals only grow. Per-day `prompts`/`turns` are `u32`. Rendering uses saturating sums too (`{tokens}`, `{tokens_in}`).
