# claude-presence documentation

Start with the [project README](../README.md) for install and quick start. These pages go deeper.

| Page | What it covers |
|---|---|
| [architecture.md](architecture.md) | Components, data flow, threads and timers, the session state machine |
| [ipc-and-security.md](ipc-and-security.md) | Hook wire format, control events, socket/pipe locations and permission checks, threat model, privacy |
| [ledger.md](ledger.md) | `ledger.db` schema, dedup rules, crash consistency, migration from `ledger.json` |
| [configuration.md](configuration.md) | Every `config.toml` key, template variables, per-status sections, reloading |
| [services.md](services.md) | Per-platform service files, start/stop/log commands, reinstall and uninstall, what is verified |
| [troubleshooting.md](troubleshooting.md) | Symptom, cause, fix; log lines explained; feeding a hook by hand |
| [development.md](development.md) | Build, checks, test-driven workflow, testing platform code, sub-agent workflow, CI |

Other project files: [AGENTS.md](../AGENTS.md) (guide for coding agents, invariants),
[CLAUDE.md](../CLAUDE.md) (sub-agent workflow), [TODO.md](../TODO.md) (open findings and
verification status).
