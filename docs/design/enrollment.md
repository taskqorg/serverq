# Independent DashQ connection

Accepted implementation target under [serverq.md](../../serverq.md). Apply [shared connection semantics](https://github.com/taskqorg/specq/blob/main/engineering/product-connections.md) through the coreq linked client and [DashQ C1–C7](https://github.com/taskqorg/dashq/blob/main/docs/design/enrollment.md). Implemented as `serverq enroll`, `connection status` and `disconnect` ([serverq.md § 2, § 5](../../serverq.md#5-service)); live qualification pending.

`serverq enroll <https-url>`, `serverq connection status` and `serverq disconnect` use the same browser/headless approval, protected credential, projection receipt and revoke lifecycle as AgentQ. No AgentQ installation or Langfuse dependency. The existing service handles delivery; no extra daemon or scheduler. Stable random installation identity survives updates, never derives from hostname; lost identity creates a new installation.

Send only a coalesced latest unsent snapshot. Connection state and credentials are durable and excluded from pruning; they contain no authoritative product state. Heartbeat freshness and underlying observation freshness remain separate. Snapshot transport failure never changes primary command success or blocks primary work. C3's accepted credential lifetime exception is confined to observation, never commands or forge access.

## ServerQ projection

Existing checks, report/state formats and rules.json remain authoritative. Produce an allowlisted projection: installation/runtime/version, catalog hash, per-rule ID/status/original observation time/reason, numeric evidence and incident verification state. Manual rules stay unverified; unsupported probes stay unknown. Retain schema-1 readers through additive metadata only. Never export raw detail, paths, unit names, container IDs or report.json wholesale; use fixed-vocabulary reasons. Recommendations reference the exact rule/catalog revision, never duplicate thresholds. Implemented as `serverq projection` (fields and reason vocabulary: [serverq.md § 3](../../serverq.md#3-check-and-report)); delivered through coreq after each check.

A fresh connection may expose an old report as stale; it cannot rerun or repair checks remotely. No implied cross-product host/filesystem join. Historical findings research supplies context, not additional approved features.

## Qualification

Prove an independently installed product can enroll, show a real matching snapshot in DashQ, recover after network/store restart, disconnect offline, revoke and reconnect without identity or scope leakage. Test denied/wrong-workspace requests, lost replies, bounded storage, schema compatibility and primary-function independence. Reuse coreq conformance; do not reimplement its auth or transport. Ship only after applicable E1–E10 and the product's normal checks pass.
