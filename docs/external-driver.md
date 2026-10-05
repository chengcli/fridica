# The external-driver surface

A local program can drive a thread's work instead of its parent turns: start
workers in a fixed order, post study claims and results, stop workers, and read
what happens on the [event feed](events.md). Fridica stays the durable
substrate (workers, placement, limits, the outbox and its egress gate, the
store); the driver owns the policy. [fridica-research](https://github.com/chengcli/fridica-research)
is the first driver (fridica#126, fridica#130).

Every route below is on the owner-only control socket, takes a JSON object and
answers JSON. Like every control route, it rejects an unknown body field with
`400 unknown_body_field`, the read-only desktop capability with `403`, and
answers errors as `{"error": "<code>"}`. A body may carry `actor`; it is
ignored, since authority comes from the connection. A thread is named by its
full ID (`workspace:channel:root_ts`) or `#channel:TS`.

## Who drives a thread

```
POST /threads/<id>/driver   {"driver": "external" | "parent"}
→ {"driver": "external", "changed": true}
```

`fridica threads <id> driver external|parent` does the same. A thread is
driven by its `parent` until set otherwise. A change bumps the thread's
version, so a turn that loaded the old driver commits nothing and runs again,
and is audited (`action: driver`). Errors: `400 invalid_driver`,
`404 no_such_thread`.

In an `external` thread:

- worker results and worker interruptions, and messages whose Fridica metadata
  has a `kind` starting with `study_` (a peer's `study_claim`, say), are
  settled with no parent call and no reply; their finished jobs count as
  reported. The driver reads them on the feed as `job_result` and `peer_post`;
- anyone else who addresses the owner still gets a parent turn, but the parent
  may not delegate (its prompt says `delegation_allowed: false`, and a
  delegation is refused for repair).

A peer study post that mentions the owner still opens an ask; when it comes
due, the parent answers it as usual.

## Delegate

```
POST /threads/<id>/delegate
{"role": "tester", "brief": "ref: …\n…", "context": "fresh" | "fork",
 "worker_id": "worker-…", "ephemeral": false, "backend": "same" | "other" | "<name>",
 "deliverable": "report", "tags": ["…"]}
→ {"join_group": "group-…", "jobs": [{"job_id": "job-…", "worker_id": "worker-…", "role": "tester"}]}
```

One delegation per call, validated and placed exactly as a parent's
(`fridica_core::delegation::prepare`, with the thread's scope and the host's
roles): role, brief (at most 40000 characters), deliverable, the
per-thread limit of persistent workers, placement. Omitted fields take the
defaults shown first above; `role` defaults to `general`, `worker_id` to a new
worker. `worker_id` resumes that live worker of the thread in its backend
session. `backend: same` is the machine's default backend, as a parent
delegation without a backend gets; `other` is another configured backend (the
thread's machine's first, then any machine's); any other value names one.

The workers and jobs are committed in one unit of work, without a parent turn:
the thread's streaks and reply budget are untouched and nothing is posted. The
next pass of the daemon (every 250 ms) starts the jobs, like those of a parent
turn. Each call is its own join group, with no inbox item behind it
(`inbox_id` is `null` on the job view).

`tags` are the driver's own correlation labels (at most 16, each 1 to 200
characters). They are stored on the job, echoed on `GET /threads/<id>` `jobs[]`
and never select a machine. This is not the parent's delegation `tags`, which
are machine selectors.

Refusals: `409 slots` when the thread has as many persistent workers as
`limits.max_workers_per_thread`; `400 invalid_role`, `invalid_brief`,
`invalid_context`, `invalid_deliverable`, `invalid_tags`, `invalid_backend`,
`placement` (no machine, workspace or backend fits), `invalid_delegation`
(anything else `prepare` refuses); `403 delegation_disabled` (the channel may
not delegate); `404 unknown_worker` (`worker_id` is not a live worker of the
thread), `no_such_thread`; `409 no_other_backend`, `thread_closed` (closed,
archived or cleaned), `observe_only`.

## Post

```
POST /threads/<id>/post
{"text": "…", "details": "…", "meta": {"kind": "study_claim" | "study_result" | "study_root" | "report",
 "status": "complete" | "waiting" | "blocked"}, "client_id": "…"}
→ {"outbox_id": 42}

POST /channels/<channel>/post      (kind study_root only)
→ {"outbox_id": 43}   or, for a root already sent, {"outbox_id": 43, "thread_id": "T:C:TS", "thread": "T:C:TS"}
```

The post is queued in the outbox with Fridica's metadata (`kind`, `status`,
the owner, the thread and its turn count) and goes out through the dispatcher
like any post, so the egress gate applies. It reserves no reply: a paused
thread still posts it, and the reply budget is untouched. `details` (not on a
root) follows as an uploaded `details-<outbox_id>.md`. `client_id` (8 to 80
letters, digits or `-`) makes the call idempotent: the same `client_id` with
the same post answers the same `outbox_id`; with a different post,
`409 client_id_conflict`.

A `study_root` posts a new root message in the thread's channel, or in
`<channel>` (a configured channel's ID or recorded name). Its thread does not
exist until Slack has the message, so the answer names it only when the post
was already sent (a repeated `client_id`); the call never waits for Slack.
Otherwise the driver learns the new thread from the echoed `message` event,
whose `ts` equals its `thread` (the post's text can carry a `ref:` line to
match it). A root posted from `/channels/<channel>/post` is kept under the
placeholder thread `<workspace>:<channel>:channel`.

The outbox kind is the metadata kind, except a driver's `report`, which is
queued as `driver_report`: a refusal of a driver's post never gives the parent
a rewrite turn. A refused or rate-limited post is an `outbox` event with that
`post_kind`; a sent post is no event, only the `message` it becomes.

Errors: `400 invalid_text` (empty or over 40000 characters), `invalid_details`,
`invalid_meta`, `unknown_meta_field`, `invalid_post_kind`, `invalid_status`,
`invalid_client_id`; `404 no_such_thread`, `unknown_channel`;
`409 observe_only`.

## Stop a worker

```
POST /threads/<id>/workers/<worker_id>/stop   {"mode": "stop" | "interrupt"}
→ {"stopped": true}   or   {"interrupted": true | false}
```

The owner's worker control (`fridica workers <id> stop|interrupt`) for a
worker of this thread: `stop` (the default) cancels its queued jobs and
retires it, `interrupt` interrupts its running attempt (`false` when nothing
was running). Errors: `400 invalid_mode`, `404 no_such_worker` (not a worker
of this thread).

## Reading results

`job_result` on the feed carries each finished, failed or interrupted job
attempt with its join group, worker, role and result; `peer_post` carries
another owner's posts with their metadata. After a restart, `GET /threads/<id>`
has the same facts: each of `jobs[]` has `tags`, `role` (its worker's),
`join_group`, `attempt`, `result`, `error` and `job_status` (`finished`,
`failed` or `interrupted` once done, else its `status`). The join group is
`join_group`; `inbox_id` is the parent turn behind a parent's delegation and
`null` for a driver's.
