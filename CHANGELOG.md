# Changelog

All notable changes to claude-presence are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!--
Releasing: rename (or copy) [Unreleased] to `## [X.Y.Z] - YYYY-MM-DD`
matching Cargo.toml before tagging vX.Y.Z. The release workflow refuses a
tag without that section and publishes its body as the release notes.
-->

## [Unreleased]

### Changed

- **Breaking:** `ledger.json` / `seen.bin` from releases before 0.2.0 are
  no longer imported. Upgrading straight from such a release rebuilds the
  stats from the transcripts still on disk.

## [0.2.0] - 2026-10-05

### Added

- `claude-presence tui`: a terminal dashboard with the daemon and Discord
  status, the current card, live sessions, lifetime stats with 60-day
  charts, and the config file (with a reload key). Falls back to the stats
  on disk when the daemon is down. Build without it with
  `--no-default-features`.
- `__state` and `__reload` control messages on the hook channel: `__state`
  returns a JSON snapshot of the daemon's sessions, `__reload` reloads the
  config like SIGHUP (which Windows lacks). Like `__shutdown`, they are sent
  only by claude-presence itself; `hook` never forwards them.
- Colored terminal logs: on a terminal, log lines get a local clock and
  colored level tags (off with `NO_COLOR` or `TERM=dumb`); piped stderr and
  the Windows log file stay plain.
- Signed releases: prebuilt archives for Linux (musl, x86_64/aarch64), macOS
  (x86_64/aarch64) and Windows (x86_64/aarch64), with `SHA256SUMS`, a keyless
  Sigstore signature of it (`SHA256SUMS.sigstore.json`, made by the release
  workflow) and build provenance attestations.
- `install.sh` (Linux, macOS) and `install.ps1` (Windows) install scripts:
  they always check the archive against `SHA256SUMS`, verify the signature
  on `SHA256SUMS` when `cosign` is installed (required with
  `CLAUDE_PRESENCE_REQUIRE_SIGNATURE=1`), and check the attestation when the
  GitHub CLI is installed and logged in.

### Changed

- **Breaking:** lifetime stats move from `ledger.json` + `seen.bin` to a
  SQLite `ledger.db`. Saves write only changed rows in one transaction, so a
  crash or failed save never loses or double counts; a busy or unreadable
  database is never overwritten and a corrupt one is moved aside. The first
  start imports the old files and renames them to `*.bak`; older releases
  can't read `ledger.db` (to downgrade, restore the `.bak` files).
- Windows binaries link the C runtime statically, so they no longer need the
  Visual C++ Redistributable.

### Fixed

- No more spurious warning about a missing `seen.bin`.
- `{prompts}` counts the prompt of the running turn exactly once until
  `Stop`: it no longer sticks at +1 after a custom slash command, now
  applies in resumed sessions, and stays during notifications and
  compaction.
