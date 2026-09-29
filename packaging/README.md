# Fridica native candidate

This is an experimental Linux x86_64 installation, separate from the production
Python `fridica` command. It contains `bin/fridica-candidate` and
`bin/fridica-overseer-candidate`. The latter currently exposes the offline campaign
planner; campaign automation remains opt-in development work. Dashboard serving
and redesign are deferred. Cross-platform maturin wheels and public publishing
are later release work.

## Requirements and installation

Read `build-manifest.json` for the exact source fingerprint, Rust toolchain,
required glibc version, shared libraries and file hashes. Build and independently
reproduce from the matching checkout with `python scripts/package_candidate.py
--verify-reproducible --smoke` (Cargo.lock dependencies must already be cached;
use `cargo fetch --locked` when network access is available). The recipe uses the
native Rust toolchain and binutils `readelf`; reproducibility is scoped to the
same source, toolchain and host environment, with separate clean target trees. The native binaries
need no Python Fridica package or source checkout. Execution helpers require
`/usr/bin/python3`; packaged operator tools require Python 3.11+. Enable only
execution machines whose backend authentication, MCP inventory and confinement
prerequisites pass `doctor`, `doctor-isolation` and `start --check-ready` on the
deployment host. SSH keys/agent and `gh` login remain owner-managed.

Verify the archive SHA-256 against the trusted build output, and use the supplied
stdlib installer (from the same trusted build):

```sh
python3 install-candidate.py candidate.tar.gz /absolute/new/install --sha256 EXPECTED_SHA256
/absolute/new/install/bin/fridica-candidate build-info
/absolute/new/install/bin/fridica-candidate assets --list
```

The destination must not exist. Nothing changes PATH, replaces the Python
launcher, installs a service or reads credentials. Retain versioned installation
directories; point the service at a specific binary. SHA-256 detects corruption;
it is not a publisher signature. This local candidate is not a public release.

## Configure and initialize

Use absolute paths below. Keep the config/state/credentials directory private
(mode 0700), credentials and config files mode 0600. Run `init` to create the
starter template and companion contract/Slack manifest, then `configure` (see
`--help`) and edit workspaces and inventories. `configure --detect` uses your
configured Slack credentials; it is unnecessary for an offline rehearsal.

```sh
/absolute/install/bin/fridica-candidate init --config /private/fridica/config.toml
/absolute/install/bin/fridica-candidate check-config --config /private/fridica/config.toml
/absolute/install/bin/fridica-candidate init-state --config /private/fridica/config.toml
/absolute/install/bin/fridica-candidate start --check-ready --config /private/fridica/config.toml
/absolute/install/bin/fridica-candidate doctor --config /private/fridica/config.toml --json
```

`init-state` creates a fresh v6 database without credentials or network calls;
legacy schemas require explicit migration. `check-config` returns the config
fingerprint. Record it with `build-info` in the deployment checklist. Add the
credentials file to `isolation.private_files` **before** finishing configuration. Complete private-file and MCP inventories for every
enabled execution target; unrestricted workers remain within the trusted-owner
model. No automatic SSH agent forwarding is introduced.

## Observe-only service, controls and logs

Store `SLACK_APP_TOKEN` and `SLACK_USER_TOKEN` (or configured names) in a private
systemd EnvironmentFile, never in the unit/argv/checklist. Include PATH and HOME
if the user service manager lacks the authenticated backend environment. Print a
user unit; inspect the output before installing it:

```sh
/absolute/install/bin/fridica-candidate service-print --config /private/fridica/config.toml --environment-file /private/fridica/credentials.env
```

Save it as `~/.config/systemd/user/fridica-candidate.service` on the deployment
host, then use:

```sh
systemctl --user daemon-reload
systemctl --user start fridica-candidate
/absolute/install/bin/fridica-candidate status --config /private/fridica/config.toml
journalctl --user -u fridica-candidate -f
systemctl --user stop fridica-candidate
```

The unit defaults to `--observe-only` with no model/worker/post adapters. This
mode still connects to Slack and persists intake; perform it only on the intended
host with owner-managed credentials. Control authority uses the owner's local
Unix identity. SIGINT/SIGTERM drain the service and remove the socket; a second
process cannot open the same state. `KillMode=control-group` bounds abandoned
children on forced service termination. For foreground startup, load credentials
in the environment and run `start --observe-only --config ...`; stop with Ctrl-C.
No worker-process survival across daemon restarts is promised.

## Active opt-in

Work through `DEPLOYMENT.md` first. Active operation replies, delegates and posts
in Slack, so it is an explicit flag:

```sh
/absolute/install/bin/fridica-candidate start --active --config /private/fridica/config.toml
```

Active startup reruns readiness and doctor before reading credentials or opening
state, and refuses a config that changes during those probes. A bare `start`
refuses to run. For an active unit, pass `--active` to `service-print`. Start with one channel/target;
keep campaign actions, channel report posting and MCP controls disabled until
separately enabled and verified.

## Backup, upgrade and explicit recovery

Stop all daemons that could use the database. Keep backups outside worker-readable
workspaces and add them to the private inventory. The stdlib snapshot helper
acquires the same database lock as Rust, uses SQLite Backup (including committed
WAL content), verifies integrity, and includes the matching config and hashes:

```sh
python3 /absolute/install/share/state_snapshot.py backup --database /private/fridica/state.sqlite3 --config /private/fridica/config.toml --output /private/new-snapshot
python3 /absolute/install/share/state_snapshot.py restore --snapshot /private/new-snapshot --output /private/new-recovery
```

Snapshot and recovery destinations must be new. Incomplete destinations have no
valid manifest; do not use them. This helper covers daemon DB and config only:
copy contract/repository policies, artifacts, credentials, report exports and any
separate overseer database according to your retention requirements. It does not
claim to back up an entire deployment.

For an existing Python installation, retain its environment for two weeks. Make
and rehearse a snapshot on copies before touching the stopped production pair:

```sh
/absolute/install/bin/fridica-candidate migrate --database /copy/state.sqlite3 --config /copy/config.toml --dry-run
/absolute/install/bin/fridica-candidate migrate --database /copy/state.sqlite3 --config /copy/config.toml
/absolute/install/bin/fridica-candidate migrate --database /copy/state.sqlite3 --config /copy/config.toml --rollback
```

Migration journals and backups support interrupted migration recovery. Repeat
`migrate` to complete an interrupted operation. `--rollback` refuses after durable
v6 mutations; do not bypass this guard or downgrade automatically. On the actual
upgrade, omit rollback after checking the dry run. Never run both daemons against
one DB.

For explicit restoration after live mutations, first snapshot the stopped current
pair, then restore the selected earlier snapshot into a **new** recovery directory.
Review its config alongside the original path printed by the helper: relative
paths resolve from the original config directory. Restore the matching config to
that original location only after preserving the current one, changing
`[state].path` to the new recovered DB's absolute path. Alternatively create a
new config and make **all** relative paths absolute. Validate that pair with the
matching runtime; a restored v5 DB needs the retained Python environment or an
explicit v6 migration. Review missed/delivered work before resuming: recovery loses
writes since the snapshot and external Slack/GitHub effects are not undone.
Do not reuse a
migration journal from another DB path or copy WAL/SHM files over live SQLite.
