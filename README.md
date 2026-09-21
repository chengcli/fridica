# Fridica

Fridica is a local persona that connects your Slack identity to Claude Code
or Codex. It listens in channels you choose, decides when to participate, works in
configured project directories, and replies in Slack threads as you. Replies use
your first-person voice, without a Fridica introduction or visible signature.
Machine-readable metadata remains for loop protection; legacy signed messages
are still recognized. Your own messages never trigger your agent.

This first release runs one owner per daemon and one Slack app per owner. Anyone
in an allowed channel can trigger workspace actions. It includes both CLI
backends, SQLite context and task storage, clarification conversations, and loop
limits. It does not include a shared relay, browser OAuth onboarding, MCP server,
or automation of the Claude/Codex desktop UI.

## Install

Use macOS or Linux with Python 3.11 or newer. Install and authenticate either
[Claude Code](https://code.claude.com/docs/en/setup) or
[Codex CLI](https://developers.openai.com/codex/cli) before you install fridica.


### Sandbox dependencies on Linux

Both backends run agent commands inside a [bubblewrap](https://github.com/containers/bubblewrap)
sandbox on Linux; macOS uses the built-in `sandbox-exec` and needs nothing extra.

| Backend | `bubblewrap` | `socat` | Notes |
| --- | --- | --- | --- |
| Claude | required (system package) | required | Fridica starts Claude with `sandbox.failIfUnavailable`, so Claude refuses to run at all when either is missing |
| Codex | bundled, but a system `bwrap` on `PATH` is preferred | not needed | Installing the system package lets one AppArmor profile cover both backends |

**1. Install the packages.**

```bash
# Debian / Ubuntu
sudo apt install bubblewrap socat

# Fedora
sudo dnf install bubblewrap socat
```

**2. Allow bubblewrap to create user namespaces (Ubuntu 24.04 and later).**
Ubuntu's default AppArmor policy blocks unprivileged user namespaces, so every
sandboxed command fails with `bwrap: loopback: Failed RTM_NEWADDR: Operation not
permitted` even though the packages are installed. Check the setting:

```bash
sysctl kernel.apparmor_restrict_unprivileged_userns
```

If it prints `1`, install an AppArmor profile that grants `bwrap` the
capability (the profile applies to `bwrap` only, not to the commands it runs
inside the sandbox), then reload AppArmor:

```bash
sudo tee /etc/apparmor.d/bwrap > /dev/null <<'EOF'
abi <abi/4.0>,
include <tunables/global>

profile bwrap /usr/bin/bwrap flags=(unconfined) {
  userns,
  include if exists <local/bwrap>
}
EOF
sudo systemctl reload apparmor
```

If it prints `0` or `No such file or directory`, skip this step. This follows
[Claude Code's sandboxing guide](https://code.claude.com/docs/en/sandboxing);
Codex uses the same system `bwrap`, so the profile fixes both backends.

**3. Verify.**

```bash
fridica doctor   # expect: PASS AI sandbox
```

`doctor` checks that the packages are present and then runs `/bin/true` inside a
bubblewrap user namespace, so it fails with the exact `bwrap:` error when either
step above is incomplete. `start` runs the same check and refuses to launch on
failure. Without it, a running daemon would return the generic "I couldn't
complete this request" reply within seconds for any request that needs a
command. The local log records the agent's exit status and a bounded tail of its
stderr; that diagnostic text is never sent to Slack.

`init` creates `~/.config/fridica/config.toml`, without overwriting existing
configuration, and copies two editable files beside it: `manifest.yaml` for the
Slack app and `contract.md`, the rulebook every agent run reads (see
[Agent contract](#agent-contract)). For a different location, use
`fridica init --config /path/config.toml`.

### Install via pypi
```bash
pip install fridica
fridica init
```

### Install locally to an existing python virtual environment
```bash
git clone https://github.com/chengcli/fridica
pip install -e .
fridica init
```

## Configure Slack

Use **one Slack app per person**, with a user token for that person's account.
Do not share tokens. Other channel members need no installation to interact with
your running Fridica; anyone in an allowed channel can request workspace actions.
Separate owners must not share an app: multiple Socket Mode connections divide
events rather than broadcasting them to every connection. See
[Slack's Socket Mode documentation](https://docs.slack.dev/apis/events-api/using-socket-mode/).

### 1. Create the app and set permissions

Open [Slack app management](https://api.slack.com/apps), choose **Create New App →
From an app manifest**, select your workspace, and paste
[`slack/manifest.yaml`](slack/manifest.yaml). The manifest sets up public channels.
Verify these settings before installation:

| Slack settings page | Setting | Required value |
| --- | --- | --- |
| Socket Mode | Enable Socket Mode | On |
| Basic Information → App-Level Tokens | Generate token and scopes | `connections:write`; save the `xapp-` token |
| Event Subscriptions | Enable Events | On |
| Event Subscriptions → Subscribe to events on behalf of users | Public-channel event | `message.channels` |
| OAuth & Permissions → User Token Scopes | Public-channel messages | `channels:history` |
| OAuth & Permissions → User Token Scopes | Channel information and membership checks | `channels:read` |
| OAuth & Permissions → User Token Scopes | Send replies as your account | `chat:write` |
| OAuth & Permissions → User Token Scopes | User-profile access included in the manifest | `users:read` |

`users:read` is included for user-profile access, but current mention rendering
does not require a profile lookup. **Bot Token Scopes and bot event subscriptions
are not used.** You do not need a public Request URL with Socket Mode.

For **private channels**, also add these before installation:

| Slack settings page | Additional values |
| --- | --- |
| OAuth & Permissions → User Token Scopes | `groups:history`, `groups:read` |
| Event Subscriptions → Subscribe to events on behalf of users | `message.groups` |

Keep the public-channel settings if monitoring both types. DMs and group DMs are
not supported. The authorized person must belong to every configured channel.
Event subscriptions and their corresponding scopes are both necessary; see
[Slack's Events API](https://docs.slack.dev/apis/events-api/) and
[private-channel events](https://docs.slack.dev/reference/events/message.groups/).

**Do not add unrelated scopes:**
- `links:read` and `links:write` are for shared-link events and custom unfurls;
  ordinary replies containing URLs need only `chat:write`. Fridica disables unfurls.
- File upload/download, reactions, channel administration, email, and bot mention
  scopes are not needed for current functionality. Local workspace file access
  is controlled by the agent, not Slack scopes.
- `metadata.message:read` was listed in older manifests, but Slack documents it
  as a **bot/legacy-bot scope**, not a user-token scope. Do not add a bot token just
  for this scope. Fridica still attempts to attach metadata to outgoing replies;
  metadata availability and acceptance are separate from ordinary message access.
  See [Slack's metadata scope reference](https://docs.slack.dev/reference/scopes/metadata.message.read/)
  and [link permissions](https://docs.slack.dev/messaging/working-with-files/).

### 2. Install and export tokens

Under **OAuth & Permissions**, select **Install to Workspace** and authorize as
the person Fridica will represent. An administrator may need to approve the app
and requested scopes. Copy the **User OAuth Token** starting with `xoxp-`, not a
bot token (`xoxb-`). Both tokens must belong to the **same app and intended workspace**.

For `SLACK_APP_TOKEN`, open **Basic Information → App-Level Tokens** and copy the
`xapp-` token already generated in step 1 with `connections:write`. Token generation
should be complete by this point; reuse that token rather than creating another.
For `SLACK_USER_TOKEN`, use the `xoxp-` **User OAuth Token** from
**OAuth & Permissions** after installation.

Export them in the terminal where Fridica will run:

   ```bash
   export SLACK_APP_TOKEN='xapp-your-token'
   export SLACK_USER_TOKEN='xoxp-your-token'
   ```

After changing scopes, **reinstall the app**, update the exported user token if
Slack replaces it, and restart Fridica. Save event-subscription changes as well.
Never commit tokens to Git or put them in an agent-accessible workspace.

### 3. Configure your local identity and channels

After `fridica init` and exporting your user token, detect your identity and
choose channels from a numbered list:

```bash
fridica configure --detect
```

This gets `owner_id` and `workspace_id` from Slack's
[`auth.test`](https://docs.slack.dev/reference/methods/auth.test/) and discovers
joined, non-archived channels using
[`conversations.list`](https://docs.slack.dev/reference/methods/conversations.list/).
Select channel numbers separated by commas; Fridica saves their IDs automatically.
It never enables all discovered channels without your selection. Blank input
cancels without changing the file. Private-channel discovery requires `groups:read`;
if a channel-type read scope is missing, a warning explains which scope to add.
Receiving private messages still requires `groups:history` and `message.groups`.
Detection does not require AI setup or the app-level token and posts no messages.

For noninteractive setup, select by channel **name**, not ID:

```bash
fridica configure --detect --channel-name general --channel-name my-project
```

Names must uniquely match discovered channels. Repeat `--channel-name` to select
multiple channels. Unknown names or failed discovery leave the file unchanged.

Manual ID options remain available:

```bash
fridica configure --owner-id U123ABC --workspace-id T123ABC --channel-id C123ABC
fridica configure --channel-id C123ABC --channel-id G456DEF
```

Each option is optional, but supply at least one. Repeated `--channel-id` options
**replace the full channel list**; omitted settings and TOML comments are preserved.
Use `--config /path/config.toml` for a nondefault file. The command validates ID
formats locally; it does not discover IDs, contact Slack, or change Slack permissions.
Restart Fridica afterward. Owner/workspace IDs must match the user token, and a
different identity needs a separate `state_path` rather than reusing old state.

Edit `~/.config/fridica/config.toml`. Set `owner_id` to the member ID of the person
who authorized the user token, `workspace_id` to the Slack workspace ID, and
`channels` to the exact channel IDs to monitor. Use an existing local project
directory. Choose a backend and authenticate its CLI separately from Slack.

Example configuration (replace the IDs and directory):

```toml
owner_id = "U123ABC"
workspace_id = "T123ABC"
channels = ["C123ABC"]
workspace = "~/projects/my-project"
additional_workspaces = []
backend = "codex"
profile = "I maintain the simulation package and help diagnose test failures."
general_messages = true
context_limit = 50
timeout = 600
cooldown = 60
max_turns = 6
resume_sessions = true
session_timeout = 1209600
allowed_domains = []
```

### Agent contract

`~/.config/fridica/contract.md` holds the rules that every agent run must read
and obey. `init` copies the packaged default there; edit it freely. The daemon
reloads the file for each run, so changes apply to the next reply without a
restart. Two headings are required:

| Section | Sent to | Purpose |
| --- | --- | --- |
| `## Participation` | the tool-less classification call | when to join a conversation that did not @mention you |
| `## Replies` | the call that does the work | voice, scope, what may be changed, how to end a thread |
| any other `##` heading | the call that does the work, after `## Replies` | project or team rules, such as the default's `## Repo rules` |

Prose before the first heading is for people and is never sent to the model.
Fridica appends only the conversation data (owner, profile, task, bounded thread
history, and the new message) plus a one-line note when a thread's session is
being resumed. Rules that the code enforces regardless of the contract: replies
are limited to 3500 characters, the status must be `complete`, `waiting`, or
`blocked`, the sandbox and workspace roots come from `config.toml`, and Slack
tokens never reach the agent. A contract that is missing either heading, has an
empty section, or exceeds 64 KiB fails `fridica doctor`, and until it is fixed
replies are refused with the generic "I couldn't complete this request" notice
while the reason is logged locally.

To keep the contract elsewhere, set `contract = "path/to/rules.md"` in
`config.toml`; relative paths resolve from the config file's directory. Without
that setting Fridica uses `contract.md` beside `config.toml` when it exists,
otherwise the packaged default. The default's text is the previous built-in rule
set, so upgrading without editing changes nothing.

### 4. Verify reception, then replies

1. Run `fridica doctor`. It checks token **format** and local AI setup, not granted
   Slack scopes. `start` checks Slack identity and channel membership.
2. Run `fridica start --observe-only`, then send a **new** `test` message in a
   configured channel. Expect `INFO Observed event ... in ...; no agent or delivery`.
3. Stop with Ctrl-C and run `fridica start`. Have **another person** @mention you
   in a new thread. Your own messages never trigger your agent.

| Symptom | Check |
| --- | --- |
| `Listening as ...`, but no observed event | Enable Events, **user** event subscriptions and matching history scopes, saved changes/reinstallation, channel ID and membership, matching tokens, and no competing Socket Mode daemon |
| Events observed, no reply | Stop observe-only mode; use another person's explicit @mention; check AI setup and local failure logs |
| Reply says "I couldn't complete this request" within seconds | The agent process exited before doing work. Read the `WARNING Agent response unavailable` log line, which includes the agent's stderr tail, and run `fridica doctor`. See [Sandbox dependencies](#sandbox-dependencies-on-linux) |
| `missing_scope` when sending | Confirm `chat:write` is a **User Token Scope**, reinstall, refresh the token if changed, and restart |
| Metadata-related rejection | Inspect the exact error; adding link or unrelated scopes does not fix metadata restrictions |
| Old event reported as failed/interrupted/ambiguous | Inspect locally; restarting does not replay agent actions or uncertain replies |

Observe-only never invokes AI or sends replies. Messages sent before startup are
not fetched. Scopes, subscriptions, membership, and workspace policy all affect
delivery; a successful connection alone does not verify reception.

## Run

```bash
fridica doctor
fridica start --observe-only
fridica start
```

`doctor` prints a PASS or FAIL for each local check: operating system,
configuration, the agent contract, each Slack token's format, AI executable
availability, required CLI flags, sandbox dependencies, and AI sign-in. It runs `claude auth status` or
`codex login status` for the configured backend without invoking a model or
printing account details. The sandbox check confirms that the
[sandbox dependencies](#sandbox-dependencies-on-linux) are on `PATH` and that
bubblewrap can create a user namespace; `start` performs the same check and
refuses to run when it fails.
Independent checks continue after failures; checks blocked by invalid
configuration or a missing executable show SKIP. The command exits nonzero if
any check fails or is skipped. Sign-in status does not guarantee that a later
model request will succeed or that credits are available. `start` verifies the Slack user
and workspace identity and channel membership. `--observe-only` records messages
without invoking either model or posting replies. Stop with Ctrl-C or SIGTERM.
All subcommands accept `--config PATH`; `python -m fridica` is also supported.

Fridica responds to mentions of the owner and follow-ups while a task is waiting
for clarification. Other messages pass through a separate classification call
with tools disabled, governed by the `## Participation` section of the
[agent contract](#agent-contract). Classification failure means silence. Set
`general_messages = false` to disable unsolicited participation. The owner’s own
messages supply context but never directly trigger their agent.

An explicit human @mention always requests a threaded reply, even with general
participation disabled or a cooldown active. If the thread has exhausted its
action budget or an earlier task needs inspection, Fridica replies with a brief
explanation without running more actions or retrying old work. This does not
override observe-only mode, channel restrictions, duplicate suppression, or
automated-message loop protection. Slack rejection or uncertain delivery can
still prevent a reply; inspect the local logs rather than automatically resending.

Only the structured final answer is delivered to Slack. The default contract's
`## Replies` rules exclude internal commentary, unsolicited summaries, and tool
transcripts from that answer;
CLI progress and stderr are never used as reply text. Sanitized failure categories
stay in local logs, while Slack receives a short actionable notice. This output
boundary does not guarantee that a model will never put unwanted prose in its final
answer. Known participant IDs in reply prose become Slack mentions, displayed as
people's names (and potentially notifying them); code and URLs are preserved.
No additional scopes are required for mention rendering. Closing answers
(`complete` or `blocked`) are instructed not to @mention anyone; they should omit
direct address or use plain names. Mentions are reserved for `waiting` replies
that need someone's response. Built-in blocker notices do not mention anyone.

Replies stay in their original thread. Each thread has a persistent six-turn
default budget; use a new thread for a new task after that budget is exhausted.
Generated messages initiate responses only when explicitly addressed or following
an active task. A per-channel cooldown limits unsolicited replies. Other agents'
metadata is a loop-control hint, not an authorization credential.

Messages from other people's Fridica instances arrive as that person's own
messages. If their Slack app also has a bot user, Slack adds a `bot_id` to the
user-token post; Fridica still accepts it because the `user` field names the
sender. Only messages without a `user` (true bot posts) and edits, joins, and
other subtypes are ignored. When an ignored event @mentions you, the daemon logs
a warning naming the event and the fields that caused the rejection so a missing
reply can be traced without reading Slack.

### Continuity between turns

With `resume_sessions = true` (the default), each Slack thread maps to one
backend session. The first reply in a thread starts a session (`claude
--session-id` or a persisted `codex exec` thread) and Fridica stores its
identifier with the thread's task. Later turns resume it with `claude --resume`
or `codex exec resume`, so the agent keeps its own reasoning, tool results, and
file knowledge instead of re-reading the workspace from scratch. The bounded
Slack history is still sent as reference, with the new message marked as the
only new input. Classification calls remain stateless.

A stored session is resumed only while the thread stays active. After
`session_timeout` seconds without a reply in that thread (default `1209600`,
two weeks), the next turn starts a fresh session and stores its identifier; set
`0` to never resume. If the backend reports that a stored session no longer
exists (for example after the provider's session files were removed), Fridica
logs it and starts a fresh session for that thread. Other failures are not retried. Resumed turns use the
same sandbox, permission, and workspace settings as new ones; for Codex, the
`resume` subcommand receives the equivalent `-c` settings because it lacks the
`--sandbox` and `--add-dir` flags. Set `resume_sessions = false` to return to
one ephemeral session per reply.

## Workspace authority

The selected agent can read, edit, and run commands using its provider-supported
sandbox in the configured workspace roots. Task-command network access is
disabled unless `allowed_domains` lists hosts (see
[Network access](#network-access)). Provider API access is still needed to run the model. Claude requires
its [sandbox dependencies](#sandbox-dependencies-on-linux). Fridica does
not enable bypass-permission flags or automatically approve broader access.
Claude enables native Edit and Write tools in `acceptEdits` mode for the workspace
and `additional_workspaces`, alongside sandboxed Bash for file operations such as
renaming or deleting files. Classification still has no tools. Codex continues to
use its `workspace-write` sandbox. No blanket permission-bypass flag is enabled.
OS file permissions, managed policies, and provider-protected paths still apply;
this does not grant administrator access or unrestricted writes outside the roots.
Blocked actions require local intervention; there is no remote approval UI.
Inside a run, Claude may attempt calls the policy forbids, such as network
access or a write outside the roots; the CLI denies each one, tells the model,
and the model continues. The reply is still delivered, the contract requires it
to say what it could not do, and the local log lists every denied call with its
tool and target (command or path, never file contents) so you can widen access
deliberately. Codex enforces the same policy inside its own sandbox.

Only grant access to project directories you intend Slack participants to use.
The provider sandboxes may permit reads beyond writable project directories and
use temporary files; Fridica does not claim complete filesystem read isolation.
Managed provider settings and project instructions remain part of the execution
environment. Slack tokens are removed from agent subprocess environments, but
do not store credentials in project files accessible to the agent.

Fridica supplies its own bounded conversation history for each invocation and,
with `resume_sessions`, resumes only sessions it created; existing desktop
conversations are not imported. The optional `model` setting
is passed to the selected provider. No model name or paid API key is required by
Fridica itself; each CLI uses its own authentication and billing.

### Network access

By default, commands the agent runs cannot reach the network: `git fetch`,
`pip install`, and similar calls are denied inside the run, the model is told,
and the reply says what it could not do. To allow specific hosts, list them:

```toml
allowed_domains = ["github.com", "*.pypi.org"]
```

Entries are host names, optionally with a leading `*.` wildcard, and are
lower-cased. A single `"*"` entry allows every host, which is full internet
access for task commands. They apply only to task runs; classification never has network
access, and the model's own API traffic is unaffected. Restart the daemon after
changing the list.

| Backend | Effect of a non-empty list |
| --- | --- |
| Claude | The sandbox proxy admits outbound requests to the listed hosts only; anything else is denied inside the run. Traffic must pass through the proxy, so HTTPS remotes are the reliable choice; SSH remotes generally do not connect from inside the sandbox. |
| Codex | Codex cannot filter by host, so any entry enables full network access for Codex task commands (`sandbox_workspace_write.network_access=true`). |

Network access lets a Slack request send workspace contents to the listed hosts
and fetch code from them. List only hosts you trust, keep credentials out of the
workspace roots, and remember that anyone in an allowed channel can trigger a
run. Leave the list empty to keep the previous behavior.

### Scoped file access

Set `file_access = true` to replace native agent tools with checked file
operations. This is opt-in; the default workspace mode above is unchanged.

```toml
file_access = true
workspace = "/absolute/path/project/docs"
additional_workspaces = []
read_only_workspaces = ["/absolute/path/project/data"]
```

`workspace` and `additional_workspaces` are the maximum writable roots.
All listed roots are readable, including their descendants; read-only roots
always take precedence. Unlisted paths are refused. Keep the config, contract,
state database, credentials, and Fridica's installed code outside these roots.
Symlinks, hard links, special files, parent traversal, and `.git`, `.codex`,
`.claude`, `.ssh`, and `.env` components are refused. Denials also cover case
and Unicode normalization aliases, conservatively on case-sensitive systems.

| Level | Operation | Authorization |
| --- | --- | --- |
| L0 | Read a listed text file | Automatic |
| L1 | Create or replace a text file | Local approval, or a matching active sender/channel/path grant |
| L2 | Delete a text file | New local approval for that exact request, every time |

The model proposes operations with native tools disabled. The controller checks
paths and permissions, saves the exact change in SQLite, then applies it. An
existing file must match the content supplied to the model and the content
reviewed locally. Writes use an atomic replacement; interrupted operations are
never rerun automatically. Revocation affects operations not yet claimed for
execution, and does not undo completed changes.

Manage requests from a local terminal or ask your Desktop agent to run these
commands after reviewing the request. No Slack message can approve or grant
access. Commands work while the daemon is running and return JSON:

```sh
fridica permissions status
fridica permissions status REQUEST_ID       # includes before and proposed contents
fridica permissions approve REQUEST_ID
fridica permissions reject REQUEST_ID
fridica permissions grant --sender U123ABC --channel C123ABC --path /absolute/path/project/docs --ttl 3600
fridica permissions revoke GRANT_ID
```

Each command accepts `--config PATH`. Omit `--ttl` for a grant that lasts until
revoked. A grant permits L1 writes only; it cannot widen the configured roots or
permit deletion. Approve an already pending request separately after reviewing it.
Approved changes run when the daemon next processes its local
queue, and results go back to the original Slack thread. Delivery retries reuse
the saved result. Root changes require a daemon restart. Desktop remains a
local control client; this does not attach a CLI session to a Desktop task.

This first version supports UTF-8 text files up to 64 KiB, existing parent
directories, and one write or deletion per request. It does not execute shell
commands, tests, merges, deployments, arbitrary sends, or directory operations.
The model gets up to eight planning calls per message, each stateless; this mode
does not resume native workspace sessions. File contents passed to the model
and before/after contents saved in SQLite may be private: only allow projects
appropriate for the selected channel, and protect the database accordingly.

Codex planning uses a named permissions profile that grants only minimal runtime
reads and reads of its temporary invocation directory, with no command network
access. It requires a CLI supporting named permissions and `--strict-config`;
unsupported configuration fails instead of falling back to workspace mode.
Claude planning uses its existing empty tool list. The provider CLI and local
controller remain trusted processes with their normal authentication/runtime
access; this is not isolation from a compromised CLI or another process running
as the same OS user. No additional model API or billing fallback is introduced.

## Local dashboard

The monitor is optional and runs separately from the Slack listener:

```sh
fridica start --config ~/.config/fridica/config.toml
# In another terminal:
fridica dashboard --config ~/.config/fridica/config.toml --port 8877
```

Open http://127.0.0.1:8877. It starts read-only. To enable local file approvals and configuration editing,
start the monitor with `--allow-approvals`; the listener does not need to restart.
The terminal prints the path to a private `.dashboard-key` file beside the state
database. Enter its contents under **Settings → Local approvals**. The key is
held in this tab's session storage, rotates on monitor restart, and is cleared by
**Lock controls**. Do not share it or send it to Slack.

The English interface has six views:

- **Overview:** counts for attention, in progress, waiting, and finished requests,
  with links to the matching lists, recent requests, and workspace information.
- **Inbox:** approvals and failures, with a separate waiting-for-information tab.
- **Requests:** searchable, paginated list or card layout. Each request opens its
  conversation, current situation, file operations, and notification status.
  Filter by requester using the person selector or an Overview requester shortcut.
- **Projects & access:** editable managed directory boundaries, GitHub repository
  labels, and per-person/channel/path write grants with expiry and revocation.
  Labels do not clone repositories or grant access. Adding a directory permits
  reads; automatic writes still require a separate grant. Up to 100 active grants
  are displayed; `fridica permissions` can inspect or manage all grants.
- **Activity:** paginated messages and local approval decisions, with Current and
  Archived views. Choose a cutoff date, review the entry count, then archive older
  entries; Restore returns them to Current. This stores a per-channel date boundary,
  so late entries with older timestamps also appear in Archived. It does not alter
  requests, delete history, or reclaim disk space. Messages show their arrival time
  and current state; decisions have their own audit timestamp.
- **Settings:** model, reasoning effort, waiting-reply and turn limits, refresh,
  control locking, listener health, and monitor shutdown.

Counts cover all stored tasks in configured channels. Lists load 50 items at a
time; thread history can load earlier messages. Slack display names are cached
when a configured user token permits the metadata lookup. Unresolved identities
show as unknown, with raw IDs available under technical details.

Approvals require `file_access` mode and apply to one exact stored proposal.
File cards show the operation, permission level, and Slack delivery status.
The review dialog shows a numbered diff with addition and deletion counts.
Completed, delivered results collapse by default; pending approvals, errors, and
unconfirmed deliveries stay expanded. Your expanded sections survive refreshes.

Review the full diff and contents before deciding. The server checks the owner,
channel, revision, configured directory boundary, and unchanged file before
queuing an approval. Changed or already-decided proposals are rejected. The
existing listener performs the operation and reports in the original Slack
thread. Approval saved, file changed, and notification delivered are distinct
states. Rejection leaves the file unchanged and queues a notification. This
monitor does not authorize arbitrary commands, repository merges, or deployment.

The page refreshes every five seconds while visible, pauses in background tabs,
and can disable auto-refresh. It reads SQLite and never calls a model. Optional
Slack name lookups are cached; no analytics or third-party scripts are loaded.
The listener records a separate heartbeat every five seconds; after 15 seconds
without one it appears offline. This is stage-level status, not token streaming,
a success score, or percentage progress.

Closing a tab leaves both processes running. Ctrl+C stops the corresponding
process; unlocked **Stop monitor** stops only the monitor. Restart it from the
terminal or Desktop. The monitor never starts automatically with the listener.

**Conversation limits.** Three consecutive delivered replies with `waiting`
status (excluding file-approval proposals) pause that thread by default. The
existing `max_turns` ceiling also pauses further work. This is a conservative
possible-loop guard, not a semantic proof that a conversation is stuck; useful
multi-step clarification may require an owner to resume. Counts follow reply
send order, not incoming event order. Configure `max_wait_replies` and
`max_turns` in `config.toml`.

A paused request appears in Inbox with its reason. It remains paused after a
restart and makes no model calls or automatic replies. **Resume** resets its
budget for future messages; earlier or delayed pre-resume messages are not
replayed. Pending approved file operations can continue after resume.
**Close request** stops further automatic processing of that thread. These
controls require the owner key. Other threads continue normally.

**Local cleanup.** Archive a completed or locally closed request from its detail
panel. It moves to **Requests → Archived**, where **Restore** brings it back.
**Preview cleanup** shows the message and proposal counts; confirmation clears
stored message text, replies, proposed file contents, diffs, and paths for that
request. Event identities and decision records remain for deduplication and
auditing. Incoming messages for cleaned threads are recorded without their text.
Cleanup does not delete project files or Slack messages and never retries an
operation. Processing, undelivered replies, and unresolved file operations block
closing or cleanup. Cleanup is irreversible and is not a secure erase of SQLite
journals, filesystem snapshots, backups, or separately stored CLI transcripts;
the database file may not shrink immediately.

**Model configuration.** The Codex adapter ignores the user's global Codex
configuration. Pin a model and effort in Fridica's configuration to avoid an
implicit CLI default, for example:

```toml
backend = "codex"
model = "gpt-5.6-luna"
reasoning_effort = "low"
max_wait_replies = 3
max_turns = 6
```

Choose a model available to your account. `reasoning_effort` applies only to
Codex; no automatic upgrade to a more expensive model is performed. Settings
provides input fields and a before/after review for these values. Saves preserve
TOML comments, reject stale forms, and replace the local file atomically. The
listener reloads supported changes between requests; the current request finishes
with its original settings. The page distinguishes saved settings from a matching
revision applied by the connected listener. Configuration errors pause queued
work until repaired. Existing paused threads remain paused.

Directory editing requires managed file access. Paths must already exist and
cannot expose the configuration, state, or protected paths; writable roots also
exclude the installed agent code. Removing
write access revokes grants outside the remaining writable boundaries. A grant
permits text-file writes only; it never authorizes deletion, Git commands, or
external actions. Directory and grant changes require a review before saving.
Channel, identity, credential, and backend changes remain outside these forms and
require a local configuration update and restart. Settings and grant decisions
are audited in SQLite. Dashboard operations do not call a model.

The server uses the existing aiohttp dependency, binds only to IPv4 loopback,
and rejects foreign Host/Origin headers and cross-site requests. Control routes
require the local key; writes also require same-origin JSON. File contents are
available only after authentication. Read-only views still expose local Slack
history and paths to other users or processes on this Mac. Keep the monitor
local; do not expose it through a public tunnel or proxy. Recognizable tokens
are masked in monitoring data, but this is not a general secret detector.

UI references: [Flower](https://flower.readthedocs.io/en/latest/) for worker and
task health, [Bull Board](https://github.com/felixmosh/bull-board) for status
filters, and [Langfuse sessions](https://langfuse.com/docs/observability/features/sessions)
for conversation timelines. These are design references, not dependencies.

## Local state and recovery

State defaults to `~/.local/state/fridica/state.sqlite3`; override `state_path`
with an absolute path outside agent workspaces. It contains message text,
task results, and delivery state, so treat it as private local data. A file lock
prevents two processes from opening the same state database. Context sent to the
model is bounded; stored history is retained until you remove the database while
the daemon is stopped. There is no historical Slack backfill on startup.

Incoming events are persisted before acknowledgment. Agent runs are serialized,
and their results are saved before Slack delivery. Rate-limited replies retry
without rerunning the agent. Interrupted executions and uncertain deliveries
are not retried automatically because file changes or Slack posts may already
have occurred. Startup logs their event IDs. Inspect them locally:

```bash
sqlite3 ~/.local/state/fridica/state.sqlite3 \
  "SELECT event_id,state FROM events WHERE state IN ('interrupted','ambiguous','failed','blocked');"
```

Check the workspace and Slack thread before requesting work again in a new
thread. Restarting cannot guarantee exactly-once execution across an external
agent, filesystem, and Slack. Raw subprocess output and Slack tokens are not
logged. Stop the daemon and use a separate state database when changing owner.

When `resume_sessions` is enabled, the providers also keep their own transcripts
on disk: Claude under `~/.claude/projects/` and Codex under `~/.codex/sessions/`.
They contain the Slack text Fridica sent and the agent's tool activity, so treat
them like the state database. Removing them is safe; the next turn in an affected
thread starts a new session. Setting `resume_sessions = false` stops new
transcripts from being written.

## Python interfaces

`fridica.models` defines `Message`, `ConversationContext`, `Decision`,
`AgentResult`, `AgentBackend`, and `Transport`. `fridica.replica.Replica` combines
configuration, storage, a backend, and a transport. Alternative backends implement
`async classify(message, context)` and `async respond(message, context)`;
transports implement `async send(message, result, task_id, turn)` and return the
confirmed message timestamp. Backend responses contain `text` and a status of
`complete`, `waiting`, or `blocked`. `fridica.contract.load_contract(path)`
parses an agent contract into its `participation` and `replies` sections;
custom backends should send the matching section as their instruction.

## Validate

```bash
python -m pytest
node --test tests/dashboard.test.cjs
python -m build --no-isolation
fridica --help
python -m fridica --version
```

Frontend regression tests use Node 22 or later and its built-in test runner, with no npm dependencies.
Tests use fake Slack clients and fake agent processes and require no tokens or
live model calls. For a live smoke test, select one test channel and an empty
project directory, run `doctor`, then run `start --observe-only`. Have another
member post a message and verify the observation log. Restart normally and ask
that member to mention you with a request to create a small text file. Check the
file and threaded reply. Request a file without specifying its location
to exercise clarification, then try a request outside configured write roots to
verify blocked behavior. Repeat with the other backend. Live tests can consume
provider credits and require your Slack installation and CLI login.

## CI, releases, and deployment

The workflows follow snapy's CI → automatic tag → manual PyPI publishing flow,
adapted for a pure-Python package. Fridica produces one universal wheel and one
source distribution, rather than platform-specific compiled wheels.

- **Continuous Integration** (`.github/workflows/ci.yml`) runs on pull requests
  and pushes to `main`. It tests Python 3.11 on Ubuntu and macOS, builds both
  distributions after every matrix job passes, checks package metadata, and
  smoke-tests the installed wheel and bundled Slack manifest. Tests use fake
  agents and Slack clients; no Slack/model credentials are required.
- **Auto Tag on PR Merge** (`cd.yml`) tags the exact merge commit and creates a
  GitHub release. The first tag is `v0.1.0`; subsequent merges default to a patch
  bump. Add one of `release:major`, `release:minor`, or `release:patch` to select
  the increment. Multiple release labels fail the job. Rerunning an already
  tagged merge reuses its tag and repairs a missing GitHub release.
  Authentication uses the automatically supplied `GITHUB_TOKEN`, with
  `contents: write` permission limited to the tagging job. No GitHub App,
  private key, or personal access token is needed.
  Merged fork PRs are supported through a merged-only `pull_request_target`
  event; the checkout is verified to belong to `main` before release code runs.
  Tag jobs use [GitHub's concurrency queue](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency)
  to run serially (up to 100 pending runs).
- **Publish to PyPI** (`release.yml`) is manually dispatched with an existing
  stable tag, such as `v0.1.0`. It verifies the tag belongs to `main`, reruns the
  full CI workflow on its resolved commit, checks that both artifact versions
  match the requested tag, then publishes those exact artifacts. Publishing
  does not run on every merge or tag push.

Versions come from Git tags using
[hatch-vcs](https://github.com/ofek/hatch-vcs). `fridica.__version__` and the CLI
read installed package metadata. Untagged/dirty checkouts produce development
versions; reinstall an editable checkout after changing tags to refresh its
installed version. Full Git history is fetched in CI. Source distributions carry
version metadata so they also build without Git.

Repository maintainers must configure these GitHub settings before using CD:

1. Ensure repository/organization Actions policies allow the tagging job's
   `GITHUB_TOKEN` to have **Contents: write** permission. The workflow requests
   this explicitly; do not create a token secret. If tag rules restrict `v*`
   creation, configure them to allow this workflow's tag creation. The built-in
   token does not bypass repository rules. Existing `BUMP_BOT_APP_ID` and
   `BUMP_BOT_PRIVATE_KEY` settings are unused and can be removed from Fridica.
2. Create a GitHub Actions environment named `pypi`. Add `PYPI_API_TOKEN` as an
   environment secret using a PyPI account authorized to publish `fridica`.
   Configure required reviewers if publication needs an approval gate. A new
   PyPI project may need an account-scoped token for its first upload; replace
   it with a project-scoped token afterward.
3. Protect `main` and require CI before merging. Auto-tagging reacts to a merge,
   so branch protection supplies its CI gate. Publishing independently reruns
   all tests. Require the matrix test jobs and the `package` job as checks.
4. After a release tag exists, open **Actions → Publish to PyPI → Run workflow**
   on `main`, enter the tag, and approve the `pypi` environment if configured.
   PyPI versions are immutable; use a new tag for changed artifacts rather than
   overwriting a published version.

Tags and releases created using `GITHUB_TOKEN` do not trigger downstream
tag-push or release-event workflows. Fridica's publishing workflow is manually
dispatched and reruns CI itself, so it does not depend on those events. See
[GitHub's workflow-trigger rules](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow).

Only the package is deployed by CI. Run the daemon on each owner's machine,
where their Slack tokens, agent authentication, and project directories live:

```bash
python -m pip install --upgrade 'fridica==0.1.0'
fridica doctor
fridica start
```

Replace `0.1.0` with the published version. Stop the running daemon before an
upgrade, then restart it in the same environment. No remote daemon, Slack app,
GitHub secrets, or PyPI project is provisioned by installing these workflows.

For local workflow linting with actionlint 1.7.12, use
`actionlint -ignore 'unexpected key "queue" for "concurrency" section' .github/workflows/*.yml`.
That version's schema predates GitHub's documented `queue` field; the exception
only suppresses that schema mismatch.
