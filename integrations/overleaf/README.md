# Scoped native Overleaf MCP integration

**Installing from public source?** Follow the [source installation, owner login
and local/SSH worker guide](../../docs/overleaf-native-source-install.md). It covers
pip/npm preparation, actual recipient discovery, idle activation and native reads
without a private archive. This feature-branch trial is separate from official
main and PyPI v0.10.3.

The existing `overleaf` server composes bundled Interleaf Git/compile tools with
`@netique/overleaf-mcp` in a separate process. Native comment list, full-thread
read, anchored creation, reply, file read, pending tracked-change inspection and
tracked suggestions are enabled by the adapter. Suggestions require an observed
version, `strict_version=true` and `track=on`; accept/reject and untracked native
writes are not exposed. A source/package check is not live Overleaf validation.

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

## One owner login and reuse

`owner-login.mjs` uses upstream's saved-cookie store, or its original
`captureCookie` function when login is needed. Upstream owns the persistent
**dedicated** Chrome profile, Network domain, location check and ordinary
human-controlled login/SSO window. These are accepted upstream behavior. The
helper never reads an everyday browser profile, password/MFA fields or provider
cookies. It selects only `overleaf_session2` and optional `GCLB` from the returned
Overleaf cookies and verifies the expected account and explicitly configured project page over
HTTPS. Account/project mismatch or a transient server failure does not trigger
an automatic new login. No 30-minute lease is imposed.

Upstream's dedicated store uses a private directory and a mode-0600 cookie file;
the dedicated Chrome profile is retained. The verified, scoped envelope also goes
to Fridica's OS credential store through the `import-verified-session` stdin
bridge. No cookie is placed in argv, config, logs or stdout. Session renewal
cannot silently change the saved account. The optional `--expires-at` is only
for an explicitly timed grant. Otherwise reuse ends on actual server/cookie
expiry or owner revocation.

The existing local launch or encrypted SSH stdin route forwards only to the explicitly bound
actual worker UUID, registered machine/workspace and configured job channel.
All native calls are fixed to the single configured project and seven methods above. The
worker independently rechecks account/project before operations and supplies
upstream through a portable anonymous socketpair and `pass_fds`, with no worker
credential file. Worker browser fallback is disabled.
Coordinator/other-worker forwarding remains denied. Built-in Git/compile behavior
is retained. These are application gates around an account-wide session cookie,
not server-issued cryptographic project scopes.

Every write requires a stable `operation_id` UUID. Durable safe receipts prevent
adapter replay; reuse the UUID when an outcome is unknown. Re-read/reconcile an
uncertain comment anchor or tracked suggestion; never retry with a new UUID as
a substitute. Existing comments and tracked suggestions must not be duplicated.
Revoke by removing the OS entry and binding, stopping holder workers, clearing
the dedicated upstream store/profile and invalidating that Overleaf session.
Deleting the OS entry alone does not invalidate saved or running copies.

## Standalone owner login

Use `fridica overleaf-auth login` from an ordinary desktop terminal; Codex and
Claude Desktop are not required. Login does not initialize a model backend,
start a listener/worker or require a worker UUID. macOS and desktop Linux need
Node >=20, Chrome/Chromium and a working OS credential store (macOS Keychain or
Linux Secret Service). Node is found on PATH or standard system locations;
`--node /absolute/node` overrides it. Upstream discovers Chrome/Chromium (and its
other supported browsers); `--browser /absolute/browser` overrides discovery.
No model client, npm or compiler is required to use a prepared portable bundle.

Use the matching Fridica integration build and either the prepared public-source
directory from the guide or a supplied companion bundle; installing the public
pip release alone does not provide this setup. For a bundle, check the archive's
supplied SHA-256 and extract it into an owner-controlled directory.
The extracted `overleaf-native` directory carries
pinned JS dependencies, licenses and corresponding AGPL source. A release may
place it in `<prefix>/share/fridica/overleaf-native`; automatic discovery also
checks beside the owner config, `~/Library/Application Support/Fridica` on macOS,
and `$XDG_DATA_HOME/fridica` (default `~/.local/share/fridica`) on Linux. Otherwise
pass `--resources`. A prepared bundle needs no dependency download or npm
installation. A source checkout needs the guide's locked npm preparation first.

For this fictional example, the expected account is `alice@example.com` and the
editor URL is `https://www.overleaf.com/project/0123456789abcdef01234567`.
The project ID is the 24-character string immediately after `/project/`:
`0123456789abcdef01234567`. Assume the extracted `overleaf-native` directory is
in your home directory's `Downloads` folder (`$HOME/Downloads/overleaf-native`).
Substitute your own authorized account, project ID and real bundle location:

```sh
fridica overleaf-auth login --expected-email alice@example.com --project 0123456789abcdef01234567 --resources "$HOME/Downloads/overleaf-native"
```

Complete the browser login and any MFA yourself. Fridica verifies the expected
account and project before saving the session. Subsequent invocations reuse
the saved scope/resources and valid session:

```sh
fridica overleaf-auth login
```

AI agents: extract the project ID from the authorized editor URL; never guess it.
Leave password and MFA entry to the human.

Use `--config /absolute/config.toml` for a nondefault owner config, substituting
its actual absolute path; use the same option on subsequent invocations. Missing config
is initialized through ordinary Fridica setup; Slack and workers may be configured
later. Login parses only Overleaf settings. On first use,
Fridica materializes its embedded helper into a private, versioned directory
beside that config and links the selected bundle's pinned dependencies. It checks
the upstream version/source pin and loads the auth modules before opening any
browser. `--prepare-only` saves/validates scope/resources without a browser,
network, credential-store read/write or model initialization. The helper verifies
the exact expected account and project over HTTPS before saving an unbound
session. Human login/MFA is needed only when the upstream saved session is absent
or actually expired. No artificial renewal interval is imposed.

Login saves a session; it does not enable forwarding. Recipient binding is a
separate, explicit owner action once a worker and its adapter are ready. Use
`fridica overleaf-auth bind-recipient` with the actual `--config`, `--worker-id`,
registered `--machine` and `--workspace`, and authorized `--channel` values.

The channel must be in the owner's Slack allowlist. Missing scope, missing
channel or missing recipient denies forwarding. The launcher compares saved
account/project to the explicit scope before encrypted forwarding; the adapter
independently verifies identity/project on every native operation. Optional
`--expires-at UNIX_SECONDS` applies only to an owner-requested timed grant.
Neither command activates or reloads the listener; use existing owner controls
when idle. Configuration is not authorization to access another account/project.

### Upgrade from the prototype

Keep the existing native session entry, dedicated upstream profile/cookie store,
`native_adapter` argv/dependencies and `native_recipient` table. Their formats are
unchanged. Make a sibling copy of the owner config for the candidate deployment.
Before activating the new listener, use the new binary's `login --prepare-only
--config /absolute/copied-config.toml` with the **same** expected email, project
and channel, and the supplied bundle. This adds explicit scope without re-login, credential reads,
rebinding or writes to worker resources. The old listener cannot reload the new
configuration fields: retain a pre-upgrade config/binary for rollback and switch
through the existing idle owner activation workflow. Until explicit scope is
present, the new listener denies native forwarding. No implicit owner/project
fallback is shipped. Bound scopes cannot be silently changed by `login`.

This source change is checked on Linux. A compatible macOS feature binary and
owner-side activation are still required on macOS; the Linux binary cannot run
there. Building Fridica requires Rust >=1.91 and the ordinary platform linker/SDK.
The portable archive is platform-independent JS, not a Fridica binary or Node
runtime. Live authentication and native writes are separate from resource checks.

On macOS or Linux workers, use the installed adapter, Node >=20 and Python
>=3.10 with POSIX `pass_fds` and `socketpair`. No memfd or procfs is required. Preserve existing `[overleaf]` settings, adding:

```toml
[overleaf]
enabled = true
native_adapter = ["/absolute/python3", "/assigned/folder/fridica/integrations/overleaf/adapter.py", "--node", "/absolute/node"]
```

For source deployment, run `npm ci --no-audit --no-fund` in this
directory with Node/npm on PATH. The pinned Git dependency's explicit prepare
hook builds its dist; do not use `--ignore-scripts`. To recreate the portable
archive, run `python package-portable.py --upstream /assigned/folder/upstream
--output /assigned/folder/fridica-overleaf-portable-20261124.tar.gz`.

## Focused verification

For this compatibility delta, run `node --test owner-login.test.mjs` here and
`node --test tests/unit/sessionFd.spec.mjs` in the built upstream checkout.
The adapter's existing actual-process smoke is
`TASK_NODE=/absolute/node TASK_TMP=/assigned/scratch python3 -m unittest test_adapter.AdapterChecks.test_actual_pinned_upstream_discovery_and_memory_session`.
The source guide records credential-free installation/preparation and the
unexecuted Darwin handshake/receipt command. All execution for this delta was
on Linux, with no live login or manuscript access.

Existing preparation/recipient coverage remains in `tests/native_recipient.rs`;
it was not rerun for this JS/Python delta. The fork's retained lost-ACK check is
`node --test --import tsx tests/unit/socketLostAck.spec.ts`; it fails before the
earlier no-replay correction and passes after it (historical result, not rerun
here). Package/tool discovery is credential-free.
These checks do not authenticate or modify Overleaf. Live login, read-only
comment/change inspection and any expressly requested new write remain separate
verification steps; no extra review round or broad test matrix is required.
