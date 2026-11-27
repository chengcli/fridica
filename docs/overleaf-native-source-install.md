# Overleaf account login and worker setup

## Installed wheel

Install a matching Fridica wheel in the owner's Python 3.11+ environment and on
each registered worker. Native Overleaf is bundled Rust through fridica-agent
and Interleaf; it requires no Node, npm, Python adapter or worker browser.
Chrome/Chromium is needed on the owner's desktop only for a fresh sign-in.
Use Keychain on macOS or an unlocked Secret Service in a Linux user D-Bus session.

```sh
fridica overleaf-auth login --expected-email owner@example.com
```

Enter passwords/MFA yourself in the isolated browser window. An optional
`--browser /absolute/chrome` selects Chrome/Chromium explicitly. Subsequent
`fridica overleaf-auth login` calls reuse the same saved account and valid OS-store
session. Only a missing session or confirmed expiry opens a browser during this
owner command; account mismatch and transient errors do not trigger a new login.
Use `--config /absolute/config.toml` consistently for a nondefault configuration.
`--prepare-only` saves preferences and checks readiness without credential,
browser or network access. Login does not start/reload the daemon or change
workspace/channel permissions.

## Tasks and tools

Give each manuscript task its authorized Overleaf editor URL. Extract the
24-character ID after `/project/`; never guess. Several projects may share a
channel, with a separate project grant for each admitted task. Every native call
requires that task's `project_id`:

```json
{"name":"native_list_comments","arguments":{"project_id":"0123456789abcdef01234567"}}
```

Fridica's local Rust broker checks the admitted worker, channel and project,
then passes the native session through memory pipes. The environment carries a
broker socket locator, never the raw cookie. Workers run the bundled Rust server;
no cookie files, browser fallback or adapter paths are needed. Native mode exposes the seven
comment/file/Track Changes tools and `native_compile`, with no Git-write tools,
native accept/reject or untracked native edits. Suggestions require the exact
native version and stay pending. Mutations use stable UUIDs and durable receipts;
uncertain outcomes require read-only reconciliation without replay. Compilation
requires worker TeX Live/`latexmk` and supplies native snapshot/build evidence.
Git mode retains its separate token credentials and local Git workflow.

## Existing restrictions

Upgrade and renewal retain existing account/project/channel/recipient limits,
including saved schema-2 restrictions. Normal setup needs no worker UUID binding.
For an intentionally narrow configuration, keep its existing optional scope:

```toml
[overleaf.native_scope]
expected_email = "owner@example.com"
project = "0123456789abcdef01234567"
channel = "C012ABCDEF0"
```

The channel must already be allowed. Use `fridica overleaf-auth bind-recipient
--help` only for an explicit binding to a registered worker. Changing the saved
account requires revocation. Removing the OS entry alone does not revoke running
copies; stop holders and invalidate the corresponding Overleaf session.

## Source installation

Source builds need Rust >=1.91, Cargo, a platform compiler/SDK, Git and Python
>=3.11. Preserve existing source/runtime state, check out the intended revision,
and record `git rev-parse HEAD`:

```sh
python3 -m venv .venv
. .venv/bin/activate
python -m pip install .
fridica overleaf-auth login --expected-email owner@example.com
```

The normal wheel build includes the Rust native backend. No npm step, resource
archive, `--resources` override or separate native executable is required. See
[build and verification](../integrations/overleaf/README.md#build-and-verification).

## Deployment limits

Preserve the previous binary/configuration and a consistent state backup when
changing an existing installation. Follow the owner's idle activation process;
do not start a second listener. Source changes and synthetic wheel tests do not
certify publication, live login, authenticated manuscript operations or runtime
activation. Historical tests of the retired adapter do not certify this backend.
