# Worker isolation

Confinement is configured per machine and workspace (`resources.gpus` turns on
`gpu_confine`; see the README). There is no `[isolation]` section to fill in.

Confined workers see the target's normal files, including SSH keys, git
configuration and `gh` credentials, so they can commit and push; confinement
bounds writes and devices. Local confined workers still have the daemon config,
state, control socket, egress deny list and configured contract/repository files
masked automatically. There is no `private_files` setting (it was removed;
configurations that still set it are rejected with an explicit message).

Workers never get MCP servers, on any machine:

- Claude workers start with `--strict-mcp-config` and an empty MCP configuration.
- Codex workers go through a fixed startup helper that reads the target's Codex
  settings layers (user, profile, project and ancestor `config.toml` files, plus
  a custom `CODEX_HOME`) and adds a `mcp_servers.<name>.enabled=false` override
  for every server it finds, before `app-server` initializes. Owner files are
  never edited. Malformed, oversized or linked settings refuse startup with a
  fixed diagnostic.
- Confined launches present sanitized copies of the backend settings inside the
  mount view: every MCP server is disabled there too, with its credentials
  removed, while the owner's files on disk stay untouched.

The former MCP inventory (`settings_files`, `mcp_inventory_complete`,
`mcp_aliases`, `mcp_urls` and the `[isolation.remote.*]` tables) was removed:
with no MCP at all for workers, there is nothing to review or register. A
configuration that still sets one of them is refused with a message naming it;
delete the setting. An empty `[isolation]` table is accepted.

To check startup prerequisites across every configured machine, workspace and
backend, without Slack credentials or an existing database, use:

```sh
cargo run --offline --bin fridica -- start --check-ready \
  --config /path/to/experimental.toml --timeout 30
```

This checks the existing control-directory permissions, owner contract/repository
inputs, parent executable, worker home/workspace and executable presence, reviewed
MCP inventories, and supported settings sources. Confined targets also check the
namespace and executable visibility inside it. No backend is invoked, even for a
version check. Parent checks use its existing tool-less startup policy. Local/SSH
helper probes may run; there are no Slack/GitHub/MCP calls, database opens, control
sockets or backend-state/slot directory creation. Missing control directories are
left for actual startup to provision. Temporary SSH housekeeping is removed.

The JSON report binds results to the build version and configuration fingerprint,
identifies machine/workspace/backend names and fixed failure codes, and omits
private paths and settings. `startup_checks_passed` requires every target and the
host/parent checks to pass. Readiness codes include `host_paths_refused`, `owner_inputs_refused`,
`backend_missing` and `workspace_refused`. Failure or cancellation exits nonzero.
SIGINT/SIGTERM finish cleanup of the current bounded probe before skipping later
probes; the timeout is per probe, and cleanup may take additional time.

A passing report does not check backend versions/protocols/authentication, Slack
access, database migration/locking, or guarantee writable state provisioning.
`fridica start` reruns these checks before state/recovery and daemon service I/O.
`--check-ready` and `--observe-only` are mutually exclusive.

Confinement on every configured machine and workspace is probed by the same
`start --check-ready` run (the separate `doctor-isolation` command was folded
into it). The probe shares the worker's path checks, settings parser and mount
helper. It opens mount sources without following symlinks, sanitizes settings in
memory, mounts workspace/backend state read-only, and verifies private paths are
absent inside the namespace. It leaves configuration, state, workspace and
backend settings unchanged. SSH uses its existing authentication and watchdog,
temporary housekeeping directories, and an already trusted host key; the probe
does not reuse or leave a persistent SSH master or enroll/update host keys. The
target must have a supported system Python (3.11+) and Linux bubblewrap with
usable user/PID namespaces and descriptor/data bind options.

Each target's `isolation` result in the readiness report is one fixed code:

| `isolation` | Meaning |
| --- | --- |
| `passed` | Settings checks and namespace probe succeeded. |
| `inventory_refused` | Missing/unsafe paths, hard links, or overlapping mount sources. |
| `settings_refused` | Unsafe/malformed settings or an unsupported backend home. |
