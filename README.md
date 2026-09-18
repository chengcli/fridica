# Fridica

Fridica is a local personal agent that connects your Slack identity to Claude Code
or Codex. It listens in channels you choose, decides when to participate, works in
configured project directories, and replies in Slack threads as you. Replies carry
a `[via fridica]` label and machine-readable metadata.

This first release runs one owner per daemon and one Slack app per owner. Anyone
in an allowed channel can trigger workspace actions. It includes both CLI
backends, SQLite context and task storage, clarification conversations, and loop
limits. It does not include a shared relay, browser OAuth onboarding, MCP server,
or automation of the Claude/Codex desktop UI.

## Install

Use macOS or Linux with Python 3.11 or newer. Install and authenticate either
[Claude Code](https://code.claude.com/docs/en/setup) or
[Codex CLI](https://developers.openai.com/codex/cli) separately.

```bash
python -m venv .venv
source .venv/bin/activate
python -m pip install -e '.[dev]' hatchling
fridica init
```

`init` creates `~/.config/fridica/config.toml`, without overwriting existing
configuration. For a different location, use `fridica init --config /path/config.toml`.

## Configure Slack

1. Open [Slack app management](https://api.slack.com/apps), choose **Create New
   App → From an app manifest**, select your workspace, and paste
   [`slack/manifest.yaml`](slack/manifest.yaml).
2. In **Basic Information → App-Level Tokens**, generate an app token with
   `connections:write`. Socket Mode must be enabled.
3. Install the app to the workspace under **OAuth & Permissions**. Copy the
   **User OAuth Token** beginning with `xoxp-`, not a bot token. Workspace
   administrators may need to approve installation and requested scopes.
4. Export both tokens in the terminal where Fridica will run:

   ```bash
   export FRIDICA_SLACK_APP_TOKEN='xapp-your-token'
   export FRIDICA_SLACK_USER_TOKEN='xoxp-your-token'
   ```

5. Edit the generated TOML file. Supply your Slack member ID, workspace ID,
   channel IDs, an existing project directory, and an owner profile describing
   your projects and expertise. Choose `backend = "claude"` or `"codex"`.
   Authenticate the selected CLI before starting Fridica.

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
```

The manifest enables public-channel events. For private channels, explicitly add
the `groups:history` and `groups:read` user scopes and the `message.groups` user
event, reinstall the app, and add the channel ID to your configuration. DMs and
group DMs are outside this release's channel policy. Fridica never assumes that
an app can see everything your Slack account can see: scopes, subscriptions,
membership, and workspace policy determine delivery.

Each owner must create a separate Slack app for this release. Multiple Socket
Mode connections to a shared app divide events between connections; they do not
broadcast every event to every owner. See [Slack's Socket Mode documentation](https://docs.slack.dev/apis/events-api/using-socket-mode/).

## Run

```bash
fridica doctor
fridica start --observe-only
fridica start
```

`doctor` checks local configuration, token presence, executable availability,
and required CLI flags without invoking a model. `start` verifies the Slack user
and workspace identity and channel membership. `--observe-only` records messages
without invoking either model or posting replies. Stop with Ctrl-C or SIGTERM.
All subcommands accept `--config PATH`; `python -m fridica` is also supported.

Fridica responds to mentions of the owner and follow-ups while a task is waiting
for clarification. Other messages pass through a separate classification call
with tools disabled. Classification failure means silence. Set
`general_messages = false` to disable unsolicited participation. The owner’s own
messages supply context but never directly trigger their agent.

Replies stay in their original thread. Each thread has a persistent six-turn
default budget; use a new thread for a new task after that budget is exhausted.
Generated messages initiate responses only when explicitly addressed or following
an active task. A per-channel cooldown limits unsolicited replies. Other agents'
metadata is a loop-control hint, not an authorization credential.

## Workspace authority

The selected agent can read, edit, and run commands using its provider-supported
sandbox in the configured workspace roots. Task-command network access is
disabled. Provider API access is still needed to run the model. Claude requires
its sandbox dependencies (including bubblewrap and socat on Linux). Fridica does
not enable bypass-permission flags or automatically approve broader access.
Claude performs file changes through sandboxed Bash; its built-in Edit and Write
tools are not exposed, because their permissions are separate from the Bash sandbox.
Blocked actions require local intervention; there is no remote approval UI.

Only grant access to project directories you intend Slack participants to use.
The provider sandboxes may permit reads beyond writable project directories and
use temporary files; Fridica does not claim complete filesystem read isolation.
Managed provider settings and project instructions remain part of the execution
environment. Slack tokens are removed from agent subprocess environments, but
do not store credentials in project files accessible to the agent.

Fridica supplies its own bounded conversation history for each invocation;
existing desktop conversations are not imported. The optional `model` setting
is passed to the selected provider. No model name or paid API key is required by
Fridica itself; each CLI uses its own authentication and billing.

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

## Python interfaces

`fridica.models` defines `Message`, `ConversationContext`, `Decision`,
`AgentResult`, `AgentBackend`, and `Transport`. `fridica.replica.Replica` combines
configuration, storage, a backend, and a transport. Alternative backends implement
`async classify(message, context)` and `async respond(message, context)`;
transports implement `async send(message, result, task_id, turn)` and return the
confirmed message timestamp. Backend responses contain `text` and a status of
`complete`, `waiting`, or `blocked`.

## Validate

```bash
python -m pytest
python -m build --no-isolation
fridica --help
python -m fridica --version
```

Tests use fake Slack clients and fake agent processes and require no tokens or
live model calls. For a live smoke test, select one test channel and an empty
project directory, run `doctor`, then run `start --observe-only`. Have another
member post a message and verify the observation log. Restart normally and ask
that member to mention you with a request to create a small text file. Check the
file and labelled threaded reply. Request a file without specifying its location
to exercise clarification, then try a request outside configured write roots to
verify blocked behavior. Repeat with the other backend. Live tests can consume
provider credits and require your Slack installation and CLI login.
