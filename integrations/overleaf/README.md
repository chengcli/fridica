# Native Overleaf through Rust Interleaf

Fridica bundles its native Overleaf backend through fridica-agent and Rust
Interleaf. Native operation needs no Node, npm, Python adapter or worker browser.

## Owner login

```sh
fridica overleaf-auth login --expected-email owner@example.com
```

Enter passwords and MFA yourself in the isolated Chrome/Chromium window. Use
`--browser /absolute/chrome` only when automatic browser discovery is unsuitable.
Later `fridica overleaf-auth login` reuses the verified account and valid session
from the same OS credential-store entry. A browser opens only for a missing
session or confirmed expiry during an owner-invoked login; account mismatch,
malformed saved data and transient failures stop instead. macOS uses Keychain;
Linux needs an unlocked Secret Service in the owner's D-Bus session.

Login is independent of projects, channels, worker IDs and model clients.
`--prepare-only` saves preferences and checks Rust readiness without reading
credentials, opening a browser or making a network request. Use `--config` for a
nondefault configuration. Neither command starts or reloads a listener.

## Authorized worker operations

Install a matching Fridica build on each registered worker. Normal launch
admission, workspace confinement, channel permissions and tool approvals still
apply. Fridica's local Rust broker checks the admitted worker, channel and task
project, then supplies the native session through memory pipes. Environment
values contain only the broker socket locator, never the raw cookie. Workers do
not read browser stores or run an owner login. The parent/coordinator receives
no native session.

Supply the editor URL in the requesting Slack thread. Every tool call requires
that task's explicit `project_id`; another project needs another authorized task.
Native mode exposes only:

- `native_list_comments`, `native_read_comment_thread`, `native_reply_comment`
- `native_read_file`, `native_list_tracked_changes`
- `native_suggest_edit`, `native_add_comment`, `native_compile`

Suggestions remain pending and require the exact native version. Every mutation
uses a stable UUID and durable receipt; an uncertain result requires read-only
reconciliation, never automatic replay or a replacement UUID. Legacy receipts
remain authoritative during migration. Native accept/reject, untracked edits
and Git writes are unavailable in this mode. Native compilation requires TeX
Live/`latexmk` on the worker and reports the exact snapshot/build evidence.
Git mode remains separate and uses its existing Git-token credentials.

Existing account/project/channel/recipient restrictions remain effective across
upgrade and renewal. Account-wide cookies are restricted by Fridica's admission
and call guards, not by an Overleaf-issued per-project credential. Revocation
requires stopping holders and invalidating their Overleaf session as well as
removing the saved OS entry.

## Build and verification

Use the normal Rust/Python wheel workflow; no native resource preparation exists:

```sh
maturin build --release --locked --out wheelhouse
scripts/smoke_wheel.sh wheelhouse/*.whl
```

The installed-wheel smoke prepares account-only configuration, discovers all
eight native tools with no Node/Python helper on PATH, and refuses a missing
project using synthetic data. It does not validate a real login, manuscript
operation, deployment or listener activation. See the [installation guide](../../docs/overleaf-native-source-install.md).
