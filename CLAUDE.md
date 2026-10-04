# CLAUDE.md

@AGENTS.md

## Claude Code workflow

Orchestrate the sub-agents in `.claude/agents/` rather than doing everything
in the main thread:

1. **advisor** (Sonnet): consult first when a change adds a dependency, a
   thread or timer, touches the hook wire format, the ledger format, config
   schema, or service installation.
2. **developer**: implements the change and its tests, and runs every check
   listed in AGENTS.md (including the Windows and macOS clippy targets).
3. **auditor**: reviews the resulting diff. Hand its findings back to the
   developer until it reports no high or medium findings.
4. **docs-writer** (Sonnet): updates README.md when commands, config keys,
   template variables, paths or service behavior change.

Independent steps (e.g. auditor + docs-writer) can run in parallel.

## Attribution

Commits and PR descriptions are credited to one sub-agent picked at random
from `advisor`, `auditor`, `developer`, `docs-writer`:

- commits: a `Sub-agent: <name>` trailer;
- PR descriptions: end with `🤖 Written by the <name> sub-agent`.

Keep any other trailers your harness requires.

## Resuming work

Start from `TODO.md`. The usual loop for an item: advisor (if the item
touches IPC, the ledger format, config schema or services) → developer →
auditor until no high/medium findings → docs-writer if user-facing behavior
changed → commit with a `Sub-agent:` trailer → push → check CI.

## Gotchas

- The hook path must stay fast and silent: no config loading, no JSON
  parsing, no output, always exit 0.
- Claude Code runs hooks through a shell (bash on Windows too): hook commands
  use quoted absolute paths with forward slashes.
- Windows named pipe and Task Scheduler code can only be type-checked here;
  reason about it carefully and say so in the PR when behavior is unverified.
- Only local Claude Code sessions reach the daemon. A cloud session (including
  one opened from the desktop app) runs its hooks remotely, so when testing,
  feed hooks by hand: `echo '{"session_id":"t","cwd":"/"}' | claude-presence hook UserPromptSubmit`.
