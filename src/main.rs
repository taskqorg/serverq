//! Read-only Linux server audit (serverq.md § 3). Author: Tight <tight@kirs.online>.
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RULES: &[u8] = include_bytes!("../rules.json");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DOCKER: [&str; 3] = ["docker", "--host", "unix:///var/run/docker.sock"];
const USAGE: &str = "usage: serverq check [--force] [--filesystem PATH] [--home PATH]";

type Obj = Map<String, Value>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&args).unwrap_or_else(|e| {
        eprintln!("serverq {} failed; no healthy result claimed: {e}", args.first().map_or("", String::as_str));
        2
    });
    std::process::exit(code);
}

fn run(args: &[String]) -> Result<i32, String> {
    if args.first().map(String::as_str) != Some("check") {
        return Err(USAGE.into());
    }
    let (mut home, mut filesystem, mut force) = (None, "/".to_string(), false);
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--home" => home = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--filesystem" => filesystem = it.next().ok_or(USAGE)?.clone(),
            "--force" => force = true,
            _ => return Err(format!("unknown argument {a}; {USAGE}")),
        }
    }
    let home = home.unwrap_or_else(|| coreq::Tool::new("serverq", "ServerQ", "taskqorg/serverq", VERSION).install);
    let _lock = lock(&home)?;
    let report = audit(&home, &filesystem, force)?;
    println!("{}", pretty(&report));
    let statuses: Vec<&str> = report["results"].as_object().into_iter().flatten().map(|(_, r)| r["status"].as_str().unwrap_or("unknown")).collect();
    Ok(if statuses.contains(&"unknown") { 2 } else if statuses.contains(&"warn") { 1 } else { 0 })
}

/// Exclusive `<home>/lock`; a busy lock is an error, never a skipped audit.
fn lock(home: &Path) -> Result<File, String> {
    let e = |e: std::io::Error| format!("{}: {e}", home.display());
    fs::DirBuilder::new().recursive(true).mode(0o700).create(home).map_err(e)?;
    fs::set_permissions(home, fs::Permissions::from_mode(0o700)).map_err(e)?;
    let p = home.join("lock");
    let f = OpenOptions::new().append(true).create(true).mode(0o600).open(&p).map_err(e)?;
    fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).map_err(e)?;
    f.try_lock().map_err(|_| "serverq is already running; check was not started")?;
    Ok(f)
}

fn audit(home: &Path, filesystem: &str, force: bool) -> Result<Value, String> {
    let rules: Obj = serde_json::from_slice(RULES).map_err(|e| format!("rules.json: {e}"))?;
    let number = |k: &str, field: &str| rules.get(k).and_then(|r| r[field].as_f64()).ok_or(format!("rules.json: {k} lacks {field}"));
    let now = now();
    let mut state = load_state(&home.join("state.json"))?;
    let revision = sha256(&[RULES, VERSION.as_bytes()].concat());
    if force || state.get("revision").and_then(Value::as_str) != Some(&revision) {
        state.insert("cache".into(), json!({}));
    }
    let mut results = Obj::new();
    match statvfs(filesystem) {
        Ok([total, free, files, favail]) => {
            results.insert("disk-headroom".into(), capacity(total, free, number("disk-headroom", "minimum_percent")?));
            results.insert("inode-headroom".into(), capacity(files, favail, number("inode-headroom", "minimum_percent")?));
        }
        Err(e) => {
            for k in ["disk-headroom", "inode-headroom"] {
                results.insert(k.into(), unknown(format!("Filesystem collection failed: {e}")));
            }
        }
    }
    results.insert("failed-services".into(), failed_units(command(&["systemctl", "--failed", "--no-legend", "--plain", "--no-pager"])));
    results.insert("bounded-docker-logs".into(), docker_logs(which("docker"), Path::new("/var/run/docker.sock").exists(), &mut |a| command(a)));
    let journal = match journal_fingerprint() {
        Ok(fp) => {
            let cache = state.entry("cache").or_insert(json!({})).as_object_mut().ok_or("invalid state cache")?;
            let ttl = number("bounded-journal", "ttl_seconds")?;
            cached(cache, "bounded-journal", &fp, now, ttl, || match command(&["systemd-analyze", "cat-config", "systemd/journald.conf"]) {
                Ok(text) => journal_policy(&text),
                Err(e) => unknown(e),
            })?
        }
        Err(e) => unknown(format!("Journal config unavailable: {e}")),
    };
    results.insert("bounded-journal".into(), journal);
    incidents(&mut state, &results, now)?;
    let manual: Vec<&String> = rules.iter().filter(|(_, r)| r["manual"] == true).map(|(k, _)| k).collect();
    let report = json!({"schema": 1, "at": now, "revision": revision, "filesystem": filesystem, "results": results,
        "incidents": state.get("incidents").cloned().unwrap_or(json!({})), "manual_rules": manual});
    state.insert("revision".into(), revision.into());
    state.insert("last_run".into(), now.into());
    save_json(&home.join("state.json"), &state.into())?;
    save_json(&home.join("report.json"), &report)?;
    Ok(report)
}

fn unknown(detail: String) -> Value {
    json!({"status": "unknown", "detail": detail})
}

fn capacity(total: u64, available: u64, minimum: f64) -> Value {
    if total == 0 {
        return unknown("Capacity unavailable".into());
    }
    let percent = available as f64 * 100.0 / total as f64;
    json!({"status": if percent < minimum { "warn" } else { "ok" }, "detail": format!("{percent:.1}% available; minimum {minimum}%"),
        "available": available, "total": total})
}

/// `[total bytes, free bytes, inodes, free inodes]` as `shutil.disk_usage` and `os.statvfs` report them.
fn statvfs(path: &str) -> Result<[u64; 4], String> {
    let c = std::ffi::CString::new(path).map_err(|_| "InvalidPath")?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(format!("{:?}", std::io::Error::last_os_error().kind()));
    }
    let block = s.f_frsize as u64;
    Ok([s.f_blocks as u64 * block, s.f_bavail as u64 * block, s.f_files as u64, s.f_favail as u64])
}

/// Runs without a shell, 15 s timeout; spawn failure, nonzero exit or timeout is an error.
fn command(argv: &[&str]) -> Result<String, String> {
    let fail = |why: String| format!("{} collection failed: {why}", argv[0]);
    let mut child = Command::new(argv[0]).args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| fail(format!("{:?}", e.kind())))?;
    let mut out = child.stdout.take().ok_or_else(|| fail("no stdout".into()))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = tx.send(out.read_to_end(&mut buf).map(|_| buf));
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait().map_err(|e| fail(format!("{:?}", e.kind())))? {
            Some(s) => break s,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail("timeout".into()));
            }
        }
    };
    if !status.success() {
        return Err(fail(status.code().map_or("signal".into(), |c| format!("exit {c}"))));
    }
    // a grandchild holding the pipe open must not outlive the deadline either
    let left = deadline.saturating_duration_since(Instant::now());
    let buf = rx.recv_timeout(left).map_err(|_| fail("timeout".into()))?.map_err(|e| fail(format!("{:?}", e.kind())))?;
    String::from_utf8(buf).map_err(|_| fail("invalid UTF-8".into()))
}

fn which(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| fs::metadata(d.join(name)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)))
}

fn failed_units(output: Result<String, String>) -> Value {
    match output {
        Ok(text) => {
            let units: Vec<&str> = text.lines().filter_map(|l| l.split_whitespace().next()).collect();
            json!({"status": if units.is_empty() { "ok" } else { "warn" }, "detail": format!("{} failed system units", units.len()), "units": units})
        }
        Err(e) => unknown(e),
    }
}

fn journal_policy(text: &str) -> Value {
    let mut values = Obj::new();
    let mut section = "";
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
        } else if section == "[Journal]" && !line.starts_with(['#', ';'])
            && let Some((k, v)) = line.split_once('=')
        {
            values.insert(k.trim().into(), v.trim().into());
        }
    }
    let positive = |v: &str| {
        let d = v.strip_suffix(['K', 'M', 'G', 'T', 'P', 'E']).unwrap_or(v);
        d.starts_with(|c: char| ('1'..='9').contains(&c)) && d.bytes().all(|b| b.is_ascii_digit())
    };
    let keys = ["SystemMaxUse", "SystemKeepFree", "RuntimeMaxUse"];
    let missing: Vec<&str> = keys.into_iter().filter(|k| !values.get(*k).and_then(Value::as_str).is_some_and(positive)).collect();
    let detail = if missing.is_empty() { "Persistent and runtime journals have explicit bounds".into() } else { format!("Missing explicit positive journal bounds: {}", missing.join(", ")) };
    let shown: Obj = keys.iter().map(|k| (k.to_string(), values.get(*k).cloned().unwrap_or(Value::Null))).collect();
    json!({"status": if missing.is_empty() { "ok" } else { "warn" }, "detail": detail, "values": shown})
}

/// Every container of the local system daemon, regardless of the caller's Docker context.
fn docker_logs(cli: bool, socket: bool, run: &mut dyn FnMut(&[&str]) -> Result<String, String>) -> Value {
    match (cli, socket) {
        (false, true) => unknown("Docker CLI unavailable".into()),
        (false, false) => json!({"status": "ok", "detail": "System Docker not installed"}),
        _ => docker_inspect(run).unwrap_or_else(|e| unknown(format!("Docker log collection failed: {e}"))),
    }
}

fn docker_inspect(run: &mut dyn FnMut(&[&str]) -> Result<String, String>) -> Result<Value, String> {
    let listed = run(&[&DOCKER[..], &["container", "ls", "--all", "--quiet", "--no-trunc"]].concat())?;
    let ids: Vec<&str> = listed.split_whitespace().collect();
    let hex = |i: &&str| i.len() == 64 && i.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if ids.iter().collect::<HashSet<_>>().len() != ids.len() || !ids.iter().all(hex) {
        return Err("invalid container inventory".into());
    }
    let inspected = if ids.is_empty() { String::new() } else { run(&[&DOCKER[..], &["inspect", "--type", "container", "--format", "{{json .HostConfig.LogConfig}}"], &ids].concat())? };
    let lines: Vec<&str> = inspected.lines().collect();
    if lines.len() != ids.len() {
        return Err("incomplete container inspection".into());
    }
    let (mut deviations, mut unsupported) = (vec![], vec![]);
    for (id, line) in ids.iter().zip(lines) {
        let config: Value = serde_json::from_str(line).map_err(|_| "invalid log configuration")?;
        let (Some(driver), Some(options)) = (config.get("Type").and_then(Value::as_str), config.get("Config").and_then(Value::as_object)) else {
            return Err("invalid log configuration".into());
        };
        match driver {
            "none" | "journald" => {}
            "json-file" | "local" => {
                let count = options.get("max-file").and_then(Value::as_str).is_some_and(|c| c.starts_with(|c: char| ('1'..='9').contains(&c)) && c.bytes().all(|b| b.is_ascii_digit()));
                if !(count && options.get("max-size").and_then(Value::as_str).is_some_and(positive_size)) {
                    deviations.push(*id);
                }
            }
            _ => unsupported.push(*id),
        }
    }
    Ok(json!({"status": if !unsupported.is_empty() { "unknown" } else if !deviations.is_empty() { "warn" } else { "ok" },
        "detail": format!("{} containers inspected; {} lack explicit positive rotation; {} unsupported log drivers", ids.len(), deviations.len(), unsupported.len()),
        "missing_rotation": deviations, "unsupported": unsupported}))
}

/// `[0-9]+(\.[0-9]+)?([kmg]b?|b)?`, case-insensitive, with a positive number.
fn positive_size(size: &str) -> bool {
    let s = size.to_ascii_lowercase();
    let s = s.strip_suffix('b').unwrap_or(&s);
    let n = s.strip_suffix(['k', 'm', 'g']).unwrap_or(s);
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    let shaped = match n.split_once('.') {
        Some((a, b)) => digits(a) && digits(b),
        None => digits(n),
    };
    shaped && n.parse::<f64>().is_ok_and(|v| v > 0.0)
}

/// Every journald config file systemd reads, symlink targets included.
fn journal_fingerprint() -> Result<String, String> {
    let e = |e: std::io::Error| format!("{:?}", e.kind());
    let mut data = Vec::new();
    for dir in ["/usr/lib/systemd", "/usr/local/lib/systemd", "/run/systemd", "/etc/systemd"] {
        let root = Path::new(dir);
        let mut drop_ins: Vec<PathBuf> = fs::read_dir(root.join("journald.conf.d"))
            .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".conf") && !n.starts_with('.'))).collect())
            .unwrap_or_default();
        drop_ins.sort();
        for path in std::iter::once(root.join("journald.conf")).chain(drop_ins) {
            let exists = path.exists();
            data.extend(path.to_string_lossy().as_bytes());
            data.extend(if exists { &b"present"[..] } else { b"absent" });
            if path.is_symlink() {
                data.extend(fs::read_link(&path).map_err(e)?.to_string_lossy().as_bytes());
            }
            if exists {
                data.extend(fs::read(&path).map_err(e)?);
            }
        }
    }
    Ok(sha256(&data))
}

/// A hit needs the same fingerprint and an age in `[0, ttl)`; `unknown` is never cached.
fn cached(cache: &mut Obj, key: &str, fingerprint: &str, now: f64, ttl: f64, probe: impl FnOnce() -> Value) -> Result<Value, String> {
    if let Some(previous) = cache.get(key) {
        let bad = || format!("invalid state cache entry {key}; preserve it and investigate");
        let (Some(fp), Some(at)) = (previous.get("fingerprint").and_then(Value::as_str), previous.get("at").and_then(Value::as_f64)) else { return Err(bad()) };
        if fp == fingerprint && (0.0..ttl).contains(&(now - at)) {
            let mut hit = previous.get("result").and_then(Value::as_object).ok_or_else(bad)?.clone();
            hit.insert("cached".into(), true.into());
            hit.insert("checked_at".into(), previous["at"].clone());
            return Ok(hit.into());
        }
    }
    let mut result = probe();
    if result["status"] != "unknown" {
        cache.insert(key.into(), json!({"fingerprint": fingerprint, "at": now, "result": result}));
    } else {
        cache.remove(key);
    }
    result["cached"] = false.into();
    result["checked_at"] = now.into();
    Ok(result)
}

/// A `warn` opens or continues an incident; a later `ok` verifies it.
fn incidents(state: &mut Obj, results: &Obj, now: f64) -> Result<(), String> {
    let records = state.entry("incidents").or_insert(json!({})).as_object_mut().ok_or("invalid state incidents")?;
    for (key, result) in results {
        let bad = || format!("invalid state incident {key}; preserve it and investigate");
        let previous = match records.get(key) {
            Some(p) => Some(p.get("state").and_then(Value::as_str).ok_or_else(bad)? != "verified"),
            None => None,
        };
        if result["status"] == "warn" {
            let mut record = match previous {
                Some(true) => records[key].as_object().cloned().ok_or_else(bad)?,
                _ => json!({"first_seen": now, "occurrences": 0, "state": "observed"}).as_object().cloned().unwrap_or_default(),
            };
            let n = record.get("occurrences").and_then(Value::as_u64).ok_or_else(bad)?;
            record.insert("last_seen".into(), now.into());
            record.insert("occurrences".into(), (n + 1).into());
            records.insert(key.clone(), record.into());
        } else if result["status"] == "ok" && previous == Some(true) {
            records[key]["state"] = "verified".into();
            records[key]["verified_at"] = now.into();
        }
    }
    Ok(())
}

/// Missing state starts fresh; another schema or unreadable JSON is an error, and the file stays as it is.
fn load_state(path: &Path) -> Result<Obj, String> {
    let bad = || format!("invalid state {}; preserve it and investigate", path.display());
    let text = match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({"schema": 1, "cache": {}, "incidents": {}}).as_object().cloned().unwrap_or_default()),
        r => r.map_err(|e| format!("{}: {e}", path.display()))?,
    };
    match serde_json::from_slice(&text) {
        Ok(Value::Object(s)) if s.get("schema") == Some(&json!(1)) && ["cache", "incidents"].iter().all(|k| s.get(*k).is_none_or(Value::is_object)) => Ok(s),
        _ => Err(bad()),
    }
}

/// Temp file, fsync, rename, directory fsync; mode 0600 in a 0700 folder.
fn save_json(path: &Path, value: &Value) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".serverq-{}-{}", std::process::id(), path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp")));
    let _ = fs::remove_file(&tmp);
    let r = (|| {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(format!("{}\n", pretty(value)).as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r.map_err(|e| format!("{}: {e}", path.display()))
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

fn sha256(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("serverq-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn journal(value: &str) -> String {
        ["SystemMaxUse", "SystemKeepFree", "RuntimeMaxUse"].iter().fold("[Journal]\n".to_string(), |s, k| s + &format!("{k}={value}\n"))
    }

    /// Fake docker: one canned output per call, recording the argv.
    fn docker(outputs: Vec<Result<&str, &str>>, calls: &mut Vec<Vec<String>>) -> Value {
        let mut outputs = outputs.into_iter();
        docker_logs(true, true, &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            outputs.next().expect("unexpected docker call").map(String::from).map_err(String::from)
        })
    }

    fn status(v: &Value) -> &str {
        v["status"].as_str().unwrap()
    }

    #[test]
    fn limits() {
        assert_eq!(status(&capacity(100, 5, 10.0)), "warn");
        assert_eq!(status(&capacity(100, 15, 10.0)), "ok");
        assert_eq!(status(&capacity(0, 0, 10.0)), "unknown");
        assert_eq!(capacity(100, 15, 10.0)["detail"], "15.0% available; minimum 10%");
    }

    #[test]
    fn effective_journal_policy() {
        assert_eq!(status(&journal_policy("[Journal]\nSystemMaxUse=256M\nSystemKeepFree=2G\nRuntimeMaxUse=64M\n")), "ok");
        assert_eq!(status(&journal_policy("[Journal]\nSystemMaxUse=256M\nSystemMaxUse=\nSystemKeepFree=2G\n")), "warn");
        assert_eq!(status(&journal_policy("[Journal]\nSystemMaxUse=0\nSystemKeepFree=2G\n")), "warn");
        assert_eq!(status(&journal_policy(&format!("[Other]\n{}", &journal("1G")[10..]))), "warn", "keys outside [Journal] do not count");
    }

    #[test]
    fn journal_size_syntax() {
        for v in ["1K", "1M", "1G", "1T", "1P", "1E", "1024"] {
            assert_eq!(status(&journal_policy(&journal(v))), "ok", "{v}");
        }
        for v in ["1KB", "1GB", "1EB", "0", "-1", "unlimited"] {
            assert_eq!(status(&journal_policy(&journal(v))), "warn", "{v}");
        }
    }

    #[test]
    fn cache_invalidation_and_expiry() {
        let mut cache = Obj::new();
        let mut calls = 0;
        for (fp, at) in [("a", 100.0), ("a", 105.0), ("b", 106.0), ("b", 117.0), ("b", 90.0)] {
            cached(&mut cache, "x", fp, at, 10.0, || {
                calls += 1;
                json!({"status": "ok"})
            })
            .unwrap();
        }
        assert_eq!(calls, 4);
        let hit = cached(&mut cache, "x", "b", 91.0, 10.0, || unreachable!()).unwrap();
        assert_eq!((&hit["cached"], &hit["checked_at"]), (&json!(true), &json!(90.0)));
        cache.insert("x".into(), json!({"broken": 1}));
        assert!(cached(&mut cache, "x", "b", 91.0, 10.0, || json!({"status": "ok"})).is_err(), "corrupt cache is an error");
    }

    #[test]
    fn unknown_not_cached() {
        let mut cache = Obj::new();
        cached(&mut cache, "x", "a", 100.0, 10.0, || json!({"status": "unknown"})).unwrap();
        assert!(!cache.contains_key("x"));
    }

    #[test]
    fn atomic_state_and_corruption() {
        let path = scratch("state").join("state.json");
        save_json(&path, &json!({"schema": 1})).unwrap();
        assert_eq!(load_state(&path).unwrap()["schema"], 1);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        for broken in ["broken", "[]", r#"{"schema": 2}"#, r#"{"schema": 1, "cache": []}"#] {
            fs::write(&path, broken).unwrap();
            assert!(load_state(&path).is_err(), "{broken}");
            assert_eq!(fs::read_to_string(&path).unwrap(), broken, "corrupt state is kept");
        }
    }

    #[test]
    fn command_error_not_healthy() {
        assert_eq!(status(&failed_units(command(&["serverq-no-such-command"]))), "unknown");
        assert_eq!(status(&failed_units(command(&["false"]))), "unknown");
        assert_eq!(command(&["echo", "x"]), Ok("x\n".into()));
        let found = failed_units(Ok("a.service loaded failed failed A\n\nb.service loaded failed failed B\n".into()));
        assert_eq!((status(&found), &found["units"]), ("warn", &json!(["a.service", "b.service"])));
        assert_eq!(status(&failed_units(Ok(String::new()))), "ok");
    }

    #[test]
    fn docker_rotation_checks_existing_containers_on_local_daemon() {
        let ids = ["a".repeat(64), "b".repeat(64)];
        let inspected = format!("{}\n{}", json!({"Type": "json-file", "Config": {"max-size": "10m", "max-file": "3"}}), json!({"Type": "json-file", "Config": {}}));
        let mut calls = vec![];
        let result = docker(vec![Ok(&ids.join("\n")), Ok(&inspected)], &mut calls);
        assert_eq!(status(&result), "warn");
        assert_eq!(result["missing_rotation"], json!([ids[1]]));
        assert_eq!(calls[0][..3], DOCKER);
        assert!(calls[0].contains(&"--all".into()));
        assert_eq!(calls[1][calls[1].len() - 2..], ids);
    }

    #[test]
    fn docker_rotation_bounds_and_supported_drivers() {
        for (driver, options, want) in [
            ("json-file", json!({"max-size": "10m", "max-file": "3"}), "ok"),
            ("local", json!({"max-size": "1.5MB", "max-file": "1"}), "ok"),
            ("local", json!({}), "warn"),
            ("json-file", json!({"max-size": "-1", "max-file": "3"}), "warn"),
            ("json-file", json!({"max-size": "0m", "max-file": "3"}), "warn"),
            ("json-file", json!({"max-size": "10m", "max-file": "0"}), "warn"),
            ("json-file", json!({"max-size": 10, "max-file": "3"}), "warn"),
            ("journald", json!({}), "ok"),
            ("none", json!({}), "ok"),
            ("fluentd", json!({}), "unknown"),
        ] {
            let line = json!({"Type": driver, "Config": options}).to_string();
            assert_eq!(status(&docker(vec![Ok(&"a".repeat(64)), Ok(&line)], &mut vec![])), want, "{driver} {options}");
        }
    }

    #[test]
    fn docker_collection_fails_closed() {
        let id = "a".repeat(64);
        let twice = format!("{id}\n{id}");
        for listed in [Err("denied"), Ok("invalid-id"), Ok(twice.as_str())] {
            assert_eq!(status(&docker(vec![listed], &mut vec![])), "unknown", "{listed:?}");
        }
        for inspected in ["", "{}", "null", "{broken", r#"{"Type": "json-file", "Config": null}"#] {
            assert_eq!(status(&docker(vec![Ok(&id), Ok(inspected)], &mut vec![])), "unknown", "{inspected}");
        }
        assert_eq!(status(&docker(vec![Ok("")], &mut vec![])), "ok", "no containers");
        assert_eq!(status(&docker_logs(false, false, &mut |_| unreachable!())), "ok");
        assert_eq!(status(&docker_logs(false, true, &mut |_| unreachable!())), "unknown");
    }

    #[test]
    fn incident_recovery_and_recurrence() {
        let mut state = Obj::new();
        let at = |s: &str| json!({"disk-headroom": {"status": s}}).as_object().cloned().unwrap();
        incidents(&mut state, &at("warn"), 100.0).unwrap();
        incidents(&mut state, &at("warn"), 101.0).unwrap();
        assert_eq!(state["incidents"]["disk-headroom"]["occurrences"], 2);
        incidents(&mut state, &at("unknown"), 102.0).unwrap();
        assert_eq!(state["incidents"]["disk-headroom"]["state"], "observed");
        incidents(&mut state, &at("ok"), 103.0).unwrap();
        assert_eq!(state["incidents"]["disk-headroom"]["state"], "verified");
        incidents(&mut state, &at("warn"), 104.0).unwrap();
        assert_eq!(state["incidents"]["disk-headroom"]["first_seen"], 104.0);
        state["incidents"]["disk-headroom"] = json!({"occurrences": 1});
        assert!(incidents(&mut state, &at("warn"), 105.0).is_err(), "corrupt incident is an error");
    }

    #[test]
    fn embedded_rules_drive_the_checks() {
        let rules: Obj = serde_json::from_slice(RULES).unwrap();
        for (k, field) in [("disk-headroom", "minimum_percent"), ("inode-headroom", "minimum_percent"), ("bounded-journal", "ttl_seconds")] {
            assert!(rules[k][field].is_number(), "{k}.{field}");
        }
    }
}
