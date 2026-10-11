# ServerQ

Minimal server-care recommendations and a stateful, read-only Linux checker for AI agents.
Author: Tight <tight@kirs.online>.

Give an agent this instruction:

> Use https://github.com/taskqorg/serverq to inspect this server. Read AGENTS.md and rules.json. Run the read-only checker, investigate deviations and propose scoped, reversible changes. Do not delete unidentified data or replace existing monitoring. Add newly agreed reusable maintenance principles to ServerQ and verify them.

## First version

Python 3.10+ on Linux; no packages, model calls, root requirement or network requests at runtime. The initial rules cover filesystem/inode headroom, failed system services, bounded journal configuration, service ownership/retention, safe maintenance, verified repairs and compatible updates. `rules.json` is canonical. This is not a guarantee of indefinite uptime or complete security compliance.

```sh
python3 serverq.py check
python3 serverq.py check --force
python3 -m unittest discover -s tests -v
```

JSON output and private `~/.serverq/report.json` show `ok`, `warn` or `unknown` per automated check and list outstanding manual rules. Exit codes: 0 automated checks pass, 1 deviations, 2 collection/state error. A 0 does NOT certify manual rules. systemd tools are required for systemd checks; unsupported environments are unknown, not healthy. This initial version checks system-level failed units, not user units or application endpoints; use existing service monitors for those.

`--home PATH` isolates state; `--filesystem PATH` selects the filesystem. Only the selected filesystem is checked; inventory additional mounts separately. CLI failures are visible on stderr; a stale report does not prove current health.

## State and efficiency

The private directory contains an exclusive lock, atomically replaced state and latest report (0600). Incidents are bounded by known rule IDs, aggregated per rule and resolved only by a fresh passing observation; unknown never resolves them. No raw logs, tokens, prompts or file contents are stored or published.

Capacity and service state are read on every run. Only journal configuration evaluation is cached, with a content fingerprint of supported systemd config roots, full checker/rules revision invalidation and a finite TTL. Reports label cache provenance and original check time. `--force` bypasses cache. Cache is an optimization, not permission to skip dynamic health checks. Corrupt state fails visibly; preserve and investigate it rather than silently resetting it.

## Installation and periodic checks

Use a reviewed commit; install `serverq.py` and `rules.json` together in `~/.serverq/app/`. The checker writes only its private state and reports, not system settings. The example user-systemd units in `systemd/` run checks hourly with hard time/memory/task limits; adapt the Python path on unusual hosts. User timers need a running user manager (and linger for unattended operation).

```sh
mkdir -p ~/.serverq/app ~/.config/systemd/user
chmod 700 ~/.serverq
install -m 600 serverq.py rules.json ~/.serverq/app/
install -m 644 systemd/serverq-check.service systemd/serverq-check.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now serverq-check.timer
systemctl --user start serverq-check.service
systemctl --user status serverq-check.timer
cat ~/.serverq/report.json
```

Exit 1 is an expected completed audit with deviations, not a failed collector; systemd accepts it. Exit 2 fails the unit. Reports are local; scheduled failures appear in the service journal. No Telegram integration or public issue submission is implemented. Keep existing monitoring and alerts. The checker cannot verify backup restoration or actual log caps for all services: the service contract remains a manual inventory/review.

## Operating model

`observe → local incident → diagnose → scoped repair → original-scenario verification → recommendation/test`

Borrow the separation of diagnosis, owned repairs and verified outcomes from agent-context, not the assumption that an agent may execute arbitrary incident instructions. Initial implementation intentionally has no automatic destructive cleanup, AI loop, installer self-healing or auto-update. New supported reversible recipes, safe reporting and compatible release updates can be added after real qualification. Until then update deliberately from a reviewed release, validate tests and the host scenario, retain the previous app and state, and roll back on failure.

When a cleanup is needed, establish exact owner, retention, live-process safety and recovery needs first. Address the producer's unbounded growth, not only yesterday's accumulated files. Do not add a second job that competes with a host's existing maintenance.
