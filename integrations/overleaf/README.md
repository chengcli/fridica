# Native Overleaf through Rust Interleaf

This development source connects Fridica → fridica-agent → Rust Interleaf for
33 canonical native tools and two compatibility helpers. Native operations need
no Node, npm, Python adapter or worker browser. It does not change an installed
release or start a listener.

## Sign in once

On the owner machine, with a matching development build:

```sh
fridica overleaf-auth login --expected-email owner@example.com
# Self-hosted Overleaf / Community Edition:
fridica overleaf-auth login --base-url https://overleaf.example.org --expected-email owner@example.com
```

Enter passwords/MFA yourself in the isolated Chrome/Chromium window. Later
logins reuse the verified account and valid OS-store session. macOS uses
Keychain; Linux needs an unlocked Secret Service in the owner's D-Bus session.
Use `--browser /absolute/chrome` only to select a browser explicitly, and use
`--config` consistently for a nondefault configuration. `--prepare-only` checks
Rust readiness without credentials, a browser or a network call. A saved session
is never silently retargeted to a different origin.

Existing sessions retain their seven authorized operations. The owner may expand
only the tool capabilities, without another login:

```sh
fridica overleaf-auth grant --all
```

The schema-4 capability grant retains saved account, project, channel, recipient
and expiry restrictions. It cannot widen those scopes or deploy the new build.
Normal task admission, tool approvals and workspace confinement still apply.

## Projects and tasks

Give a manuscript task its Overleaf editor URL. An unbound task can discover
projects and search/read a named project within its saved scope. Each project
call carries an explicit `project_id`. Before any project mutation, the host must
confirm that project for the requesting thread; tool arguments alone cannot
rebind a task. Creating a project is available only to an unbound task with
unrestricted account/project scope. Several papers may share one allowed channel.

Fridica's Rust broker checks the worker, channel, task, project, account and
expiry, then passes the session through memory pipes. Model processes receive
only a socket locator, never the cookie. Workers do not read browser stores or
perform owner login. Git mode retains its separate token and checkout workflow.

## Capabilities

| Capability | Tools |
| --- | --- |
| Connection and discovery (3) | `native_ping`, `native_list_projects`, `native_open_project` |
| Tree, search and reads (3) | `native_list_files`, `native_search_project`, `native_read_file` |
| Project/file management (7) | `native_create_project`, `native_create_doc`, `native_create_folder`, `native_upload_file`, `native_rename_entity`, `native_move_entity`, `native_delete_entity` |
| Text edits (2) | `native_edit_file`, `native_find_and_replace` |
| Comments (9) | `native_list_comments`, `native_read_comment_thread`, `native_reply_comment`, `native_add_comment`, `native_resolve_comment`, `native_reopen_comment`, `native_delete_comment`, `native_edit_comment_message`, `native_delete_comment_message` |
| Track Changes (4) | `native_list_tracked_changes`, `native_set_track_changes`, `native_accept_changes`, `native_reject_changes` |
| Cloud build (3) | `native_compile`, `native_read_log`, `native_label_pages` |
| Native history (2) | `native_list_history`, `native_diff_file` |

These are the 33 canonical tools. The helpers are `native_suggest_edit`, an exact
pending-only replacement, and `native_compile_local`, local TeX compilation of a
verified live-text snapshot with exported assets. `native_compile` uses the
Overleaf cloud compiler. Only the local helper requires worker TeX Live/`latexmk`;
its packages/fonts may differ, and exported assets are not one atomic snapshot.

Text edits require the exact native version, preserve unrelated pending records
and verify fresh readback. Canonical edits support tracking `on`, `off` or `auto`;
a server-forced tracking switch is reported, never silently bypassed. Legacy
suggestions remain pending. Accept/reject requires exact IDs or explicit filters
and document versions. Comment edit/delete checks original content and author. Message edits are
limited to your own messages; deleting another author's content requires an
explicit project-owner request. The tracking
switch changes only the logged-in user.

Every mutation uses a stable UUID and a durable receipt. Keep the same UUID and
arguments when checking its outcome. Lost acknowledgements or partial readback
remain uncertain; use read-only reconciliation. There is no automatic replay,
replacement UUID or destructive rollback. Preserve receipt state across host
migration. Removing the saved OS entry alone does not revoke running holders;
stop them and invalidate the corresponding Overleaf session when revoking access.

## Build and validation

Use matching reviewed source revisions for Interleaf, fridica-agent and Fridica,
and retain their normal package and CI gates. Once dependency requirements are
satisfied, the standard wheel workflow includes the Rust backend:

```sh
maturin build --release --locked --out wheelhouse
scripts/smoke_wheel.sh wheelhouse/*.whl
```

Protocol fixtures and wheel smoke checks do not certify real-account operation,
Mac acceptance, release publication or listener activation. Expanded live-account
and macOS acceptance remain pending. Registry publication requires matching
released dependencies and maintainer review; do not skip package/publish dry-run
or CI when those dependencies are unavailable. See the
[installation guide](../../docs/overleaf-native-source-install.md).
