# Independent DashQ connection

Accepted implementation target under [serverq.md](../../serverq.md). Apply [shared connection semantics](https://github.com/taskqorg/specq/blob/main/engineering/product-connections.md) through the coreq linked client and [DashQ C1–C7](https://github.com/taskqorg/dashq/blob/main/docs/design/enrollment.md). Implemented as `serverq enroll`, `connection status` and `disconnect` ([serverq.md § 2, § 5](../../serverq.md#5-service)); qualified below.

`serverq enroll <https-url>`, `serverq connection status` and `serverq disconnect` use the same browser/headless approval, protected credential, projection receipt and revoke lifecycle as AgentQ. No AgentQ installation or Langfuse dependency. The existing service handles delivery; no extra daemon or scheduler. Stable random installation identity survives updates, never derives from hostname; lost identity creates a new installation.

Send only a coalesced latest unsent snapshot. Connection state and credentials are durable and excluded from pruning; they contain no authoritative product state. Heartbeat freshness and underlying observation freshness remain separate. Snapshot transport failure never changes primary command success or blocks primary work. C3's accepted credential lifetime exception is confined to observation, never commands or forge access.

## ServerQ projection

Existing checks, report/state formats and rules.json remain authoritative. Produce an allowlisted projection: installation/runtime/version, catalog hash, per-rule ID/status/original observation time/reason, numeric evidence and incident verification state. Manual rules stay unverified; unsupported probes stay unknown. Retain schema-1 readers through additive metadata only. Never export raw detail, paths, unit names, container IDs or report.json wholesale; use fixed-vocabulary reasons. Recommendations reference the exact rule/catalog revision, never duplicate thresholds. Implemented as `serverq projection` (fields and reason vocabulary: [serverq.md § 3](../../serverq.md#3-check-and-report)); delivered through coreq after each check.

A fresh connection may expose an old report as stale; it cannot rerun or repair checks remotely. No implied cross-product host/filesystem join. Historical findings research supplies context, not additional approved features.

## Qualification

Prove an independently installed product can enroll, show a real matching snapshot in DashQ, recover after network/store restart, disconnect offline, revoke and reconnect without identity or scope leakage. Test denied/wrong-workspace requests, lost replies, bounded storage, schema compatibility and primary-function independence. Reuse coreq conformance; do not reimplement its auth or transport. Ship only after applicable E1–E10 and the product's normal checks pass.

### Evidence (coreq v0.11.0, 2026-10-15)

Live, macOS (aarch64) against `https://dash.ufobe.com/machine`, workspace `gradus`, no AgentQ or Langfuse on the host: `serverq enroll --headless` printed one pairing code; after one owner approval it printed `Connected ... generation 1 snapshot r-d1a3850f...` (receipt `projected`); `connection status` exit 0 with that snapshot `projected`; `disconnect` printed `Disconnected` (revoked, exit 0); `connection status` then `disconnected`, exit 2. Live reconnect needs a second approval and is covered by the fake DashQ below.

| Case | Applies | Evidence |
|---|---|---|
| E1 | ServerQ in place of AgentQ | live run above; `connection_lifecycle` (allowlisted projection only, enroll never reruns a check, old report keeps its observation time) |
| E2 | yes | coreq conformance (deny, expire); repeated enroll verifies the same grant, no duplicate (`connection_lifecycle`) |
| E3 | yes | coreq crash-safe `connection.json`; `connection_lifecycle` outage retry |
| E4 | yes | `connection_lifecycle`: outage keeps only the latest unsent snapshot, checks and schedule untouched; `unsent` is not connected |
| E5 | yes | live revoke; `connection_lifecycle`: old-grant snapshot rejected after reconnect to another workspace, dashboard revoke ends sending |
| E6 | yes | `projection_exports_only_the_allowlist`, `every_non_ok_result_has_a_known_reason`; schema-1 report unchanged |
| E7 | yes | `connection_lifecycle`: offline disconnect exit 1, revoke retried, nothing sent after; `connection_commands_without_enrollment` |
| E8 | yes | live run on a host without AgentQ/Langfuse; no command channel or write permission |
| E9 | client side | `service_install_migrates_runs_updates_and_rolls_back`; coreq update prunes only `releases/` |
| E10 | no | DashQ UI, qualified in dashq |

Platforms: tests run in CI on Linux and Windows; the live run was macOS. Live runs on the authorized Linux and Windows hosts are not yet recorded.
