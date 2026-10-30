# Windows and macOS service detail

Binding detail of [serverq.md](../../serverq.md) § 7 and § 8: how `service` commands manage the OS unit outside Linux. A change to it edits this file with the contract rules (R10).

## Windows Scheduled Task

- `service install` ends a running task first (Windows locks a running exe), then copies the binary in as on Linux.
- It then registers coreq's Scheduled Task `ServerQ-serverq` from `<install>/ServerQ-serverq.xml`: this user's logon, `LeastPrivilege`, no admin, launcher supervision and `<install>/serverq.log` per coreq.md.
- The task runs `service run` without arguments. The loop, `status.json` (`host` from `COMPUTERNAME`) and `service status` are serverq.md § 5.
- `schtasks /End` ends only the launcher, so `stop`, `restart` (then `/Run`), `uninstall` and a reinstall also end every other `serverq.exe` running from the install folder, except the caller and its launcher.
- `uninstall` deletes the task, failing when this user may not (a task registered elevated), and its XML, never the install folder.
- Files: no Unix modes or folder fsync; the install folder keeps the user's ACL. The lock opens with write access, which Windows needs to lock it.

## macOS LaunchAgent

- `service install` uses coreq v0.11.0 to install the binary and this user's `~/Library/LaunchAgents/com.serverq.serverq.plist`, label `com.serverq.serverq`, running the launcher with `service run`.
- The LaunchAgent uses `RunAtLoad` and `KeepAlive`, the install folder as its working directory and `serverq.log` there for output. It needs a logged-in GUI user, no admin. Linux sandbox and resource caps do not apply.
- Install/reinstall stops only this label, writes the unit, bootstraps it and runs one check.
- `stop` boots out this label; `restart` boots it out then bootstraps it; `uninstall` boots it out and removes only its plist, never install data.
- An absent label is harmless; other launchctl failures are errors. Rollback is serverq.md § 4.
- The host label comes from `hostname`. The serverq.md § 5 loop sends only the § 3 projection, retains its schedule and checks for updates.

## macOS qualification

- serverq.md § 10 plus the macOS CLI lifecycle test with isolated HOME and fake launchctl: ownership, install, restart, stop, uninstall, refusal of other install folders and launchctl errors.
- On the authorized Mac, verify the native LaunchAgent, recovery, a projected receipt and matching DashQ `/data`, and an update check.
