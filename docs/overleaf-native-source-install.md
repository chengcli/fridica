# Native Overleaf: install the integration from public source

> Linux source installation and credential-free login preparation passed for this
> change. The npm HTTPS/SSH source-metadata mismatch in [issue #162](https://github.com/chengcli/fridica/issues/162)
> is corrected; the exact repository, immutable SHA, version and module checks remain.
> macOS execution, real login and listener activation remain separately unverified.

This is a trial of `integrate/overleaf-upstream-20261124`, **not** official main
or PyPI v0.10.3. It builds the branch's own development version. Use a separate
virtual environment and configuration; retain your old executable, config and
state backup. Do not assume another release's database is compatible. No private
archive or model-client application is needed. A prepared public checkout can
serve directly as `--resources`.

## 1. Build and prepare on the owner's desktop

Use macOS or desktop Linux with Python **>=3.11**, its venv/pip support,
Rust **>=1.91** and Cargo, Git, Node **>=20** and npm on PATH. Install the ordinary
platform C/C++ compiler, linker and development headers/SDK (for example, Xcode
Command Line Tools on macOS). Actual login also needs Chrome/Chromium and an
unlocked OS credential store: Keychain on macOS, or Secret Service in a user
D-Bus session on Linux. A headless SSH host is the worker, not the login desktop.

Run this in a POSIX shell. `FRIDICA_ROOT` is a **new, unused**, owner-controlled
directory; change its example location if necessary. The other paths are derived
from it. The commands fetch the named public branch, record its full remote SHA
once, and build that detached revision. For a repeat install, assign
`FRIDICA_REV` your previously recorded full SHA instead of resolving a new head;
fetch the named branch first and require that revision to be in its history.

```sh
FRIDICA_ROOT="$HOME/fridica-native-trial"
FRIDICA_BRANCH=integrate/overleaf-upstream-20261124
test ! -e "$FRIDICA_ROOT"
mkdir -m 700 "$FRIDICA_ROOT"
git clone --no-checkout --branch "$FRIDICA_BRANCH" https://github.com/chengcli/fridica.git "$FRIDICA_ROOT/source"
FRIDICA_REV="$(git -C "$FRIDICA_ROOT/source" rev-parse "refs/remotes/origin/$FRIDICA_BRANCH")"
git -C "$FRIDICA_ROOT/source" merge-base --is-ancestor "$FRIDICA_REV" "refs/remotes/origin/$FRIDICA_BRANCH"
git -C "$FRIDICA_ROOT/source" checkout --detach "$FRIDICA_REV"
printf '%s\n' "$FRIDICA_REV"
python3 -m venv "$FRIDICA_ROOT/venv"
. "$FRIDICA_ROOT/venv/bin/activate"
cd "$FRIDICA_ROOT/source"
FRIDICA_BUILD_ID="$FRIDICA_REV" python -m pip install .
cd integrations/overleaf
npm ci --no-audit --no-fund
FRIDICA_RESOURCES="$(pwd -P)"
FRIDICA_BIN="$FRIDICA_ROOT/venv/bin/fridica"
FRIDICA_CONFIG="$FRIDICA_ROOT/config/config.toml"
NODE_BIN="$(command -v node)"
"$FRIDICA_BIN" build-info
```

Stop on any failed command. `pip install .` downloads build dependencies and
compiles/installs binaries into this venv. `npm ci` replaces this checkout's
`node_modules`, downloads locked dependencies and runs the pinned Git
dependency's normal `prepare` hook to build its JS. Do **not** use
`--ignore-scripts`. No global package installation, listener start or login is
part of these commands. Keep the checkout/resources in place after installation.
Node must resolve to an absolute executable; if `command -v node` reports a shell
function, set `NODE_BIN` to the real executable instead.

The lock retains `@netique/overleaf-mcp@0.4.0` at
[`42135403af5e76b80348303d742a5e1fe4da6d30`](https://github.com/happysky19/overleaf-mcp-native/commit/42135403af5e76b80348303d742a5e1fe4da6d30),
including the no-replay correction and inherited-FD session transport. Preserve its AGPL license/notices and
corresponding source when redistributing; see the [integration notes](../integrations/overleaf/README.md#dependency-and-narrow-correction).

## 2. Prepare, then let the human log in

Fictional example: account `alice@example.com`, authorized editor URL
`https://www.overleaf.com/project/0123456789abcdef01234567`. Its project ID is
the 24-character string immediately after `/project/`: `0123456789abcdef01234567`.
Substitute your authorized values below. **Agents: extract the ID from the
authorized editor URL, never guess; leave password/MFA entry to the human.**
`FRIDICA_RESOURCES` above is the absolute prepared `integrations/overleaf` path.

```sh
OVERLEAF_EMAIL=alice@example.com
OVERLEAF_PROJECT=0123456789abcdef01234567
"$FRIDICA_BIN" overleaf-auth login --prepare-only --config "$FRIDICA_CONFIG" --expected-email "$OVERLEAF_EMAIL" --project "$OVERLEAF_PROJECT" --resources "$FRIDICA_RESOURCES" --node "$NODE_BIN"
"$FRIDICA_BIN" overleaf-auth login --config "$FRIDICA_CONFIG"
```

The first command creates private setup files beside the config, saves the
explicit scope/resource paths and prepares a cached helper linked to these
dependencies. It opens no browser, credential store, network or model backend.
The second command verifies the expected account/project over HTTPS and saves
the verified session in the OS store. If needed, a dedicated browser profile
opens for human login/SSO/MFA; everyday browser profiles are not read. Login
works from an ordinary terminal without Codex, Claude Desktop or a worker UUID.

Later, run `"$FRIDICA_BIN" overleaf-auth login --config "$FRIDICA_CONFIG"`.
A valid saved session is reused until actual expiry/revocation, with no forced
renewal interval. For a previously verified session, use only `--prepare-only`
when changing builds/resources; keep its account/project/channel, dedicated
profile, OS entry and existing binding. Do not log in again just to upgrade.

`--config` is optional for the default owner config, but use it consistently for
this isolated trial. `--browser /absolute/chromium` overrides system discovery.
Resources can also be a prepared companion bundle; automatic discovery checks
`overleaf-native` beside the config, `<prefix>/share/fridica/overleaf-native`,
`~/Library/Application Support/Fridica/overleaf-native`, and
`$XDG_DATA_HOME/fridica/overleaf-native` (default `~/.local/share/fridica/overleaf-native`).
Public PyPI alone does not supply these integration resources.

## 3. Prepare an eligible registered macOS or Linux worker

Login and forwarding are separate. Without an explicit channel and recipient,
forwarding is denied. Use an owner-authorized registered **macOS or Linux, local or SSH** machine
and workspace with effective policy `mode = "write"` or `"full"`,
`network = ["*"]`, and no `fetch_repos` restriction. Do not broaden an existing
policy merely to make the adapter available. The adapter uses POSIX `socketpair`/`pass_fds`; Windows workers are not supported.

On that worker, install Git, Node >=20/npm and Python >=3.10. No Linux memfd,
procfs, worker cookie file or worker browser is needed. Installing this
Fridica wheel from source requires Python >=3.11 and the build prerequisites
above. The builtin workspace sandbox needs Linux bubblewrap/user namespaces or the
existing macOS sandbox support, according to the selected machine. Worker backend authentication is needed for normal Fridica jobs;
it is independent of owner login. Native reads need no worker browser or
manual cookie transfer. Keep existing Git/compile credentials and tools for
the ordinary Overleaf workflow.

Use the same clone/venv/pip/npm commands from section 1 **on the selected
worker**, with a new worker-owned `FRIDICA_ROOT`, and set `FRIDICA_REV` to the
owner's recorded full revision. Do not substitute a newer branch head. Record
the absolute outputs of `pwd -P` in `integrations/overleaf`, `command -v node`,
and `command -v python3`; run `"$FRIDICA_BIN" build-info`. The worker needs its
own prepared dependencies. Do not copy another owner's environment/session.

The launcher calls `fridica` on the worker's PATH. Make the matching venv visible
to the local or noninteractive SSH launch, for example using the existing supported
`machines.<name>.python_env` field. Do not replace a research venv unless that
change is appropriate for the existing jobs; an already configured compatible
environment can install this wheel instead.

Here is a **configuration fragment**, not a replacement config. Fictional owner
Alice has SSH alias `alice-lab`, registered machine `lab`, workspace `papers`,
Linux install `/home/alice/fridica-native-trial`, Node `/usr/bin/node`, Python
`/usr/bin/python3`, and authorized Slack channel `C012ABCDEF0`. Substitute actual
discovered values and merge into existing tables without duplicate TOML tables.
Keep your owner/Slack identity, backend settings, other channel entries and
workspace policies. These values grant no access by themselves.

```toml
[slack]
channels = ["C012ABCDEF0"] # retain the rest of your authorized allowlist

[machines.lab]
transport = "ssh"
host = "alice-lab"
backends = ["codex"] # your already configured supported worker backend
python_env = "/home/alice/fridica-native-trial/venv"

[machines.lab.workspaces]
papers = { path = "/home/alice/papers", policy = { mode = "write", network = ["*"], fetch_repos = [] } }

[overleaf]
enabled = true
native_adapter = ["/usr/bin/python3", "/home/alice/fridica-native-trial/source/integrations/overleaf/adapter.py", "--node", "/usr/bin/node"]

[overleaf.native_scope]
expected_email = "alice@example.com"
project = "0123456789abcdef01234567"
channel = "C012ABCDEF0"
```

For an SSH Mac, keep `transport = "ssh"` and use its actual host alias and
absolute `/Users/...`/Node/Python paths. For a worker on the owner's own Mac or
Linux desktop, register `transport = "local"` and omit `host`; for example:

```toml
[machines.desktop]
transport = "local"
backends = ["codex"]
python_env = "/Users/alice/fridica-native-trial/venv"

[machines.desktop.workspaces]
papers = { path = "/Users/alice/papers", policy = { mode = "write", network = ["*"], fetch_repos = [] } }
```

Use that local machine's discovered adapter/Node/Python paths in `[overleaf]`.
On Linux substitute the actual Linux paths. Bind the actual `desktop` placement
instead of `lab` when selecting this alternative; this is a configuration
example, not permission to run workers on someone else's machine. The same
account/project/channel/worker checks apply on both launch routes. Local launch
uses Fridica's existing process credential route; SSH uses encrypted stdin.
Only the adapter's upstream child gets the anonymous session FD, with no secret
in its argv/environment and no worker credential store. Darwin's normal upstream
cookie-store location is bypassed in injected mode, regardless of XDG settings.

`overleaf.login.resources/node/browser` are desktop paths written by login;
`overleaf.native_adapter` contains **worker** paths. Keep that distinction.
Do not manually invent a `native_recipient` table. For a fresh trial, also give
`[state] path` and `control_socket` unused absolute paths (socket path under 100
bytes); a separate `--config` alone does not isolate the default state database.
Follow [ordinary setup](../README.md#initialize-fridica) for owner/Slack/backend
configuration before starting a listener. Do not run two listeners for the same
Slack owner. This guide does not migrate official-release state automatically.

## 4. Discover, bind explicitly, and switch only when idle

For an existing compatible listener, `FRIDICA_CONFIG` must identify its actual
owner config/control socket. These read-only owner controls report real IDs:

```sh
"$FRIDICA_BIN" status --config "$FRIDICA_CONFIG"
"$FRIDICA_BIN" machines --config "$FRIDICA_CONFIG"
"$FRIDICA_BIN" threads --config "$FRIDICA_CONFIG"
"$FRIDICA_BIN" workers --config "$FRIDICA_CONFIG"
```

Use `threads`' `key.channel` and `id`; inspect that thread with
`fridica threads THREAD_ID --config CONFIG_PATH`, replacing `THREAD_ID` with its
returned full ID and `CONFIG_PATH` with the same absolute config path. Its
`workers` records identify the actual `id`, `machine`, `workspace`, `slot` and
`session_id`. Cross-check `machines` and `workers`; a slot number or backend
session ID is **not** a worker UUID. If no worker exists, use the owner's normal
Fridica delegation workflow to create a persistent worker for that authorized
thread and placement, then discover its identity. Login can already be complete.
For initial channel discovery before any thread exists, the supported
`fridica configure --config CONFIG_PATH --detect` uses the owner's Slack setup;
review its selected channel list because it replaces that list, not appends it.

Make a sibling config copy for activation. In the commands below, `CHANNEL_ID`,
`WORKER_ID`, `MACHINE_NAME` and `WORKSPACE_NAME` must be the observed values;
`NEXT_CONFIG` must not already exist. The following values are fictional syntax
examples, **not runnable binding authorization**:

```sh
CHANNEL_ID=C012ABCDEF0
WORKER_ID=worker-12345678-1234-4234-8234-123456789abc
MACHINE_NAME=lab
WORKSPACE_NAME=papers
NEXT_CONFIG="$FRIDICA_ROOT/config/native-next.toml"
test ! -e "$NEXT_CONFIG"
cp -p "$FRIDICA_CONFIG" "$NEXT_CONFIG"
chmod 600 "$NEXT_CONFIG"
"$FRIDICA_BIN" overleaf-auth bind-recipient --config "$NEXT_CONFIG" --channel "$CHANNEL_ID" --worker-id "$WORKER_ID" --machine "$MACHINE_NAME" --workspace "$WORKSPACE_NAME"
```

This writes `overleaf.native_recipient.worker_id/machine/workspace` and
`overleaf.native_scope.channel`; it reads no credential and starts/forwards
nothing. Leave optional `expires_at`/`--expires-at` unset unless the owner wants
a timed grant. A valid existing binding needs no repeat bind: preserve its
table and prepare only the matching resources/scope in the copied config.

Before activation, prevent new admissions using the owner's established controls
and wait for `status` to show zero `running_jobs`, `queued_jobs` and
`pending_approvals`; inspect `workers`/the relevant thread as well. Retain the old
binary/config and a consistent state backup. Configure the **actual existing**
service or launch command to use the matching new binary and `NEXT_CONFIG` with
the same OS user/keyring environment. There is no universal service name and
no `fridica reload` command. Use that owner's existing service manager's normal
idle restart workflow, not a guessed unit/plist or a second listener. If running
manually, the supported launch is `"$FRIDICA_BIN" start --config "$NEXT_CONFIG"`
only after the old listener has stopped. Recheck status/placement afterward.

The binding takes effect on worker launch/resume under the new configuration;
an already running worker does not gain credentials by editing a file. Reuse
the same persistent worker through ordinary controls. If it has been replaced,
discover the new actual UUID and obtain an explicit binding for that recipient.
Do not weaken the gate or reuse an unrelated worker. The existing local process or encrypted SSH
stdin path forwards the verified session, and the adapter rechecks account,
project, methods, channel and recipient before native calls.

## 5. Read-only native verification

In the authorized thread, ask **the bound worker** to use its existing `overleaf`
MCP server's `tools/list`, then the following `tools/call` requests. These are MCP
tool names/arguments, not additional shell subcommands. Replace the fictional
project ID with the authorized one. There must be no native writes or dummy data.

```json
{"name":"native_list_comments","arguments":{"project_id":"0123456789abcdef01234567","include_resolved":true}}
{"name":"native_list_tracked_changes","arguments":{"project_id":"0123456789abcdef01234567","limit":200,"full_text":true}}
```

If comments exist, copy one actual 24-character `thread_id` from the list into
`native_read_comment_thread` with arguments
`{"project_id":"0123456789abcdef01234567","thread_id":"ACTUAL_RETURNED_THREAD_ID"}`.
Never invent a thread ID or create a comment to test reading. An empty list is
valid; report the thread-read step as skipped. Report truncation if the tracked
change result hits its limit. Stop on auth/scope failures; do not retry with
another account, project or worker. Pending Track Changes are native objects,
not TeX diffs. Preparation alone does not prove authentication or operation.

## Credential-free Darwin check before deployment

The following invocation is provided for an owner to run on macOS; it was **not
executed on a Mac** during this change. It performs an actual upstream MCP
initialize/tool-list handshake with a fake cookie, verifies no worker cookie
file is created, then exercises existing safe-receipt persistence/replay checks.
It opens no browser, contacts no Overleaf server and runs no model worker. Use
the matching prepared resources and an ordinary Node executable:

```sh
cd "$FRIDICA_RESOURCES"
TASK_NODE="$NODE_BIN" TASK_TMP="$FRIDICA_ROOT" python3 -m unittest test_adapter.AdapterChecks.test_actual_pinned_upstream_discovery_and_memory_session test_adapter.AdapterChecks.test_receipt_prevents_replay_after_success_and_unknown_outcome
```

Safe receipts use exclusive creation, file and directory `fsync`, and atomic
replacement in the assigned workspace on both platforms. A persistence error
before dispatch refuses the write; never suppress it or bypass the receipt.
Darwin filesystem behavior, actual sandboxed launch and SSH forwarding remain
owner-side verification steps. Keep activation deferred if these fail.

## Verification limits

The installation/preparation smoke is on Linux with isolated owner configuration
and no credentials, model backend, browser or listener. Desktop browser login,
OS-store use, SSH forwarding and live native calls require each owner's separate
authorized activation. macOS source build/activation was not exercised by this
smoke; these commands are instructions, not a claim that another owner's setup
has been activated.
