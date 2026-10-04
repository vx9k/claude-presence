# CLAUDE.md

@AGENTS.md

## Claude Code workflow

Orchestrate the sub-agents in `.claude/agents/` rather than doing everything
in the main thread:

1. **advisor** (Sonnet): consult first when a change adds a dependency, a
   thread or timer, touches the hook wire format, the ledger format, config
   schema, or service installation.
2. **developer**: implements the change test-first (failing test, confirm
   it fails, then the fix) and runs every check listed in AGENTS.md
   (including the Windows and macOS clippy targets).
3. **auditor**: reviews the resulting diff. Hand its findings back to the
   developer until it reports no high or medium findings.
4. **docs-writer** (Sonnet): updates README.md and `docs/` when commands,
   config keys, template variables, paths, log messages or service behavior
   change. README stays a front page; details live in `docs/`.

Independent steps (e.g. auditor + docs-writer) can run in parallel.

## Attribution

Commits and PR descriptions are credited to one sub-agent picked at random
from `advisor`, `auditor`, `developer`, `docs-writer`:

- commits: a `Sub-agent: <name>` trailer;
- PR descriptions: end with `🤖 Written by the <name> sub-agent`.

Keep any other trailers your harness requires.

## Commit messages

Commits and PR titles strictly follow Conventional Commits 1.0.0:
`<type>(<scope>)!: <description>`, imperative, lowercase, no period, header
≤ 72 characters; `!` plus a `BREAKING CHANGE:` footer for breaking changes;
footers in order `BREAKING CHANGE:`, `Refs:`/`Closes:`, `Sub-agent:`, then
harness trailers. Allowed types, scopes and what counts as breaking are in
[docs/development.md](docs/development.md#commit-messages). Split mixed work
into one commit per type. PRs are squash-merged, so the PR title must
conform too.

## Resuming work

Start from `TODO.md` (and `docs/architecture.md` if the code is new to you).
Stack related PRs when later work depends on earlier fixes. The usual loop for an item: advisor (if the item
touches IPC, the ledger format, config schema or services) → developer →
auditor until no high/medium findings → docs-writer if user-facing behavior
changed → conventional commit with a `Sub-agent:` trailer → push → check CI.

## Gotchas

- The hook path must stay fast and silent: no config loading, no JSON
  parsing, no output, always exit 0.
- Claude Code runs hooks through a shell (bash on Windows too): hook commands
  use quoted absolute paths with forward slashes.
- Windows named pipe and Task Scheduler code can only be type-checked here;
  reason about it carefully and say so in the PR when behavior is unverified.
  Tests that do run on Windows run in CI, so prefer testable pure helpers.
- Sub-agents share one working tree: don't run two editing agents on the same
  files at once, and don't commit while a developer is mid-change.
- Only local Claude Code sessions reach the daemon. A cloud session (including
  one opened from the desktop app) runs its hooks remotely, so when testing,
  feed hooks by hand: `echo '{"session_id":"t","cwd":"/"}' | claude-presence hook UserPromptSubmit`.
