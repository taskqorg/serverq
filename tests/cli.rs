//! The real `serverq` binary with isolated state and a fake release.
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
    fs::write(d.join("inventory.json"),"[]").unwrap();
    d
}

fn check(home: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_serverq")).args(["check", "--home"]).arg(home).args(extra).output().unwrap()
}

fn report(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap()
}

/// #55 A2–A7/R1/R3: successive metadata samples reach the existing shipper without coalescing or secret content.
#[test]
fn inventory_samples_survive_restart_and_ship_without_file_or_environment_contents() {
    let home = fs::canonicalize(scratch("inventory")).unwrap();
    let root = home.join("project");
    fs::create_dir_all(root.join("large")).unwrap();
    let sentinel = "invented-file-content-ghp_aaaaaaaaaaaaaaaaaaaaaaaa";
    fs::write(root.join("large/private.txt"), sentinel).unwrap();
    fs::write(home.join("inventory.json"), serde_json::json!([root]).to_string()).unwrap();
    let run = || Command::new(env!("CARGO_BIN_EXE_serverq")).args(["check", "--home"]).arg(&home)
        .env("SERVERQ_INVENTED_SECRET", "invented-environment-value").output().unwrap();
    let first = report(&run());
    assert!(first["inventory"]["entities"].as_array().is_some(), "A2: the report must carry an inventory sample");
    let host = &first["inventory"]["entities"][0];
    assert_eq!(host["entity"], "host");
    assert_eq!(host["os"], std::env::consts::OS);
    let statuses: Vec<_> = first["results"].as_object().unwrap().values().map(|r| r["status"].as_str().unwrap()).collect();
    assert_eq!(host["status"], if statuses.contains(&"unknown") { "unknown" } else if statuses.contains(&"warn") { "warn" } else { "ok" });
    for field in ["os_version", "uptime_s", "cpu_count", "cpu_used_milli", "ram_total_bytes", "ram_used_bytes", "swap_total_bytes", "swap_used_bytes", "load_1_milli", "load_5_milli", "load_15_milli"] {
        assert!(host.get(field).is_some(), "A2: {field} is present");
        assert_eq!(host[field].is_null(), host["unknown"].get(field).is_some(), "R1: {field} has a reason iff unavailable");
    }
    if cfg!(windows) { assert_eq!(host["unknown"]["load_1_milli"], "unsupported-os"); }
    // R1/A7: Windows commit accounting is not a measurement of pagefile usage.
    if cfg!(windows) {
        for field in ["swap_total_bytes", "swap_used_bytes"] {
            assert!(host[field].is_null());
            assert_eq!(host["unknown"][field], "unsupported-os");
        }
    }
    fs::write(root.join("large/growth.bin"), vec![0; 16384]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let second = report(&run());
    let project_size = |r: &Value| r["inventory"]["entities"].as_array().unwrap().iter()
        .find(|r| r["entity"] == "project_root").unwrap()["disk_used_bytes"].as_u64().unwrap();
    assert!(project_size(&second) > project_size(&first));
    assert_ne!(first["inventory"]["trace"], second["inventory"]["trace"]);
    assert!(second["inventory"]["at_ms"].as_u64() > first["inventory"]["at_ms"].as_u64());
    let store = coreq::telemetry::Store::open(&home.join("telemetry.db")).unwrap();
    let connection = home.join("connection");
    fs::create_dir_all(&connection).unwrap();
    fs::write(connection.join("connection.json"), serde_json::json!({"installation": "invented", "active": {
        "destination": "https://dash.example", "credential": "invented", "generation": 1}}).to_string()).unwrap();
    struct Receiver(Vec<Value>);
    impl coreq::enroll::Wire for Receiver {
        fn post(&mut self, _: &str, _: &str, _: Option<&str>, body: &Value) -> Result<(u16, Value), String> {
            let records = body["records"].as_array().unwrap();
            self.0.extend(records.iter().cloned());
            Ok((200, serde_json::json!({"protocol": 1, "receipts": records.iter().map(|r|
                serde_json::json!({"event_id": r["event_id"], "state": "accepted"})).collect::<Vec<_>>()})))
        }
        fn now(&self) -> u64 { 0 }
        fn sleep(&mut self, _: u64) {}
        fn show(&mut self, _: &str, _: &str) {}
    }
    let mut receiver = Receiver(vec![]);
    let mut backoff = coreq::enroll::Backoff::default();
    while store.ship(&connection, &mut backoff, &mut receiver).unwrap().more {}
    let wire = serde_json::to_string(&receiver.0).unwrap();
    for private in [sentinel, "invented-environment-value", "private.txt", "growth.bin"] {
        assert!(!wire.contains(private), "A6: {private} must stay on the host");
    }
    let projects: Vec<_> = receiver.0.iter().map(|r| &r["payload"]["observation"])
        .filter(|o| o["inventory"]["entity"] == "project_root").collect();
    assert_eq!(projects.len(), 2, "A5: both samples survive process restart and ship");
    assert!(projects[1]["inventory"]["disk_used_bytes"].as_u64() > projects[0]["inventory"]["disk_used_bytes"].as_u64());
    assert_ne!(projects[0]["started_ms"], projects[1]["started_ms"]);
    assert_eq!(projects[0]["trace"], first["inventory"]["trace"]);
    drop(store);
    fs::remove_dir_all(home).unwrap();
}

/// Explicit diagnostics retain original time and reject injected authority before any observation is written.
#[test]
fn diagnostic_import_is_host_scoped_typed_and_preserves_producer_time() {
    let home=scratch("diagnostic-source");let dest=scratch("diagnostic-dest");
    let me=env!("CARGO_BIN_EXE_serverq");
    let output=Command::new(me).args(["inspect","--home"]).arg(&home).output().unwrap();
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let mut sample:Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(sample["source"],"ssh-diagnostic");
    let at=sample["at_ms"].clone();let file=home.join("diagnostic.json");fs::write(&file,sample.to_string()).unwrap();
    let run=||Command::new(me).arg("import").arg(&file).arg("--home").arg(&dest).output().unwrap();
    let imported=run();assert!(imported.status.success(),"{}",String::from_utf8_lossy(&imported.stderr));
    let count={let store=coreq::telemetry::Store::open(&dest.join("telemetry.db")).unwrap();let rows=store.export(0,0,1024).unwrap();
        assert!(rows["events"].as_array().unwrap().iter().any(|r|r["observation"]["kind"]=="inventory"&&r["observation"]["started_ms"]==at));
        rows["events"].as_array().unwrap().len()};
    sample["entities"][0]["command"]="invented-authority".into();fs::write(&file,sample.to_string()).unwrap();
    assert!(!run().status.success());
    let store=coreq::telemetry::Store::open(&dest.join("telemetry.db")).unwrap();assert_eq!(store.export(0,0,1024).unwrap()["events"].as_array().unwrap().len(),count);
    drop(store);fs::remove_dir_all(home).unwrap();fs::remove_dir_all(dest).unwrap();
}

/// R1/R3/§ 6: command failures name their local cause without exporting raw diagnostics.
#[cfg(unix)]
#[test]
fn inventory_probe_failure_keeps_local_cause_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = scratch("inventory-probe");
    let bin = home.join("bin");
    fs::create_dir(&bin).unwrap();
    let tool = if cfg!(target_os = "macos") { "launchctl" } else { "systemctl" };
    let path = bin.join(tool);
    fs::write(&path, "#!/bin/sh\necho invented-private-probe-error >&2\nexit 5\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_serverq")).args(["check", "--home"]).arg(&home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display())).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let command = if cfg!(target_os = "macos") { "launchctl list" } else { "systemctl show" };
    assert!(stderr.lines().any(|l| l.starts_with("serverq: inventory unknown:")
        && l.contains(command) && l.contains("exit 5: invented-private-probe-error")), "{stderr}");
    let sample = report(&output);
    assert_eq!(sample["inventory"]["collections"]["services"]["status"], "unknown");
    assert_eq!(sample["inventory"]["collections"]["services"]["reason"], "collection-failed");
    assert!(sample["inventory"]["collections"]["services"]["count"].is_null());
    let store = coreq::telemetry::Store::open(&home.join("telemetry.db")).unwrap();
    let records = store.export(0, 0, 1024).unwrap();
    assert!(!records.to_string().contains("invented-private-probe-error"));
    let services = records["events"].as_array().unwrap().iter().map(|e| &e["observation"])
        .find(|o| o["operation"] == "inventory.services").unwrap();
    assert_eq!(services["error"], "collection-failed");
    assert!(services["units"].is_null());
    drop(store);
    fs::remove_dir_all(home).unwrap();
}

/// A2/A3, R1 and § 9: a collection failure must survive the binary/store/shipper/D11 boundary as failed.
#[test]
fn check_observations_reach_dashq_as_failed() {
    use coreq::{enroll::{Backoff, Wire}, telemetry::Store};
    #[derive(Default)]
    struct Dash(Vec<Value>);
    impl Wire for Dash {
        fn post(&mut self, _: &str, route: &str, _: Option<&str>, body: &Value) -> Result<(u16, Value), String> {
            assert_eq!(route, "/ingest");
            let records = body["records"].as_array().unwrap();
            self.0.extend(records.iter().cloned());
            Ok((200, serde_json::json!({"protocol": 1, "receipts": records.iter().map(|r|
                serde_json::json!({"event_id": r["event_id"], "state": "accepted"})).collect::<Vec<_>>()})))
        }
        fn now(&self) -> u64 { 0 }
        fn sleep(&mut self, _: u64) {}
        fn show(&mut self, _: &str, _: &str) {}
    }
    let home = scratch("observations");
    let missing = home.join("private-absent-filesystem");
    let output = check(&home, &["--filesystem", missing.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(report(&output)["results"]["disk-headroom"]["status"], "unknown");
    assert!(home.join("telemetry.db").exists(), "each check must write a CoreQ observation");
    let store = Store::open(&home.join("telemetry.db")).unwrap();
    assert_eq!(store.shipper().unwrap()["cursor"], 0, "unshipped observations are protected before enrollment");
    let connection = home.join("connection");
    fs::create_dir_all(&connection).unwrap();
    // Invented local grant; only the fake DashQ below receives it.
    fs::write(connection.join("connection.json"), serde_json::json!({"installation": "invented", "active": {
        "destination": "https://dash.example", "credential": "invented", "generation": 1}}).to_string()).unwrap();
    let mut dash = Dash::default();
    let shipped = store.ship(&connection, &mut Backoff::default(), &mut dash).unwrap();
    let checks: Vec<_> = dash.0.iter().filter(|r| r["payload"]["observation"]["kind"] == "check").collect();
    assert!(shipped.sent >= RESULTS.len());
    assert_eq!(checks.len(), RESULTS.len(), "inventory does not replace any rule observation");
    assert_eq!(checks.iter().map(|r| r["payload"]["observation"]["operation"].as_str().unwrap()).collect::<BTreeSet<_>>(), RESULTS.into());
    for record in checks {
        assert_eq!(record["schema"], "coreq-observation/1");
        let o = &record["payload"]["observation"];
        assert_eq!(o["kind"], "check");
        assert!(o["started_ms"].as_u64().is_some() && o["duration_us"].as_u64().is_some());
        assert!(o["host"].is_string() || o["unknown"]["host"].is_string());
        assert!(!record.to_string().contains(home.to_str().unwrap()), "private paths stay local");
        if ["disk-headroom", "inode-headroom"].contains(&o["operation"].as_str().unwrap()) {
            let reason = if cfg!(windows) { "unsupported-os" } else { "collection-failed" };
            assert_eq!((o["outcome"].as_str(), o["error"].as_str()), (Some("failed"), Some(reason)));
            assert!(o["disk_bytes"].is_null());
            assert_eq!(o["unknown"]["disk_bytes"], reason);
        }
    }
    assert_eq!(store.shipper().unwrap()["backlog_rows"], 0);
    drop(store);
    // R1/R3: a corrupt observation store stays intact and cannot suppress or change the check report.
    let corrupt = home.join("corrupt");
    fs::create_dir(&corrupt).unwrap();
    fs::write(corrupt.join("telemetry.db"), b"broken").unwrap();
    let output = check(&corrupt, &["--filesystem", missing.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(report(&output)["results"]["disk-headroom"]["status"], "unknown");
    assert_eq!(fs::read(corrupt.join("report.json")).unwrap(), output.stdout);
    assert!(String::from_utf8_lossy(&output.stderr).contains("telemetry unknown"));
    assert_eq!(fs::read(corrupt.join("telemetry.db")).unwrap(), b"broken");
    fs::remove_dir_all(home).unwrap();
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
    // serverq.md § 3, § 5: the projection names the catalog of this report and no local path
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

/// A4 / serverq.md § 5: CLI ages use one whole unit and show exact UTC; missing times stay unavailable.
#[test]
fn service_status_human_time() {
    let home = scratch("human-time");
    let run = || Command::new(env!("CARGO_BIN_EXE_serverq")).args(["service", "status", "--home"]).arg(&home).output().unwrap();
    let missing = run();
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stdout).contains("? unavailable"));
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600;
    fs::write(home.join("status.json"), serde_json::json!({"check": {"at": at, "exit": 0}}).to_string()).unwrap();
    let fresh = run();
    assert_eq!(fresh.status.code(), Some(0));
    let out = String::from_utf8_lossy(&fresh.stdout);
    assert!(out.contains("Check: ok <1m ago (") && out.contains(" UTC)"), "{out}");
    fs::write(home.join("status.json"), r#"{"check":{"at":0,"exit":0}}"#).unwrap();
    let stale = run();
    assert_eq!(stale.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&stale.stdout).contains("d ago (1970-01-01 00:00 UTC)"));
    for invalid in [serde_json::Value::Null, serde_json::json!(u64::MAX)] {
        fs::write(home.join("status.json"), serde_json::json!({"check": {"at": invalid, "exit": 0}}).to_string()).unwrap();
        let out = run();
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stdout).contains("? unavailable"));
    }
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

/// A3 / serverq.md § 4–5: both Windows launchers honor current; a missing release cannot run embedded commands.
#[cfg(windows)]
#[test]
fn windows_launchers_honor_current() {
    use std::os::windows::process::CommandExt;

    let home = scratch("launchers");
    let tool = coreq::Tool { name: "serverq", title: "ServerQ", repo: "taskqorg/serverq", version: env!("CARGO_PKG_VERSION"), install: home.clone(), fault: None };
    let bytes = fs::read(env!("CARGO_BIN_EXE_serverq")).unwrap();
    coreq::update::prepare_install(&tool, &bytes).unwrap();
    coreq::update::activate_install(&tool).unwrap();
    for current in [tool.version, "9.9.9"] {
        fs::write(home.join("current"), current).unwrap();
        for name in [tool.exe_name(), tool.gui_exe_name()] {
            let out = Command::new(home.join(&name)).arg("rules").env("SERVERQ_INSTALL", &home).creation_flags(0x0800_0000).output().unwrap();
            if current == tool.version {
                assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
                assert_eq!(out.stdout, include_bytes!("../rules.json"));
            } else {
                assert_eq!(out.status.code(), Some(1), "{name} must refuse missing current");
                assert!(out.stdout.is_empty(), "{name} must not dispatch embedded rules");
                assert!(String::from_utf8_lossy(&out.stderr).contains("start 9.9.9"), "{name}: {}", String::from_utf8_lossy(&out.stderr));
            }
        }
    }
    fs::remove_dir_all(home).unwrap();
}

/// serverq.md § 7: the repeating trigger restarts the launcher, so Windows `stop` tells the operator it is temporary.
/// Copies of this binary stand in for schtasks and powershell: Windows looks beside the running exe first, and the
/// fake's nonzero exit (the real script exits 0) proves the host's real task was not touched.
#[cfg(windows)]
#[test]
fn windows_stop_says_it_is_temporary() {
    let home = scratch("stop");
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    for name in ["serverq.exe", "schtasks.exe", "powershell.exe"] {
        fs::copy(env!("CARGO_BIN_EXE_serverq"), bin.join(name)).unwrap();
    }
    let out = Command::new(bin.join("serverq.exe")).args(["service", "stop"]).env("SERVERQ_INSTALL", &home).env("PATH", &bin).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("`powershell"), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("within 5 minutes") && stdout.contains("serverq service uninstall"), "{stdout}");
    fs::remove_dir_all(home).unwrap();
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
  while [ $# -gt 1 ]; do shift; done
  cat "$FAKE_RELEASE/${1##*/}"; exit;;
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
  ok "\"credential\":\"cred-1\",\"installation\":\"$(cat "$d/installation")\",\"generation\":1,\"product\":\"serverq\",\"workspace\":\"ws-1\",\"schemas\":[\"serverq-report/1\",\"coreq-observation/1\"]";;
/connect/ack) ok '"state":"active","generation":1';;
/ingest)
  case "$cfg" in *coreq-observation/1*)
    receipts=
    for id in $(printf '%s\n' "$cfg" | grep -o 'event_id\\":\\"[0-9a-f]*' | sed 's/.*\\"//'); do
      grep -qx "$id" "$d/observations" 2>/dev/null || echo "$id" >> "$d/observations"
      receipts="${receipts}${receipts:+,}{\"event_id\":\"$id\",\"state\":\"accepted\"}"
    done
    ok "\"receipts\":[$receipts]"; exit;;
  esac
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

/// DashQ E3 (serverq.md § 3, § 5) through the real binary and coreq's HTTPS wire against a fake DashQ: a crash
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
    write_exe(&bin.join("gh"), r#"#!/bin/sh
[ "$1" = --version ] && { echo 'gh version 2.49.0'; exit; }
[ "$FAKE_ATTEST_FAIL" = 1 ] && exit 1
exit 0
"#);
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
    wait(|| text(dash.join("observations")).lines().count() >= RESULTS.len());
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

/// serverq.md § 5 and § 4: preparation failure preserves the active files; install, service, update and rollback
/// against a fake `systemctl` and a fake release served by a fake `curl` retain incidents, identity and credentials.
#[cfg(target_os = "linux")]
#[test]
fn service_install_runs_updates_and_rolls_back() {
    let d = scratch("service");
    let (home, bin, rel, log, dash) = (d.join("h"), d.join("bin"), d.join("rel"), d.join("systemctl.log"), d.join("dash"));
    let install = home.join(".local/share/serverq");
    for p in [&bin, &rel, &dash, &install] {
        fs::create_dir_all(p).unwrap();
    }
    write_exe(&bin.join("systemctl"), "#!/bin/sh\necho \"$*\" >> \"$FAKE_LOG\"\n");
    write_exe(&bin.join("curl"), FAKE_CURL);
    write_exe(&bin.join("gh"), r#"#!/bin/sh
[ "$1" = --version ] && { echo 'gh version 2.49.0'; exit; }
[ "$FAKE_ATTEST_FAIL" = 1 ] && exit 1
exit 0
"#);
    let incident = r#"{"first_seen": 1, "last_seen": 2, "occurrences": 2, "state": "observed"}"#;
    fs::write(install.join("state.json"), format!(r#"{{"schema": 1, "cache": {{}}, "incidents": {{"disk-headroom": {incident}}}}}"#)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let run = |exe: &Path, args: &[&str]| {
        Command::new(exe).args(args).arg("--home").arg(&install).env("HOME", &home).env("PATH", &path).env("FAKE_LOG", &log).env("FAKE_RELEASE", &rel).env("FAKE_DASH", &dash).output().unwrap()
    };
    let me = Path::new(env!("CARGO_BIN_EXE_serverq"));
    let version = env!("CARGO_PKG_VERSION");

    let release = install.join("releases").join(version).join("serverq");
    fs::create_dir_all(release.parent().unwrap()).unwrap();
    fs::write(&release, b"different bytes").unwrap();
    assert_eq!(run(me, &["service", "install"]).status.code(), Some(2), "immutable release conflict");
    assert_eq!(fs::read(&release).unwrap(), b"different bytes");
    assert!(!log.exists() && !install.join("current").exists() && !install.join("serverq").exists(), "preparation failure has no service or activation effects");
    fs::remove_file(&release).unwrap();

    let first = run(me, &["service", "install"]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let state: Value = serde_json::from_str(&text(install.join("state.json"))).unwrap();
    assert_eq!(state["incidents"]["disk-headroom"]["first_seen"], 1, "installation preserves existing incidents");
    let unit = text(home.join(".config/systemd/user/serverq.service"));
    for line in ["ProtectSystem=strict", "MemoryMax=128M", "Restart=always", &format!("ReadWritePaths={}", install.display()), &format!("ExecStart=\"{}/serverq\" service run", install.display())] {
        assert!(unit.contains(line), "{line}");
    }
    assert_eq!(
        text(&log).lines().filter(|l| l.starts_with("--user")).collect::<Vec<_>>().join("\n"),
        "--user daemon-reload\n--user enable --now serverq.service"
    );
    assert_eq!(text(install.join("current")), version);
    fs::remove_file(&log).unwrap();
    assert!(run(me, &["service", "install"]).status.success());
    assert_eq!(text(&log).lines().filter(|l| l.starts_with("--user")).collect::<Vec<_>>(), ["--user daemon-reload", "--user enable --now serverq.service", "--user restart serverq.service"]);

    // A different installation cannot replace or restart this host's unit.
    let native=home.join(".config/systemd/user/serverq.service");let owned=fs::read(&native).unwrap();
    fs::write(&native,"[Service]\nExecStart=/another/installation/serverq service run\n").unwrap();
    fs::remove_file(&log).unwrap();
    let refused=run(me,&["service","install"]);assert_eq!(refused.status.code(),Some(2));
    assert_eq!(text(install.join("current")),version);assert!(!log.exists());fs::write(&native,owned).unwrap();

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
    fs::write(rel.join("provenance.sigstore.jsonl"),"invented bundle for fake gh").unwrap();
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
    let oldest = [("gh", "echo 'gh version 2.49.0'"), ("curl", "echo 'curl 7.20.2 (fake) libcurl/7.20.2'"), ("systemctl", "echo 'systemd 239'"), ("systemd-analyze", "echo 'systemd 239'"), ("docker", "echo 'Docker version 1.13.1, build fake'")];
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
[ "$1" != list ] || { printf 'PID\tStatus\tLabel\n'; exit 0; }
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
    // § 5: a preparation failure must not boot out a running LaunchAgent or change its registration.
    let release = install.join("releases").join(env!("CARGO_PKG_VERSION")).join("serverq");
    let bytes = fs::read(&release).unwrap();
    let calls = fs::read(home.join("calls")).unwrap();
    fs::write(&release, b"different bytes").unwrap();
    assert_eq!(run(&["service", "install"], false).status.code(), Some(2));
    assert_eq!(fs::read(home.join("calls")).unwrap(), calls);
    assert!(home.join("loaded").exists());
    assert_eq!(fs::read_to_string(&plist).unwrap(), xml);
    fs::write(&release, bytes).unwrap();
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
