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
[Agent contract](#agent-contract)). The repository list is shared and ships with
the package (see [Repository list](#repository-list)). For a different location, use
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
allowed_domains = ["*"]
```

### Remote working folder over SSH

The working folder can live on another machine. Write the roots as
`host:/absolute/path`, where `host` is an alias from `~/.ssh/config` (or
`user@host`):

```toml
workspace = "dart9:/mnt/data1/projects/my-project"
additional_workspaces = ["dart9:/mnt/data1/projects/shared-lib"]
```

Every per-turn run then happens on that host: the reply, the tool-less classification,
summaries and debriefs, and the environment checks. Roots on *other* hosts, such as
`additional_workspaces = ["dart9:/mnt/data1/projects"]` next to a local `workspace`,
do not take part in replies at all: they make that host available to the heavy tasks
described below, each confined to its own roots (see [Heavy tasks](#heavy-tasks)). Fridica starts each one as `ssh -T host 'cd /path && codex exec ...'` (or
`claude -p ...`) with no PTY, feeds the prompt on stdin, and reads the structured
result from stdout, so SSH itself is the transport and the security boundary;
nothing listens on a port and no filesystem is mounted. Slack, the state database,
the dashboard, and the configuration stay on the machine that runs `fridica start`.

Requirements on the remote host:

- `ssh host true` must succeed from this machine without any prompt (key
  authentication; Fridica uses `BatchMode=yes`). Put the alias, user, and identity
  file in `~/.ssh/config`. Fridica multiplexes all of its connections through one
  `ControlMaster` socket (in `$XDG_RUNTIME_DIR/fridica`, or `/tmp/fridica-ssh-<uid>`),
  so a burst of runs does not trip the server's connection limits.
- The backend CLI must be installed and signed in **for the remote user's login
  shell**: `ssh host 'command -v codex && codex login status'` (or
  `claude auth status`) is what `fridica doctor` runs.
- For Claude, `bwrap` and `socat` on the remote host (see the sandbox dependencies
  above); for Codex, a system `bwrap` is used when present.
- With a remote `workspace`, every other root carries a `host:` prefix too.
  `read_only_workspaces` stay on the workspace's host. `file_access` (scoped file
  access) covers local roots only: it is rejected with a remote `workspace`, while
  remote roots in `additional_workspaces` simply become heavy-task hosts next to it.

`fridica doctor` adds an `SSH connection` check that connects, confirms the roots are
directories, and checks the remote OS, then runs the executable, capability, sandbox,
and sign-in checks on the host. Each further heavy-task host gets its own
`Heavy-task host` check covering the connection, its roots, the backend and its
sign-in, and `bwrap` when it declares GPUs. A run that cannot reach the host is logged as an SSH
failure (status 255) rather than an agent error. The remote agent is bounded by
`timeout` when the host has it, so a dropped connection cannot leave it running.

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
being resumed. Rules that the code enforces regardless of the contract: a reply
over 7000 characters is posted in part with the full text attached as its details
file, the status must be `complete`, `waiting`, or
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

### Repository list

Fridica ships one repository list for the whole team: `src/fridica/repos.toml`
in this repository, installed as `fridica/repos.toml` and read by every agent
run. It is the same for everyone, so **changes go through a pull request to
`main`**; the test suite validates the file on every pull request and refuses
unknown fields, missing names or URLs, non-https URLs, and duplicate names.
Upgrading Fridica delivers the new list; `init` does not copy it.

```toml
[[repos]]
name = "snapy-cli"
url = "https://github.com/chengcli/snapy"
collaborators = ["Cheng Li", "Tianhao Le", "Xi Zhang"]   # first entry is the owner
notes = "Hydrodynamic core"
```

`name`, an `https` `url`, and at least one collaborator are required; `notes`
is optional, and names must be unique. **The first collaborator is the
repository owner.** The agent treats the owner's word as final for that
repository: requests from other people get the analysis or preparation they
ask for, but merging, releasing, or changing conventions is stated as the
owner's decision, and when a thread carries conflicting instructions the
owner's are followed. The owner is spelled out as an `owner` field in the data
the agent receives. Entries deliberately carry no local path:
each person keeps checkouts wherever they like, and the agent finds the checkout
for a chosen repository under its workspace roots by matching the git remote
URL. Listing a repository grants no access; reads and writes still follow the
workspace roots in `config.toml`.

The list is sent to the agent as data inside every classification and reply
call, never as instructions. The contract's `## Replies` rule tells the agent
to resolve which repository a request means by matching names and URLs and the
requester against the collaborators, to name the chosen repository in its
reply, and, when more than one entry could match or none does, to ask with
status `waiting` listing the candidates instead of guessing.

To try a change before opening the pull request, set `repos = "path/to/list.toml"`
in `config.toml` (relative paths resolve from the config file's directory).
`doctor` then reports the list as a local override. An invalid file fails
`doctor` and blocks replies with the generic notice until fixed.

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
configuration, the agent contract, the repository list, each Slack token's
format, AI executable availability, required CLI flags, sandbox dependencies,
and AI sign-in. It runs `claude auth status` or
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

### When to restart

The daemon loads its code and configuration once at startup and re-reads only a
few files for each agent run, so what changed decides whether a restart is
needed:

| Change | Restart needed? |
| --- | --- |
| `contract.md` (agent rules) | No; re-read for every run, applies to the next reply |
| `repos.toml` (shared repository list, including an upgrade that delivers a new one) | No; re-read for every run |
| Settings the dashboard can edit (`model`, `reasoning_effort`, `max_turns`, `max_wait_replies`, directory access) | No; the listener reloads them between requests |
| Any other `config.toml` key: channels, identity, tokens, backend, `allowed_domains`, `resume_sessions`, `session_timeout` | Yes |
| Fridica's own code: a `pip install --upgrade`, a `git pull` on an editable install, or any edited `.py`, `.js`, or `.html` file | Yes |
| Sandbox packages or the AppArmor profile | No; the sandbox is set up for each run. If `start` had refused to launch because of them, simply start it again |

A merged and pulled branch therefore needs a restart even when the daemon is
already running the same feature from your working tree: the process still holds
the old modules in memory. Restart with Ctrl-C in the daemon's terminal or tmux
pane followed by `fridica start`; no message is lost, because incoming events are
acknowledged and stored before processing, and undelivered replies resume. The
startup log lists earlier events that ended blocked or uncertain so you can inspect
them; it never replays them. Slack occasionally drops a Socket Mode event even
while connected, so besides the one-hour catch-up at startup the daemon re-reads
the last 15 minutes of channel and active-thread history every 5 minutes, and
the full hour again after a failed pass.
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
default budget (`max_turns`). When the last allowed reply has been delivered,
Fridica wraps the thread up: it posts a short stop notice in the thread, asks the
agent for a summary of the whole discussion (a tool-less call governed by the
contract's `## Thread summaries` section), posts that summary as a new top-level
message in the channel, and prepares the new thread with a fresh turn budget and
the old thread's session so the work continues with full context. The summary
post @mentions nobody, so the new thread only continues when a person replies
to it; two agents cannot chain threads indefinitely. The exhausted thread stays
paused (visible in the dashboard with the reason and the new thread's
timestamp) and later mentions there are recorded but not answered. If the
summary cannot be produced or posted, the stop notice says so, the failure is
logged, and nothing is retried automatically.

Every reply also carries the agent's judgement of whether the discussion is
finished: the original request resolved, every action item raised in the thread
done or explicitly handed off, and nobody waiting on anyone. When a delivered
reply says `finished` (only possible with status `complete`), Fridica asks the
agent for a debrief (a tool-less call governed by the contract's `## Debriefs`
section) and posts it as a new top-level channel message headed "Debrief: this
discussion is finished." It names people plainly and @mentions nobody. A thread
is debriefed once per finish; if the conversation continues afterwards, a later
finished reply produces a fresh debrief. A finished thread that is also at its
turn limit gets the debrief instead of the continuation summary. A failed
debrief is logged and not retried.
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

By default (`file_access = true`) the agent works on the local roots through scoped
file operations (see [Scoped file access](#scoped-file-access)); heavy tasks then run
only on remote hosts. With `file_access = false`, or with a remote `workspace`, the
selected agent instead reads, edits, and runs commands using its provider-supported
sandbox in the configured workspace roots. Task-command network access follows
`allowed_domains`, which defaults to every host (see
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

### Heavy tasks

Per-turn replies are one short CLI run each. With `heavy_tasks = true`, the reply
agent may decide that a request needs long-running or hardware-heavy work (a full
build, a long test suite, a GPU or multi-core job) and hand it to a **persistent
worker** instead of doing it in the turn: it returns a self-contained brief in the
structured reply's `escalate` field, names the host in `escalate_host`, and tells the
requester that the job has started. Fridica then runs the brief on that host, locally
or over SSH, and posts the worker's report as a follow-up message in the same Slack
thread when it finishes.

**Hosts.** Every host that owns a root is a candidate: the workspace's own host
(`local`, or its SSH alias) and every other alias among `additional_workspaces`. With
`file_access = true` the local roots stay under scoped file access and are not a
candidate, so at least one remote root is required and the reply agent hands work off
with the planner's `escalate` operation (host name in `path`, brief in `content`). The
agent sees each host's roots and `[resources.<host>]` hardware as data and picks the
one the job needs; a worker on a host can only write inside that host's roots, and it
starts in the first root listed for that host. With a local `workspace` and
`additional_workspaces = ["dart9:/mnt/data1/projects"]`, replies run on this machine
and a GPU job runs on dart9 inside `/mnt/data1/projects`. A host the agent names that
is not configured falls back to the first candidate: the workspace's own host, or the
first remote host under `file_access`.

- With Codex the worker is `codex app-server`, a long-lived process speaking JSON-RPC
  over JSONL on stdin/stdout (OpenAI marks the command experimental). Fridica sends
  `initialize`, `thread/start` (or `thread/resume` for a thread it created earlier),
  and one `turn/start` per job, and reads the final agent message when the turn
  completes. With Claude it is `claude -p --input-format stream-json --output-format
  stream-json` with the same sandbox, permission, and tool flags as task runs.
- The worker keeps the per-turn policy: workspace-write sandbox, network access only
  when `allowed_domains` lists hosts, and no approvals (a host that declares GPUs is
  confined differently; see GPU access below). Fridica has no approval UI, so
  any approval request from the agent is declined and logged. One difference for
  Codex: `codex app-server` has no `--ignore-user-config`, so the owner's
  `~/.codex/config.toml` on the working host, including any MCP servers it defines,
  applies to heavy jobs (the feature switches Fridica passes still turn off apps,
  plugins, hooks, browser and computer use). Keep that file minimal on a host that
  runs heavy tasks.
- One worker per Slack thread. The process stays alive between jobs and exits after
  `heavy_task_idle` seconds without work (default `1800`); the backend thread it
  created is remembered in the state database, so the next job in that Slack thread
  resumes it in a fresh process. A job is bounded by `heavy_task_timeout` seconds
  (default `14400`, four hours). While a job runs, the reply agent sees
  `worker.state = "running"` in its data and answers progress questions itself; a
  second brief is ignored until the first finishes.
- A failed or timed-out job posts a short notice in the thread and is not retried. A
  job cut off by restarting Fridica is reported as interrupted on the next start and
  is not resumed automatically.
- Anyone in an allowed channel can trigger hours of compute this way, which is why
  the setting is off by default. `fridica doctor` checks that the backend provides
  `codex app-server` or `--input-format stream-json`.

Declare the hardware heavy tasks may use per host, `[resources.local]` for this
machine and `[resources.<alias>]` for each SSH host among the roots (a plain
`[resources]` table describes the workspace's host):

```toml
[resources.dart9]
cpus = 8                          # sets OMP_NUM_THREADS for the worker
gpus = [0, 1]                     # device indices; sets CUDA_VISIBLE_DEVICES ([] = no GPU)
gpu_type = "NVIDIA A100 80GB"
memory_gb = 128
notes = "Jobs longer than 10 minutes go through Slurm: srun --gres=gpu:1."
```

The table is sent to the reply agent and to the worker as data, so the agent can
judge what a request needs, and the worker process is started with matching
`OMP_NUM_THREADS` and `CUDA_VISIBLE_DEVICES` (exported through SSH for a remote
host). Nothing is measured or enforced beyond those variables; the notes are the
place for site rules such as a batch scheduler.

**GPU access.** Both backends sandbox commands with bubblewrap, whose minimal `/dev`
hides the GPU device nodes: inside their sandbox `nvidia-smi` cannot reach the driver
and CUDA finds no device. Declaring `gpus` for a host therefore makes its heavy worker
run inside **Fridica's own bubblewrap** instead: `/dev` is bound in full so CUDA works,
the whole filesystem is visible read-only, and writes are allowed only in that host's
designated roots, a private `/tmp`, and the backend's own state directory (whose
settings and hook files stay read-only, so a job cannot plant anything that would run
outside the confinement later). The backend's sandbox is turned off inside (Codex
threads use `danger-full-access`, Claude runs with its sandbox disabled and Bash
allowed) because Fridica's wrapper already confines the process. The wrapper shares
the host's network: the CLI itself must reach the model API, and bubblewrap cannot
separate that from the commands the job runs, so `allowed_domains` does not restrict a
GPU worker's commands. Configured roots should be real directories rather than
symlinks. This needs `bwrap` on that host and Linux; `fridica doctor` checks both. The
per-turn replies keep their normal sandbox, and the reply agent is told that GPU work
must be escalated to a GPU host. `CUDA_VISIBLE_DEVICES` still limits the devices. Set
`gpu_access = false` for a host to keep the backend's sandbox there (and lose GPU
access), or leave `gpus` out.

### Network access

By default (`allowed_domains = ["*"]`) commands the agent runs may reach any host.
With an empty list, `git fetch`, `pip install`, and similar calls are denied inside
the run, the model is told, and the reply says what it could not do. To allow only
specific hosts, list them:

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

`file_access = true` replaces native agent tools with checked file operations on the
local roots. It is the default whenever `workspace` is local. Remote roots in
`additional_workspaces` are unaffected: they host heavy tasks with the agent's native
tools, confined to their own roots. A config that enables `heavy_tasks` without any
remote root leaves the local roots as the only place to run them, so it either falls
back to the native tools when `file_access` is left out, or fails when it is set
explicitly.

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
does not resume native workspace sessions. With `heavy_tasks`, a planning call may
instead return `escalate`, which starts a persistent worker on a remote host exactly
as in the native mode; the local roots are never touched by that worker. File contents passed to the model
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

The optional monitor runs separately from the Slack listener and makes no model
calls. Start it in another terminal:

```sh
fridica dashboard --config ~/.config/fridica/config.toml --port 8877
```

Open http://127.0.0.1:8877. Add `--allow-approvals` to enable local controls.
The terminal prints the path to a private `.dashboard-key` file; enter its
contents under **Settings → Local approvals**. The key rotates on restart.
Keep the monitor local: read-only views expose Slack history and paths, and
recognized-token masking is not a general secret detector.

- **Inbox / Requests:** review tasks, conversations and file proposals; approve
  or reject an exact diff in managed file-access mode. The listener applies the
  decision and reports to Slack. Changed files require a fresh proposal.
- **Projects & access:** edit managed directories and per-person write grants.
  Repository labels do not grant access; grants do not permit deletion, Git
  commands or external actions.
- **Settings:** edit model, effort and conversation limits. Changes apply between
  requests. Channel, identity, credentials and backend require a config edit
  and restart. Configuration editing needs the `--config` path used above.
- **Activity:** archive older entries by date or restore them. Archiving hides
  entries from Current; it does not delete history or reclaim disk space.

The page refreshes while visible; auto-refresh can be disabled. Closing a tab
leaves services running. **Stop monitor** stops only the monitor.

### Task context and corrections

**Task & handoff** shows the repository, assignee, next step and blockers.
Repositories come from the shared `repos.toml`; changing that list requires a
PR and package upgrade. Task ownership does not grant access.

Claims link to source messages and remain unverified reports. Under
**Correct task or conclusion**, record a replacement and its evidence. Local
corrections take precedence over model updates and keep an audit trail. Saving
requires the local key, a current revision and no related operation in flight;
it neither grants permissions nor resumes execution.

Acknowledgments and repeated replies can stay silent. Three turns without
recorded progress pause the task; this is a heuristic, not automatic fact
checking. Blocked or paused threads make no further model calls or replies.
Use **Resume** for future messages or **Close request** to stop the thread.
Resuming a blocked thread also answers the latest message someone else posted
while it was blocked, even one an agent posted without mentioning you; the failed
request itself is not retried, and resuming a paused thread replays nothing. Continuation threads share task notes and
the progress counter; a no-progress pause does not create a continuation.

Task notes are bookkeeping attached to a reply, never a reason to withhold it.
Before a note is recorded, each field the model produced is checked and, where
it cannot be salvaged, dropped: a repository name is matched to the shared list
ignoring case, an assignee may be a member ID, a `<@ID>` mention, or a display
name that the dashboard's name cache maps to exactly one member who has posted
in the channel, and a claim must be an exact excerpt of a message in the task.
Every drop or correction is logged locally with the event ID and reason, the
reply is delivered unchanged, and the thread stays answerable. Only the fields
that pass are saved. Before this rule, a display name in the assignee field
replaced the whole reply with a "task update could not be validated" notice and
blocked the thread.

### Cleanup

Archive a finished or closed request, then use **Preview cleanup** to clear
its stored messages, proposals and shared task notes. Related continuations
must first be closed or archived. Cleanup is irreversible; deduplication IDs
and decision records remain. It does not delete project files, Slack messages,
CLI transcripts or backups, and is not a secure erase of SQLite journals.

## Local state and recovery

State defaults to `~/.local/state/fridica/state.sqlite3`; override `state_path`
with an absolute path outside agent workspaces. It contains message text,
task results, and delivery state, so treat it as private local data. A file lock
prevents two processes from opening the same state database. Context sent to the
model is bounded; stored history remains until local cleanup. There is no
historical Slack backfill on startup.

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
configuration, storage, a backend, and a transport and owns the rules of
engagement. `fridica.agents` holds the Claude and Codex backends; they build on
`fridica.runner` (bounded subprocess execution with a scrubbed environment),
`fridica.prompts` (structured-output schemas and prompt composition), and
`fridica.checks` (the local environment checks used by `doctor` and `start`). Alternative backends implement
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

Before opening a pull request, run the contributor hooks once over the whole tree:

```bash
python -m pip install pre-commit
pre-commit run --all-files   # or `pre-commit install` to run them on every commit
```

They check file hygiene (whitespace, file endings, merge markers, valid TOML, YAML
and JSON, leftover debugger calls) and lint with ruff's default rules as configured
in `pyproject.toml`; no formatter is applied.

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
