# Experimental Rust worker isolation configuration

Use these settings in a separate experimental Rust configuration. The installed
Python daemon rejects the new `[isolation]` section. Do not add it to a live Python
configuration or migrate a live database to v6 to try these settings. Active Rust
startup remains gated; this configures the worker launcher library and offline
validation, with an explicit target runtime probe.

Existing configurations remain valid without this section. Confined workers see
the target's normal files, including SSH keys, git configuration and `gh`
credentials, so they can commit and push; confinement bounds writes and devices.
Local confined workers still have the daemon config, state, control socket and
configured contract/repository files masked automatically. There is no
`private_files` setting (it was removed; configurations that still set it are
rejected with an explicit message).

```toml
[isolation]
# Relative paths resolve beside this configuration; ~/ uses the daemon's home.
# Additional MCP configuration sources outside the automatically scanned layers.
# Explicit sources are required to exist and must be TOML or JSON.
settings_files = ["backend-settings/extra.toml"]
# Set true only after reviewing every local source and registering opaque aliases.
mcp_inventory_complete = true

# Register identities for opaque wrappers or HTTP servers. Do not put secrets here.
mcp_aliases = ["desktop-fridica"]
mcp_urls = ["http://127.0.0.1:8765/mcp"]

# "compute" must already be an SSH machine in [machines.compute].
[isolation.remote.compute]
# Must exactly match machines.compute.host, including the owner account/SSH alias.
host = "owner@compute"
settings_files = ["~/.config/owner/extra-mcp.json"]
# This declaration applies only to the exact SSH machine/host above.
mcp_inventory_complete = true
```

The remote table records the MCP review for that exact SSH target; changing an
SSH host requires changing its binding too, and a stale binding rejects the
configuration. This does not create an SSH key or enable agent forwarding. The
launch helper still rejects unsafe symlinks/hard links and overlapping mount
sources on the execution target.

MCP aliases and endpoints supplement the launch helper's default-layer discovery.
They apply to local and remote workers constructed from this configuration.
URL identities must use HTTP(S) and cannot contain user information, queries or
fragments. Never place credentials in URL paths or alias names either. These
settings register identities to disable and sanitize; they do not enable an MCP
service or contact an endpoint. Opaque wrappers and configuration sources still
need an owner inventory; discovery does not establish its completeness.

`settings_files` adds up to 64 required TOML/JSON sources per target to the same
bounded, no-follow parser. Local relative paths resolve beside the configuration;
remote paths must be absolute or start with `~/` and expand on that target.
Missing, malformed, oversized or linked explicit files refuse discovery. Confined
launches sanitize these sources in their mount view; unrestricted Codex discovers
aliases and appends disable overrides without editing the files. Claude uses its
strict empty MCP configuration. Inventory sources hidden by a masked daemon
directory are refused rather than silently omitted.

`mcp_inventory_complete` defaults to false. It asserts that the owner has reviewed
all effective sources, including dynamic/cloud configuration and opaque wrappers,
and has registered aliases for sources that cannot be represented by these files.
It does not prove that assertion or fetch cloud settings. Do not set it merely to
make the check pass. Actual-target review and backend conformance remain required.
Changing either field invalidates an existing launcher's configuration binding.

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
host/parent checks to pass. Unreviewed targets report `mcp_inventory_unreviewed`;
other readiness codes include `host_paths_refused`, `owner_inputs_refused`,
`backend_missing` and `workspace_refused`. Failure or cancellation exits nonzero.
SIGINT/SIGTERM finish cleanup of the current bounded probe before skipping later
probes; the timeout is per probe, and cleanup may take additional time.

A passing report always retains `active_launch_ready: false` and lists the later
compatibility, replay/recovery, packaging and deployment gates. It does not check
backend versions/protocols/authentication, Slack access, database migration/locking,
or guarantee writable state provisioning. Active library host startup reruns these
checks before state/recovery and daemon service I/O; the active CLI remains gated.
`--check-ready` and `--observe-only` are mutually exclusive.

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
supervisor reconfiguration rejects incompatible remote
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
explicit identity provisioning, an owner coverage declaration and target validation.
Readiness and explicit source inventory are implemented; active CLI enablement,
real inventory review and backend conformance remain later gates;
see the [implementation status](v0.4-implementation-status.md).

The override behavior follows the official [configuration precedence](https://learn.chatgpt.com/docs/config-file/config-basic)
and [MCP settings](https://learn.chatgpt.com/docs/extend/mcp?surface=cli) documentation.
