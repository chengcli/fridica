# Experimental Rust worker isolation configuration

Use these settings in a separate experimental Rust configuration. The installed
Python daemon rejects the new `[isolation]` section. Do not add it to a live Python
configuration or migrate a live database to v6 to try these settings. Active Rust
startup remains gated; this configures the worker launcher library and offline
validation.

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

Trusted daemon composition can use `SystemLauncher::from_config` to construct
the existing local/SSH launch adapters with these settings. Runtime preflight,
active CLI composition and real backend conformance remain unfinished; see the
[implementation status](v0.4-implementation-status.md).
