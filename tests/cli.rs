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
    let missing = check(&home, &["--filesystem", home.join("absent").to_str().unwrap()]);
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(report(&missing)["results"]["disk-headroom"]["status"], "unknown");
    fs::write(home.join("state.json"), "broken").unwrap();
    let broken = check(&home, &[]);
    assert_eq!(broken.status.code(), Some(2));
    assert!(broken.stdout.is_empty());
    assert!(String::from_utf8_lossy(&broken.stderr).contains("no healthy result claimed"));
    assert_eq!(fs::read_to_string(home.join("state.json")).unwrap(), "broken", "corrupt state is never reset");
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

fn write_exe(p: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(p, text).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

fn text(p: impl AsRef<Path>) -> String {
    fs::read_to_string(p).unwrap_or_default().trim().to_string()
}

/// serverq.md § 5, § 6 and § 4 against a fake legacy folder, a fake `systemctl` and a fake release served by a fake `curl`.
#[test]
fn service_install_migrates_runs_updates_and_rolls_back() {
    if !cfg!(target_os = "linux") {
        eprintln!("the service is Linux only; skipped");
        return;
    }
    let d = scratch("service");
    let (home, bin, rel, log) = (d.join("h"), d.join("bin"), d.join("rel"), d.join("systemctl.log"));
    let install = home.join(".local/share/serverq");
    let legacy = home.join(".serverq");
    for p in [&bin, &rel, &legacy.join("bootstrap")] {
        fs::create_dir_all(p).unwrap();
    }
    write_exe(&bin.join("systemctl"), "#!/bin/sh\necho \"$*\" >> \"$FAKE_LOG\"\n");
    write_exe(&bin.join("curl"), "#!/bin/sh\nwhile [ $# -gt 1 ]; do [ \"$1\" = -o ] && out=$2; shift; done\ncp \"$FAKE_RELEASE/${1##*/}\" \"$out\"\n");
    let incident = r#"{"first_seen": 1, "last_seen": 2, "occurrences": 2, "state": "observed"}"#;
    fs::write(legacy.join("state.json"), format!(r#"{{"schema": 1, "cache": {{}}, "incidents": {{"disk-headroom": {incident}}}}}"#)).unwrap();
    fs::write(legacy.join("report.json"), r#"{"schema": 1, "results": {}}"#).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let run = |exe: &Path, args: &[&str]| {
        Command::new(exe).args(args).arg("--home").arg(&install).env("HOME", &home).env("PATH", &path).env("FAKE_LOG", &log).env("FAKE_RELEASE", &rel).output().unwrap()
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

    // a fake second release: a bad checksum keeps current, a good one installs in place, rollback restores previous
    let asset = format!("serverq-1.0.1-linux-{}", std::env::consts::ARCH);
    fs::copy(me, rel.join(&asset)).unwrap();
    fs::write(rel.join("latest.json"), format!(r#"{{"version": "1.0.1", "files": {{"linux-{}": "{asset}"}}}}"#, std::env::consts::ARCH)).unwrap();
    fs::write(rel.join("SHA256SUMS"), format!("{}  {asset}\n", "0".repeat(64))).unwrap();
    assert_eq!(run(&launcher, &["update"]).status.code(), Some(2));
    assert_eq!(text(install.join("current")), version);
    assert!(!install.join("releases/1.0.1").exists() && !install.join("releases/1.0.1.tmp").exists());
    let sums = Command::new("sha256sum").arg(&asset).current_dir(&rel).output().unwrap().stdout;
    fs::write(rel.join("SHA256SUMS"), sums).unwrap();
    let updated = run(&launcher, &["update"]);
    assert!(updated.status.success(), "{}", String::from_utf8_lossy(&updated.stderr));
    assert_eq!((text(install.join("current")), text(install.join("previous"))), ("1.0.1".into(), version.into()));
    assert!(install.join("releases/1.0.1/serverq").exists());
    fs::remove_file(&log).unwrap();
    assert!(run(&launcher, &["rollback"]).status.success());
    assert_eq!((text(install.join("current")), text(install.join("previous"))), (version.into(), "1.0.1".into()));
    assert!(install.join("paused").exists() && text(&log) == "--user restart serverq.service");
    assert!(run(&launcher, &["service", "resume"]).status.success());
    assert!(!install.join("paused").exists());
    assert!(run(&launcher, &["service", "uninstall"]).status.success());
    assert!(!home.join(".config/systemd/user/serverq.service").exists() && install.join("current").exists(), "uninstall keeps the install folder");
}

#[test]
fn selftest_and_rules() {
    let out = Command::new(env!("CARGO_BIN_EXE_serverq")).arg("selftest").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let rules = Command::new(env!("CARGO_BIN_EXE_serverq")).arg("rules").output().unwrap();
    assert_eq!(rules.stdout, fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("rules.json")).unwrap());
}
