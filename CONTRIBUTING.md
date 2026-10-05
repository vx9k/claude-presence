# Contributing

Thanks for helping. claude-presence is small on purpose, so the bar for a
change is "does it keep the daemon lean, fast and silent?" Read the
[Code of Conduct](CODE_OF_CONDUCT.md) first; it applies everywhere in this
project.

## Before you start

- **Bugs and ideas:** open an issue using the
  [templates](.github/ISSUE_TEMPLATE). For anything bigger than a small fix,
  agree on the approach in an issue before writing code.
- **Security problems:** never in a public issue; see
  [SECURITY.md](SECURITY.md).
- **Open work:** [TODO.md](TODO.md) lists known findings and what still
  needs verifying on real systems. Testing on a platform we couldn't (macOS
  launchd, OpenRC, dinit, a real Windows session) is a welcome contribution
  on its own.

## Making a change

1. Read [AGENTS.md](AGENTS.md): the goals, the module map and the
   **invariants** (hook wire format, IPC endpoint checks, ledger rules,
   Discord rate limit). A change that breaks one of them won't be merged.
2. Prefer the simplest thing that works: reuse what is already here, then
   `std`. New dependencies need a strong case.
3. Write the failing test first, then the fix
   ([test-driven workflow](docs/development.md#test-driven-workflow)).
   Tests never touch real Discord, `settings.json`, services or user dirs.
4. Run the [checks](docs/development.md#checks):

   ```sh
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

   CI runs them on Linux, macOS and Windows. Say in the PR which platforms
   you could only check through CI.
5. Update README.md or `docs/` when commands, config keys, template
   variables, paths or service behavior change, and add a line under
   `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for user-visible
   changes.

## Commits and pull requests

- Commit messages and PR titles follow
  [Conventional Commits](docs/development.md#commit-messages), e.g.
  `fix(ipc): reject pipes owned by other users`. PRs are squash-merged, so
  the PR title is what lands on `main`.
- Keep a PR to one logical change; stack related PRs when one depends on
  another.
- Describe what changed, why, and how you tested it.

Contributions are accepted under the project's
[Apache-2.0 license](LICENSE).

AI coding agents: [AGENTS.md](AGENTS.md) and [CLAUDE.md](CLAUDE.md) are
written for you; the same rules apply.
