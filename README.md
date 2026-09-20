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
[Codex CLI](https://developers.openai.com/codex/cli) separately.

```bash
python -m venv .venv
source .venv/bin/activate
python -m pip install -e '.[dev]'
fridica init
```

`init` creates `~/.config/fridica/config.toml`, without overwriting existing
configuration. For a different location, use `fridica init --config /path/config.toml`.

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
```

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
configuration, each Slack token's format, AI executable availability, required
CLI flags, and AI sign-in. It runs `claude auth status` or `codex login status`
for the configured backend without invoking a model or printing account details.
Independent checks continue after failures; checks blocked by invalid
configuration or a missing executable show SKIP. The command exits nonzero if
any check fails or is skipped. Sign-in status does not guarantee that a later
model request will succeed or that credits are available. `start` verifies the Slack user
and workspace identity and channel membership. `--observe-only` records messages
without invoking either model or posting replies. Stop with Ctrl-C or SIGTERM.
All subcommands accept `--config PATH`; `python -m fridica` is also supported.

Fridica responds to mentions of the owner and follow-ups while a task is waiting
for clarification. Other messages pass through a separate classification call
with tools disabled. Classification failure means silence. Set
`general_messages = false` to disable unsolicited participation. The owner’s own
messages supply context but never directly trigger their agent.

An explicit human @mention always requests a threaded reply, even with general
participation disabled or a cooldown active. If the thread has exhausted its
action budget or an earlier task needs inspection, Fridica replies with a brief
explanation without running more actions or retrying old work. This does not
override observe-only mode, channel restrictions, duplicate suppression, or
automated-message loop protection. Slack rejection or uncertain delivery can
still prevent a reply; inspect the local logs rather than automatically resending.

Only the structured final answer is delivered to Slack. Agent instructions exclude
internal commentary, unsolicited summaries, and tool transcripts from that answer;
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

## Workspace authority

The selected agent can read, edit, and run commands using its provider-supported
sandbox in the configured workspace roots. Task-command network access is
disabled. Provider API access is still needed to run the model. Claude requires
its sandbox dependencies (including bubblewrap and socat on Linux). Fridica does
not enable bypass-permission flags or automatically approve broader access.
Claude enables native Edit and Write tools in `acceptEdits` mode for the workspace
and `additional_workspaces`, alongside sandboxed Bash for file operations such as
renaming or deleting files. Classification still has no tools. Codex continues to
use its `workspace-write` sandbox. No blanket permission-bypass flag is enabled.
OS file permissions, managed policies, and provider-protected paths still apply;
this does not grant administrator access or unrestricted writes outside the roots.
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
