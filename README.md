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

### Troubleshoot missing messages

If `fridica start --observe-only` prints `INFO Listening as ...` but nothing
appears after you send a message, startup succeeded but event reception has not
been verified. Check the following in [your Slack app settings](https://api.slack.com/apps):

1. Open **Event Subscriptions** and turn **Enable Events** on.
2. Under **Subscribe to events on behalf of users**, add `message.channels`
   for public channels. Adding only a bot event subscription is not sufficient
   for this user-token setup.
3. Under **OAuth & Permissions → User Token Scopes**, confirm
   `channels:history` is present.
4. For a private channel, add the `message.groups` user event and the
   `groups:history` and `groups:read` user scopes. Keep the public-channel
   subscriptions and scopes if you also monitor public channels.
5. Save changes and reinstall the app if permissions changed. If Slack issues
   a replacement user token, update `FRIDICA_SLACK_USER_TOKEN`. Confirm that
   both the `xapp-` app token and `xoxp-` user token belong to this same app.
6. Confirm the channel ID is listed in your Fridica configuration and the
   authorized Slack user belongs to it. Stop Fridica with Ctrl-C and restart:

   ```bash
   fridica start --observe-only
   ```

7. Send a **new** message such as `test` to that channel after startup. The
   terminal should print:

   ```text
   INFO Observed event ... in ...; no agent or delivery
   ```

Observe-only mode does not invoke AI or send Slack replies. Messages sent before
startup are not fetched. If there is still no output, check that another process
is not using the same app's Socket Mode connection and receiving its events.
Slack requires both an event subscription and its corresponding OAuth scope;
see the [Events API documentation](https://docs.slack.dev/apis/events-api/).

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
