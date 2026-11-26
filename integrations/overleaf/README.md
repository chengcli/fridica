# Native Overleaf through Fridica

The existing `overleaf` MCP combines built-in Interleaf Git/compile tools with
`@netique/overleaf-mcp` in a separate process. It exposes native comment list,
full-thread read, anchored creation, reply, file read, tracked-change inspection
and pending suggestions. Suggestions require a current `expected_version`,
`strict_version=true` and `track=on`. Accept/reject and untracked native edits
are not exposed.

## Standalone owner login

```sh
fridica overleaf-auth login --expected-email alice@example.com
# Later: reuse the verified account and its valid saved session.
fridica overleaf-auth login
```

Use your own expected account; humans enter passwords/MFA. Login is independent
of projects, channels and workers, and does not initialize a model client.
Node >=20, Chrome/Chromium and an unlocked OS credential store are needed on
macOS or desktop Linux. `--config`, `--node` and `--browser` are optional overrides.
`--prepare-only` checks resources and saves configuration without login, network
or credential access. Login enables `[overleaf]` in that config; neither command
starts or reloads a listener.

The helper reuses upstream dedicated-profile login, saved cookies and
`validateCookie` account identity validation. It checks the expected email before
importing the session to Fridica's OS store via stdin. A valid saved session is
reused until actual expiry or revocation; account mismatch and transient server
errors do not launch another login. There is no forced renewal interval. Normal
browser resources and human-selected SSO remain upstream behavior; everyday
profiles, password/MFA fields and provider cookies are not extracted.

## Automatic workers, explicit projects

Install a matching Fridica wheel and Node on registered local/SSH workers.
The installed wheel supplies resources under `share/fridica/overleaf-native`;
Fridica discovers them on the execution target. Ordinary launch admission must
allow the placement, backend and channel. The workspace must permit the existing
Overleaf integration (`write`/`full`, unrestricted network, no `fetch_repos`).
Do not expand a placement's policy merely to enable tools. Tool approvals remain
in force; attaching capability is not authorization for unrelated work.

The supervisor derives channel and recipient from the admitted job and worker
record. The launcher rechecks the exact registered placement and MCP command
before forwarding the session using the existing local/SSH credential route.
Normal setup needs no manually entered worker UUID, adapter path or Git token.
The parent/coordinator and diagnostic probes receive no native session. Doctor
checks the installed native/built-in tools without a Git token and reports that
authentication remains unverified until an admitted task. The
adapter supplies only its upstream child through an anonymous inherited socket;
no worker credential file or browser fallback is used. Built-in Git/compile
retains its separate credentials and workspace sandbox.

Every native call requires a `project_id` from the authorized task. These two
fictional calls can run in the same channel on the same worker:

```json
{"name":"native_list_comments","arguments":{"project_id":"0123456789abcdef01234567"}}
{"name":"native_list_comments","arguments":{"project_id":"abcdef0123456789abcdef01"}}
```

Extract the ID from the task's Overleaf editor URL; never guess. Fridica refuses
missing targets and checks account/project identity for the requested page on
each call. Upstream's existing project selector and serialization lock handle
switching. No active-session default can silently retarget a call.

Every write requires a stable `operation_id` UUID. Safe durable receipts prevent
replay after success or uncertainty; read/reconcile ambiguous outcomes instead
of retrying with a new UUID. Receipt digests include the project and payload.
These are application restrictions around an account-wide cookie, not
server-issued cryptographic project scopes.

## Optional restrictions and source deployments

Existing `native_scope.project`, `native_scope.channel`, `native_recipient`,
`native_adapter` and saved schema-2 grants remain effective. Login retains omitted
legacy restrictions; renewal cannot widen saved restrictions or change account.
Schema-2 forwarding still requires configured recipient and channel grants;
missing either denies forwarding. A deliberate new login creates schema 3.
An absent project restriction on a new schema-3 session supports explicitly
requested projects. Its absent recipient restriction uses trusted admission,
not arbitrary workers. An absent account scope still denies forwarding.

For deliberately narrow deployments, retain a project/channel scope and use
`fridica overleaf-auth bind-recipient` with an actual registered worker. Optional
`--expires-at` applies only to an owner-requested timed grant. Removing an OS
entry alone cannot revoke already running copies or upstream's dedicated store:
stop holder workers and invalidate/clear that dedicated session as needed.
Do not drop old restrictions as an incidental upgrade step.

Source installation and explicit `native_adapter` overrides remain supported;
see the [source guide](../../docs/overleaf-native-source-install.md). Owner
`login.resources/node/browser` paths are not copied into remote worker launch.

## Dependency and narrow correction

Upstream is `@netique/overleaf-mcp@0.4.0`, source
[`cd248f856ba63af449eb58cb0eb0af04fd933ac0`](https://github.com/netique/overleaf-mcp/tree/cd248f856ba63af449eb58cb0eb0af04fd933ac0).
Its published registry integrity is
`sha512-gJtiF9tUWWB2GPwBTPIWNyNu49L7wLkwq8oBmGhRYtKJixu4RqukewtM0VMZ9rtmKrhCzbUf1E/aOx9u/ocrZg==`.
The integration pins the no-replay correction and inherited-session transport at
[`happysky19/overleaf-mcp-native@42135403af5e76b80348303d742a5e1fe4da6d30`](https://github.com/happysky19/overleaf-mcp-native/commit/42135403af5e76b80348303d742a5e1fe4da6d30),
branch `fix/inherited-session-fd-20261124`. Its package-lock integrity is
`sha512-T40Szds5KDonX/iDwPP/Rl3BksZgww7rVc6Sul4b7AUTp1LIC9o918lXeaZ1k93YxG2Ks3cN4cF7VjbnuPmpxw==`.

The fork corrects `applyOtUpdate` in `src/api/socket.ts`, retains one lost-ACK
check and an explicit `prepare` build hook, and adds the optional `OL_SESSION_FD`
transport with one focused injection/cache/fail-closed smoke. It never automatically resubmits an
attempted mutation. It clears stale document state, reconnects/re-reads when
possible, and returns an uncertain outcome for caller reconciliation. Ordinary owner login,
CDP, dedicated-profile capture, cookie storage and native comment/OT protocol
remain upstream code. Injected worker mode reads a bounded JSON envelope once
from an anonymous inherited descriptor and caches it in memory. It bypasses the
actual cookie store on both macOS and Linux (Darwin ignores `XDG_CONFIG_HOME`),
closes the descriptor, and refuses disk/browser fallback on invalid or expired
sessions. Only the descriptor number is in the upstream environment. Exact
repository/full-SHA HTTPS and SSH npm metadata are accepted; other sources,
branches, shortened SHAs and mismatched versions are rejected. No generated dist or dependencies are committed.

AGPL-3.0-or-later applies to the upstream process and correction. Preserve its
LICENSE/notices and corresponding source. The portable archive contains those
notices and `upstream-source.tar.gz` at the exact fork revision. Fridica's MIT
integration does not copy the native protocol or implement a browser/CDP client.

## Build and focused checks

With Node/npm and the normal Rust/Python build tools on PATH:

```sh
npm ci --prefix integrations/overleaf --no-audit --no-fund
python integrations/overleaf/package-portable.py --wheel-data
maturin build --release --locked --out wheelhouse
scripts/smoke_wheel.sh wheelhouse/*.whl
```

The existing packager includes pure-JS dependencies, adapter/helper, license
notices and exact corresponding AGPL source in Maturin's wheel data directory.
Use `--upstream /absolute/clean/pinned-checkout` to reuse source. A portable bundle
can still be built with `--output /absolute/bundle.tar.gz`. No runtime downloader
is used. Source-only installs without wheel data need explicit resources.

Focused checks are `node --test integrations/overleaf/owner-login.test.mjs`,
`TASK_NODE=/absolute/node TASK_TMP=/scratch python -m unittest discover -s
integrations/overleaf -p test_adapter.py`, the native recipient and selected
supervisor tests, and the installed-wheel smoke. They use synthetic fixtures;
preparation and tool discovery do not prove live authentication, pending
suggestions, deployment or listener activation. The pinned upstream no-replay
correction and its historical lost-ACK regression are retained unchanged.
