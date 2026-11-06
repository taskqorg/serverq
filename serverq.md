# serverq contract

serverq checks server-care rules and reports read-only host, workload and disk inventory to agents and monitors. It never repairs a server, takes commands, runs AI or promises uptime (R2, R3, R8).

This file is the whole contract of **v1**, the Rust binary built on [coreq](https://github.com/taskqorg/coreq). It follows the [specq release contract](https://github.com/taskqorg/specq/blob/main/engineering/release-contract.md); coreq.md holds the shared update, launcher, service and `status.json` detail.
The runtime uses CoreQ v0.23.4, pinned to `68d5d5065bd9373d039fa7c5f0ee4e66f7150398`, with bounded output, attested updates and R56 self-resource observations. Capture shares the existing writer; ship passes fix their input sequence; Windows tasks use normal priority. Delivery rechecks the current grant before sending.

## 1. Principles

- **R1 Unknown is never healthy.** [specq verification](https://github.com/taskqorg/specq/blob/main/engineering/verification.md) binds serverq: unknown is never healthy (here the status `unknown`), and remediation is scoped and separate.
- **R2 Read-only.** Checks and inventory write only serverq's install folder. They never change server settings, delete operator data, reset failures, restart other services, run AI or apply recommendations. Server changes go through ordinary TaskQ tasks with their own authorization.
- **R3 No command channel.** Independent enrollment adds approved observation delivery through coreq (§ 5), including typed inventory (§ 11). Checks and inventory remain read-only. No AgentQ/Langfuse dependency: DashQ can neither rerun nor repair a check, scan or workload.
- **R4 One catalog.** `rules.json` at the repository root is the only recommendation catalog. The binary embeds its exact bytes at build time; no other file, doc or agent copies its thresholds.
- **R5 Manual rules stay manual.** A rule with `manual: true` is listed, never claimed as automatically verified. Exit 0 never certifies manual rules.
- **R6 Dynamic checks every time.** Capacity and service checks run on every pass. Only configuration checks are cached (§ 3).
- **R7 Compatible reports.** Exit codes and the report schema are those of the Python serverq; a monitor reading either keeps working. A release changes `report.json` and `state.json` only additively (N/N-1); a new schema number needs a cutover task.
- **R8 Scope.** Read-only status, resources, services, containers, sites, folders, observed links and disk use are in scope. Projects are optional interpretations, never required configuration.
- AI analysis belongs to consumers. serverq promises no uptime, security compliance, backup restoration or application monitoring, and adds no unattended git pull, cleanup, self-improvement loop or remote execution.
- **R9 Canonical server-care principles.** A server maintenance task that establishes a reusable principle adds or refines it in `rules.json` with evidence, a verification method and tests. Machine-specific inventory stays private, outside this repository.
- **R10 Spec first.** A change that alters a decision edits this file, or the linked detail that holds it (§ 12), first, in the same deliverable as the code.

## 2. Commands

| Command | Effect | Exit |
|---|---|---|
| `serverq check [--force] [--filesystem PATH]` | One audit (§ 3); prints the report JSON and writes `report.json` | 0 all automated checks `ok`; 1 completed with a `warn`; 2 any `unknown`, collection or state failure |
| `serverq rules` | Prints the embedded `rules.json` bytes unchanged | 0 |
| `serverq projection` | Prints the allowlisted projection of `report.json` (§ 3); never runs a check | 0; 2 no or invalid report, invalid identity |
| `serverq profile` | Prints the profile report (§ 9); never runs a check | 0; 2 unreadable profile |
| `serverq enroll <https-url> [--headless]` | coreq pairing: one browser approval (`--headless` only prints the link and code), then sends the current projection and polls its receipt every 2 s for at most 60 s; never runs a check | 0 `Connected` (receipt `projected`); 2 not verified (the connection stays, the service retries) or error |
| `serverq connection status` | Prints the connection and its last snapshot (below); never a credential | 0 none, or active with its last snapshot `projected`; 2 otherwise |
| `serverq disconnect` | Stops sending at once and revokes through coreq | 0 revoked or never connected; 1 `Disconnected locally; revocation pending`; 2 error |
| `serverq update` | coreq update (§ 4) | coreq |
| `serverq rollback` | coreq rollback: `previous` into `current`, then `service pause` (§ 4) | coreq |
| `serverq doctor` | Checks each host requirement of this OS (§ 6), one line per tool; never runs a check | 0 every requirement met; 2 any missing, below minimum or unreadable |
| `serverq selftest` | coreq harness: the embedded `rules.json` parses and one `check` in a temporary install, deleted afterwards, completes with a report naming every automated rule, whatever their status | 0 pass; 2 fails |
| `serverq service install\|run\|pause\|resume\|status\|restart\|stop\|uninstall` | coreq service (§ 5) | 0 done; 2 error or, for `status`, unknown |

- Errors go to stderr as `serverq <command> failed; no healthy result claimed: <reason>`, exit 2.
- `--home PATH` (every command) replaces the install folder; tests and isolated runs use it. There is no `--version` or `--unpin` (no pin, § 4) and no `--lock-fd` or `--in-transaction` (no journal).
- `connection status` prints `Connection: none|pending|active|disconnected`, destination, workspace and generation, and pending revokes. Its snapshot line is `Snapshot: <event> unsent|accepted|projected`.
- The snapshot line reads `Snapshot: unknown (<reason>)` when `snapshot.json` or `sent.json` is present but unreadable. That file is kept for investigation, never read past to an older receipt.
- A `doctor` line is `ok|fail <tool> found <version|present|missing|unreadable> minimum <version|os> for <needs>`, with `; fix: <step>` on a failure.

## 3. Check and report

- **Unknown (R1).** Probes use CoreQ bounded execution without a host shell, with a 15 s timeout and 1 MiB output cap; a nonzero exit or timeout is `unknown` for that rule. Its `detail` names the command (at most 200 characters) and the error: the spawn error, the exit with the first 300 characters of stderr, or the timeout.
- **Corrupt state (R1).** `state.json` schema 1, unchanged: `schema`, `cache`, `incidents`, `revision`, `last_run`. Another schema or unreadable JSON is an error: the file is kept for investigation, never reset.
- **One audit at a time (R1).** An exclusive lock `<install>/lock` serializes `check` runs; a busy lock is exit 2, never a skipped audit reported as healthy.

Checks, in the order of `rules.json`:

| Rule | Probe | Cached |
|---|---|---|
| `disk-headroom` | free bytes of `--filesystem` (default `/`) against `minimum_percent` | no |
| `inode-headroom` | available inodes against `minimum_percent` | no |
| `failed-services` | `systemctl --failed --no-legend --plain --no-pager` (system units only) | no |
| `bounded-docker-logs` | every container of the local system daemon `unix:///var/run/docker.sock`, stopped ones included; remote contexts ignored | no |
| `bounded-journal` | `systemd-analyze cat-config systemd/journald.conf`: positive `SystemMaxUse`, `SystemKeepFree`, `RuntimeMaxUse` | yes, `ttl_seconds` |

- Docker: `json-file`/`local` need positive `max-size` and `max-file`; `journald` and `none` pass; other drivers are `unknown`; a socket without the CLI is `unknown`; no daemon is `ok`. Rootless daemons stay a manual inventory item.
- Cache: key, fingerprint of the probed config files, time. A hit needs the same fingerprint and an age in `[0, ttl)`. An `unknown` result is never cached. `--force`, or a `revision` different from the stored one, clears the cache.
- `revision` = sha256 of the embedded `rules.json` bytes plus the binary version.
- Incidents per rule: a `warn` opens or continues `{first_seen, last_seen, occurrences, state: observed}`; a later `ok` sets `state: verified, verified_at`. They survive restarts.
- `report.json` schema 1: `schema`, `at` (Unix seconds), `revision`, `catalog` (sha256 of the embedded `rules.json` bytes; additive), `filesystem`, `results`, `incidents`, `manual_rules`.
- Additive `inventory` holds the latest sample (§ 11); inventory completeness never changes the existing rule exit codes.
- Each result: `status` `ok|warn|unknown`, `detail`, probe fields, `cached`, `checked_at` for cached rules, and `reason` on every non-`ok` result (additive).
- `reason` vocabulary: `unsupported-os`, `collection-failed`, `capacity-unavailable`, `below-minimum`, `failed-units`, `missing-journal-bounds`, `docker-cli-missing`, `unsupported-log-driver`, `missing-rotation`.
- Files are written atomically (temp file, fsync, rename, directory fsync), mode 0600, in a 0700 folder.

The projection holds only these fields:

- **Allowlist (R3).** No `detail`, paths, unit names, container IDs or rule instructions; nothing beyond the fields below.
- Top level: `schema` 1, `product`, `installation`, `version`, `runtime` (`<os>-<arch>`), `catalog`, `observed_at`, `rules`, `incidents`. `catalog` is the report's; a report without it has it only when its `revision` is this binary's, else null.
- `rules`, per rule ID: `status` `ok|warn|unknown|manual`, `observed_at` (`checked_at` of a cached result), `reason` from the vocabulary or null, and `evidence`: `available`, `total` and counts of `units`, `missing_rotation`, `unsupported`.
- `incidents`: `state` `observed|verified`, `first_seen`, `last_seen`, `occurrences`, `verified_at`.
- `<install>/installation` holds a random 128-bit hex identity, made when missing; an invalid one is an error and is kept.

## 4. Release, update, rollback

coreq owns all of it; serverq passes tool name `serverq`, install folder and release repository `taskqorg/serverq` as parameters.

- **Verified artifacts.** Each release artifact passes `selftest` before publishing. Installers and the update keep the sha256 check.
- Release: CI on a `v*` tag builds `serverq-<ver>-linux-<arch>` for `x86_64` and `aarch64`, `serverq-<ver>-windows-x86_64.exe` (§ 7), `serverq-<ver>-macos-aarch64` (§ 8), `SHA256SUMS` and `latest.json`.
- Attestation: a separate job with only `id-token: write` and `attestations: write` attests each binary and `SHA256SUMS` with `actions/attest-build-provenance` (pinned by commit SHA) before the release is created. Its public `provenance.sigstore.jsonl` bundle is a release asset; hosts verify it without a GitHub token.
- One binary (static on Linux): hosts need no Python, git or toolchain. The repository keeps version `1.0.0-dev`; the tag sets the release version, so a build from a branch is older than any release.
- Install folder `~/.local/share/serverq` (`<install>`): immutable `releases/<ver>/`, `current`, `previous`, the launcher `<install>/serverq`, `status.json`, `paused`, plus serverq's `report.json`, `state.json`, `installation`, `lock`, `profile.json`, `trace`, `trace.jsonl`, `telemetry.db` and its CoreQ sidecars.
- Update: coreq steps (read `latest.json`, download into `releases/<ver>.tmp/`, verify sha256 and GitHub provenance from this repository's release workflow before executing `selftest`, swap `current`). Each update prunes `releases/` to `current` and `previous`; a folder in use stays until the next update.
- Automatic rollback: the launcher restores `previous` after three `service run` starts of a version that never wrote `releases/<ver>/healthy`; `service run` writes it after its first completed check.
- Manual `rollback` writes `previous` into `current`, swaps them, pauses updates and restarts an installed service; `service resume` returns to normal updates. coreq never reinstalls a version whose folder exists, so a rolled-back release returns only with a newer one.
- No own updater: CoreQ downloads one bounded binary over HTTPS, checks `SHA256SUMS` and the release-workflow attestation, and smoke-tests it with `selftest` before the swap; CI builds the release.
- No pin (`pin.json`, `--unpin`): rollback pauses updates in coreq's service state; resume is the explicit unpin. A bad release is fixed forward by a newer one.
- No transaction journal: switching versions is one atomic rename of `current`; releases are immutable and data lives outside them, so there is nothing to restore. No `update.json`: the update outcome goes to coreq's `status.json`.

## 5. Service

One user unit, `serverq.service`, runs the checks and updates.

- **Checks never stop.** `pause` writes `<install>/paused` and stops updates only; checks continue, since stopping them would hide deviations. `resume` removes it.
- **Stale is unknown (R1).** A `status.json` that is missing, unreadable, or holds a check older than 2 × `check_every` reads unknown with a reason.
- **Delivery is passive (R3).** Delivery never changes a check result, its exit or its schedule; its outcome and retry reason are CoreQ observations. Resource samples never refresh the report: DashQ shows it stale by its own observation time.
- `service install` serializes with updates, rollback and native service changes through `<install>/lock`, freezes the running binary bytes, calls coreq `prepare_install`, then `activate_install` (current release and launcher), then stops and reinstalls the native unit on Windows/macOS. Preparation failures leave the active service untouched; activation errors report uncertainty.
- On Linux it then writes `~/.config/systemd/user/serverq.service` running `<install>/serverq service run --home <install>` with `Restart=always`, `RestartSec=10`, and loads it with `daemon-reload` and `enable --now`.
- macOS stop confirms native absence and the recorded process incarnation has exited within 30 s before a new start. Bootstrap retries once after an observed absent job and exit 5, within a 30 s phase deadline; an uncertain native state never permits a retry. Post-stop uncertainty is rechecked until the phase deadline, never converted to absence.
- A matching existing unit is restarted so reinstall runs the activated release; another installation's unit is refused. Explicit `update` also restarts an installed service after a successful activation.
- serverq has one unit per host, not one per project, so it fills coreq's `Unit` itself instead of coreq's `<name>-<project>` unit. Windows: § 7. macOS: § 8. Another OS is an error.
- A host that must check without a login session needs `loginctl enable-linger <user>` (an operator step; serverq does not run it).
- The unit sets these resource caps and sandbox: `UMask=0077`, `NoNewPrivileges=yes`, `PrivateTmp=yes`, `ProtectSystem=strict`, `ReadWritePaths=<install>`, `MemoryMax=128M`, `TasksMax=32`.
- `service run` loop: a check 5 min after start, then every `check_every` (3600 s); the coreq update check 15 min after start, then every `update_every` (86400 s). Env `SERVERQ_CHECK_EVERY` and `SERVERQ_UPDATE_EVERY` (seconds) override the periods for tests; each first pass waits at most its period.
- Own-process resource telemetry and delivery wake every 30 s using CoreQ R56; missing data remains unknown.
- One loop: a check and an update never overlap. Missed periods do not create catch-up check storms. A check result 1 is a normal pass, not a service failure. An installed update exits 75 and systemd starts the new `current`. Each pass prints one line to the journal.
- Windows stop snapshots only owned-installation PID and native creation time, excludes the caller and its launcher, and terminates through CoreQ identity fencing. It confirms those incarnations exited within a 30 s phase; transiently unavailable identity is probed until the deadline; unconfirmed exit stays a failure. No process arguments or environments are read.
- `restart`, `stop` and `uninstall` run `systemctl --user restart`, `stop` and `disable --now`; `uninstall` also deletes the unit file, never the install folder.
- `<install>/status.json` (coreq common keys, `project` `serverq`) adds `check`: `at`, `exit`, counts per status, `report` path, and `update_at`, `paused`. `error` holds the last check, update or requirement failure (§ 6).
- `service status` prints `Check: ok|warn|unknown <age>` and `Update: <version|up to date|paused|unknown>`. Human time follows [specq Time](https://github.com/taskqorg/specq/blob/main/caveman/rules/indicators.md#time): age first with exact UTC beside it; an unavailable time reads `? unavailable`. Retry countdowns use the same shared formatter.
- `<install>/connection/` holds coreq's `connection.json` (paired with `<install>/installation` through coreq's request identity, so the grant and the projection name one installation), `lock`, `snapshot.json` and `sent.json`.
- `snapshot.json` is the latest unsent record: schema `serverq-report/1`, event ID `r-` plus 32 hex of the payload's sha256, observed at the report's `at`. `sent.json` is the last accepted one.
- With an active connection, each completed check replaces `snapshot.json` (coalesced: an older unsent one is dropped) and delivers it; `rejected` ends it. A transient failure retries after coreq's backoff without moving the check or update schedule.
- Every pass retries queued revokes. Monitors read `report.json` or `status.json`; serverq adds no second scheduler, cleaner or notifier.
- Enrollment requests both `serverq-report/1` and `coreq-observation/1`. After snapshot delivery, the same service pass calls CoreQ's observation shipper (§ 9), including when no snapshot is queued.
  It uses its own backoff within the existing loop; delivery errors never change audit results or schedules. Existing grants without the observation schema need explicit reenrollment; no scope is expanded silently.

## 6. Host requirements

serverq follows [specq host requirements](https://github.com/taskqorg/specq/blob/main/engineering/release-contract.md#host-requirements). It runs these external tools on a host and links no library beyond the OS C library (static on Linux):

| Tool | OS | Minimum | Needed by |
|---|---|---|---|
| `systemctl` (systemd) | Linux | 239 | `failed-services`, `service` |
| `systemd-analyze` (systemd) | Linux | 239 (`cat-config`) | `bounded-journal` |
| `docker` CLI | Linux, while `/var/run/docker.sock` exists | 1.13 (`container ls`) | `bounded-docker-logs` |
| `gh` | every OS | 2.49.0 (`attestation verify`) | trusted updates; missing verification refuses the candidate |
| `curl` | every OS | 7.20.2 (`--proto`) | `update`, `enroll`, delivery (coreq) |
| `launchctl`, `id`, `hostname` | macOS | shipped with the OS | `service`, host label |
| `schtasks`, `powershell`, `whoami`, `icacls` | Windows | shipped with the OS | `service`, connection folder ACL (coreq) |

- **Never installs (R2).** serverq runs as the user and never installs or upgrades a system package; the doctor stays failed until the operator applies the fix step.
- **Failures name their cause (R1).** A failed external call names its command and error (§ 3 for probes; service and launchctl errors carry the command and its stderr); no failure reads as an empty result. coreq's own calls follow coreq.md.
- **Minimums follow use.** A change that starts using a new tool, flag or option raises its minimum here in the same change; tests cover each minimum and a missing tool.
- `service install` (onboarding), `serverq update` and each service update pass run the doctor and print every failed requirement with its fix step. The service writes one journal line and `requirements: ...` in `status.json` `error`. Install and update still complete.
- Version: the first token of the first line of `<tool> --version` that starts with a digit, cut to its digits and dots, compared numerically. An OS-shipped tool is checked for presence on `PATH` only.
- Not checked: the browser opener of `enroll` (`xdg-open`, `open`, `rundll32`); `--headless` and the printed link cover its absence.

## 7. Windows

A Windows host runs the service and reports the Linux-only rules as unknown; Windows probes are a later task. Scheduled Task detail: [platforms](docs/design/platforms.md).

- **Unsupported is unknown (R1).** Every rule whose probe is Linux-only is `unknown` with `reason: unsupported-os`, never `ok`, so `check` exits 2. On Windows that is all five automated rules (no `statvfs`, systemd or system Docker socket); on macOS the three systemd and Docker rules.
- **Allowlisted delivery (R3).** DashQ receives the allowlisted projection and check observations (§ 9) through the shared enrollment client; never upload raw `status.json` or `report.json`.
- Release: `serverq-<ver>-windows-x86_64.exe`, built and selftested like the Linux artifacts. Install folder `%LOCALAPPDATA%\serverq` (coreq default).
- `service install` registers coreq's Scheduled Task `ServerQ-serverq` for this user's logon plus a time trigger repeating every 5 minutes, `LeastPrivilege`, no admin.
- The task runs the GUI launcher `serverqw.exe` with `service run` and no other arguments, so `--home` other than the default is an error for `service install`.
- `stop` ends the current run only and says so: the repeating trigger starts the launcher again within 5 minutes. A lasting stop is `uninstall`, which deletes the task, never the install folder.

## 8. macOS

LaunchAgent detail and qualification: [platforms](docs/design/platforms.md).

- **Unsupported is unknown (R1).** Disk and inode capacity run through `statvfs`; the three Linux-only rules remain `unknown` with `unsupported-os`, so a completed check exits 2. Manual rules remain unverified.
- **No false update success (R1).** A missing macOS release artifact is an update error, never success.
- **Data survives removal.** Removing the LaunchAgent reverses installation without deleting reports or credentials.
- Release: `serverq-<ver>-macos-aarch64`, built and selftested on an Apple Silicon runner; checksums, attestation and updates follow § 4. Intel Macs are not a release target.
- `service install` installs this user's LaunchAgent `com.serverq.serverq` through coreq. Only the default install folder (§ 4) is supported; a different `--home` or `SERVERQ_INSTALL` is an error before service effects.

## 9. Profiling

serverq follows [specq built-in profiling](https://github.com/taskqorg/specq/blob/main/engineering/profiling.md). Operations, budgets, the `profile.json` store and the report: [profiling detail](docs/design/profiling.md).

- **No silent zero (R1).** An operation without samples reads `unknown` with `reason: no-samples`, a malformed record with `reason: corrupt-record`; neither reads as zero.
- **Metadata only.** No message text, file content, environment value or credential is stored. Typed inventory identities are the sole exception to the check observations' ban on paths, unit names and container IDs (§ 11).
- Legacy counters are always on; legacy traces are off until `<install>/trace` exists. Delivery retries keep their CoreQ outcomes and backoff; a retry never establishes recovery.
- CoreQ universal capture starts before CLI, worker and service execution on the same retained store. Commands, service passes and dependency failures inherit caller context; refusals and delivery retries keep their reasons. Missing terminal evidence stays unknown.
  Capture creates a private root when absent; an existing linked, non-private or foreign-owned Unix root is refused without changing its permissions.
  Selftest uses only its temporary installation and closes capture before removal. Capture failure remains unknown without changing product exit codes.
- **Check observations (R1, R3).** Each automated rule evaluation in § 3, including cached and unsupported results, writes one CoreQ `kind: check` observation with the rule ID as `operation`, UTC start, monotonic duration, outcome, error class and host.
  CLI checks, install checks, selftest and service checks use this same path; manual rules emit none. `ok` maps to outcome `ok`, error `none`; `warn` and `unknown` map to `failed`, with the result's stable `reason` as the error class.
  A failed collection leaves unavailable quantities null with that reason, never zero or healthy. Missing identity or context fields remain null with a reason; no path, raw error, unit name or container ID enters a record.
- **One observation store and shipper.** `<install>/telemetry.db` is the installation's CoreQ local store, reused for the process lifetime. CoreQ `Store::ship` sends observations through the approved connection to DashQ D11, with durable cursors and cursor-safe pruning.
  Store or shipper failures are reported as telemetry unknown, never as successful capture or delivery, and do not change the checks. The existing local profile remains during the shared contract's parity rollout; it is not an observation collector or export source.

## 10. Checks

`cargo test --locked`, then `serverq selftest`, then `serverq check --home <temp>` on the authorized host. The [contract lint](https://github.com/taskqorg/specq/blob/main/engineering/contracts.md#lint) checks this file in CI.

- Tests cover enroll, coalesced delivery, receipts, crash recovery, cross-workspace reconnect and offline disconnect against a fake DashQ, each rule's `ok`/`warn`/`unknown`, cache hit, TTL and revision invalidation, corrupt state and exit codes.
- They cover install preparation failure without service effects, update then rollback of an enrolled install against a fake release served by a fake `curl`, `doctor` against tool fakes at, below and missing each minimum, and human-time boundaries.
- Linux-only service tests compile only on Linux; on other OSes the CLI test asserts `unsupported-os`. CI runs the tests on Linux, Windows and macOS aarch64; the Scheduled Task is verified on the authorized Windows host.

## 11. Inventory

- Every completed audit collects one timestamped inventory sample. Its host status is the rule aggregate `ok|warn|unknown`, never a promise of uptime. An unsupported, unavailable or partial measurement is null with a stable reason (R1); a missing list is unknown, never an empty healthy list.
- Host fields: OS and version, uptime, 1/5/15-minute load, logical CPU count and usage, total/used RAM and swap. Windows swap is unknown `unsupported-os`: commit accounting is not pagefile usage. Mounts report total, used and available bytes. CPU usage is thousandths of a logical CPU; system and process usage use two refreshes at least the platform's minimum interval apart.
- Loaded systemd services, Windows services and this user's LaunchAgents carry status, CPU, RAM and disk usage. Linux uses systemd accounting; Windows uses process-tree CPU/RAM and shared PIDs stay unknown; macOS measures each LaunchAgent PID. Service disk is unknown without ownership proof. Container disk means its writable layer, excluding shared images and bind mounts.
- No project manifest is required. Default roots come from local data directories, native service working directories and observed container bind mounts/volumes. The scan emits folders, not inferred projects. Existing `inventory.json` remains an explicit root override for compatibility; invalid configuration is preserved and unknown.
- Linux process identities include PID/start time; only executable and working-directory links, never arguments or environments. Container discovery supports the local system Docker and the current user's Podman. Image, mount, process, service and site links are facts; ownership is never inferred from a similar name.
- Site discovery reads only domain/routing fields from recognized local nginx/Caddy configuration, never emits a raw line, and marks unsupported layouts unknown. No file content or arbitrary configuration enters observations.
- Inventory runs in a separate copy of the same binary with a 60 s deadline and bounded output; timeout produces an unknown sample retaining the local host identity for diagnostic import, without blocking the service loop. Container inventory never calls `inspect --size`; storage is measured from discovered local mounts, with shared data represented once.
- The scan runs in a dedicated background-priority thread and reads only directory entries and metadata: no file contents, links/reparse targets or other filesystems. Directory capabilities open each component without following links. It reports the 100 largest folders through depth 4, using allocated bytes on Unix and logical bytes on Windows; hard links count once per tree.
- A scan visits at most 100,000 entries through traversal depth 128 and takes at most 15 seconds between metadata calls. It opens no further roots after either budget is exhausted. Truncation or denied access makes affected totals unknown. The report includes scope, bounds, duration, CPU time and priority outcome; telemetry records scan CPU usage without a hard CPU ceiling.
- Linux, Windows and macOS collect native resources; unsupported fields stay unknown with reasons. macOS project CPU/RAM attribution is unsupported; it never uses Sysinfo's process reader, which reads arguments and environments. Collection never requests elevation or TCC grants, reads arguments or environments, or inspects project contents.
- Explicit site and sshhole diagnostics may read only their documented configuration/account fields locally; no hashes or raw configuration leave the host.
- Each entity becomes a CoreQ `kind: inventory` observation with one sample trace and its observation time. Only CoreQ R54's typed host, mount, service, container, project-root and folder identity and measurement fields leave the host; raw reports, command output and configuration never do. The check projection allowlist (§ 3) stays unchanged.
- Collection observations carry list counts or null with their reason; the scan operation carries CPU microseconds and wall duration. An invalid entity is rejected independently and counted as loss; valid neighboring entities survive. Store failures and invalid identities are visible in the report's inventory telemetry status.
- The existing CoreQ store and shipper (§ 9) retain successive samples under CoreQ's 30-day and physical-cap retention. Unsent samples are cursor-protected, never coalesced like snapshots. Store-full or rejected samples are reported unknown/loss; consumers compute growth only from two known measurements of the same identity and different observation times.
- Receiver compatibility requires DashQ's typed inventory allowlist at `3dba34d12b5881b946132c2e55e1c1e747352502` or a descendant. This is code compatibility, not proof of production rollout.

## 12. Authorized diagnostics and rollout
- `serverq inspect` is a one-shot, local administrative operation, never a remote command route. It inventories available container runtimes and checks sshhole in running containers. It never starts stopped containers, attempts a password login, installs a scanner or repairs/restarts workloads. Stopped or unreadable containers stay unknown.
- sshhole evaluates musl, OpenSSH package/fix evidence, effective password/PAM/root settings and the presence of star/SHA-crypt account classes. Only enum results/counts leave the target, never usernames, hashes or raw files. Exposure conditions without verified patch evidence are `potentially-affected`, not a proven successful login.
- Older unqualified distro backports stay unknown when conditions cannot be established; no vulnerable-version claim from an image name alone.
- A missing `sshd` in the probe's standard paths is unknown `sshd-not-found-in-probe-scope`, not an unreadable SSH configuration or proof that no custom daemon exists.
- `serverq import FILE` validates a bounded diagnostic sample and records original producer times in this installation's observation store. It never executes supplied text, changes service authority or makes an old observation fresh. Administrative SSH is separate from the persistent service's permissions.
- Persistent services run under the chosen non-administrative account without a new sudo or Docker grant. Privileged one-shot collection and native startup verification are operator onboarding steps; Windows/macOS retain their native account lifecycle.
- Rollout acceptance per host: verified artifact and running version, native PID/state/restart evidence where supported, fresh local inventory/resource samples, approved connection, durable receipts and matching DashQ readback. Reinstall preserves identity/history, creates no competing owner and has a previous-release rollback.

## 13. Linked detail

Binding detail, loaded on demand:

- [docs/design/platforms.md](docs/design/platforms.md): the Windows Scheduled Task and macOS LaunchAgent lifecycle, and the macOS qualification.
- [docs/design/profiling.md](docs/design/profiling.md): profiled operations and budgets, the `profile.json` store, traces and the ranked report.
