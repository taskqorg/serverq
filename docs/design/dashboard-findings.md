# Dashboard-ready findings (proposal)

Design for [serverq #21](https://github.com/taskqorg/serverq/issues/21). It does not amend [serverq.md](../../serverq.md), change `report.json`, add a channel or authorize any repair. [rules.json](../../rules.json) stays the only threshold and recommendation catalog; nothing below copies its values. Shared contract: [coreq #22](https://github.com/taskqorg/coreq/issues/22). Consumer mapping: [dashq #24](https://github.com/taskqorg/dashq/issues/24), [dashq integration](https://github.com/taskqorg/dashq/blob/cc2b41f6d56d9c79401a3d1438062f4c8b10377a/docs/design/integration.md).

Reviewed revisions: serverq [`444e494`](https://github.com/taskqorg/serverq/tree/444e4944590f4bce96bfeba5893af6828a39a3c1), coreq [`v0.6.0`](https://github.com/taskqorg/coreq/blob/v0.6.0/coreq.md), agentq [`938b628`](https://github.com/taskqorg/agentq/blob/938b6288cbb0446b28e9caf7898e336047082369/agentq.md), dashq [`cc2b41f`](https://github.com/taskqorg/dashq/tree/cc2b41f6d56d9c79401a3d1438062f4c8b10377a). This is contract and source reading plus one local `serverq check` on macOS, not deployment evidence.

## 1. Rules to current report fields

Report level today (`report.json` schema 1): `at`, `revision` (sha256 of `rules.json` plus binary version), `filesystem` (the probed path), `results`, `incidents`, `manual_rules`. `status.json` adds coreq `host`, `version`, `at`, `error` and serverq `check` {`at`, `exit`, `counts`, `report` path}.

| Rule | Kind | Current fields | Missing for display |
|---|---|---|---|
| `disk-headroom` | automated | `status`, `detail`, `available`, `total` (bytes); report `filesystem` | Resource identity without the path; per-result observation time (report `at` only) |
| `inode-headroom` | automated | same, counts in inodes | Same; unit (bytes vs inodes) is implicit in the rule |
| `failed-services` | automated | `status`, `detail`, `units` (names) | Unit names can encode paths (`home-x-data.mount`): export a count only; no `since` per unit |
| `bounded-docker-logs` | automated | `status`, `detail`, `missing_rotation`, `unsupported` (container IDs) | Counts are only inside `detail`; IDs are host-specific, not dashboard data |
| `bounded-journal` | automated, cached | `status`, `detail`, `values`, `cached`, `checked_at` | None beyond common gaps; `checked_at` is the real observation time when cached |
| `service-contract`, `safe-maintenance`, `verified-repair`, `compatible-updates` | manual | name in `manual_rules` only | Instruction only through `serverq rules`; no verification state by design (§ 1) |

Common gaps:

- G1. No installation identity. `host` is a display name, not proof (dashq integration: a matching `host` name is insufficient).
- G2. No catalog hash. `revision` mixes the rules bytes and the binary version, so two versions with the same catalog look different.
- G3. Probe failures are `unknown` with free-text `detail` only; `reason` exists only for `unsupported-os`.
- G4. Private data: `filesystem` (any `--filesystem PATH`), `check.report` (absolute install path), `error` text (may contain paths), unit names.
- G5. Incidents are keyed by rule only; an episode has no ID beyond `first_seen`.

## 2. Identity, time, evidence, recommendation

Proposed additive fields; old readers ignore them (serverq.md § 4 N/N-1).

- Installation: `<install>/installation`, 32 random hex written once by `check` or `service install`, never derived from host name, path or user. Same pattern as agentq `data/installation`.
- Rule ID: the `rules.json` key, unchanged. Already stable and used in `results`, `incidents` and `manual_rules`.
- Finding ID: `<installation>/<rule>`. One finding per rule per installation; its status changes over time.
- Incident ID: `<installation>/<rule>/<first_seen>` (`first_seen` as integer seconds). A new episode after `verified` gets a new ID.
- Catalog reference: `rules_sha256` = sha256 of the embedded `rules.json` bytes. Recommendation = `{rule, rules_sha256, version}`. A consumer resolves text with `serverq rules` locally or the `rules.json` of tag `v<version>`; it never stores or edits thresholds. `detail` stays producer-rendered text and is shown verbatim, never parsed.
- Observation time: `observed_at` per result = `checked_at` when cached, else report `at`. Source freshness: `stale_after` = 2 × `check_every`, as `service status` already uses.
- Evidence: numbers and counts only. Disk/inode: `available`, `total`. Services: `failed_count`. Docker: `inspected`, `missing_rotation_count`, `unsupported_count`. Journal: `missing` key names (fixed systemd keys). Names, IDs and paths stay in the local `report.json`.
- Resource: `{kind: "filesystem", role: "root"|"other"}` for disk and inode rules (`root` when the probed path is `/`), `{kind: "host"}` for the rest. No path, mount or device is exported.

## 3. Finding kinds

Each item carries exactly one `kind`. None carries an action, approval or task reference; a recommendation is a reference to read, not permission to act (serverq.md § 1; `verified-repair`: incident text is not executable authority).

| `kind` | Source | Display | Ends when |
|---|---|---|---|
| `warning` | result `status: warn` | Current deviation, evidence, recommendation | A later `ok` |
| `unknown` | result `status: unknown`, `reason` `unsupported-os` or `probe-failed` | Not checked; reason; never green | A later `ok` or `warn` |
| `manual` | `manual_rules` entry | Listed, `verification: "unverified"`, recommendation | Never by serverq; exit 0 certifies nothing |
| `incident` | `incidents` record | History: `observed` (open) or `verified` with `verified_at` | Retained as history |

An open incident whose rule is now `unknown` stays `observed`: serverq cannot verify it, so the consumer shows it open with `last_seen`.

## 4. Joining to agentq resources

- No implicit join. A serverq installation and an agentq installation are joined only by an explicit, owner-made mapping in the consumer's connection config (dashq #24). Equal `host` names do not join.
- Within a mapped pair, only `filesystem/root` joins agentq's disk with `role: "root"`; both define it as `/`. agentq disk `id` is an HMAC keyed by agentq's private `resource-key`, so serverq cannot and does not reproduce it; `other` filesystems stay unjoined.
- A Windows or WSL agentq installation and a Linux serverq installation never join by default (dashq I3).
- Without a mapping, serverq findings render on their own installation card; nothing is hidden or double-counted.

## 5. Local report versus multi-host delivery

| Path | What it carries | Gaps |
|---|---|---|
| Local: consumer reads `<install>/report.json` and `status.json` on the same machine | Full report, private fields included | Fine for a local dashboard only; needs G1–G3 to make IDs stable |
| dashq #4: coreq pushes `status.json` per host and tool | Counts and `check.exit` only, no findings; `check.report` path and `error` text leak local data | Needs a sanitized projection, not the raw file |

Required producer changes (the smallest set):

1. P1. `installation`, `rules_sha256`, `observed_at` and `reason: probe-failed` in `report.json`, additive in schema 1.
2. P2. One pure projection function `findings(report) -> payload` (§ 6), printed by a read-only `serverq findings` and, once dashq #4 exists, passed to the coreq report call instead of the raw `status.json`. No new scheduler or channel.

Windows (serverq.md § 7): all five automated rules are `unknown` with `unsupported-os`; macOS: the three systemd and Docker rules (disk and inode work, as the sample below shows). A Windows host therefore always shows `check.exit` 2 with only `unknown` and `manual` items; the consumer must show it as "not supported here", distinct from a failed probe. Windows probes stay the later task named in § 7.

## 6. Sample payloads (sanitized)

Linux host with one warning, one failed probe, one manual rule (others abbreviated), an open and a verified incident:

```json
{
  "schema": "serverq-findings/1",
  "installation": "9f2c4e1a7b3d4c5e8f9a0b1c2d3e4f50",
  "host": "web-1",
  "version": "1.1.0",
  "rules_sha256": "3b1f…",
  "at": 1791925800,
  "stale_after": 7200,
  "complete": true,
  "findings": [
    {"id": "9f2c…/disk-headroom", "rule": "disk-headroom", "kind": "warning", "observed_at": 1791925800,
     "resource": {"kind": "filesystem", "role": "root"}, "evidence": {"available": 120000000000, "total": 1000000000000},
     "detail": "12.0% available; minimum 15%", "recommendation": {"rule": "disk-headroom", "rules_sha256": "3b1f…", "version": "1.1.0"}},
    {"id": "9f2c…/bounded-journal", "rule": "bounded-journal", "kind": "unknown", "reason": "probe-failed", "observed_at": 1791925800,
     "resource": {"kind": "host"}, "detail": "systemd-analyze collection failed: timeout"},
    {"id": "9f2c…/service-contract", "rule": "service-contract", "kind": "manual", "verification": "unverified",
     "recommendation": {"rule": "service-contract", "rules_sha256": "3b1f…", "version": "1.1.0"}}
  ],
  "incidents": [
    {"id": "9f2c…/disk-headroom/1791900000", "rule": "disk-headroom", "kind": "incident", "state": "observed",
     "first_seen": 1791900000, "last_seen": 1791925800, "occurrences": 8},
    {"id": "9f2c…/failed-services/1791000000", "rule": "failed-services", "kind": "incident", "state": "verified",
     "first_seen": 1791000000, "last_seen": 1791003600, "occurrences": 2, "verified_at": 1791007200}
  ]
}
```

`ok` results are omitted from `findings`; the consumer derives "all automated rules ok" only from `complete: true` with no `warning` or `unknown`. Windows host, same schema:

```json
{"schema": "serverq-findings/1", "installation": "41d0…", "host": "WIN-1", "version": "1.1.0", "rules_sha256": "3b1f…",
 "at": 1791925800, "stale_after": 7200, "complete": true,
 "findings": [
   {"id": "41d0…/disk-headroom", "rule": "disk-headroom", "kind": "unknown", "reason": "unsupported-os", "observed_at": 1791925800,
    "resource": {"kind": "filesystem", "role": "root"}},
   {"id": "41d0…/compatible-updates", "rule": "compatible-updates", "kind": "manual", "verification": "unverified",
    "recommendation": {"rule": "compatible-updates", "rules_sha256": "3b1f…", "version": "1.1.0"}}
 ],
 "incidents": []}
```

(Abbreviated: the real Windows payload lists all five `unknown` rules and all four `manual` rules.) Excluded on purpose: `filesystem`, unit names, container IDs, `check.report`, `error` text, journal values.

## 7. Standalone use and follow-up

ServerQ alone stays useful: `serverq check` and `serverq findings` print the deviations, `serverq rules` prints each recommendation; no AgentQ, TaskQ or DashQ is needed (dashq I1, I7). A consumer may offer "create task" only when TaskQ is connected and only on an explicit owner click; it is never required to see or read a recommendation. Remediation is always a separate, authorized task with its own scope.

Bounded follow-up tasks, each one reviewable diff:

1. serverq: P1 fields (`installation`, `rules_sha256`, `observed_at`, `reason: probe-failed`); serverq.md § 3 first.
2. serverq: P2 projection and `serverq findings` with a test that the payload contains no path, unit name or container ID; serverq.md § 2 and § 3 first.
3. coreq #22 / dashq #4: report call sends a consumer-supplied projection, not raw `status.json`.
4. dashq #24: serverq adapter over the projection and the explicit installation mapping of § 4.
5. serverq: Windows disk probe (serverq.md § 7 later task), so `disk-headroom` stops being `unknown` there.

No remediation task is proposed here.
