# Scoped native Overleaf MCP integration

This adapter composes the existing bundled Interleaf Git/compile tools with a
separate, **unmodified** `@netique/overleaf-mcp@0.4.0` process. It exposes native
comment list/full-thread read/reply and pending-change inspection under the
existing `overleaf` server. Anchored comment creation and pending tracked edits
are wired but **not activated**: this pin retries `applyOtUpdate` after an ambiguous
socket-close failure. The required no-replay guarantee cannot be supplied by an
outer MCP adapter. Do not describe this candidate as fully ready.

## Pin and license

- Upstream source: [netique/overleaf-mcp@cd248f856ba63af449eb58cb0eb0af04fd933ac0](https://github.com/netique/overleaf-mcp/tree/cd248f856ba63af449eb58cb0eb0af04fd933ac0).
- npm version: `0.4.0`; registry SHA-512 integrity:
  `sha512-gJtiF9tUWWB2GPwBTPIWNyNu49L7wLkwq8oBmGhRYtKJixu4RqukewtM0VMZ9rtmKrhCzbUf1E/aOx9u/ocrZg==`.
- `package-lock.json` pins the complete process dependency graph. Install with
  `npm ci --ignore-scripts --no-audit --no-fund` in this directory (Node >=20).
- Upstream is AGPL-3.0-or-later. Its package retains `LICENSE`, `README.md`, and
  `CHANGELOG.md`. Preserve those notices and provide the pinned corresponding
  source when redistributing. The adapter calls MCP over stdio; it imports,
  ports, or copies no upstream implementation into Fridica's MIT Rust code.

## Capabilities verified in the pinned source

The source anchors below all refer to the full upstream commit above.
`src/tools/comments.ts` registers `list_comments`, `read_comment_thread`,
`add_comment`, and `reply_comment`. Full messages come from
`read_comment_thread`; the `list_comments(full=true)` implementation does not
actually populate `full_thread`, so use the dedicated read tool.
`add_comment` posts the first message then anchors it with a comment OT operation.
`src/ot/editPipeline.ts` submits native `meta.tc` updates;
`src/tools/trackedChanges.ts` lists pending native ranges. This is native Track
Changes, not TeX annotation, latexdiff, or a Git edit. Source inspection and
credential-free tool discovery do **not** establish live Overleaf behavior.

`src/api/socket.ts` (`withReconnectingSocket`, `applyOtUpdate`) retries an OT emit
once after a socket-close error, including closure after submission but before
acknowledgement. There is no configuration switch to disable it. Consequently
`native_add_comment` and `native_suggest_edit` fail before any network call.
The smallest remaining dependency change is upstream support for refusing
ambiguous OT retries; then pin that unmodified release and remove the two-entry
`REPLAY_UNSAFE` gate after the retained reviewer checks the delta. No upstream
source is patched here, and no issue/PR is created by this branch.

The adapter exposes only seven native operations: list comments, read thread,
add (gated), reply, read file, list tracked changes, and suggest edit (gated).
Suggestions require an observed native version, `strict_version=true`, and
`track=on`. Accept/reject, tracking-mode changes, comment resolution/deletion,
project discovery/creation, and direct untracked native edits are not exposed.
Built-in Interleaf retains its existing Git/compile behavior and permissions.

## Authentication and scope

The host owner config binds one actual worker UUID on `dungeon3:snapy`.
The supervisor additionally requires job channel `C0CL1NW0DGS`. All native calls
are fixed to project `6b0483a71b304b7fd2141775`. The launcher denies credential
lookup/forwarding by default, strips ambient copies from other workers and the
coordinator, and forwards the bound session using the existing encrypted SSH
stdin framing. Never put the cookie in argv, TOML, logs, or ordinary files.

The previously reviewed `fridica overleaf-auth sign-in` owner helper is reused.
It opens a fresh human-controlled Chrome window on macOS using anonymous CDP
pipes; the human supplies credentials/MFA/SSO. It polls only cookies applicable
to the fixed Overleaf project URL and selects `overleaf_session2` plus `GCLB`.
It has no request subscriptions, Network.enable, Fetch.enable, DOM scripts,
existing-profile access, provider-cookie reads, global cookie dumps, or browser
URL firewall. Independent fixed-project HTTPS verification checks the expected
email and binds the account ID. Cleanup precedes saving to the dedicated OS
credential store. Failed/cancelled acquisition does not save a session.

Upstream's packaged `overleaf-mcp login` is deliberately **not used**: at this
pin it enables the network domain, evaluates a page script and uses a persistent
profile. Neither the inspected official Fridica base nor this pinned upstream
package supplies a compatible packaged owner login command. The retained
Fridica owner helper must be available on the owner platform; a Linux candidate
or a passing `--help` check is not a macOS build/login verification. This change
performs no Mac build or login and does not install or activate a listener.

The worker adapter rechecks account ID/email and project with a fixed HTTPS
request before each native operation, disallowing proxies and redirects. The
upstream subprocess then performs its own identity bootstrap via `/project` and
connects only to the supplied project. The project-list tool is never exposed.
A sealed Linux memfd, reached through a temporary symlink, supplies upstream's
cookie store; there is no secret-bearing worker disk file. Other credentials,
NODE_OPTIONS, proxy variables and browser settings are not inherited by that
process. Browser fallback is disabled with an explicit nonexistent executable:
auth expiry cannot open another login or trigger an HTTP mutation retry.
Session validity ends at actual cookie/server expiry, owner revocation, or an
explicit optional deadline; there is no artificial 30-minute lease.

An Overleaf cookie remains an account-wide bearer. These are application gates,
not server-issued project scopes. A trusted worker holding a credential is not
cryptographically prevented from bypassing its adapter. Revocation requires
removing the owner entry/config binding, stopping holder sessions and invalidating
the dedicated Overleaf session; deleting a saved entry alone is insufficient.

## Deployment and owner invocation

Provision the inspected Fridica branch with its ordinary dependencies, and
install the adapter on the worker under its assigned writable folder. No
changes to Interleaf or fridica-agent are needed. The old custom branches are
historical recovery inputs, not build dependencies of this branch.

In the owner config, keeping the current `[overleaf]` settings, set:

```toml
[overleaf]
enabled = true
native_adapter = ["/absolute/python3", "/assigned/folder/fridica/integrations/overleaf/adapter.py", "--node", "/absolute/node"]
```

With the owner-platform helper installed, the one-time commands are:

```sh
fridica overleaf-auth sign-in --expected-email "$OVERLEAF_EXPECTED_EMAIL"
fridica overleaf-auth bind-recipient --config "$FRIDICA_OWNER_CONFIG" --worker-id "$AUTHORIZED_WORKER_ID" --machine dungeon3 --workspace snapy
```

These placeholders must be the existing account email, real owner configuration,
and intended actual recipient. `--expires-at UNIX_SECONDS` is optional only for
an expressly timed grant. Load the updated owner configuration using the normal
Fridica owner controls. Subsequent allowed operations reuse the stored session.
No sign-in is performed by installation or tool discovery. Existing comments
and the pending tracked trial must not be replayed, accepted, or rejected.

Every native write requires a stable `operation_id` UUID. A receipt containing
only a payload hash/state/safe identifiers is persisted before submission; the
adapter never retries writes and rejects reused IDs with changed payloads.
Unknown outcomes require reading/reconciling the existing thread. Keep the
`.fridica-overleaf-receipts` directory with the assigned workspace. A new UUID
is a new operation; callers must reuse IDs for retries.

## Focused checks

```sh
TASK_NODE=/absolute/node TASK_TMP=/assigned/folder python -m unittest discover -s integrations/overleaf -p test_adapter.py -v
cargo test --lib overleaf
cargo test --lib secrets::tests
cargo test --test native_recipient --test worker_credentials
cargo test --test parent native_recipient_never_includes_parent_backend
cargo build --bin fridica
```

The four Python checks cover actual pinned subprocess discovery/memory-only
credential delivery, scope/tracking/replay gates, expected account/project, and
durable receipt replay prevention. The Rust checks cover the retained acquisition
selection and credential route. None logs in or mutates Overleaf. Live login,
read-only inspection of existing comments/changes, and any expressly requested
new write remain separate evidence stages.
