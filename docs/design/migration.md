# Legacy migration

Binding detail of [serverq.md](../../serverq.md) § 5: how `serverq service install` moves a host from the Python `serverq.py` (0.2.x) layout to v1. A change to these steps edits this file with the contract rules (R10).

## Trigger

Once per host, by `serverq service install` from the downloaded binary. The legacy layout is a real `~/.serverq` folder with `bootstrap/` or `app`. macOS has no legacy migration (serverq.md § 8).

## Steps

1. Take the legacy `~/.serverq/lock`. A `transaction.json` present: refuse; the operator first runs the legacy `check`, which recovers it.
2. Validate legacy `state.json` and `report.json` (schema 1). Corrupt: refuse, change nothing.
3. Stop the four legacy units: `systemctl --user disable --now` the two timers, `systemctl --user stop` the two oneshot services, which have no install section. Their files stay.
4. Copy `state.json` and `report.json` into `<install>`; the new `revision` clears the cache, incidents carry over.
5. Rename `~/.serverq` to `<install>/legacy/`, then link `~/.serverq` to `<install>`, so `~/.serverq/report.json` keeps working for existing monitors.
6. Install and start `serverq.service`, run one check, print every step done.

## Failure and rollback

- A failure before step 5 re-enables the legacy timers and leaves `~/.serverq` unchanged. A failure after it prints the exact rollback.
- Rollback (while `<install>/legacy` exists): `service uninstall`, remove the `~/.serverq` link, rename `<install>/legacy` back to `~/.serverq`, `systemctl --user enable --now serverq-check.timer serverq-update.timer`.
- `legacy/` exists for the rollback only; once the new install is verified the operator deletes it by hand, and serverq never touches it again.
- A second install finds the link and does not migrate. `pin.json`, `update.json` and `versions/` stay in `legacy/` unread.
- The last Python release, v0.2.1, stays downloadable.
