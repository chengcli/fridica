# Upgrading from v0.2 to v0.3

v0.3 (PR #22 and later) is a new program, not an update of v0.2. Two things show that
immediately:

- **The v0.2 config is rejected.** It has flat keys such as `owner_id`, so `fridica start`
  and `fridica doctor` stop with `unknown keys in top level: …`.
- **The v0.2 database cannot be migrated.** Starting v0.3 on it fails in migration V1 with
  `table runtime already exists`.

There is no `fridica migrate`. The upgrade takes about ten minutes by hand.

## Steps

1. **Stop the v0.2 daemon**, including any listener or dashboard services.
2. **Move the old files aside.** Nothing in them is read again. The database uses
   SQLite's write-ahead log, so move its `-wal` and `-shm` files with it: after an
   unclean stop, recent data is still in them.

   ```bash
   mv ~/.config/fridica/config.toml ~/.config/fridica/config.v0.2.toml
   cd ~/.local/state/fridica
   for suffix in "" -wal -shm; do
     if [ -e "state.sqlite3$suffix" ]; then mv "state.sqlite3$suffix" "state.v0.2.sqlite3$suffix"; fi
   done
   ```

   Keep `contract.md` if you edited it. v0.3 reads the same file, but compare it with the
   packaged `src/fridica/parent/contract.md`, which gained sections on delegation and
   worker reports.
3. **Install v0.3** (`git pull`, then `python -m pip install -e .`) and run `fridica init`.
   It detects your Slack identity and channels and writes a commented `config.toml`.
4. **Carry your settings over** with the key map below.
5. **Check it:** `fridica doctor`, then `fridica start`. Workers start with a new history.
   Threads from before the upgrade are not resumed, and catch-up reads the last hour.

## Key map

| v0.2 key | v0.3 |
| --- | --- |
| `owner_id` | `[owner] slack_user` |
| `profile` | `[owner] profile` |
| `contract` | `[owner] contract` |
| `workspace_id` | `[slack] workspace` |
| `channels` | `[slack] channels` |
| `app_token_env`, `user_token_env` | `[slack] app_token_env`, `[slack] user_token_env` |
| `general_messages` | `[slack] general_messages` |
| `cooldown` | `[slack] cooldown` |
| `backend` | `[parent] backend` for the coordinating agent; `[machines.<name>] backends` for workers |
| `model`, `reasoning_effort` | `[parent] model`, `[parent] reasoning_effort` |
| `timeout` | `[parent] timeout` (one parent call; default 180 s, not 600) |
| `context_limit` (messages) | `[parent] context_chars` (characters of thread history; default 24000) |
| `repos` | `[parent] repos` |
| `max_wait_replies` | `[limits] max_wait_replies` |
| `session_timeout` | `[limits] session_timeout` |
| `heavy_task_timeout` | `[limits] job_timeout` |
| `heavy_task_idle` | `[limits] worker_idle` |
| `allowed_domains` | `[policy] network` (`["*"]` still means every host). **The defaults differ:** v0.2 allowed every host when the key was absent, while v0.3 allows none, and `fridica init` writes only GitHub and PyPI. Set `network = ["*"]` to keep v0.2's behaviour |
| `state_path` | `[state] path`. Point it at a new file, never the v0.2 database |
| `workspace = "~/p"` | `[machines.local]` with `transport = "local"`, and `p = "~/p"` under `[machines.local.workspaces]` |
| `workspace = "host:/p"`, `additional_workspaces` | one `[machines.<host>]` per host, with `transport = "ssh"`, `host = "<host>"`, and its folders under `[machines.<host>.workspaces]` |
| `read_only_workspaces` | a workspace with a policy: `name = { path = "…", policy = { mode = "read-only" } }` |
| `[resources.<host>]` (`cpus`, `gpus`, `gpu_type`, `memory_gb`, `notes`) | `[machines.<host>] resources = { … }` with the same keys |
| `[resources]` or `[resources.local]` (the primary host) | `resources = { … }` on the machine that holds `workspace` (`[machines.local]` for a local one) |
| `gpu_access = false` | `[machines.<host>] policy = { gpu_confine = false }` |

**Keys with no equivalent. Drop them:**

- `max_turns`: v0.3 has no turn limit. Loops are stopped by `max_wait_replies` and
  `[limits] max_no_progress`.
- `resume_sessions`: workers always resume their own sessions, and the coordinating
  agent keeps no session.
- `heavy_tasks`: delegation is always available. v0.2 had it off by default, so if you
  never turned it on, set `[slack] delegate_channels = []` to keep work off in every
  channel, or list the channels whose members may start jobs.
- `file_access`: files are governed by the backend sandbox and `[policy] mode`
  (`read-only`, `write` or `full`) per machine or workspace.
- `ssh_host`, `remote_hosts`: these are now the `[machines.*]` tables.

## Example

A v0.2 setup with a local project, a GPU host `dart9` and a read-only data folder:

```toml
# v0.2
owner_id = "U012ABCDEF"
workspace_id = "T012ABCDEF"
channels = ["C012ABCDEF"]
workspace = "~/projects/kintera"
additional_workspaces = ["dart9:/mnt/data1/projects"]
read_only_workspaces = ["~/data"]
max_turns = 20
heavy_tasks = true
[resources.dart9]
gpus = [0, 1]
gpu_type = "Quadro RTX 4000"
```

becomes:

```toml
# v0.3
[owner]
slack_user = "U012ABCDEF"

[slack]
workspace = "T012ABCDEF"
channels = ["C012ABCDEF"]

[policy]
network = ["*"]                   # v0.2's default; v0.3 allows no host unless listed

[machines.local]
transport = "local"
backends = ["claude"]

[machines.local.workspaces]
kintera = "~/projects/kintera"
data = { path = "~/data", policy = { mode = "read-only" } }

[machines.dart9]
transport = "ssh"
host = "dart9"
tags = ["cuda"]
backends = ["claude"]
resources = { gpus = [0, 1], gpu_type = "Quadro RTX 4000" }

[machines.dart9.workspaces]
projects = "/mnt/data1/projects"
```

`max_turns` and `heavy_tasks` are gone: there is no turn limit, and delegation is
always on (this setup had `heavy_tasks = true`).
