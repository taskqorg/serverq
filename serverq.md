# serverq contract

serverq checks a Linux server against the agreed server-care rules and reports deviations to agents and monitors. This file is the whole contract of **v1**, the Rust binary built on [coreq](https://github.com/taskqorg/coreq). It follows the [specq release contract](https://github.com/taskqorg/specq/blob/main/engineering/release-contract.md); coreq.md holds the shared update, launcher, service and `status.json` detail. v1.0.0 replaces the Python `serverq.py` (0.2.x); § 6 moves a host over. A change that alters a decision edits this file first, in the same deliverable as the code. No history here; at most 200 lines.

## 1. Principles

[specq verification](https://github.com/taskqorg/specq/blob/main/engineering/verification.md) binds serverq: unknown is never healthy (here the status `unknown`), and remediation is scoped and separate.

- **Read-only.** serverq writes only its own install folder. It never changes server settings, deletes operator data, resets failures, restarts other services, runs AI or applies recommendations. Remediation is a separate action by an agent or operator.
- **One catalog.** `rules.json` at the repository root is the only recommendation catalog. The binary embeds its exact bytes at build time; no other file, doc or agent copies its thresholds.
- **Dynamic checks every time.** Capacity and service checks run on every pass. Only configuration checks are cached (§ 3).
- **Manual rules stay manual.** A rule with `manual: true` is listed, never claimed as automatically verified. Exit 0 never certifies manual rules.
- **Canonical server-care principles.** A server maintenance task that establishes a reusable principle adds or refines it in `rules.json` with evidence, a verification method and tests. Machine-specific inventory stays private, outside this repository.
- **Scope.** serverq promises no uptime, security compliance, backup restoration or application monitoring. It adds no unattended git pull, generic cleanup, self-improvement loop or remote execution.

## 2. Commands

| Command | Effect | Exit |
|---|---|---|
| `serverq check [--force] [--filesystem PATH]` | One audit (§ 3); prints the report JSON and writes `report.json` | 0 all automated checks `ok`; 1 completed with a `warn`; 2 any `unknown`, collection or state failure |
| `serverq rules` | Prints the embedded `rules.json` bytes unchanged | 0 |
| `serverq update` | coreq update (§ 4) | coreq |
| `serverq rollback` | coreq rollback: `previous` into `current`, then `service pause` (§ 4) | coreq |
| `serverq selftest` | coreq harness: the embedded `rules.json` parses and one `check` in a temporary install, deleted afterwards, completes with a report naming every automated rule, whatever their status | 0 pass; 2 fails |
| `serverq service install\|run\|pause\|resume\|status\|restart\|stop\|uninstall` | coreq service (§ 5) | 0 done; 2 error or, for `status`, unknown |

- `--home PATH` (every command) replaces the install folder; tests and isolated runs use it.
- Exit codes and the report schema are the Python ones; a monitor reading either keeps working.
- `--version` and `--unpin` are dropped with the pin (§ 4). `--lock-fd` and `--in-transaction` are dropped with the journal.
- Errors go to stderr as `serverq <command> failed; no healthy result claimed: <reason>`, exit 2.

## 3. Check and report

Checks, in the order of `rules.json`:

| Rule | Probe | Cached |
|---|---|---|
| `disk-headroom` | free bytes of `--filesystem` (default `/`) against `minimum_percent` | no |
| `inode-headroom` | available inodes against `minimum_percent` | no |
| `failed-services` | `systemctl --failed --no-legend --plain --no-pager` (system units only) | no |
| `bounded-docker-logs` | every container of the local system daemon `unix:///var/run/docker.sock`, stopped ones included; remote contexts ignored | no |
| `bounded-journal` | `systemd-analyze cat-config systemd/journald.conf`: positive `SystemMaxUse`, `SystemKeepFree`, `RuntimeMaxUse` | yes, `ttl_seconds` |

- Docker: `json-file`/`local` need positive `max-size` and `max-file`; `journald` and `none` pass; other drivers are `unknown`; a socket without the CLI is `unknown`; no daemon is `ok`. Rootless daemons stay a manual inventory item.
- Commands run without a shell, with a 15 s timeout; a nonzero exit or timeout is `unknown` for that rule.
- Cache: key, fingerprint of the probed config files, time. A hit needs the same fingerprint and an age in `[0, ttl)`. An `unknown` result is never cached. `--force`, or a `revision` different from the stored one, clears the cache. `revision` = sha256 of the embedded `rules.json` bytes plus the binary version.
- Incidents per rule: a `warn` opens or continues `{first_seen, last_seen, occurrences, state: observed}`; a later `ok` sets `state: verified, verified_at`. They survive restarts.
- `report.json` schema 1, unchanged: `schema`, `at` (Unix seconds), `revision`, `filesystem`, `results` (`status` `ok|warn|unknown`, `detail`, probe fields, `cached`, `checked_at` for cached rules), `incidents`, `manual_rules`.
- `state.json` schema 1, unchanged: `schema`, `cache`, `incidents`, `revision`, `last_run`. Another schema or unreadable JSON is an error: the file is kept for investigation, never reset.
- Files are written atomically (temp file, fsync, rename, directory fsync), mode 0600, in a 0700 folder. An exclusive lock `<install>/lock` serializes `check` runs; a busy lock is exit 2, never a skipped audit reported as healthy.

## 4. Release, update, rollback

coreq owns all of it; serverq passes tool name `serverq`, install folder and release repository `taskqorg/serverq` as parameters.

- Release: CI on a `v*` tag builds `serverq-<ver>-linux-<arch>` for `x86_64` and `aarch64` and `serverq-<ver>-windows-x86_64.exe` (§ 7), `SHA256SUMS` and `latest.json`. Each artifact passes `selftest` before publishing. One static binary: hosts need no Python, git or toolchain.
- Install folder `~/.local/share/serverq` (`<install>`): immutable `releases/<ver>/`, `current`, `previous`, the launcher `<install>/serverq`, `status.json`, `paused`, plus serverq's `report.json`, `state.json`, `lock`. The repository keeps version `1.0.0-dev`; the tag sets the release version, so a build from a branch is older than any release.
- Update: coreq steps (read `latest.json`, download into `releases/<ver>.tmp/`, verify sha256, `selftest`, swap `current`). Each update prunes `releases/` to `current` and `previous`; a folder in use stays until the next update.
- Rollback: the launcher restores `previous` after three `service run` starts of a version that never wrote `releases/<ver>/healthy`; `service run` writes it after its first completed check. A manual `rollback` writes `previous` into `current`, swaps them, pauses updates and restarts an installed service; `service resume` returns to normal updates. coreq never reinstalls a version whose folder exists, so a rolled-back release returns only with a newer one.
- Compatibility N/N-1: a release changes `report.json` and `state.json` only additively; a new schema number needs a cutover task.

Dropped from the Python version, because coreq covers them:

| Python behavior | Why dropped |
|---|---|
| Own updater (GitHub Releases API, ZIP plus manifest, bounded downloads, unpack checks, packaged tests on update, `tools/package_release.py`, three `versions/`) | coreq downloads one binary checked against `SHA256SUMS` and smoke-tests it with `selftest` before the swap; CI builds the release |
| Pin (`pin.json`, `--unpin`) | Rollback pauses updates in coreq's service state; resume is the explicit unpin. A bad release is fixed forward by a newer one |
| Transaction journal (`transaction.json`, bootstrap dispatcher, `app` pointer recovery) | Switching versions is one atomic rename of `current`; releases are immutable and data lives outside them, so there is nothing to restore |
| `update.json` | The update outcome goes to coreq's `status.json` |

## 5. Service

One OS unit replaces `serverq-check.service`, `serverq-check.timer`, `serverq-update.service` and `serverq-update.timer`.

- `serverq service install` copies the running binary in as coreq `install_self` (current release and launcher), then writes `~/.config/systemd/user/serverq.service` running `<install>/serverq service run --home <install>` with `Restart=always`, `RestartSec=10`, and loads it with `daemon-reload` and `enable --now`. serverq has one unit per host, not one per project, so it fills coreq's `Unit` itself instead of coreq's `<name>-<project>` unit. Windows: § 7. Another OS is an error. A host that must check without a login session needs `loginctl enable-linger <user>` (an operator step; serverq does not run it).
- The unit keeps the Python resource caps and sandbox: `UMask=0077`, `NoNewPrivileges=yes`, `PrivateTmp=yes`, `ProtectSystem=strict`, `ReadWritePaths=<install>`, `MemoryMax=128M`, `TasksMax=32`.
- `service run` loop: a check 5 min after start, then every `check_every` (3600 s); the coreq update check 15 min after start, then every `update_every` (86400 s). Env `SERVERQ_CHECK_EVERY` and `SERVERQ_UPDATE_EVERY` (seconds) override the periods for tests; each first pass waits at most its period. One loop: a check and an update never overlap. A check result 1 is a normal pass, not a service failure. An installed update exits 75 and systemd starts the new `current`. Each pass prints one line to the journal.
- `pause` writes `<install>/paused` and stops updates only; checks continue, since stopping them would hide deviations. `resume` removes it. `restart`, `stop` and `uninstall` run `systemctl --user restart`, `stop` and `disable --now`; `uninstall` also deletes the unit file, never the install folder.
- `<install>/status.json` (coreq common keys, `project` `serverq`) adds `check`: `at`, `exit`, counts per status, `report` path, and `update_at`, `paused`. `error` holds the last check or update failure. Missing, unreadable, or a check older than 2 × `check_every`, reads unknown with a reason. `service status` prints `Check: ok|warn|unknown <age>` and `Update: <version|up to date|paused|unknown>`.
- Monitors read `report.json` or `status.json`; serverq adds no second scheduler, cleaner or notifier.

## 6. Migration of the legacy layout

Once per host, by `serverq service install` from the downloaded binary. The legacy layout is a real `~/.serverq` folder with `bootstrap/` or `app`.

1. Take the legacy `~/.serverq/lock`. A `transaction.json` present: refuse; the operator first runs the legacy `check`, which recovers it.
2. Validate legacy `state.json` and `report.json` (schema 1). Corrupt: refuse, change nothing.
3. Stop the four legacy units: `systemctl --user disable --now` the two timers, `systemctl --user stop` the two oneshot services, which have no install section. Their files stay.
4. Copy `state.json` and `report.json` into `<install>`; the new `revision` clears the cache, incidents carry over.
5. Rename `~/.serverq` to `<install>/legacy/`, then link `~/.serverq` to `<install>`, so `~/.serverq/report.json` keeps working for existing monitors. `legacy/` exists for the rollback only; once the new install is verified the operator deletes it by hand, and serverq never touches it again.
6. Install and start `serverq.service`, run one check, print every step done.

- A failure before step 5 re-enables the legacy timers and leaves `~/.serverq` unchanged. A failure after it prints the exact rollback.
- Rollback (while `<install>/legacy` exists): `service uninstall`, remove the `~/.serverq` link, rename `<install>/legacy` back to `~/.serverq`, `systemctl --user enable --now serverq-check.timer serverq-update.timer`.
- A second install finds the link and does not migrate. `pin.json`, `update.json` and `versions/` stay in `legacy/` unread.
- The last Python release, v0.2.1, stays downloadable.

## 7. Windows

A Windows host runs the service and reports the Linux-only rules as unknown; Windows probes are a later task.

- Release: `serverq-<ver>-windows-x86_64.exe`, built and selftested like the Linux artifacts. Install folder `%LOCALAPPDATA%\serverq` (coreq default).
- `service install` ends a running task first (Windows locks a running exe), copies the binary in as on Linux, then registers coreq's Scheduled Task `ServerQ-serverq` from `<install>/ServerQ-serverq.xml`: this user's logon, `LeastPrivilege`, no admin, launcher supervision and `<install>/serverq.log` per coreq.md. It runs `service run` without arguments, so `--home` other than the default is an error. The loop, `status.json` (`host` from `COMPUTERNAME`) and `service status` are § 5.
- `schtasks /End` ends only the launcher, so `stop`, `restart` (then `/Run`), `uninstall` and a reinstall also end every other `serverq.exe` running from the install folder, except the caller and its launcher. `uninstall` deletes the task, failing when this user may not (a task registered elevated), and its XML, never the install folder.
- Check: every rule whose probe is Linux-only is `unknown` with `reason: unsupported-os`, never `ok`, so `check` exits 2. On Windows that is all five automated rules (no `statvfs`, systemd or system Docker socket); on macOS the three systemd and Docker rules. `report.json` adds `reason` to those results only.
- Files: no Unix modes or folder fsync; the install folder keeps the user's ACL. The lock opens with write access, which Windows needs to lock it.
- dashq shows the host once hosts push `status.json` (dashq #4); serverq adds no own channel.

## 8. Checks

`cargo test --locked`, then `serverq selftest`, then `serverq check --home <temp>` on the authorized host. Tests cover each rule's `ok`/`warn`/`unknown`, cache hit, TTL and revision invalidation, corrupt state, exit codes, the migration steps against a fake legacy folder, and update then rollback against a fake release served by a fake `curl`. Linux-only service tests compile only on Linux; on other OSes the CLI test asserts `unsupported-os`. CI runs the tests on Linux and Windows; the Scheduled Task is verified on the authorized Windows host.
