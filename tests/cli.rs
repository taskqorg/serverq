//! The real `serverq check` binary with isolated state, and its parity with the Python checker.
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

/// Same host, same input: the same report keys per result and the same exit code as `serverq.py check`.
#[test]
fn python_parity() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("serverq.py");
    if Command::new("python3").arg("--version").output().is_err() || !script.exists() {
        eprintln!("python3 or serverq.py unavailable; parity skipped");
        return;
    }
    let shape = |r: &Value| -> Vec<(String, BTreeSet<String>)> {
        let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<BTreeSet<_>>();
        let mut s = vec![("".into(), keys(r))];
        s.extend(r["results"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), keys(v))));
        s.sort();
        s
    };
    for extra in [&[][..], &["--filesystem", "/nonexistent-serverq"][..]] {
        let (rust_home, py_home) = (scratch("parity-rust"), scratch("parity-py"));
        let py = Command::new("python3").arg(&script).args(["check", "--home"]).arg(&py_home).args(extra).output().unwrap();
        let rust = check(&rust_home, extra);
        assert_eq!(rust.status.code(), py.status.code(), "exit code {extra:?}");
        assert_eq!(shape(&report(&rust)), shape(&report(&py)), "report keys {extra:?}");
        let state = |h: &Path| serde_json::from_slice::<Value>(&fs::read(h.join("state.json")).unwrap()).unwrap().as_object().unwrap().keys().cloned().collect::<BTreeSet<_>>();
        assert_eq!(state(&rust_home), state(&py_home), "state keys {extra:?}");
    }
}
