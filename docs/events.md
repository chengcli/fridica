# The event feed

A read-only, owner-only feed of what the daemon sees and decides, for local
tools that follow Fridica: a coordinating session that wants to be woken when a
thread it asked in gets a reply, a sign-off lands, a turn ends blocked, a thread
is paused, or a post fails. It replaces reading `state.sqlite3` directly: the
tables there are internal and change with migrations; this feed is versioned.

```sh
fridica events --since 0                 # everything the ledger still has, one JSON object per line
fridica events --since 41820 --follow    # resume after cursor 41820 and keep following
fridica events                           # {"v":1,"next":41977}: where the ledger ends now
fridica events --follow                  # only new events from now on
```

Over the control socket the same objects come from
`GET /events?after=<cursor>&limit=<n>`, which answers
`{"v":1,"events":[...],"next":<cursor>,"scanned":<n>}`. `GET /events` without
`after` answers with no events and `next` at the ledger's end.

## Cursor

Every object carries a `cursor`, an integer that only grows. It is the sequence
number of the daemon's replay ledger, so it stays valid across daemon restarts
and schema migrations. A read scans at most `limit` ledger records (1 to 1000,
default 100 over the API, 1000 in the CLI) after `after`; some records are
internal and produce no event, so a page may hold fewer events than records
scanned, or none. `next` is the last record scanned, or `after` when there was
nothing to scan. Resume from `next`: a client that does so sees every event
exactly once, with no gap and no repeat. `scanned < limit` means the page
reached the ledger's end for now; the feed's own requests are ledger records,
so `next` alone never stands still. `--follow` polls once a second when a page
at the end had no events, and reads on at once otherwise.

## Object

Every event has these fields:

| field | meaning |
|---|---|
| `v` | schema version, `1` |
| `cursor` | the resume point, see above |
| `time` | when the daemon recorded it (Unix seconds) |
| `kind` | `message`, `turn`, `thread_control`, `outbox`, `job` or `daemon` |
| `workspace` | Slack workspace ID, `""` for `daemon` |
| `channel` | `{"id": "C…", "name": "ai-human-plume"}`; `name` is `null` until the daemon has recorded it |
| `thread` | the thread's root message `ts`, or `null` when the event has no thread |

and then fields of its kind:

**`message`**, a message stored from a configured channel
: `ts`, `sender` (user ID), `sender_name` (when known, else `null`),
  `mentions_owner`, `text`, `files` (attachment count), `source` (`socket` or
  `catchup`). For the owner's own posts, which carry Fridica's metadata, also
  `turn_status` (`complete`, `waiting` or `blocked`) and `turn_kind` (`reply`,
  `report`, …).

**`turn`**, a parent decision committed for a thread
: `outcome`: one of `blocked`, `waiting`, `delegated`, `replied`,
  `handed_off_without_post` (nothing posted, but a summary or next step was
  recorded) or `no_reply`, chosen in that order of precedence; `trigger_ts`
  (the message that caused the turn, when one did); `status` (the reply's
  status); `delegations` (count); `summary`, `next_step`, `blocker` (each at
  most 300 characters).

**`thread_control`**, a thread's control state changed
: `action`: `paused`, `resumed`, `closed`, `archived`, `restored`, `cleaned`
  or `instructed`; `reason` (for a pause); `actor`: `owner`, `system` or
  `desktop_read_only`.

**`outbox`**, a post that did not go out (sent posts are not events; the
message they become is)
: `outcome`: `rejected`, `failed` or `ambiguous`; `code` (for example
  `egress_ai_trailer`, `rate_limited`, `delivery_timeout`); `post_kind`
  (`reply`, `report`, `upload`); `outbox_id`; `attempt`.

**`job`**, a worker job
: `action`: `started`, `finished`, `failed` or `interrupted`; `job_id`,
  `attempt`; on `started` also `worker_id`, `machine`, `workspace`, `backend`;
  on completion `status` (the worker's own status) or `code` (the failure).

**`daemon`**
: `action`: `started` (with `observe_only`) or `stopped` (with `failure`, or
  `null` for a clean stop).

Example:

```json
{"v":1,"cursor":41821,"time":1790880844.7,"kind":"turn","workspace":"TJ6E2EJ2K",
 "channel":{"id":"C0C3XG2UXBL","name":"ai-human-plume"},"thread":"1790880807.547759",
 "outcome":"delegated","trigger_ts":"1790880807.547759","status":"complete",
 "delegations":1,"summary":"","next_step":"","blocker":""}
```

## Versioning

`v` names the schema of the object. Within a version, fields are only added:
a client must ignore fields it does not know. Removing or renaming a field, or
changing a field's meaning, bumps `v`. New `kind` values and new `action` or
`outcome` values may appear within a version, so a client should treat unknown
ones as "something happened" rather than fail.

## Scope

Read-only and local: the feed is served only on the control socket, to the
owner's authority; the read-only desktop capability cannot read it. It adds no
control action. Message text is the owner's own channel data; the daemon's
prompts, raw Slack bodies and tokens never appear.
