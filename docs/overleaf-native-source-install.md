# Native Overleaf development setup

## Install and sign in

Use matching reviewed development revisions of Fridica, fridica-agent and
Interleaf on the owner machine and registered workers. A matching Fridica wheel
bundles the Rust native backend; no Node, npm, Python adapter or worker browser is
needed. Python 3.11+ may be used to install the wheel. Chrome/Chromium is needed
only for an owner-driven fresh sign-in. macOS uses Keychain; Linux needs an
unlocked Secret Service in the owner's D-Bus session.

```sh
fridica overleaf-auth login --expected-email owner@example.com
# For self-hosted Overleaf / Community Edition:
fridica overleaf-auth login --base-url https://overleaf.example.org --expected-email owner@example.com
```

Enter passwords/MFA yourself. Later login calls reuse the same verified account
and valid saved session; only a missing session or confirmed expiry opens the
browser. Account mismatch, malformed saved data and transient failures stop.
The configured origin is explicit, and credentials are never silently moved to
another host. Use `--browser /absolute/chrome` for explicit browser selection and
`--config /absolute/config.toml` consistently for a nondefault configuration.
`--prepare-only` checks readiness without credentials, a browser or network access.
Neither login nor grant installs a build, starts or reloads a listener.

## Existing sessions

Upgrading source does not widen a saved grant. Existing sessions keep their seven
previously authorized operations until the owner opts into expanded tools:

```sh
fridica overleaf-auth grant --all
```

No relogin is needed. The schema-4 grant expands tool capabilities only; saved
account, project, channel, recipient and expiry restrictions stay in force. It
cannot unlock another paper or channel. Keep any intentionally narrow existing
scope, for example:

```toml
[overleaf.native_scope]
expected_email = "owner@example.com"
project = "0123456789abcdef01234567"
channel = "C012ABCDEF0"
```

The channel must already be allowed. Use `fridica overleaf-auth bind-recipient
--help` only when an explicit worker binding is intended. Changing the saved
account requires revocation. To revoke running access, stop holders and invalidate
the Overleaf session as well as removing the OS-store entry.

## Use in a task

Give the requesting thread the authorized Overleaf editor URL. Unbound tasks may
discover, search and read within their saved scope. Each project operation names
its exact `project_id`; never guess the ID. Before a project mutation, Fridica
must confirm that project for the requesting thread. Changing tool arguments
cannot rebind the task. Project creation requires an unbound task with
unrestricted account/project scope.

```json
{"name":"native_list_comments","arguments":{"project_id":"0123456789abcdef01234567"}}
```

The broker checks the admitted worker, account, channel, task project and expiry.
It supplies credentials only through Rust memory pipes; model processes get a
socket locator. Worker browser/cookie files are unnecessary. Tool approvals and
workspace confinement still apply.

The [capability matrix](../integrations/overleaf/README.md#capabilities) lists all
33 canonical tools plus `native_suggest_edit` and `native_compile_local`.
`native_compile` runs on Overleaf's cloud compiler; `native_compile_local` builds
a verified live-text snapshot locally and requires TeX Live/`latexmk`. Local
packages/fonts and exported asset consistency can differ from the cloud build.
Git mode remains a separate token-based checkout workflow.

Read a file's current version before editing. Canonical edits support complete
`new_content` or exact replacements and tracking `on`, `off` or `auto`; existing
pending suggestions must be preserved. Server-forced tracking is reported even
if `off` was requested. The legacy suggestion alias remains pending-only.
Accept/reject requires precise selection and exact versions per document.
Message edit/delete verifies original content and author. Edits target only your
own messages; deleting another author's content requires an explicit project-owner
request. The review switch changes only you.

Every mutation needs a stable `operation_id` UUID. Reusing the same ID and
arguments reads its durable receipt instead of writing again. Unknown/partial
outcomes require read-only reconciliation, never a new UUID, automatic replay
or destructive rollback. Preserve receipts when migrating execution hosts.

## Source builds and release gates

Source builds need Rust >=1.91, Cargo, a platform compiler/SDK, Git and Python
>=3.11. Record the intended reviewed heads with `git rev-parse HEAD`; use matching
dependency revisions across the three repositories. Once dependency requirements
are satisfied, install through the standard build path:

```sh
python3 -m venv .venv
. .venv/bin/activate
python -m pip install .
```

There is no npm/resource preparation step or separate native executable. See
[build and validation](../integrations/overleaf/README.md#build-and-validation).
Registry publication requires matching released dependency APIs and maintainer
approval. Keep package/publish dry-run and normal CI gates; an unavailable
registry dependency is a gate to resolve, not a reason to skip verification.

## Acceptance and activation

Source/protocol tests and synthetic wheel smoke tests do not prove live-account
behavior, all 33 tools on a real server, Mac wheel acceptance or release readiness.
Expanded live-account and Mac acceptance remain pending. Historical adapter tests
do not certify this Rust backend. These source instructions do not update the
running service.

Before an authorized deployment, preserve the prior binary/configuration and a
consistent state backup. Follow the owner's idle activation process and keep one
listener. Publication, account acceptance and runtime activation are separate
steps subject to their existing maintainer/owner approvals.
