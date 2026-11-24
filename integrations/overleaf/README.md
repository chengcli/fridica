# Scoped native Overleaf MCP integration

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
The integration pins the necessary no-replay correction at
[`happysky19/overleaf-mcp-native@e4ad4f97d5730a448e5a2c3aa60838ef2d1f8981`](https://github.com/happysky19/overleaf-mcp-native/commit/e4ad4f97d5730a448e5a2c3aa60838ef2d1f8981),
branch `fix/no-ot-replay-20261124`. Its package-lock integrity is
`sha512-m+xcz1MuYENEntPC+ANfYz/L2cBhkWbTOW4EqifSbEEuyG63XkBJKZIQGGXUQyzYuRKCe2qQuRIT+0A4yWnbTw==`.

The fork changes only `applyOtUpdate` in `src/api/socket.ts`, adds one lost-ACK
check and an explicit `prepare` build hook. It never automatically resubmits an
attempted mutation. It clears stale document state, reconnects/re-reads when
possible, and returns an uncertain outcome for caller reconciliation. Login,
CDP, dedicated-profile capture, cookie storage and native comment/OT protocol
remain upstream code. No generated dist or dependencies are committed.

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
Overleaf cookies and verifies the expected account and fixed SNAP V3 page over
HTTPS. Account/project mismatch or a transient server failure does not trigger
an automatic new login. No 30-minute lease is imposed.

Upstream's dedicated store uses a private directory and a mode-0600 cookie file;
the dedicated Chrome profile is retained. The verified, scoped envelope also goes
to Fridica's OS credential store through the `import-verified-session` stdin
bridge. No cookie is placed in argv, config, logs or stdout. Session renewal
cannot silently change the saved account. The optional `--expires-at` is only
for an explicitly timed grant. Otherwise reuse ends on actual server/cookie
expiry or owner revocation.

The existing encrypted SSH stdin route forwards only to the explicitly bound
actual `dungeon3:snapy` worker UUID in job channel `C0CL1NW0DGS`. All native calls
are fixed to SNAP V3 `6b0483a71b304b7fd2141775` and the seven methods above. The
worker independently rechecks account/project before operations and supplies
upstream through a sealed Linux memfd. Worker browser fallback is disabled.
Coordinator/other-worker forwarding remains denied. Built-in Git/compile behavior
is retained. These are application gates around an account-wide session cookie,
not server-issued cryptographic project scopes.

Every write requires a stable `operation_id` UUID. Durable safe receipts prevent
adapter replay; reuse the UUID when an outcome is unknown. Re-read/reconcile an
uncertain comment anchor or tracked suggestion; never retry with a new UUID as
a substitute. Existing comments and the tracked trial must not be duplicated.
Revoke by removing the OS entry and binding, stopping holder workers, clearing
the dedicated upstream store/profile and invalidating that Overleaf session.
Deleting the OS entry alone does not invalidate saved or running copies.

## Install and invoke

The portable archive is built on dungeon3. It contains JS dependencies and needs
no npm installation or compiler on the Mac. Use the available Node executable
and Google Chrome. After transferring the archive and checking its reported
SHA-256, install on the owner Mac:

```sh
mkdir -p "$HOME/Library/Application Support/Fridica"
tar -xzf fridica-overleaf-portable-20261124.tar.gz -C "$HOME/Library/Application Support/Fridica"
/Applications/ChatGPT.app/Contents/Resources/node "$HOME/Library/Application Support/Fridica/overleaf-native/owner-login.mjs" --expected-email "$OVERLEAF_EXPECTED_EMAIL" --fridica "$FRIDICA_FEATURE_BIN" --config "$FRIDICA_OWNER_CONFIG" --worker-id "$AUTHORIZED_WORKER_ID"
```

The email, absolute feature-binary/config paths and actual recipient UUID are
explicit owner inputs, not inferred from a channel or another agent. Complete
the human login/MFA if Chrome opens. The helper saves the verified session and
binds the recipient; reload through ordinary owner controls. It does not start
a worker, send a message or modify a manuscript.

**Platform requirement:** the host listener must run this Fridica feature branch
for the import bridge and recipient gates. The portable JS archive does not
upgrade an existing listener. A compatible macOS Fridica binary must be supplied
by the normal build/distribution path (Rust >=1.91, macOS SDK/linker and ordinary
crate dependencies if building). This job builds on dungeon3 only; the retained
Linux candidate cannot run on macOS. No macOS build, human login, OS-store write,
listener activation or live native write is claimed by this package.

On the worker, use the installed adapter and Node >=20; Python >=3.10 and Linux
memfd are required. Preserve existing `[overleaf]` settings, adding:

```toml
[overleaf]
enabled = true
native_adapter = ["/absolute/python3", "/assigned/folder/fridica/integrations/overleaf/adapter.py", "--node", "/absolute/node"]
```

For source deployment on dungeon3, run `npm ci --no-audit --no-fund` in this
directory with Node/npm on PATH. The pinned Git dependency's explicit prepare
hook builds its dist; do not use `--ignore-scripts`. To recreate the portable
archive, run `python package-portable.py --upstream /assigned/folder/upstream
--output /assigned/folder/fridica-overleaf-portable-20261124.tar.gz`.

## Focused verification

The affected owner helper check is `node --test owner-login.test.mjs`. The one
existing Rust boundary smoke is `cargo test --test native_recipient --locked
--offline`. The fork's one lost-ACK check is
`node --test --import tsx tests/unit/socketLostAck.spec.ts`; it fails before the
correction and passes after it. Package/tool discovery is credential-free.
These checks do not authenticate or modify Overleaf. Live login, read-only
comment/change inspection and any expressly requested new write remain separate
verification steps; no extra review round or broad test matrix is required.
