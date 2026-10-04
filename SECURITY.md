# Security policy

## Supported versions

claude-presence is pre-1.0. Only the latest release and the `main` branch get
security fixes. Upgrade with `cargo install --git https://github.com/vx9k/claude-presence`
and re-run `claude-presence install`.

## Reporting a vulnerability

Please **do not open a public issue**. Report privately through GitHub:
[Security → Report a vulnerability](https://github.com/vx9k/claude-presence/security/advisories/new).

Include:

- the affected version (`claude-presence --version`) and OS;
- what an attacker can do, and from which position (another local user, a
  process running as you, a malicious transcript, a fake Discord client, ...);
- steps or a proof of concept to reproduce it.

You will get an acknowledgement as soon as possible, normally within a week. Once a fix is
released, the advisory is published and you are credited unless you ask
otherwise.

## Scope

claude-presence is a per-user daemon. Its trust boundary is your user account:
anything running as you, or as root/Administrator, is trusted. In scope:

- **Hook endpoint.** Another local user being able to send events to your
  daemon, read them, or make your hooks talk to an endpoint they control
  (Unix socket permissions and directory checks, the Windows named pipe DACL
  and pipe-owner check).
- **Discord IPC.** A fake Discord endpoint impersonating your user or reading
  more than the activity payload sent to it.
- **Untrusted input.** Crashes, hangs, unbounded memory use or miscounting
  caused by hook payloads, transcripts or `.git` metadata.
- **Install and uninstall.** Changes to `settings.json` beyond claude-presence's
  own hook entries, unsafe service definitions, or a user `PATH` edit that
  could make another program run instead.
- **Information leaks.** Data shown on the Discord card that the configuration
  says should be hidden.

Out of scope:

- What you choose to show on the card. Templates can include project names,
  branches and file names, and Discord shows them to whoever can see your
  profile.
- Attacks that already need your account, root or Administrator rights.
- Bugs in Discord, Claude Code or the operating system.
- Cloud Claude Code sessions, which never reach the daemon.

## Design notes

How the hook endpoint and Discord connection are secured, and why, is
documented in [docs/ipc-and-security.md](docs/ipc-and-security.md).
