//! The real `serverq` binary with isolated state, a fake legacy folder and a fake release.
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RESULTS: [&str; 5] = ["disk-headroom", "inode-headroom", "failed-services", "bounded-docker-logs", "bounded-journal"];

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("serverq-cli-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn check(home: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_serverq")).args(["check", "--home"]).arg(home).args(extra).output().unwrap()
}

fn report(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap()
}

#[test]
fn real_cli_isolated_state_and_failure() {
    let home = scratch("real");
    let first = check(&home, &[]);
    assert!(matches!(first.status.code(), Some(0..=2)));
    let r = report(&first);
    assert_eq!(r["results"].as_object().unwrap().keys().map(String::as_str).collect::<BTreeSet<_>>(), RESULTS.into());
    assert_eq!(fs::read(home.join("report.json")).unwrap(), first.stdout);
    let second = report(&check(&home, &[]));
    if r["results"]["bounded-journal"]["status"] != "unknown" {
        assert_eq!(second["results"]["bounded-journal"]["cached"], true);
        assert_eq!(report(&check(&home, &["--force"]))["results"]["bounded-journal"]["cached"], false);
    }
    // serverq.md § 7: a Linux-only probe elsewhere is unknown with its reason, never ok, and exits 2
    let linux_only: &[&str] = if cfg!(windows) { &RESULTS } else { &RESULTS[2..] };
    if !cfg!(target_os = "linux") {
        assert_eq!(first.status.code(), Some(2));
        for k in linux_only {
            assert_eq!((r["results"][k]["status"].as_str(), r["results"][k]["reason"].as_str()), (Some("unknown"), Some("unsupported-os")), "{k}");
        }
    }
    // docs/design/enrollment.md: the projection names the catalog of this report and no local path
    let projection = || Command::new(env!("CARGO_BIN_EXE_serverq")).args(["projection", "--home"]).arg(&home).output().unwrap();
    let p: Value = serde_json::from_slice(&projection().stdout).unwrap();
    assert_eq!(p["catalog"], r["catalog"]);
    assert_eq!(p["rules"].as_object().unwrap().len(), RESULTS.len() + r["manual_rules"].as_array().unwrap().len());
    assert!(!String::from_utf8_lossy(&projection().stdout).contains(home.to_str().unwrap()));
    assert_eq!(serde_json::from_slice::<Value>(&projection().stdout).unwrap()["installation"], p["installation"]);
    let missing = check(&home, &["--filesystem", home.join("absent").to_str().unwrap()]);
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(report(&missing)["results"]["disk-headroom"]["status"], "unknown");
    fs::write(home.join("state.json"), "broken").unwrap();
    let broken = check(&home, &[]);
    assert_eq!(broken.status.code(), Some(2));
    assert!(broken.stdout.is_empty());
    assert!(String::from_utf8_lossy(&broken.stderr).contains("no healthy result claimed"));
    assert_eq!(fs::read_to_string(home.join("state.json")).unwrap(), "broken", "corrupt state is never reset");
    // serverq.md § 9: every check counts, a failed one with its error class, and the store holds no path
    let profile = Command::new(env!("CARGO_BIN_EXE_serverq")).args(["profile", "--home"]).arg(&home).output().unwrap();
    assert_eq!(profile.status.code(), Some(0));
    let p: Value = serde_json::from_slice(&profile.stdout).unwrap();
    let row = p["operations"].as_array().unwrap().iter().find(|o| o["op"] == "cli.check").unwrap();
    assert_eq!(row["count"], if r["results"]["bounded-journal"]["status"] != "unknown" { 5 } else { 4 });
    assert_eq!(row["errors"]["invalid-state"], 1);
    assert!(!fs::read_to_string(home.join("profile.json")).unwrap().contains(home.to_str().unwrap()));
    // an argument parse failure counts as cli.other
    let bad = Command::new(env!("CARGO_BIN_EXE_serverq")).args(["--bad", "--home"]).arg(&home).output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
    let profile = Command::new(env!("CARGO_BIN_EXE_serverq")).args(["profile", "--home"]).arg(&home).output().unwrap();
    let p: Value = serde_json::from_slice(&profile.stdout).unwrap();
    let row = p["operations"].as_array().unwrap().iter().find(|o| o["op"] == "cli.other").unwrap();
    assert_eq!((row["count"].as_u64(), row["errors"]["error"].as_u64()), (Some(1), Some(1)));
}

#[test]
fn exclusive_lock() {
    let home = scratch("lock");
    let lock = fs::File::create(home.join("lock")).unwrap();
    lock.try_lock().unwrap();
    assert_eq!(check(&home, &[]).status.code(), Some(2));
    assert!(!home.join("state.json").exists());
}

#[test]
fn bad_arguments_are_errors() {
    let home = scratch("args");
    assert_eq!(check(&home, &["--bogus"]).status.code(), Some(2));
    assert_eq!(Command::new(env!("CARGO_BIN_EXE_serverq")).output().unwrap().status.code(), Some(2));
}

/// serverq.md § 2: a never-enrolled install has no connection and refuses a non-HTTPS destination.
#[test]
fn connection_commands_without_enrollment() {
    let home = scratch("connection");
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_serverq")).args(args).arg("--home").arg(&home).output().unwrap();
    let status = run(&["connection", "status"]);
    assert_eq!((status.status.code(), String::from_utf8_lossy(&status.stdout).trim()), (Some(0), "Connection: none"));
    assert_eq!(run(&["disconnect"]).status.code(), Some(0));
    let bad = run(&["enroll", "http://dash.example", "--headless"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("destination must be https"));
}

#[cfg(target_os = "linux")]
fn write_exe(p: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(p, text).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(target_os = "linux")]
fn text(p: impl AsRef<Path>) -> String {
    fs::read_to_string(p).unwrap_or_default().trim().to_string()
}

/// A fake `curl`: `-o OUT URL` copies a release from `$FAKE_RELEASE`; `--config -` answers coreq's HTTPS wire as a fake
/// DashQ at `https://dash.example/machine` with its state in `$FAKE_DASH`: `log` (route and event per request), `events`
/// (stored event IDs, once each), `approved` (the owner approved the code), `lost` (store the snapshot, lose the reply).
#[cfg(target_os = "linux")]
const FAKE_CURL: &str = r#"#!/bin/sh
case " $* " in *" --config "*) ;; *)
  while [ $# -gt 1 ]; do [ "$1" = -o ] && out=$2; shift; done
  cp "$FAKE_RELEASE/${1##*/}" "$out"; exit;;
esac
cfg=$(cat)
d=$FAKE_DASH
route=$(printf '%s\n' "$cfg" | sed -n 's|^url = "https://dash.example/machine\(.*\)"$|\1|p')
ev=$(printf '%s\n' "$cfg" | grep -o 'r-[0-9a-f]\{32\}' | head -n 1)
echo "$route $ev" >> "$d/log"
ok() { printf 'HTTP/1.1 200 OK\r\n\r\n{"protocol":1,%s}\n200' "$1"; }
no() { printf 'HTTP/1.1 %s\r\n\r\n{"protocol":1,"error":"%s"}\n%s' "$1" "$2" "$1"; }
case $route in
/connect/device)
  printf '%s\n' "$cfg" | grep -o 'installation\\":\\"[0-9a-f]*' | sed 's/.*"//' > "$d/installation"
  ok '"device_code":"d","user_code":"ABCD-1234","verification_uri":"https://dash.example/connect","expires_in":600,"interval":1';;
/connect/token)
  [ -e "$d/approved" ] || { no 400 authorization_pending; exit; }
  ok "\"credential\":\"cred-1\",\"installation\":\"$(cat "$d/installation")\",\"generation\":1,\"product\":\"serverq\",\"workspace\":\"ws-1\",\"schemas\":[\"serverq-report/1\"]";;
/connect/ack) ok '"state":"active","generation":1';;
/ingest)
  grep -qx "$ev" "$d/events" 2>/dev/null || echo "$ev" >> "$d/events"
  [ -e "$d/lost" ] && { printf '\n000'; exit; }
  ok "\"receipts\":[{\"event_id\":\"$ev\",\"state\":\"accepted\"}]";;
/connection/receipt)
  if grep -qx "$ev" "$d/events"; then
    ok "\"installation\":\"$(cat "$d/installation")\",\"generation\":1,\"schema\":\"serverq-report/1\",\"event_id\":\"$ev\",\"state\":\"projected\""
  else no 404 unknown_event; fi;;
/connection/revoke) ok '"state":"revoked","generation":1';;
*) no 404 not_found;;
esac
"#;

/// Waits up to 30 s for `done`.
#[cfg(target_os = "linux")]
fn wait(done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !done() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(done(), "timed out");
}

/// Every file of `<install>/connection` plus `<install>/installation`, by name.
#[cfg(target_os = "linux")]
fn connection_files(install: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = fs::read_dir(install.join("connection")).unwrap().map(|e| e.unwrap()).map(|e| (e.file_name().into_string().unwrap(), fs::read(e.path()).unwrap())).collect();
    v.push(("installation".into(), fs::read(install.join("installation")).unwrap()));
    v.sort();
    v
}

/// DashQ E3 (docs/design/enrollment.md) through the real binary and coreq's HTTPS wire against a fake DashQ: a crash
/// before approval, between send and receipt, and of the service resumes without a second grant or a lost snapshot;
/// connection state stays private (E5 storage).
#[cfg(target_os = "linux")]
#[test]
fn enrollment_recovers_from_crashes() {
    use std::os::unix::fs::PermissionsExt;
    let d = scratch("crash");
    let (home, bin, dash) = (d.join("h"), d.join("bin"), d.join("dash"));
    for p in [&bin, &dash] {
        fs::create_dir_all(p).unwrap();
    }
    write_exe(&bin.join("curl"), FAKE_CURL);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let cmd = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_serverq"));
        c.args(args).arg("--home").arg(&home).env("PATH", &path).env("FAKE_DASH", &dash).env("SERVERQ_CHECK_EVERY", "1").env("SERVERQ_UPDATE_EVERY", "3600");
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        c
    };
    let log = || text(dash.join("log"));
    let count = |route: &str| log().lines().filter(|l| l.starts_with(&format!("{route} "))).count();
    let enroll = ["enroll", "https://dash.example/machine", "--headless"];
    let status = || Command::new(env!("CARGO_BIN_EXE_serverq")).args(["connection", "status", "--home"]).arg(&home).env("PATH", &path).env("FAKE_DASH", &dash).output().unwrap();
    assert!(matches!(cmd(&["check"]).status().unwrap().code(), Some(0..=2)));

    // a crash while the owner has not approved yet: the pending pairing resumes with the same code
    let mut c = cmd(&enroll).spawn().unwrap();
    wait(|| count("/connect/token") > 0);
    c.kill().unwrap();
    let _ = c.wait();
    fs::write(dash.join("approved"), "").unwrap();

    // a crash between send and receipt: DashQ stored the snapshot, its reply was lost
    fs::write(dash.join("lost"), "").unwrap();
    let mut c = cmd(&enroll).spawn().unwrap();
    wait(|| count("/ingest") > 0);
    c.kill().unwrap();
    let _ = c.wait();
    let unsent: Value = serde_json::from_str(&text(home.join("connection/snapshot.json"))).unwrap();
    fs::remove_file(dash.join("lost")).unwrap();
    assert_eq!(cmd(&enroll).status().unwrap().code(), Some(0), "{}", log());
    assert!(String::from_utf8_lossy(&status().stdout).contains(&format!("Snapshot: {} projected", unsent["event_id"].as_str().unwrap())));
    assert_eq!((count("/connect/device"), count("/connect/ack")), (1, 1), "one pairing, one grant: {}", log());
    assert!(log().lines().filter(|l| *l == format!("/ingest {}", unsent["event_id"].as_str().unwrap())).count() >= 2, "the lost send repeats its event ID");
    assert_eq!(text(dash.join("events")).lines().count(), 1, "stored once");

    // a crash of the service while a snapshot is unsent: the restarted service delivers the latest one
    fs::write(dash.join("lost"), "").unwrap();
    let mut svc = cmd(&["service", "run"]).spawn().unwrap();
    wait(|| text(dash.join("events")).lines().count() > 1);
    svc.kill().unwrap();
    let _ = svc.wait();
    assert!(home.join("connection/snapshot.json").exists(), "the unsent snapshot survives the crash");
    fs::remove_file(dash.join("lost")).unwrap();
    let mut svc = cmd(&["service", "run"]).spawn().unwrap();
    wait(|| !home.join("connection/snapshot.json").exists());
    svc.kill().unwrap();
    let _ = svc.wait();
    let out = status();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!((count("/connect/device"), count("/connect/ack")), (1, 1), "restarts never pair again");
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&home.join("connection")), 0o700);
    for f in ["connection.json", "sent.json"] {
        assert_eq!(mode(&home.join("connection").join(f)), 0o600, "{f}");
    }
}

/// serverq.md § 5, § 4 and docs/design/migration.md against a fake legacy folder, a fake `systemctl` and a fake release served by a fake `curl`.
#[cfg(target_os = "linux")]
#[test]
fn service_install_migrates_runs_updates_and_rolls_back() {
    let d = scratch("service");
    let (home, bin, rel, log, dash) = (d.join("h"), d.join("bin"), d.join("rel"), d.join("systemctl.log"), d.join("dash"));
    let install = home.join(".local/share/serverq");
    let legacy = home.join(".serverq");
    for p in [&bin, &rel, &dash, &legacy.join("bootstrap")] {
        fs::create_dir_all(p).unwrap();
    }
    write_exe(&bin.join("systemctl"), "#!/bin/sh\necho \"$*\" >> \"$FAKE_LOG\"\n");
    write_exe(&bin.join("curl"), FAKE_CURL);
    let incident = r#"{"first_seen": 1, "last_seen": 2, "occurrences": 2, "state": "observed"}"#;
    fs::write(legacy.join("state.json"), format!(r#"{{"schema": 1, "cache": {{}}, "incidents": {{"disk-headroom": {incident}}}}}"#)).unwrap();
    fs::write(legacy.join("report.json"), r#"{"schema": 1, "results": {}}"#).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let run = |exe: &Path, args: &[&str]| {
        Command::new(exe).args(args).arg("--home").arg(&install).env("HOME", &home).env("PATH", &path).env("FAKE_LOG", &log).env("FAKE_RELEASE", &rel).env("FAKE_DASH", &dash).output().unwrap()
    };
    let me = Path::new(env!("CARGO_BIN_EXE_serverq"));
    let version = env!("CARGO_PKG_VERSION");

    fs::write(legacy.join("transaction.json"), "{}").unwrap();
    assert_eq!(run(me, &["service", "install"]).status.code(), Some(2), "open legacy transaction");
    assert!(fs::symlink_metadata(&legacy).unwrap().is_dir() && !log.exists(), "nothing changed");
    fs::remove_file(legacy.join("transaction.json")).unwrap();

    let first = run(me, &["service", "install"]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    assert!(String::from_utf8_lossy(&first.stdout).contains("rollback: serverq service uninstall"));
    assert_eq!(fs::read_link(&legacy).unwrap(), install);
    assert!(install.join("legacy/bootstrap").is_dir(), "legacy folder kept");
    let state: Value = serde_json::from_str(&text(install.join("state.json"))).unwrap();
    assert_eq!(state["incidents"]["disk-headroom"]["first_seen"], 1, "incidents carry over");
    let unit = text(home.join(".config/systemd/user/serverq.service"));
    for line in ["ProtectSystem=strict", "MemoryMax=128M", "Restart=always", &format!("ReadWritePaths={}", install.display()), &format!("ExecStart=\"{}/serverq\" service run", install.display())] {
        assert!(unit.contains(line), "{line}");
    }
    assert_eq!(
        text(&log).lines().filter(|l| l.starts_with("--user")).collect::<Vec<_>>().join("\n"),
        "--user disable --now serverq-check.timer serverq-update.timer\n--user stop serverq-check.service serverq-update.service\n--user daemon-reload\n--user enable --now serverq.service"
    );
    assert_eq!(text(install.join("current")), version);
    fs::remove_file(&log).unwrap();
    assert!(run(me, &["service", "install"]).status.success());
    assert!(!text(&log).contains("disable"), "a second install does not migrate");

    // the service through the launcher: an hourly check and a daily update, each one second here
    let launcher = install.join("serverq");
    assert_eq!(run(&launcher, &["service", "status"]).status.code(), Some(2), "no status yet is unknown");
    let mut svc = Command::new(&launcher).args(["service", "run", "--home"]).arg(&install).env("HOME", &home).env("PATH", &path).env("FAKE_RELEASE", &rel)
        .env("SERVERQ_CHECK_EVERY", "1").env("SERVERQ_UPDATE_EVERY", "1").stdout(std::process::Stdio::null()).spawn().unwrap();
    let status = install.join("status.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !(text(&status).contains("\"exit\"") && text(&status).contains("update: ")) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    svc.kill().unwrap();
    let _ = svc.wait();
    let s: Value = serde_json::from_str(&text(&status)).unwrap();
    assert!(s["check"]["exit"].is_u64() && s["update_at"].is_u64() && s["error"].as_str().unwrap().contains("update: "), "{s}");
    assert!(install.join("releases").join(version).join("healthy").exists(), "a completed check marks the release healthy");
    assert!(String::from_utf8_lossy(&run(&launcher, &["service", "status"]).stdout).contains("Update: unknown (update: "));

    // DashQ E9: an enrolled install keeps its grant, identity and receipts through an update and a rollback
    fs::write(dash.join("approved"), "").unwrap();
    let enrolled = run(&launcher, &["enroll", "https://dash.example/machine", "--headless"]);
    assert_eq!(enrolled.status.code(), Some(0), "{}", String::from_utf8_lossy(&enrolled.stderr));
    let connection = connection_files(&install);
    let connected = || run(&launcher, &["connection", "status"]).status.code() == Some(0) && connection_files(&install) == connection;

    // a fake second release: a bad checksum keeps current, a good one installs in place, rollback restores previous
    let (base, _) = version.split_once('-').unwrap_or((version, ""));
    let (stem, patch) = base.rsplit_once('.').unwrap();
    let next = format!("{stem}.{}", patch.parse::<u32>().unwrap() + 1);
    let asset = format!("serverq-{next}-linux-{}", std::env::consts::ARCH);
    fs::copy(me, rel.join(&asset)).unwrap();
    fs::write(rel.join("latest.json"), format!(r#"{{"version": "{next}", "files": {{"linux-{}": "{asset}"}}}}"#, std::env::consts::ARCH)).unwrap();
    fs::write(rel.join("SHA256SUMS"), format!("{}  {asset}\n", "0".repeat(64))).unwrap();
    assert_eq!(run(&launcher, &["update"]).status.code(), Some(2));
    assert_eq!(text(install.join("current")), version);
    assert!(!install.join("releases").join(&next).exists() && !install.join("releases").join(format!("{next}.tmp")).exists());
    let sums = Command::new("sha256sum").arg(&asset).current_dir(&rel).output().unwrap().stdout;
    fs::write(rel.join("SHA256SUMS"), sums).unwrap();
    fs::create_dir_all(install.join("releases/stale")).unwrap();
    let updated = run(&launcher, &["update"]);
    assert!(updated.status.success(), "{}", String::from_utf8_lossy(&updated.stderr));
    assert_eq!((text(install.join("current")), text(install.join("previous"))), (next.clone(), version.into()));
    assert!(install.join("releases").join(&next).join("serverq").exists());
    let mut kept: Vec<_> = fs::read_dir(install.join("releases")).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    kept.sort();
    let mut want = [version.to_string(), next.clone()];
    want.sort();
    assert_eq!(kept, want, "an update prunes releases to current and previous");
    assert!(connected(), "the update keeps the connection");
    fs::remove_file(&log).unwrap();
    assert!(run(&launcher, &["rollback"]).status.success());
    assert_eq!((text(install.join("current")), text(install.join("previous"))), (version.into(), next));
    assert!(install.join("paused").exists() && text(&log) == "--user restart serverq.service");
    assert!(connected(), "the rollback keeps the connection");
    assert_eq!(text(dash.join("log")).lines().filter(|l| l.starts_with("/connect/ack ")).count(), 1, "no second grant");
    assert!(run(&launcher, &["service", "resume"]).status.success());
    assert!(!install.join("paused").exists());
    assert!(run(&launcher, &["service", "uninstall"]).status.success());
    assert!(!home.join(".config/systemd/user/serverq.service").exists() && install.join("current").exists(), "uninstall keeps the install folder");
}

/// serverq.md § 6 against tool fakes on an isolated PATH: each tool at its minimum passes; one below its minimum, one
/// missing and one failing `--version` fail with tool, found version, minimum and fix, and exit 2.
#[cfg(unix)]
#[test]
fn doctor_checks_host_requirements() {
    use std::os::unix::fs::PermissionsExt;
    let bin = scratch("doctor");
    let fake = |name: &str, script: &str| {
        let p = bin.join(name);
        fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    };
    let doctor = || {
        let o = Command::new(env!("CARGO_BIN_EXE_serverq")).args(["doctor", "--home"]).arg(bin.join("home")).env("PATH", &bin).output().unwrap();
        (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let oldest = [("curl", "echo 'curl 7.20.2 (fake) libcurl/7.20.2'"), ("systemctl", "echo 'systemd 239'"), ("systemd-analyze", "echo 'systemd 239'"), ("docker", "echo 'Docker version 1.13.1, build fake'")];
    for (name, script) in oldest {
        fake(name, script);
    }
    for name in ["launchctl", "id", "hostname"] {
        fake(name, "exit 0");
    }
    let (code, out) = doctor();
    assert_eq!(code, Some(0), "{out}");
    assert!(out.lines().all(|l| l.starts_with("ok ")) && out.contains("ok curl found 7.20.2 minimum 7.20.2 for update, enroll, delivery"), "{out}");
    if cfg!(target_os = "linux") {
        assert!(out.contains("ok systemd-analyze found 239 minimum 239 for bounded-journal"), "{out}");
    }

    fake("curl", "echo 'curl 7.19.7 (fake)'");
    let (code, out) = doctor();
    assert_eq!(code, Some(2));
    assert!(out.contains("fail curl found 7.19.7 minimum 7.20.2 for update, enroll, delivery; fix: install or upgrade the OS curl package to 7.20.2 or newer"), "{out}");

    fake("curl", "echo 'curl: broken install' >&2; exit 1");
    assert!(doctor().1.contains("fail curl found unreadable (`curl --version` collection failed: exit 1: curl: broken install) minimum 7.20.2"));

    fs::remove_file(bin.join("curl")).unwrap();
    let (code, out) = doctor();
    assert_eq!(code, Some(2));
    assert!(out.contains("fail curl found missing minimum 7.20.2 for update, enroll, delivery; fix: "), "{out}");
    if cfg!(target_os = "linux") {
        fake("systemctl", "echo 'systemd 238'");
        assert!(doctor().1.contains("fail systemctl found 238 minimum 239 for failed-services, service; fix: upgrade the OS to a release with systemd 239 or newer"));
    } else {
        fs::remove_file(bin.join("launchctl")).unwrap();
        assert!(doctor().1.contains("fail launchctl found missing minimum os for service; fix: restore launchctl, which ships with macos"));
    }
}

#[test]
fn selftest_and_rules() {
    let out = Command::new(env!("CARGO_BIN_EXE_serverq")).arg("selftest").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let rules = Command::new(env!("CARGO_BIN_EXE_serverq")).arg("rules").output().unwrap();
    assert_eq!(rules.stdout, fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("rules.json")).unwrap());
}

/// serverq.md § 8: install owns one LaunchAgent; all lifecycle commands preserve data, reject alternate roots
/// before host effects and report launchctl failure. The fake adapter never touches the user's service.
#[cfg(target_os = "macos")]
#[test]
fn macos_service_lifecycle() {
    use std::os::unix::fs::PermissionsExt;
    let home = scratch("mac-service");
    let bin = home.join("bin");
    fs::create_dir(&bin).unwrap();
    let ctl = bin.join("launchctl");
    fs::write(&ctl, r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/calls"
[ -z "$FAIL_LAUNCHCTL" ] || exit 5
case "$1" in
bootout) [ -f "$HOME/loaded" ] || exit 3; rm "$HOME/loaded" ;;
bootstrap) touch "$HOME/loaded" ;;
*) exit 99 ;;
esac
"#).unwrap();
    fs::set_permissions(&ctl, fs::Permissions::from_mode(0o700)).unwrap();
    let install = home.join(".local/share/serverq");
    let plist = home.join("Library/LaunchAgents/com.serverq.serverq.plist");
    let run = |args: &[&str], fail: bool| {
        Command::new(env!("CARGO_BIN_EXE_serverq")).args(args).env("HOME", &home)
            .env_remove("SERVERQ_INSTALL").env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("FAIL_LAUNCHCTL", if fail { "1" } else { "" }).output().unwrap()
    };
    for cmd in ["install", "restart", "stop", "uninstall"] {
        let o = run(&["service", cmd, "--home", home.to_str().unwrap()], false);
        assert_eq!(o.status.code(), Some(2));
    }
    assert!(!home.join("calls").exists(), "alternate root must not touch the registered service");
    let o = run(&["service", "install"], false);
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(home.join("loaded").exists());
    let xml = fs::read_to_string(&plist).unwrap();
    assert!(xml.contains("<string>com.serverq.serverq</string>"));
    assert!(xml.contains(&format!("<string>{}/serverq</string>", install.display())));
    assert!(xml.contains("<string>service</string><string>run</string>"));
    let original = fs::read(install.join("report.json")).unwrap();
    let r: Value = serde_json::from_slice(&original).unwrap();
    for id in ["failed-services", "bounded-docker-logs", "bounded-journal"] {
        assert_eq!(r["results"][id]["reason"], "unsupported-os");
    }
    for cmd in ["restart", "stop", "stop", "restart", "install"] {
        let o = run(&["service", cmd], false);
        assert_eq!(o.status.code(), Some(0), "{cmd}: {}", String::from_utf8_lossy(&o.stderr));
    }
    for cmd in ["stop", "restart", "uninstall", "install"] {
        assert_eq!(run(&["service", cmd], true).status.code(), Some(2));
        assert!(plist.exists(), "failed unload must preserve the registration file");
    }
    assert_eq!(run(&["service", "uninstall"], false).status.code(), Some(0));
    assert!(!plist.exists());
    assert!(!home.join("loaded").exists());
    assert!(install.join("report.json").exists(), "uninstall must preserve reports");
    assert!(install.join("current").exists());
    let calls = fs::read_to_string(home.join("calls")).unwrap();
    assert!(calls.lines().all(|line| line.contains("com.serverq.serverq")));
    fs::remove_dir_all(home).unwrap();
}
