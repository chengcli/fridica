# Overleaf account login and worker setup

## Installed wheel

Install a compatible Fridica wheel in the desktop owner's Python 3.11+ environment
and on each registered worker. The wheel includes pinned native MCP resources,
licenses and corresponding source. Ordinary Node >=20 is required on both ends;
the desktop also needs Chrome/Chromium and an unlocked OS credential store
(Keychain on macOS, Secret Service in a Linux user D-Bus session). Workers use
Python and the existing Fridica workspace sandbox, with no worker browser.
No npm, Rust, model client or manual resource copying is needed for built wheels.

```sh
fridica overleaf-auth login --expected-email alice@example.com
```

Substitute your existing account. Enter passwords/MFA yourself in the dedicated
browser window. Account identity is validated upstream before Fridica saves the
session. Login needs no project/channel/worker identifiers. Subsequent
`fridica overleaf-auth login` calls reuse saved configuration and a valid session.
Use `--config /absolute/config.toml` consistently for a nondefault configuration.
`--prepare-only` saves and checks setup without network, browser or OS-store access.

Login enables Overleaf in the selected config; it does not start/restart a
listener or alter registered workspace/channel permissions. Use ordinary owner
setup for allowed channels and registered local/SSH placements. Eligible workers
receive the existing `overleaf` MCP and session automatically at admitted launch.
Remote resource discovery uses the worker's installed prefix, not desktop paths.
A matching `fridica` and Node must be visible on its noninteractive PATH, including
any configured Python environment. Native comments need no Overleaf Git token;
Git/compile retains its separate credentials and tools. Doctor checks native
resources without forwarding an account session; its warning records that
authenticated operation is checked only on an admitted task.

## Two projects in one channel

Give each task its authorized editor URL. The ID is the 24-character string
following `/project/`; extract it, never guess. One worker in one allowed channel
can use both of these fictional examples:

```json
{"name":"native_list_comments","arguments":{"project_id":"0123456789abcdef01234567"}}
{"name":"native_list_tracked_changes","arguments":{"project_id":"abcdef0123456789abcdef01"}}
```

Each native call must specify its own project, including thread reads and replies.
Missing targets fail. Upstream selects and serializes projects; Fridica checks
identity and optional restrictions. Only task-authorized operations are allowed.
Suggestions remain pending and need the native document version. Every write
needs a stable operation UUID; read/reconcile uncertain outcomes without replay.

## Optional restrictive configuration

Existing project/channel/recipient restrictions remain enforced, including
restrictions in saved schema-2 sessions. Login does not remove them, and renewal
cannot silently widen them. Keep the owner's current narrow grants during an
upgrade. A new account-only login permits project selection per authorized task;
this is not permission to access another user's accounts or projects.

For an intentionally single-project deployment, optional configuration is:

```toml
[overleaf.native_scope]
expected_email = "alice@example.com"
project = "0123456789abcdef01234567"
channel = "C012ABCDEF0" # must already be allowed by the owner
```

`fridica overleaf-auth bind-recipient --help` describes optional binding to an
actual existing worker and registered placement. Manual binding is not part of
normal setup. Existing bound recipients and timed grants remain restrictive;
replacing a legacy-bound worker requires an explicit grant update. Saved account
changes require revocation. Deleting only the OS entry does not invalidate
running copies or upstream's dedicated session/profile.

## Explicit source installation

Source builds need Rust >=1.91, Cargo, a platform compiler/SDK, Git, Python >=3.11,
Node >=20 and npm. Use a new checkout/environment; preserve existing source and
runtime state. Check out the exact revision you intend to install and record
`git rev-parse HEAD` rather than assuming a moving branch matches a release.
From that checkout:

```sh
python3 -m venv .venv
. .venv/bin/activate
npm ci --prefix integrations/overleaf --no-audit --no-fund
python -m pip install .
fridica overleaf-auth login --expected-email alice@example.com --resources "$PWD/integrations/overleaf"
```

The locked Git dependency uses its standard prepare hook; do not use
`--ignore-scripts`. Keep the prepared checkout in place. `pip install .` alone
builds the CLI, without prepared wheel data. To build an installable wheel that
also carries worker resources, use the [standard wheel commands](../integrations/overleaf/README.md#build-and-focused-checks).
For source-only worker deployments, an explicit target-side override is supported:

```toml
[overleaf]
enabled = true
native_adapter = ["/usr/bin/python3", "/absolute/prepared/integrations/overleaf/adapter.py", "--node", "/absolute/node"]
```

Owner `overleaf.login.resources/node/browser` settings are desktop paths. The
optional `native_adapter` override contains worker paths. Installed wheels need
neither override. Resource discovery also supports prepared bundles in the
standard per-user data directory; there is no automatic network download.

The immutable upstream pin is
[`42135403af5e76b80348303d742a5e1fe4da6d30`](https://github.com/happysky19/overleaf-mcp-native/commit/42135403af5e76b80348303d742a5e1fe4da6d30).
Preserve its AGPL license, notices and exact corresponding source. It retains
the no-OT-replay correction and anonymous inherited-FD session transport on
macOS/Linux. Fridica does not reimplement its browser or native protocol.

## Deployment and validation limits

Keep the previous binary/configuration and consistent state backup when changing
an existing installation. Use that owner's established idle activation workflow;
login/preparation does not reload or start services, and an already running
worker does not gain capability from a file edit. Do not start a second listener
for the same owner. Source changes and synthetic tests do not certify a published
release, real login, authenticated Overleaf operations or activation.

The installed-wheel smoke exercises account-only preparation, the real installed
native/built-in worker entry without a Git token, and missing-target refusal using
synthetic credentials. Focused adapter checks cover A/B targeting, legacy refusals,
identity mismatch and no replay offline. No live operation is implied. The
historical Darwin receipt below applies only to the stated earlier resources;
it is not a macOS execution of this account-UX change.

## Credential-free Darwin check before deployment

An owner-side verification receipt dated **2026-11-24** reports successful macOS
execution using Python **3.11.12** and standalone Node **24.22.3**, against the
matching `portable12030afb` resources. The receipt confirms the bundle SHA-256
`9fde67d2a6fe2e5588b512ec0bf5bd0a8aeb04414870774dec98f1675ccf094f`.
The following command ran from those resources and returned
**`Ran 2 tests in 1.882s`, `OK`**. This is supplied owner-side evidence, not an
execution by the source worker; the command was not rerun for this documentation
update.

```sh
cd "$FRIDICA_RESOURCES"
TASK_NODE="$NODE_BIN" TASK_TMP="$FRIDICA_ROOT" python3 -m unittest test_adapter.AdapterChecks.test_actual_pinned_upstream_discovery_and_memory_session test_adapter.AdapterChecks.test_receipt_prevents_replay_after_success_and_unknown_outcome
```

The checks verified actual inherited-FD consumption, caching and closure,
upstream MCP initialization/tool listing, absence of a cookie file, and receipt
prevention of replay after both success and unknown outcomes. Execution used
scratch `HOME`/`TMPDIR` and synthetic fixtures only; the scratch was emptied.
The owner also reports a separate Darwin inherited-FD/cache/fail-closed probe
and directory `fsync` passed; their commands were not supplied in this receipt.
No browser, Keychain, real account, Overleaf request, model worker, service or
configuration change was involved. Private receipt records remain local; no
owner paths are published here.

Safe receipts use exclusive creation, file and directory `fsync`, and atomic
replacement in the assigned workspace on both platforms. A persistence error
before dispatch refuses the write; never suppress it or bypass the receipt.
These fixture checks do not establish actual sandboxed local/SSH launch or
authenticated native operation, and did not activate a listener or another
user's machine.
