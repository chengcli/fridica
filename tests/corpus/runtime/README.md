# Ordered Rust runtime fixtures

These are newly captured synthetic fixtures, not historical Slack data. Generate
with `FRIDICA_CAPTURE_REPLAY=1 cargo test --offline --test runtime_flow complete_ordered -- --test-threads=1`.
Normal tests never rewrite them. Review fixture changes as behavioral changes.

Each fixture contains an ordered tape of full parent requests and responses or
typed errors, external job context and artifact snapshots, worker requests and
completions, and delivery arguments/outcomes. Errors are serialized as the
adapter's `Result::Err`; uncertain delivery is an explicit `Ambiguous` outcome.
The test drives intake, duplicate intake, clock changes and owner controls and
compares consistent SQLite snapshots at every named checkpoint. Every table and
column is included, including verdicts, inbox ordering, outbox content and links,
worker/session/job state, obligations, reservations, audits and replay events.

| Fixture | Additional assertion |
| --- | --- |
| success | One direct report, confirmed obligation closure, no second model call |
| ambiguous | Unknown report delivery never closes the ask or resends |
| rate_limit | Retry time and prerequisites retain ordered delivery |
| retry | One execution retry retains the backend session |
| owner_pause | Only owner resume releases a completed result |
| parent_error | Typed adapter failure creates a visible disposition |
| observe | No parent, worker, context or delivery effects |
| restart_result | Unconsumed completion is delivered once after reopening SQLite |
| restart_sending | A claimed send becomes ambiguous after reopening SQLite |

There are no waived state differences. Each fixture explicitly binds its
random temporary root to `__ROOT__` and its corresponding configuration fingerprint
to `__CONFIG__`; time and IDs are injected. These bindings do not remove columns.
The replay must consume every call and result, match the complete arguments, and
honor completion order. Separate negative tests reject incomplete, duplicated,
orphaned and misordered tape events; concurrent completion tests exercise the
replay scheduler.

This proves exact repeatability for the scripted Rust adapter boundary, not
Python/Rust protocol-envelope identity. The frozen Python differential checks in
`flow.json`, `outbox.json`, `parent.json` and the other corpus files
retain their documented projections and exceptions. Attention is scripted here;
historical evidence is neither inferred nor fabricated. Transport envelopes,
attachment confinement, EOF/reaping, remote disconnects and approval cancellation
also retain their dedicated real-process/fake-service regressions.
