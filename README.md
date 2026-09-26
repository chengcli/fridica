# Fridica

Fridica is your Slack presence. It listens in the channels you choose, as you,
with your user token. It decides when to take part and replies in threads in your
first-person voice. When a request needs real work, it delegates that work to
Claude Code or Codex **workers** running on your machines: your laptop, a GPU box
over SSH, and later a Slurm cluster.

| Slack / infrastructure | Fridica |
| --- | --- |
| Workspace | **Parent agent**: your representative, one per daemon |
| Channel | Social and routing namespace; never tied to a machine |
| Thread | **Thread session**: goal, summary, decisions, sticky machine, repo, and branch, and its workers |
| Delegated job | **Worker**: a Claude Code or Codex session bound to one machine and one workspace |
| Machine | An entry in the **machine registry**: how to reach it, its capabilities, workspaces, and policy |

```text
                     Slack (Socket Mode, your user token)
                                   │
                         ┌─────────┴─────────┐
                         │   parent agent    │  tool-less: triage, reply, delegate
                         └─────────┬─────────┘
             ┌─────────────────────┼─────────────────────┐
        thread 1720.01        thread 1720.44         thread 1721.09     one serial actor per thread
        ┌────┴────┐                │
     worker A   worker B        worker C                                 compact WorkerResults flow up
        │          │               │
   ssh: snowy  ssh: dart9        local                                   agents run on the machine itself
   codex app-  claude -p         codex app-server
   server      stream-json
```

- **The thread is the unit of state.** A reply in a thread reaches that thread's
  session, and follow-ups need not repeat "on snowy, in exocubed": the session
  remembers.
- **Machine state and conversation state are separate.** "Also try it on dart9"
  adds a second worker. The snowy worker keeps its own context, files, and backend
  session. "Fix snowy and rerun" goes only to the snowy worker.
- **Context compaction by hierarchy.** A worker may read a hundred files and run
  dozens of commands. It returns a small structured `WorkerResult` (summary, files
  changed, validation, artifacts, machine state, open questions, and a Slack-ready
  report). The parent never sees raw shell output, and Slack sees about 300 tokens.
- **One level of delegation.** You, the parent, then workers. Workers never spawn
  workers.
- **Agents never construct SSH commands.** The parent names a machine, or capability
  tags such as `cuda` or `rtx5090`, plus a workspace. The registry in `config.toml`
  says how to reach it and what workers there may do.

One daemon serves one owner with one Slack app. Anyone in a configured channel can
talk to your Fridica. Only channels you list in `delegate_channels` (default: all
configured channels) can start work on your machines.

## Install

Use macOS or Linux with Python 3.11 or newer. On the machine that runs the daemon,
install and sign in to the CLI the parent uses:
[Claude Code](https://code.claude.com/docs/en/setup) (default) or
[Codex CLI](https://developers.openai.com/codex/cli). On every machine that runs
workers, install and sign in to the backends you list for it there, on the login
shell's `PATH`.

```bash
pip install fridica              # or: git clone … && pip install -e '.[dev]'
fridica init                     # ~/.config/fridica/config.toml, contract.md, manifest.yaml
```

### Sandbox dependencies on Linux

Both backends run agent commands inside a [bubblewrap](https://github.com/containers/bubblewrap)
sandbox on Linux; macOS uses the built-in `sandbox-exec` and needs nothing extra.
Install these on **every Linux machine that runs workers**, not only the one running the daemon.

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
fridica doctor   # expect: PASS   sandbox … (bubblewrap user namespaces)
```

`doctor` checks, on every configured machine, that the packages are present and
then runs `/bin/true` inside a bubblewrap user namespace, so it fails with the
exact `bwrap:` error when either step above is incomplete.

## Configure Slack

Use **one Slack app per person**, with a user token for that person's account.
Do not share tokens. Other channel members need no installation to interact with
your running Fridica; members of `delegate_channels` can start jobs on your machines.
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
| OAuth & Permissions → User Token Scopes | Read attached text files: diffs, logs, small outputs (optional) | `files:read` |
| OAuth & Permissions → User Token Scopes | Upload details files, figures, and PDFs to threads | `files:write` |
| OAuth & Permissions → User Token Scopes | Display names, e.g. in blocked notices (included in the manifest; optional) | `users:read` |

**Attached files.** With `files:read`, the daemon reads text attachments once a message
is to be answered, and shows them to the parent as untrusted data under a header naming
the file. Limits:

- only text types are read;
- at most three files and 64 KB of text per reply, the triggering message's files
  first;
- longer files are cut with a marker;
- downloads give up after 30 seconds.

Workers never hold the Slack token, and it is only ever sent to `files.slack.com`.
Without the scope the parent sees file names only, and `fridica doctor` warns once the
daemon has started.

**Bot Token Scopes and bot event subscriptions are not used.** You do not need a
public Request URL with Socket Mode.

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
- File *download*, reactions, channel administration, email, and bot mention
  scopes are not needed. What workers may touch on your machines is decided by
  machine policies in `config.toml`, not by Slack scopes.
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

### 3. Set your identity and channels

```bash
fridica configure --detect                                  # pick channels from a numbered list
fridica configure --detect --channel-name general --channel-name my-project
fridica configure --owner-id U123ABC --workspace-id T123ABC --channel-id C123ABC   # manual
```

Detection reads your identity with
[`auth.test`](https://docs.slack.dev/reference/methods/auth.test/) and your joined
channels with [`conversations.list`](https://docs.slack.dev/reference/methods/conversations.list/),
using `SLACK_USER_TOKEN` only. It posts nothing. `configure` keeps the comments in
`config.toml` and only fills in `[owner] slack_user`, `[slack] workspace` and
`[slack] channels`.

## Configure machines

`config.toml` has a fixed set of tables, and unknown keys are errors. Defaults live
in `fridica/config/schema.py`. A complete example:

```toml
[owner]
slack_user = "U123ABC"
profile = "Planetary atmospheres; maintainer of snapy and kintera."

[slack]
workspace = "T123ABC"
channels = ["C0RESEARCH", "C0LAB"]
delegate_channels = ["C0RESEARCH"]   # who may start jobs on your machines (default: every channel)
general_messages = true              # consider joining without an @mention (a cheap triage call decides)
cooldown = 60

[parent]
backend = "claude"                   # always local and tool-less
model = ""                           # "" = the CLI's default
triage_model = ""                    # e.g. a cheaper model for the join-or-not decision
default_machine = "laptop"

[limits]
max_wait_replies = 3                 # consecutive clarifying questions before the thread pauses
max_no_progress = 3                  # acknowledgments or repeats before the thread pauses
max_delegations_per_turn = 3
max_workers_per_thread = 4
max_jobs = 4                         # jobs running at once, across all machines
job_timeout = 14400
worker_idle = 1800                   # keep an idle worker process this long for follow-ups
auto_resume = false                  # rerun jobs a restart interrupted, continuing their sessions

[policy]                             # defaults for every machine; machines and workspaces override
mode = "write"                       # read-only | write | full
network = ["github.com", "pypi.org", "files.pythonhosted.org"]
approvals = "auto"                   # auto (default) | on-request | untrusted | never
approval_timeout = 1800
auto_approve = ["pytest", "git status"]    # exact commands or "prefix …"; never with ; | & $() etc.
auto_deny = ["rm -rf"]

[machines.laptop]
transport = "local"
backends = ["claude", "codex"]
[machines.laptop.workspaces]
notes = { path = "~/notes", policy = { mode = "read-only" } }
fridica = { path = "~/scix/repos/fridica", subfolders = false }   # a single checkout: no worker1/worker2 inside it

[machines.snowy]
transport = "ssh"
host = "snowy"                       # an ~/.ssh/config alias; `ssh snowy` must work without a prompt
tags = ["cuda", "rtx5090"]
backends = ["codex", "claude"]       # the first is the default
max_workers = 3                      # live worker processes on this machine (default 4)
max_jobs = 2                         # running jobs on this machine (default 2)
resources = { cpus = 32, gpus = [0], gpu_type = "RTX 5090", memory_gb = 128 }   # GPUs turn on gpu_confine
[machines.snowy.workspaces]
exocubed = "~/scix/repos/exocubed"
canoe = "/home/me/canoe"

[machines.dart9]
host = "me@dart9"                    # transport defaults to ssh
tags = ["cuda", "gcc"]
backends = ["codex"]
[machines.dart9.workspaces]
canoe = "~/repos/canoe"

[machines.greatlakes]                # registered, validated, and shown to the parent; jobs fail until Slurm lands
transport = "slurm"
host = "greatlakes"
slurm = { account = "me0", partition = "gpu", gres = "gpu:1", time = "04:00:00" }
[machines.greatlakes.workspaces]
scratch = "/scratch/me0/me"

[state]
path = "~/.local/state/fridica/state.sqlite3"
# control_socket = "…"               # default: beside the state database, or a short runtime path
```

**How work is placed.** A delegation names a machine, or capability tags, plus a
workspace and optionally a backend. The order of precedence:

1. an explicit machine;
2. tags (preferring the thread's machine, then the least busy);
3. the thread's sticky machine;
4. `default_machine`.

A workspace that exists on exactly one machine selects that machine. Ambiguity is an
error listing the candidates, which the parent fixes in one repair round. The parent
sees machine names, tags, workspace names, and load, never filesystem paths.

**Policy modes:**

| `mode` | Codex worker | Claude worker |
| --- | --- | --- |
| `read-only` | `readOnly` sandbox | only Read, Glob, and Grep |
| `write` | `workspaceWrite` sandbox in the workspace | edits accepted, Bash sandboxed with `network` as its domain allowlist |
| `full` | no sandbox | no sandbox (`bypassPermissions` when `approvals = "never"`) |

Codex supports network access only as all or nothing, so any `network` entry gives
Codex workers full network access.

**GPUs.** The backends' own sandboxes hide the GPU device nodes, so a worker in
them cannot run CUDA. `gpu_confine` runs the backend with its own sandbox off,
inside Fridica's bubblewrap confinement, which exposes `/dev`. It turns on
automatically for `write`-mode workspaces on a machine that declares
`resources.gpus`:

- read-only workspaces keep the backend sandbox, because confinement can't enforce
  read-only;
- `full`-mode workspaces have no sandbox hiding the GPUs to begin with;
- `gpu_confine = false` opts a machine or workspace out;
- an explicit `gpu_confine = true` requires `resources.gpus` and can't be combined
  with `read-only`.

Under the confinement:

- only the worker's own workspace is writable;
- the backends' settings files are bound read-only;
- the host network is shared, because the CLI must reach its model API.

`resources` are declarative. They are shown to the parent and enforced as
`OMP_NUM_THREADS` and `CUDA_VISIBLE_DEVICES`.

**Job slots, subfolders, and GPUs.** A machine runs up to `max_jobs` jobs at once,
one per **slot**, and each worker keeps its slot for its whole life. The slots
split the machine's `resources.gpus`, passed to the job as `CUDA_VISIBLE_DEVICES`:

- 2 GPUs with `max_jobs = 2`: slot 1 gets GPU 0 and slot 2 gets GPU 1;
- 4 GPUs with 2 slots: 0–1 and 2–3;
- fewer GPUs than slots: GPUs are shared round robin.

Each slot also works in its own subfolder of a writable workspace, created on
first use:

```toml
[machines.dungeon2.workspaces]
ai = "/data01/ai_workspace"                                  # slot 1: …/worker1, slot 2: …/worker2
fridica = { path = "~/repos/fridica", subfolders = false }   # a workspace that is itself one checkout
```

Two concurrent jobs then never share a directory or a GPU. Subfolders are on by
default for writable workspaces and off for read-only ones. Turn them off for a
workspace that is a single repository checkout, where `worker1/` inside the repo
would make no sense; jobs there share the directory. A worker whose slot is
busy waits rather than moving to another slot, because its session (and, for
Claude, the ability to resume it) is tied to its directory. Multi-GPU jobs need
`max_jobs = 1`, so the single slot gets every GPU.

The daemon rereads `config.toml` when it changes. A rejected edit keeps the running
configuration. Machine changes apply to workers started afterwards.

## Agent contract

`contract.md`, next to `config.toml`, is the rulebook, and it is reread on every call.
Only text under `##` headings reaches a model:

| Section | Governs |
| --- | --- |
| `## Participation` (required) | the triage call that decides whether to join an unaddressed conversation |
| `## Replies` (required) | the parent's voice and reply rules |
| `## Delegation` | when and how the parent delegates, fans out, follows up, and composes results |
| `## Worker reports` | standing instructions every worker receives |
| `## Debriefs` | the closing debrief of a finished discussion |
| any other `##` section | given to both the parent and the workers (the packaged `## Repo rules` is an example) |

Missing optional sections fall back to the packaged ones.

## Repository list

`fridica/parent/repos.toml` ships with the package and is shared by the whole team.
Change it through a pull request. Each entry has a name, a GitHub URL, and
collaborators; the first collaborator is the owner, and their word is final. The
list travels to the parent and to workers as data. Workers find checkouts by git
remote, because entries never contain local paths. `[parent] repos = "…"` overrides
the list for local testing.

## GitHub links

When a message in a thread links a GitHub pull request or issue, the parent sees its
state as it is right now, not as someone last described it:

- title and state;
- head commit and tree, base branch, and whether the head is behind the base;
- mergeable state (`unknown` is shown as such after one retry);
- check runs on the head, where `cancelled` is its own state and never counts as
  success;
- reviews as `approved @<sha>`, with approvals on an older commit marked stale.
  `approvals 2/3` means two approvals on the head out of three reviewers whose latest
  decision is an approval or a change request;
- assignees, labels, and the status lines at the top of the body (owner, next,
  blocker, waiting on).

The rest of the body, including HTML comments, is not passed on, and the block is
marked untrusted data. A `head:` or `tree:` line in the body is not shown as a fact. It
is checked against the real commit and reported as matching or not. CI reads
`incomplete` when there are more check runs than one call lists.

At most three links per call are followed, and a slow GitHub never delays a reply by
more than 20 seconds. Results are cached per repository and number, and a rate limit
pauses all calls until it resets:

```toml
[github]
enabled = true
token_env = "FRIDICA_GITHUB_TOKEN"   # optional read-only token
cache_seconds = 180
```

Public repositories need no token, but anonymous requests are limited to 60 per hour
per IP address, and each pull request costs about five. A read-only fine-grained token
in the named variable raises that limit and reaches private repositories. Like the
Slack tokens, it is removed from every agent's environment. Changing `token_env` takes
a restart.

## Run

```bash
fridica doctor                      # config, contract, tokens, the parent CLI, and every machine
fridica start --observe-only        # store messages, call nothing, post nothing
fridica start
```

`doctor` checks the following without calling a model:

- that each SSH machine is reachable without a prompt;
- that every workspace exists;
- that each backend is installed, new enough, and signed in (for Codex, the app-server
  protocol must include approvals, `turn/interrupt` and `outputSchema`);
- the bubblewrap and socat sandbox, including user namespaces;
- a warning when `~/.codex/config.toml` defines MCP servers, because `codex app-server`
  cannot ignore the user config.

Slack authorization and channel membership are verified by `start`.

## In Slack

```text
#research
Alice:   @you compare this branch on snowy and dart9
  you:   Starting on both machines; I'll post the numbers here.          ← parent delegates two jobs
  you:   snowy (RTX 5090): 1.82 s/step. dart9: 2.34 s/step. The gap is   ← one reply once both finish
         the device init path; details attached.
Alice:   can you fix snowy and rerun?
  you:   On it.                                                           ← only the snowy worker, same session
  you:   Fixed the device selection order; 18/18 tests and the 2-GPU run pass.
```

- **@mentions** always get a reply. Unaddressed messages go through a cheap triage
  call when `general_messages` is on, limited by a per-channel cooldown. Follow-ups
  in a thread you are already part of are triaged too.
- **Clarification.** A reply with status `waiting` addresses the requester, and
  their next message is answered without a mention. `max_wait_replies` consecutive
  questions pause the thread.
- **Results.** Delegations made in one turn form a group. The parent writes one
  reply when all of them finish, and says when workers disagree. A single finished
  job's `report` is posted directly with no second parent call. Figures and PDFs
  a worker lists are uploaded after the reply. Long replies keep an executive
  summary in the thread and attach the rest as a Markdown file.
- **No turn limit.** A thread can go on as long as people keep talking to you.
  Runaway exchanges are stopped by the loop protections below instead: clarifying
  questions (`max_wait_replies`) and turns without progress (`max_no_progress`)
  pause the thread, and the dashboard or `fridica threads ID resume` restarts it.
- **Blocked threads say why, once.** When a reply ends a thread as blocked, later
  mentions get a single `Blocked: <blocker>. Next: <name> to <next_step>.` built from
  the thread's task note. People are named in plain text, so nobody is paged. After
  that the thread stays quiet until it is resumed or its blocker changes.
- **No identical resends.** A reply that repeats the thread's last reply word for word,
  with the same status and nothing new (no details file, job, or attachment), is not
  posted again. It is still posted when the message @-mentions you or asks for a
  repost, for the owner's own instruction, and for worker results; a correction is
  always posted. A dropped repeat counts as a turn without progress.
- **Debrief.** When the parent marks a discussion finished, a debrief is posted to
  the channel.
- **Several owners' Fridicas in one thread.** Every post carries metadata:
  `{owner, session, turn, status, kind}`, plus `task_id` for older versions. A
  peer's finished reply or debrief is ignored unless it addresses you, and a
  thread pauses after repeated questions or turns without progress, so agents
  cannot talk to each other forever.
- Your own messages never trigger your agent. The echo of Fridica's own posts is
  recognized and stored as history.

## Approvals

| `approvals` | Who decides a worker's requests beyond its policy |
| --- | --- |
| `never` | nobody; the request is refused and the worker carries on without it |
| `on-request` | you, when the worker needs more than its sandbox allows |
| `untrusted` | you, for edits and most commands, even inside the sandbox |
| `auto` (default) | the backend's own AI reviewer: Claude's auto permission mode, or Codex's `auto_review` guardian |

With `auto`, Codex's reviewer approves or denies each request itself (its decisions
are logged). Claude's classifier needs a model that supports auto mode. On other
models Claude falls back to default mode, logs a warning, and sends its prompts to
you instead. `doctor` checks that each backend supports auto on machines that use it.

With `on-request` or `untrusted`, a worker's request for something outside its
policy is routed to you. For Codex these are commands, file changes,
and extra permissions from the app-server protocol. For Claude, they are tool calls
outside its allowlist, through the stream-json control protocol. The request works
the same over SSH. While it waits, the job holds its machine slot.

```bash
fridica approvals                     # pending requests
fridica approvals a1b2c3 once         # or: session | deny
```

`auto_approve` and `auto_deny` prefixes decide simple commands without asking.
Anything containing shell control characters always asks. After `approval_timeout`
the request is denied and the worker continues without it. Interrupting a job
denies its pending request at once.

## Dashboard and CLI

The daemon serves a control API on a Unix socket that only you can open (mode 0600).
The CLI and the dashboard both use it and never write the database.

```bash
fridica status | threads [ID [resume|pause|close|archive|restore|clean]] | workers [ID interrupt|stop]
fridica machines | outbox [ID]          # outbox ID retries a failed or ambiguous post
fridica dashboard --port 8765           # prints http://127.0.0.1:8765/#key=…
```

The dashboard shows approvals, stalled threads, active jobs, and failed posts.
It also has thread, worker, machine, activity, and settings views. The owner can
give a thread a private instruction; it follows the normal worker scopes and
approvals. Settings can change the parent model and workload limits. Live
refresh can be switched off; viewing the page makes no model calls.

It listens on 127.0.0.1 only, rejects cross-origin requests, and requires the
printed key for every API call.

## State, recovery, and guarantees

The daemon owns a single SQLite database. `fridica/store/schema.py` is the only
module that runs DDL, and migrations are versioned.

- **Persist before acknowledging.** A Slack event is stored, together with its
  thread and inbox row, before Socket Mode is acked. Catch-up re-reads each channel
  from its last complete pass, however long the daemon was down, up to 7 days back.
  A pass that hit the paging cap is repeated from the same point, and a new database
  starts 1 hour back. Replies are refetched for threads that were active just before
  the gap, and every 5 minutes at least the last 15 minutes are re-read. Missed
  messages from the last day are handled normally, and so are older ones that mention
  you. Other older messages are stored as history but not answered.
- **One serial actor per thread, threads in parallel.** An inbox item's effects
  commit in one transaction: posts, jobs, workers, session changes, and parent-call
  records. A crash either retries the item from scratch or leaves it fully applied.
- **Every post goes through the outbox**, including notices, reports, debriefs,
  and uploads. Each has an idempotency key, and posts go out in order
  within each thread.
  - A rate limit reschedules the post.
  - A rejection fails it.
  - An unknown outcome (5xx, a dropped connection) marks it `ambiguous`, which is
    never resent automatically.
  - Posts that depend on a failed one are marked `blocked`, where you can see them.
- **Restart recovery:**
  - Jobs that were running become `interrupted`, and their thread is told.
    `auto_resume` reruns them once, continuing their sessions.
  - Pending approvals expire.
  - Posts that may have been sent become `ambiguous`.
- A second daemon on the same database is refused. The database is bound to one
  Slack identity.

## Security model

- Slack tokens and the optional GitHub token are removed from every agent's environment.
  The parent has no tools.
- Workers run under their backend's sandbox, or Fridica's bubblewrap for
  `gpu_confine`, with per-workspace policy.
- A writable local workspace may not contain Fridica itself, `config.toml`, or the
  contract.
- Links are followed only into configured channels, so nobody can make Fridica read
  a channel they cannot read.
- Workers' artifacts are read only from inside their workspace, with symlinks
  resolved, and must match their declared type (PNG, PDF, or UTF-8 Markdown).
- Message text, notes, linked messages, GitHub state, and worker results are passed to
  models as data, marked untrusted.

## Development

```bash
pip install -e '.[dev]'
python -m pytest -q                   # no network; fake Slack, fake claude/codex/ssh/bwrap executables
node --test tests/dashboard.test.cjs
ruff check src tests
```

The architecture is described in [`docs/architecture.md`](docs/architecture.md).
A data-backed design document (layers, context management, security, mapping to Slack, and a comparison
with the previous design) is in [`docs/fridica-design.pdf`](docs/fridica-design.pdf); see [`docs/README.md`](docs/README.md)
to rebuild it.

**Continuous integration** (`.github/workflows/ci.yml`) runs pytest and the node
test on Ubuntu and macOS, and builds and checks the wheel.

**Releases:**
- When a pull request is merged, `cd.yml` tags the next version using its
  `release:*` label, then creates a GitHub release.
- `release.yml` publishes a tag to PyPI after `scripts/release.py verify` confirms
  that the wheel bundles the Slack manifest, the contract, the repository list, the
  configuration template, and the dashboard.
