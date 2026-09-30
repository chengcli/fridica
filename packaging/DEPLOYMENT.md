# Candidate deployment checklist

Fill this on the intended deployment host. This checkout has no installed Slack
daemon. Packaging smoke uses synthetic identities and local adapters and cannot
certify deployment readiness. Do not put secrets in this record.

## Build and configuration

- Archive filename and trusted SHA-256:
- `build-info` version, full source_id, target:
- `check-config` fingerprint:
- Hostname/platform/glibc and enabled targets/backends:
- Config/state/control paths and private/MCP inventories:
- Operator/date and retained backup locations:

## Checkout evidence (milestones 1–4)

The packaged `build-manifest.json` identifies the source and asset hashes. Attach
the **matching** `candidate-smoke.json` produced by the clean-install rehearsal,
plus CI or local results for the build being deployed:

1. Rust active host composition and startup preparation: full Rust regression suite.
2. Confinement/credential inventory and actual backend/SSH transport fixtures:
   full Rust regression suite; repeat conformance on each deployment target.
3. CLI/control compatibility: `docs/v0.4-cli-control-compatibility.md` audit in the
   matching source; Python frozen baseline/regression ledger is retained.
4. Replay/recovery: `docs/v0.4-recovery-verification.md` matrix in the matching
   source; full deterministic fixtures and restart tests.
5. Packaging: reproducible archive, verified payload/embedded assets, isolated
   install, fresh state, observer/active-gate lifecycle, migration and snapshot
   restoration checks in `candidate-smoke.json`.

Attach exact commands/results rather than treating this template as certification.
Historical production corpus remains absent and is a later release prerequisite.
Dashboard serving/redesign, campaign live trial, cross-platform wheels and public
publishing are not covered by candidate packaging.

## Deployment checks before active operation

- Verify artifact identity and platform requirements; keep existing Python launcher.
- For every enabled target: real backend auth/protocol doctor, SSH disconnect and
  approval cancellation handling, isolation preflight, MCP inventory.
- Fresh install: initialize v6 and rehearse snapshot restoration. Existing install:
  stop Python, rehearse upgrade/rollback on copies, retain the old environment.
- Check readiness and doctor with the final configuration and deployment credentials.
- Start observe-only with intended Slack workspace/channel; reconcile catch-up,
  persisted mentions, owner pauses, outbox ambiguity and unexpected state. Stop.
- Confirm recovery choice and operator stop/status/log commands.
- Explicitly opt into active operation with `start --active`.
- Run one week of ordinary live attention and inspect obligations, throttling,
  delivery, cancellation and restart behavior before widening scope.

Only the owner decides to go active. Successful synthetic tests do not
substitute for these live checks. Campaigns, channel posting and desktop control
remain opt-in; human-only merges and owner-only resume rules still apply.
