# CLAUDE.md

@AGENTS.md

## Claude Code workflow

Work in the main thread by default: each sub-agent starts cold and re-reads
the repo, so spawn one only where it pays for itself.

1. Implement test-first (failing test, confirm it fails, then the fix) and
   run the checks in AGENTS.md once the change is done, not after every edit.
2. **auditor**: one pass over the finished diff before committing; fix its
   high/medium findings inline and re-audit only if a fix was non-trivial.
3. **advisor** (Sonnet): only when a decision is genuinely open (a new
   dependency, thread or timer; the hook wire format, ledger format, config
   schema or service installation) — not to confirm a settled plan.
4. **docs-writer** (Sonnet) / **developer**: for large, self-contained work
   that would otherwise flood the main context. Small doc updates are done
   inline: README stays a front page; details live in `docs/`.

Read targeted line ranges, not whole files or full diffs.

## Attribution

When a sub-agent did most of the work, credit it; when the main thread did,
add nothing:

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
Stack related PRs when later work depends on earlier fixes. Loop: the
workflow above → conventional commit → push → check CI.

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
