# fridica-core

Fridica's domain, without I/O: the types and pure logic that do not depend
on Slack, SQLite or worker processes.

- `ids`, `time`: thread IDs (`workspace:channel:root_ts`), clocks and ID sources.
- `parent`: the parent's request and decision, its action schema and context
  trimming, and the `Parent` adapter trait.
- `delegation`: validating a decision and placing its delegations, within a
  `Scope` (whether the channel may delegate, `Limits`, the machine `Registry`).
- `placement`: sticky, load-aware machine and workspace selection, and the
  fixed load probe's parser and assessment.
- `worker`, `result`, `approvals`: worker records, jobs, results and their
  format, failures, approval requests and automatic command rules.
- `delivery`: outbox posts and delivery outcomes, and the `Delivery` trait.
- `policy`, `render`, `failure`: attention gates, reply rendering and blocked
  turns.
- `config`: the parent, limits, placement and attention settings, and the
  machine registry. Loading them from TOML belongs to the host.

It lives in the Fridica repository and changes with it; the `fridica` crate
re-exports it as `fridica::core`.

## License

MIT
