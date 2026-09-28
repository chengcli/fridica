# Experimental Rust worker isolation configuration

Use these settings in a separate experimental Rust configuration. The installed
Python daemon rejects the new `[isolation]` section. Do not add it to a live Python
configuration or migrate a live database to v6 to try these settings. Active Rust
startup remains gated; this configures the worker launcher library and offline
validation, with an explicit target runtime probe.

Existing configurations remain valid without this section. Confined SSH workers
require an explicit target inventory before they can start. Local workers already
mask the daemon config, state, control socket and configured contract/repository
files; `private_files` adds capability files outside those directories.

```toml
[isolation]
# Relative paths resolve beside this configuration; ~/ uses the daemon's home.
private_files = ["credentials/mcp.key", "credentials/overseer.key"]

# Register identities for opaque wrappers or HTTP servers. Do not put secrets here.
mcp_aliases = ["desktop-fridica"]
mcp_urls = ["http://127.0.0.1:8765/mcp"]

# "compute" must already be an SSH machine in [machines.compute].
[isolation.remote.compute]
# Must exactly match machines.compute.host, including the owner account/SSH alias.
host = "owner@compute"
# These are TARGET paths. ~/ expands on the target, never on the daemon host.
private_files = [
  "~/.config/fridica/config.toml",
  "~/.local/state/fridica/state.sqlite3",
  "~/.local/state/fridica/control.sock",
  "~/.local/state/fridica/mcp.key",
  "/shared/fridica/control.key",
]
```

Replace the example inventory with all private files visible on that target,
including daemon files on shared storage and remote capability/control files.
The launcher does not copy the local inventory to remote hosts. Changing an SSH
host requires changing its inventory binding too; a stale binding rejects the
configuration. This does not create an SSH key or enable agent forwarding.

Inventory entries denote files. Their parent directories are masked to cover
sidecars such as SQLite WAL files, and the selected mask directories must exist
before launch. Keep them outside worker workspaces and writable backend state.
The launch helper also rejects unsafe symlinks/hard links and overlapping mount
sources on the execution target. Missing files may be inventoried before they are
created; the parent directory still supplies the boundary.

MCP aliases and endpoints supplement the launch helper's default-layer discovery.
They apply to local and remote workers constructed from this configuration.
URL identities must use HTTP(S) and cannot contain user information, queries or
fragments. Never place credentials in URL paths or alias names either. These
settings register identities to disable and sanitize; they do not enable an MCP
service or contact an endpoint. Opaque wrappers and configuration sources still
need an owner inventory; discovery does not establish its completeness.

Run the offline check against the separate configuration:

```sh
cargo run --offline --bin fridica -- check-config --config /path/to/experimental.toml
```

The `isolation` result lists counts and each SSH machine's inventory status:
`configured`, `missing` for a confined target, or `not_required` for an
unconfined target. It omits private paths, host values and MCP identities.
`runtime_checks` is always `not_run`: this command reads configuration without
creating state, starting a backend, contacting SSH, or testing mount support.
A successful check does not certify launch readiness.

To test confinement on one configured machine and workspace, use:

```sh
cargo run --offline --bin fridica -- doctor-isolation \
  --config /path/to/experimental.toml --machine local --workspace project
```

Unlike `check-config`, this starts a bounded subprocess and contacts SSH when the
selected machine uses SSH. It does not start a model/backend or contact Slack,
GitHub or MCP. The target must have a supported system Python (3.11+) and Linux
bubblewrap with usable user/PID namespaces and descriptor/data bind options.
The target home, workspace and private mask directories must already exist.
Backend settings/state directories may be absent; the probe does not create them.

The probe shares the worker's path checks, settings parser and mount helper. It
opens mount sources without following symlinks, sanitizes settings in memory,
mounts workspace/backend state read-only, and verifies inventoried private paths
are absent inside the namespace. It leaves configuration, state, workspace and
backend settings unchanged. SSH uses its existing authentication and watchdog,
temporary housekeeping directories, and an already trusted host key; the probe
does not reuse or leave a persistent SSH master or enroll/update host keys.

JSON output identifies only machine/workspace names and a fixed `check` result.
Success requires both the expected probe response and a zero exit status. A
failed check exits nonzero without printing subprocess output or private paths:

| `check` | Meaning |
| --- | --- |
| `passed` | Inventory/settings checks and namespace probe succeeded. |
| `missing_remote_inventory` | Provision an inventory for the selected SSH target. |
| `inventory_refused` | Missing/unsafe paths, hard links, or overlapping mount sources. |
| `settings_refused` | Unsafe/malformed settings or an unsupported backend home. |
| `namespace_failed` | Bubblewrap could not establish or validate the mount view. |
| `runtime_or_transport_failed` | System Python/SSH failed or returned an unexpected response. |
| `probe_failed` | Subprocess startup, deadline, output bound or cleanup failed. |
| `launch_configuration_refused` / `unsupported_transport` | The selected launch cannot be probed. |

`--timeout` defaults to 30 seconds and accepts 1–120 seconds; process cleanup can
take additional time. A successful probe always reports `active_launch_ready:
false`. It does not establish inventory completeness, backend version/protocol
conformance, writable state provisioning, unrestricted-worker MCP isolation, or
full runtime parity. Launch-time checks still run again to catch later changes.

The shared active library runtime now calls automatic admission for every job,
after its durable claim and before scoped fetch or backend execution. Confined
local/SSH jobs run the same probe, including when reusing a warm worker. There is
no cached successful probe. Admission checks the configured base workspace so
an absent slot directory does not require provisioning; the launch helper checks
the actual slot and its settings again before starting a backend. Missing base
workspaces refuse admission. Probe success does not replace launch-time checks.

Each system launcher permits one probe at a time, with a 30-second process
deadline inside the job deadline. This capacity is separate from backend process
permits and remains held until cancellation cleanup terminates and reaps the
probe. Owner controls do not wait on a supervisor lock held by admission.
Failures use fixed `worker_isolation_*` codes in durable job completions and
worker-result notifications; refusals do not trigger execution retries.
Observe-only never invokes admission, and ordinary unrestricted jobs skip the
namespace probe while retaining their existing policies.

The launcher and backend MCP startup options are bound to their construction
configuration. Runtime startup validates those bindings before recovery writes;
supervisor reconfiguration rejects incompatible private-file inventories, remote
host bindings, control/config/rule paths, and MCP identities before publishing
new configuration or changing approval policy. Rebuild the runtime adapters to
apply those changes. This is a fail-closed restriction, not live isolation reload.

Unrestricted Codex workers now run a target-side settings helper before each
backend process starts, locally and over SSH. It discovers Fridica aliases using
the same bounded, no-follow parser as confinement, then appends
`-c 'mcp_servers."alias".enabled=false'` overrides before executing `codex
app-server`. It scans user/system/managed files, user profile files, and workspace
ancestor `.codex/config.toml` files, using the execution target's home. An absolute
custom `CODEX_HOME` is supported for unrestricted workers and retains its existing
authentication and state. Claude keeps its existing `--strict-mcp-config` plus
explicit empty `--mcp-config` and disabled settings sources.

No discovery command or MCP executable runs during this scan. Registered aliases
and URL identities also apply to unrestricted Codex; other MCP servers, model
settings, backend authentication, session storage and ordinary filesystem/network
policies stay in place. Owner configuration files are neither rewritten nor copied.
Malformed, oversized or linked source files refuse startup with a fixed diagnostic;
when the helper exits 97 with its exact refusal marker, the runtime records
`worker_mcp_settings_refused` as a refusal, without execution retry. This scan
happens at backend launch, after any scoped fetch; it is not a namespace admission
probe. Warm processes retain their original startup configuration, and each new
process scans again.

The Codex helper requires system Python with TOML support (3.11+). It starts at
`/usr/bin/python3`, with fixed versioned `/usr/bin/python3.14` through `python3.11`
fallbacks. On macOS it also checks versioned Python installations under
`/opt/homebrew/bin` and `/usr/local/bin`; it never searches worker `PATH` for the
helper interpreter. Linux behavior is fixture-tested here; macOS and actual
backend versions still need deployment conformance checks.

Unrestricted workers remain in the trusted-owner model: they can still read
owner files and deliberately invoke tools themselves. This startup scan is not
credential isolation or a guarantee against concurrent settings changes. External
and cloud-delivered sources not represented by the scanned files still require
explicit identity provisioning and target validation. Active CLI startup, complete
configuration-source coverage and real backend conformance remain unfinished;
see the [implementation status](v0.4-implementation-status.md).

The override behavior follows the official [configuration precedence](https://learn.chatgpt.com/docs/config-file/config-basic)
and [MCP settings](https://learn.chatgpt.com/docs/extend/mcp?surface=cli) documentation.
