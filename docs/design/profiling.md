# Profiling detail

Binding detail of [serverq.md](../../serverq.md) § 9 under [specq built-in profiling](https://github.com/taskqorg/specq/blob/main/engineering/profiling.md). A change to it edits this file with the contract rules (R10).

## Universal capture

- CLI roots: `serverq.cli.check`, `inspect`, `import`, `rules`, `projection`, `enroll`, `connection`, `disconnect`, `doctor`, `update`, `rollback`, `profile`, `inventory-worker`, `service-run`, `service` and `other` (suffixes of `serverq.cli.`). Command arguments never become operation names.
- Service passes: `serverq.service.pass`; checks and updates: `serverq.operation.service.check` and `serverq.operation.service.update`. Paused updates are refused with `paused`.
- Delivery: `serverq.delivery.snapshot` and `serverq.operation.telemetry.ship`; a requested retry records `delivery-retry`, not success. `delivery.retry` records the retry schedule as `retried`, with its source and wait in seconds. Dependency processes use CoreQ capture, including handled failures and timeout/cleanup evidence.
- Checks inherit their CLI or service caller. Imported inventory keeps its original sample trace and producer time; success still requires durable append. Capture is not an import receipt.
- The CoreQ runtime owns the single resource watch. CLI and worker samples name their own PID, separate from daemon overhead. Selftest opens and qualifies capture in its temporary installation, closes it, then removes that installation.
- Universal capture is always on. The legacy counters and optional trace below remain for parity; they do not switch off universal capture.

## Budgets

Measured operations and budgets (p95 time, failure rate):

| Group | Operation | p95 | Failures |
|---|---|---|---|
| Request handling: one CLI command | `cli.check` | 5 s | 1% |
| | `cli.projection` | 0.5 s | 1% |
| | `cli.update` | 60 s | 5% |
| | `cli.rules`, `cli.rollback`, `cli.service` (every `service` command except `run`), `cli.other` (usage errors, including unparsable arguments) | 5 s | 1% |
| Service lifecycle: one `service run` pass | `service.check` | 5 s | 1% |
| | `service.update` | 60 s | 5% |
| Upstream calls: one probe | `probe.statvfs` | 0.1 s | 1% |
| | `probe.systemctl`, `probe.systemd-analyze` | 2 s | 1% |
| | `probe.docker` (each docker CLI call) | 5 s | 1% |

## Store

- Check observations use the CoreQ store and shipper in [serverq.md § 9](../../serverq.md#9-profiling). The legacy counters below remain local until the shared rollout's parity requirement is met.
- Each process keeps its samples in memory and merges them into `<install>/profile.json` when its command or service pass ends, only when `<install>` already exists.
- `selftest`, `profile`, `doctor` and the connection commands (`enroll`, `connection status`, `disconnect`: they wait on the network and the owner's approval) record no legacy counters. Selftest's check observations stay in its temporary install.
- Concurrent writers may lose a sample, never corrupt the file.
- `profile.json` schema 1, per operation: `count`, `failed`, `errors` (class to count), `first`, `last` (Unix seconds) and `ms`, the last 200 durations.
- Each merge rebuilds the file from valid records of the operation names above with only these fields, so the file stays bounded and foreign content is dropped. An unreadable file or malformed record is replaced by the next merge.
- A record is malformed when an error class is not listed below, error counts do not sum to `failed`, `failed` exceeds `count`, it holds more durations than `count` or 200, or a duration or time is negative, non-finite or `first` is after `last`.
- An outcome is `ok` or `failed` with an error class: `timeout`, `exit-nonzero`, `not-found`, `lock-busy`, `invalid-state`, `unknown-result` (a `check` that completed with an `unknown` rule) or `error`.
- Detailed traces: while the file `<install>/trace` exists, each merge also appends one line per call to `<install>/trace.jsonl` (`op`, `at`, `ms`, `error`), pruned to the last 1000 lines. Off by default; delete `trace` to stop.

## Report

- `serverq profile` ranks every named operation worst first: the larger of p95 over budget and failure rate over budget, then instability (p95 over p50).
- Each row has `count`, `samples`, `failure_rate`, `p50_ms`, `p95_ms`, `max_ms`, budgets, `errors` and `status` `ok|over-budget|failing|unknown`.
- The report names the window (`from`, `to`) and whether traces are on.
