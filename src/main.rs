//! Read-only Linux server audit (serverq.md § 3). Author: Tight <tight@kirs.online>.
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod profile;

const RULES: &[u8] = include_bytes!("../rules.json");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DOCKER: [&str; 3] = ["docker", "--host", "unix:///var/run/docker.sock"];
const USAGE: &str = "usage: serverq check [--force] [--filesystem PATH] | rules | projection | profile | update | rollback | selftest | service install|run|pause|resume|status|restart|stop|uninstall; every command takes --home PATH";
const UNIT: &str = "serverq.service";
const LEGACY_UNITS: [&str; 4] = ["serverq-check.timer", "serverq-update.timer", "serverq-check.service", "serverq-update.service"];
const LINUX: bool = cfg!(target_os = "linux");
const UNSUPPORTED: &str = "unsupported-os";
/// coreq's Windows unit for project `serverq`: `<title>-<project>`.
const TASK: &str = "ServerQ-serverq";
/// Fixed `reason` vocabulary of non-`ok` results; the projection exports nothing else (docs/design/enrollment.md).
const REASONS: [&str; 9] = [UNSUPPORTED, COLLECTION, "capacity-unavailable", "below-minimum", "failed-units", "missing-journal-bounds", "docker-cli-missing", "unsupported-log-driver", "missing-rotation"];

type Obj = Map<String, Value>;

/// Windows has no Unix modes: the install folder under `%LOCALAPPDATA%` keeps the user's ACL (serverq.md § 7).
#[cfg(windows)]
trait Mode {
    fn mode(&mut self, _: u32) -> &mut Self {
        self
    }
}
#[cfg(windows)]
impl Mode for fs::DirBuilder {}
#[cfg(windows)]
impl Mode for OpenOptions {}

fn private(p: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    return fs::set_permissions(p, fs::Permissions::from_mode(mode));
    #[cfg(windows)]
    {
        let _ = (p, mode);
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&args).unwrap_or_else(|e| {
        eprintln!("serverq {} failed; no healthy result claimed: {e}", args.first().map_or("", String::as_str));
        2
    });
    std::process::exit(code);
}

fn run(args: &[String]) -> Result<i32, String> {
    let mut t = coreq::Tool::new("serverq", "ServerQ", "taskqorg/serverq", VERSION);
    if let Some(i) = args.iter().position(|a| a == "--home") {
        let h = args.get(i + 1).ok_or(USAGE)?;
        t.install = std::path::absolute(h).map_err(|e| format!("{h}: {e}"))?;
    }
    // the launcher copy only picks the release to run, before parsing: a newer release may take new arguments (coreq.md § Launcher)
    let me = std::env::current_exe().and_then(fs::canonicalize).ok();
    if me.is_some() && me == fs::canonicalize(t.install.join(t.exe_name())).ok() {
        coreq::launcher::launch(&t, args);
    }
    let (mut words, mut filesystem, mut force) = (vec![], "/".to_string(), false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--home" => _ = it.next(),
            "--filesystem" => filesystem = it.next().ok_or(USAGE)?.clone(),
            "--force" => force = true,
            _ if a.starts_with("--") => return Err(format!("unknown argument {a}; {USAGE}")),
            _ => words.push(a.as_str()),
        }
    }
    let op = match words[..] {
        [w @ ("check" | "rules" | "projection" | "update" | "rollback")] => format!("cli.{w}"),
        ["selftest" | "profile"] | ["service", "run"] => String::new(),
        ["service", _] => "cli.service".into(),
        _ => "cli.other".into(),
    };
    let start = Instant::now();
    let result = dispatch(&t, &words, &filesystem, force);
    if op.is_empty() {
        profile::discard();
    } else {
        let error = match &result {
            Err(e) => Some(profile::class(e)),
            Ok(2) => Some("unknown-result"),
            _ => None,
        };
        profile::record(&op, start, error);
        profile::flush(&t.install);
    }
    result
}

fn dispatch(t: &coreq::Tool, words: &[&str], filesystem: &str, force: bool) -> Result<i32, String> {
    match words {
        ["check"] => {
            let (code, report) = check(&t.install, filesystem, force)?;
            println!("{}", pretty(&report));
            Ok(code)
        }
        ["rules"] => {
            std::io::stdout().write_all(RULES).map_err(|e| e.to_string())?;
            Ok(0)
        }
        ["projection"] => {
            let _lock = lock(&t.install)?;
            println!("{}", pretty(&projection(&t.install)?));
            Ok(0)
        }
        ["update"] => {
            match coreq::update::update(t, false)? {
                Some(v) => println!("serverq: updated to {v}"),
                None => println!("serverq: up to date"),
            }
            Ok(0)
        }
        ["rollback"] => rollback(t),
        ["selftest"] => selftest(),
        ["profile"] => profile::print(&t.install),
        ["service", cmd] => service(t, cmd),
        _ => Err(USAGE.into()),
    }
}

/// One locked audit; exit 0 all `ok`, 1 a `warn`, 2 any `unknown`.
fn check(home: &Path, filesystem: &str, force: bool) -> Result<(i32, Value), String> {
    let _lock = lock(home)?;
    let report = audit(home, filesystem, force)?;
    let statuses: Vec<&str> = report["results"].as_object().into_iter().flatten().map(|(_, r)| r["status"].as_str().unwrap_or("unknown")).collect();
    Ok((if statuses.contains(&"unknown") { 2 } else if statuses.contains(&"warn") { 1 } else { 0 }, report))
}

/// A check in a temporary install that names every automated rule; host health does not decide the outcome.
fn selftest() -> Result<i32, String> {
    let rules: Obj = serde_json::from_slice(RULES).map_err(|e| format!("rules.json: {e}"))?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let dir = std::env::temp_dir().join(format!("serverq-selftest-{}-{nanos}", std::process::id()));
    // a fresh 0700 folder: an existing path or a planted link fails instead of being written through
    fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let result = check(&dir, "/", true);
    let _ = fs::remove_dir_all(&dir);
    let (code, report) = result?;
    let missing: Vec<&String> = rules.iter().filter(|(k, r)| r["manual"] != true && report["results"].get(k.as_str()).is_none()).map(|(k, _)| k).collect();
    if !missing.is_empty() {
        return Err(format!("selftest: no result for {missing:?}"));
    }
    println!("serverq {VERSION} selftest passed; check exit {code}");
    Ok(0)
}

/// `previous` becomes `current`, updates pause and an installed service restarts on the restored release.
fn rollback(t: &coreq::Tool) -> Result<i32, String> {
    let read = |n: &str| fs::read_to_string(t.install.join(n)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let (Some(prev), Some(cur)) = (read("previous"), read("current")) else { return Err("no previous release to roll back to".into()) };
    if !t.release_exe(&prev).exists() {
        return Err(format!("previous release {prev} is missing"));
    }
    coreq::write_atomic(&t.install.join("previous"), &cur)?;
    coreq::write_atomic(&t.install.join("current"), &prev)?;
    fs::write(t.install.join("paused"), "").map_err(|e| e.to_string())?;
    println!("serverq: rolled back from {cur} to {prev}; updates paused until `serverq service resume`");
    if unit(t).path.exists() {
        restart(t)?;
    }
    Ok(0)
}

fn service(t: &coreq::Tool, cmd: &str) -> Result<i32, String> {
    let paused = t.install.join("paused");
    match cmd {
        "pause" => fs::write(&paused, "").map_err(|e| e.to_string())?,
        "resume" => match fs::remove_file(&paused) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.to_string()),
            _ => {}
        },
        "status" => return service_status(t),
        "run" => return serve(t),
        _ if !LINUX && !cfg!(windows) => return Err("the service is Linux and Windows only (serverq.md § 7)".into()),
        "install" if cfg!(windows) => return install_task(t),
        "install" => return install(t),
        "restart" => restart(t)?,
        "stop" => end(t)?,
        "uninstall" => {
            let u = unit(t);
            if cfg!(windows) {
                end(t)?;
            }
            coreq::service::run_all(&u.unload, true)?;
            // a task this user may not delete (registered elevated) must not read as uninstalled
            if cfg!(windows) && coreq::quiet(Command::new("schtasks").args(["/Query", "/TN", TASK]).stdout(Stdio::null()).stderr(Stdio::null())).status().is_ok_and(|s| s.success()) {
                return Err(format!("Scheduled Task {TASK} is still registered; delete it from the account that registered it"));
            }
            if let Err(e) = fs::remove_file(&u.path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(format!("{}: {e}", u.path.display()));
            }
            if LINUX {
                systemctl(&["daemon-reload"])?;
            }
        }
        _ => return Err(USAGE.into()),
    }
    println!("serverq: service {cmd} done");
    Ok(0)
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    coreq::service::run_all(&[[&["systemctl", "--user"][..], args].concat().iter().map(|s| s.to_string()).collect()], false)
}

fn restart(t: &coreq::Tool) -> Result<(), String> {
    if !cfg!(windows) {
        return systemctl(&["restart", UNIT]);
    }
    end(t)?;
    coreq::service::run_all(&unit(t).load[1..], false)
}

/// Stops the service until the next logon. Windows: `schtasks /End` ends only the launcher, so every other
/// `serverq.exe` of the install folder goes too, except this process and the launcher running it (coreq.md § Service units).
fn end(t: &coreq::Tool) -> Result<(), String> {
    if !cfg!(windows) {
        return systemctl(&["stop", UNIT]);
    }
    coreq::service::run_all(&unit(t).stop, true)?;
    // ponytail: PowerShell CIM, the one stock tool that lists processes with their paths; no double quotes, which the
    // Windows command line would mangle
    let script = format!(
        "$me = {pid}; $up = (Get-CimInstance Win32_Process -Filter ('ProcessId=' + $me)).ParentProcessId; \
         $mine = {{ Get-CimInstance Win32_Process -Filter 'Name=''serverq.exe''' | \
         Where-Object {{ $_.ProcessId -notin @($me, $up) -and ([string]$_.ExecutablePath).StartsWith('{dir}\\', 'OrdinalIgnoreCase') }} }}; \
         & $mine | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue; 'ended ' + $_.ProcessId + ' ' + $_.ExecutablePath }}; \
         if (& $mine) {{ 'serverq processes of the install folder survived'; exit 1 }}",
        pid = std::process::id(),
        dir = t.install.display().to_string().replace('\'', "''")
    );
    coreq::service::run_all(&[["powershell", "-NoProfile", "-NonInteractive", "-Command", &script].map(String::from).to_vec()], false)
}

/// Windows: coreq's Scheduled Task `TASK` at this user's logon, no admin. It runs `service run` without
/// arguments, so only the default install folder can be the service's.
fn install_task(t: &coreq::Tool) -> Result<i32, String> {
    if t.install != coreq::Tool::new("serverq", "ServerQ", "taskqorg/serverq", VERSION).install {
        return Err("the Scheduled Task uses the default install folder; drop --home".into());
    }
    // a running launcher or service locks its exe against the copy below
    end(t)?;
    coreq::update::install_self(t)?;
    println!("installed serverq {VERSION} as {}", t.install.join(t.exe_name()).display());
    let u = unit(t);
    coreq::service::write_unit(&u)?;
    coreq::service::run_all(&u.load, false)?;
    println!("started Scheduled Task from {}", u.path.display());
    let (code, _) = check(&t.install, "/", false)?;
    println!("check exit {code}, report {}", t.install.join("report.json").display());
    Ok(0)
}

/// The one user unit; on Linux it keeps the Python units' resource caps and sandbox.
fn unit(t: &coreq::Tool) -> coreq::service::Unit {
    if cfg!(windows) {
        return coreq::service::unit(t, &t.install, "serverq");
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let i = t.install.display();
    let path_env = std::env::var("PATH").unwrap_or_default();
    let text = format!(
        "[Unit]\nDescription=ServerQ read-only server audit and updates\n\n[Service]\nExecStart=\"{i}/serverq\" service run --home \"{i}\"\nRestart=always\nRestartSec=10\nUMask=0077\nNoNewPrivileges=yes\nPrivateTmp=yes\nProtectSystem=strict\nReadWritePaths={i}\nMemoryMax=128M\nTasksMax=32\nEnvironment=PATH={path_env}\n\n[Install]\nWantedBy=default.target\n"
    );
    let sv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    coreq::service::Unit {
        path: home.join(".config/systemd/user").join(UNIT),
        text,
        load: vec![sv(&["systemctl", "--user", "daemon-reload"]), sv(&["systemctl", "--user", "enable", "--now", UNIT])],
        unload: vec![sv(&["systemctl", "--user", "disable", "--now", UNIT])],
        stop: vec![sv(&["systemctl", "--user", "stop", UNIT])],
    }
}

/// serverq.md § 5 and § 6: migrate a legacy `~/.serverq` once, install this binary, start the unit, run one check.
fn install(t: &coreq::Tool) -> Result<i32, String> {
    let legacy = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join(".serverq");
    let real = fs::symlink_metadata(&legacy).is_ok_and(|m| m.is_dir());
    let (held, migrated) = if real && (legacy.join("bootstrap").exists() || legacy.join("app").exists()) { (Some(migrate(t, &legacy)?), true) } else { (None, false) };
    let rollback = format!(
        "rollback: serverq service uninstall; rm {l}; mv {i}/legacy {l}; systemctl --user enable --now serverq-check.timer serverq-update.timer",
        l = legacy.display(),
        i = t.install.display()
    );
    let after = |e: String| if migrated { format!("{e}; {rollback}") } else { e };
    coreq::update::install_self(t).map_err(after)?;
    println!("6. installed serverq {VERSION} as {}", t.install.join("serverq").display());
    let u = unit(t);
    coreq::service::write_unit(&u).and_then(|_| coreq::service::run_all(&u.load, false)).map_err(after)?;
    println!("6. started {}", u.path.display());
    drop(held);
    let (code, _) = check(&t.install, "/", false).map_err(after)?;
    println!("6. check exit {code}, report {}", t.install.join("report.json").display());
    if migrated {
        println!("{rollback}");
    }
    Ok(0)
}

/// Steps 1-5 of serverq.md § 6; returns the held legacy lock. A failure before step 5 restores the legacy timers.
fn migrate(t: &coreq::Tool, legacy: &Path) -> Result<File, String> {
    let held = OpenOptions::new().append(true).create(true).mode(0o600).open(legacy.join("lock")).map_err(|e| format!("legacy lock: {e}"))?;
    held.try_lock().map_err(|_| "the legacy serverq is running; retry after it ends")?;
    if legacy.join("transaction.json").exists() {
        return Err("legacy transaction.json present: run the legacy check first, which recovers it".into());
    }
    let state = legacy.join("state.json");
    if state.exists() {
        load_state(&state)?;
    }
    let report = legacy.join("report.json");
    if report.exists() {
        let r: Value = fs::read(&report).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if r["schema"] != 1 || !r["results"].is_object() {
            return Err(format!("invalid legacy report {}; preserve it and investigate", report.display()));
        }
    }
    println!("1-2. legacy lock taken; state.json and report.json valid");
    let restore = |e: String| {
        let back = systemctl(&["enable", "--now", LEGACY_UNITS[0], LEGACY_UNITS[1]]);
        format!("{e}; legacy timers re-enabled: {}", back.map_or_else(|b| b, |_| "yes".into()))
    };
    systemctl(&[&["disable", "--now"][..], &LEGACY_UNITS[..2]].concat())
        .and_then(|_| systemctl(&[&["stop"][..], &LEGACY_UNITS[2..]].concat()))
        .map_err(restore)?;
    println!("3. legacy units stopped; their files stay");
    let copy = || -> Result<(), String> {
        let _install = lock(&t.install)?;
        for f in [&state, &report].into_iter().filter(|f| f.exists()) {
            let v: Value = serde_json::from_slice(&fs::read(f).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            save_json(&t.install.join(f.file_name().unwrap_or_default()), &v)?;
        }
        Ok(())
    };
    copy().map_err(restore)?;
    println!("4. state.json and report.json copied into {}", t.install.display());
    let moved = t.install.join("legacy");
    fs::rename(legacy, &moved).map_err(|e| restore(format!("move {}: {e}", legacy.display())))?;
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&t.install, legacy);
    #[cfg(windows)]
    let linked: std::io::Result<()> = Err(std::io::ErrorKind::Unsupported.into());
    if let Err(e) = linked {
        let _ = fs::rename(&moved, legacy);
        return Err(restore(format!("link {}: {e}", legacy.display())));
    }
    println!("5. {} moved to {}; {} links to the install folder", legacy.display(), moved.display(), legacy.display());
    Ok(held)
}

fn every(var: &str, default: u64) -> Duration {
    Duration::from_secs(std::env::var(var).ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(default))
}

/// One loop: check 5 min after start then hourly, update 15 min after start then daily; never both at once.
fn serve(t: &coreq::Tool) -> Result<i32, String> {
    let (check_every, update_every) = (every("SERVERQ_CHECK_EVERY", 3600), every("SERVERQ_UPDATE_EVERY", 86400));
    let start = Instant::now();
    let (mut next_check, mut next_update) = (start + check_every.min(Duration::from_secs(300)), start + update_every.min(Duration::from_secs(900)));
    let host = fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| fs::read_to_string("/etc/hostname"))
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .map(|h| h.trim().to_string())
        .unwrap_or_default();
    let (mut check_status, mut update_at, mut check_error, mut update_error): (Value, Value, Option<String>, Option<String>) = (Value::Null, Value::Null, None, None);
    // the launcher runs `releases/<ver>/serverq`; a first completed check marks that release healthy
    let healthy = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("healthy"))).filter(|h| h.parent().and_then(Path::parent) == Some(&t.install.join("releases")));
    println!("serverq {VERSION}: service started");
    loop {
        std::thread::sleep(next_check.min(next_update).saturating_duration_since(Instant::now()));
        let paused = t.install.join("paused").exists();
        if Instant::now() >= next_check {
            next_check += check_every;
            let start = Instant::now();
            let result = check(&t.install, "/", false);
            let error = match &result {
                Err(e) => Some(profile::class(e)),
                Ok((2, _)) => Some("unknown-result"),
                _ => None,
            };
            profile::record("service.check", start, error);
            match result {
                Ok((code, report)) => {
                    let mut counts = Obj::new();
                    for (_, r) in report["results"].as_object().into_iter().flatten() {
                        let k = r["status"].as_str().unwrap_or("unknown");
                        let n = counts.get(k).and_then(Value::as_u64).unwrap_or(0) + 1;
                        counts.insert(k.into(), n.into());
                    }
                    check_status = json!({"at": now() as u64, "exit": code, "counts": counts, "report": t.install.join("report.json")});
                    println!("serverq: check exit {code} {}", check_status["counts"]);
                    if let Some(h) = &healthy {
                        let _ = fs::write(h, "");
                    }
                    check_error = None;
                }
                Err(e) => {
                    check_status = json!({"at": now() as u64, "exit": 2, "counts": {}, "report": Value::Null});
                    println!("serverq: check failed: {e}");
                    check_error = Some(format!("check: {e}"));
                }
            }
        } else {
            next_update += update_every;
            update_at = (now() as u64).into();
            if paused {
                println!("serverq: update paused");
            } else {
                let start = Instant::now();
                let result = coreq::update::update(t, false);
                profile::record("service.update", start, result.as_ref().err().map(|e| profile::class(e)));
                profile::flush(&t.install);
                match result {
                    Ok(Some(v)) => {
                        println!("serverq: updated to {v}; restarting");
                        return Ok(75);
                    }
                    Ok(None) => {
                        println!("serverq: up to date");
                        update_error = None;
                    }
                    Err(e) => {
                        println!("serverq: update failed: {e}");
                        update_error = Some(format!("update: {e}"));
                    }
                }
            }
        }
        profile::flush(&t.install);
        let error = [&check_error, &update_error].into_iter().flatten().cloned().collect::<Vec<_>>();
        let mut s = coreq::status::common(t, &host, "serverq", now() as u64, (!error.is_empty()).then(|| error.join("; ")), None);
        s["check"] = check_status.clone();
        s["update_at"] = update_at.clone();
        s["paused"] = t.install.join("paused").exists().into();
        coreq::write_atomic(&t.install.join("status.json"), &pretty(&s))?;
    }
}

/// `Check: ok|warn|unknown <age>` and `Update: <version|up to date|paused|unknown>`; unknown exits 2.
fn service_status(t: &coreq::Tool) -> Result<i32, String> {
    let max = 2 * every("SERVERQ_CHECK_EVERY", 3600).as_secs();
    let p = t.install.join("status.json");
    let s: Option<Value> = fs::read(&p).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let age = s.as_ref().and_then(|s| s["check"]["at"].as_u64()).map(|at| (now() as u64).saturating_sub(at));
    let check = match (&s, age) {
        (None, _) => format!("unknown ({} missing or unreadable)", p.display()),
        (_, None) => "unknown (no check yet)".into(),
        (_, Some(a)) if a > max => format!("unknown (last check {a}s ago, over {max}s)"),
        (Some(s), Some(a)) => format!("{} {a}s ago", ["ok", "warn"].get(s["check"]["exit"].as_u64().unwrap_or(2) as usize).unwrap_or(&"unknown")),
    };
    let update = match &s {
        _ if t.install.join("paused").exists() => "paused".to_string(),
        Some(s) if s["error"].as_str().is_some_and(|e| e.contains("update: ")) => format!("unknown ({})", s["error"].as_str().unwrap_or_default()),
        Some(s) if s["update_at"].is_u64() => format!("up to date ({})", t.current().unwrap_or(VERSION.into())),
        _ => "unknown (no update pass yet)".into(),
    };
    println!("Check: {check}\nUpdate: {update}");
    Ok(if check.starts_with("unknown") { 2 } else { 0 })
}

/// Exclusive `<home>/lock`; a busy lock is an error, never a skipped audit.
fn lock(home: &Path) -> Result<File, String> {
    let e = |e: std::io::Error| format!("{}: {e}", home.display());
    fs::DirBuilder::new().recursive(true).mode(0o700).create(home).map_err(e)?;
    private(home, 0o700).map_err(e)?;
    let p = home.join("lock");
    // write, not append: Windows locks only a handle with write access
    let f = OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&p).map_err(e)?;
    private(&p, 0o600).map_err(e)?;
    f.try_lock().map_err(|err| match err {
        fs::TryLockError::WouldBlock => "serverq is already running; check was not started".to_string(),
        fs::TryLockError::Error(err) => format!("{}: {err}", p.display()),
    })?;
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
    let start = Instant::now();
    let fs_usage = statvfs(filesystem);
    if fs_usage.as_ref().err().is_none_or(|e| e != UNSUPPORTED) {
        profile::record("probe.statvfs", start, fs_usage.as_ref().err().map(|e| profile::class(e)));
    }
    match fs_usage {
        Ok([total, free, files, favail]) => {
            results.insert("disk-headroom".into(), capacity(total, free, number("disk-headroom", "minimum_percent")?));
            results.insert("inode-headroom".into(), capacity(files, favail, number("inode-headroom", "minimum_percent")?));
        }
        Err(e) => {
            for k in ["disk-headroom", "inode-headroom"] {
                results.insert(k.into(), if e == UNSUPPORTED { unsupported() } else { unknown(COLLECTION, format!("Filesystem collection failed: {e}")) });
            }
        }
    }
    if !LINUX {
        for k in ["failed-services", "bounded-docker-logs", "bounded-journal"] {
            results.insert(k.into(), unsupported());
        }
    } else {
        linux_probes(&mut state, &mut results, now, &number)?;
    }
    incidents(&mut state, &results, now)?;
    let manual: Vec<&String> = rules.iter().filter(|(_, r)| r["manual"] == true).map(|(k, _)| k).collect();
    let report = json!({"schema": 1, "at": now, "revision": revision, "catalog": sha256(RULES), "filesystem": filesystem, "results": results,
        "incidents": state.get("incidents").cloned().unwrap_or(json!({})), "manual_rules": manual});
    state.insert("revision".into(), revision.into());
    state.insert("last_run".into(), now.into());
    save_json(&home.join("state.json"), &state.into())?;
    save_json(&home.join("report.json"), &report)?;
    Ok(report)
}

/// The allowlisted view of `report.json` for DashQ (docs/design/enrollment.md): rule IDs, enumerated values,
/// times and numbers only; detail, paths, unit names and container IDs never leave. It reads, never reruns, a check.
fn projection(home: &Path) -> Result<Value, String> {
    let p = home.join("report.json");
    let report: Value = serde_json::from_slice(&fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?).map_err(|_| format!("invalid report {}", p.display()))?;
    if report["schema"] != 1 || !report["results"].is_object() {
        return Err(format!("invalid report {}; preserve it and investigate", p.display()));
    }
    let id = |k: &&String| (1..=64).contains(&k.len()) && k.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'));
    let numbers = |v: &Value, fields: &[&str]| -> Obj { fields.iter().filter(|f| v[**f].is_number()).map(|f| (f.to_string(), v[*f].clone())).collect() };
    let at = &report["at"];
    let mut rules = Obj::new();
    for (k, r) in report["results"].as_object().into_iter().flatten().filter(|(k, _)| id(k)) {
        let mut evidence = numbers(r, &["available", "total"]);
        for f in ["units", "missing_rotation", "unsupported"] {
            if let Some(a) = r[f].as_array() {
                evidence.insert(f.into(), a.len().into());
            }
        }
        let status = r["status"].as_str().filter(|s| ["ok", "warn", "unknown"].contains(s)).unwrap_or("unknown");
        let observed = if r["checked_at"].is_number() { &r["checked_at"] } else { at };
        rules.insert(k.clone(), json!({"status": status, "observed_at": observed, "reason": r["reason"].as_str().filter(|s| REASONS.contains(s)), "evidence": evidence}));
    }
    for k in report["manual_rules"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).filter(|k| id(&k)) {
        rules.insert(k, json!({"status": "manual", "observed_at": null, "reason": null, "evidence": {}}));
    }
    let mut incidents = Obj::new();
    for (k, i) in report["incidents"].as_object().into_iter().flatten().filter(|(k, _)| id(k)) {
        if let Some(state) = i["state"].as_str().filter(|s| ["observed", "verified"].contains(s)) {
            let mut o = numbers(i, &["first_seen", "last_seen", "occurrences", "verified_at"]);
            o.insert("state".into(), state.into());
            incidents.insert(k.clone(), o.into());
        }
    }
    // a report before the `catalog` field has it only when its revision is this binary's
    let hex = |s: &&str| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let catalog = match report["catalog"].as_str().filter(hex) {
        Some(c) => Some(c.to_string()),
        None => (report["revision"].as_str() == Some(&sha256(&[RULES, VERSION.as_bytes()].concat()))).then(|| sha256(RULES)),
    };
    Ok(json!({"schema": 1, "product": "serverq", "installation": installation(home)?, "version": VERSION,
        "runtime": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH), "catalog": catalog,
        "observed_at": if at.is_number() { at.clone() } else { Value::Null }, "rules": rules, "incidents": incidents}))
}

/// Random 128-bit hex in `<install>/installation`: survives updates, never derives from the host; a lost file makes a new one.
fn installation(home: &Path) -> Result<String, String> {
    use ring::rand::SecureRandom;
    let p = home.join("installation");
    match fs::read(&p).map(|b| serde_json::from_slice::<String>(&b)) {
        Ok(Ok(s)) if s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut b = [0u8; 16];
            ring::rand::SystemRandom::new().fill(&mut b).map_err(|_| "no system randomness")?;
            let id: String = b.iter().map(|b| format!("{b:02x}")).collect();
            save_json(&p, &id.clone().into())?;
            Ok(id)
        }
        Err(e) => Err(format!("{}: {e}", p.display())),
        Ok(_) => Err(format!("invalid installation id {}; preserve it and investigate", p.display())),
    }
}

/// systemd and the system Docker socket exist only on Linux.
fn linux_probes(state: &mut Obj, results: &mut Obj, now: f64, number: &dyn Fn(&str, &str) -> Result<f64, String>) -> Result<(), String> {
    results.insert("failed-services".into(), failed_units(command(&["systemctl", "--failed", "--no-legend", "--plain", "--no-pager"])));
    results.insert("bounded-docker-logs".into(), docker_logs(which("docker"), Path::new("/var/run/docker.sock").exists(), &mut |a| command(a)));
    let journal = match journal_fingerprint() {
        Ok(fp) => {
            let cache = state.entry("cache").or_insert(json!({})).as_object_mut().ok_or("invalid state cache")?;
            let ttl = number("bounded-journal", "ttl_seconds")?;
            cached(cache, "bounded-journal", &fp, now, ttl, || match command(&["systemd-analyze", "cat-config", "systemd/journald.conf"]) {
                Ok(text) => journal_policy(&text),
                Err(e) => unknown(COLLECTION, e),
            })?
        }
        Err(e) => unknown(COLLECTION, format!("Journal config unavailable: {e}")),
    };
    results.insert("bounded-journal".into(), journal);
    Ok(())
}

const COLLECTION: &str = "collection-failed";

fn unknown(reason: &str, detail: String) -> Value {
    json!({"status": "unknown", "detail": detail, "reason": reason})
}

/// A Linux-only probe on another OS: unknown, never `ok` (serverq.md § 7).
fn unsupported() -> Value {
    json!({"status": "unknown", "detail": format!("{UNSUPPORTED}: Linux-only probe on {}", std::env::consts::OS), "reason": UNSUPPORTED})
}

fn capacity(total: u64, available: u64, minimum: f64) -> Value {
    if total == 0 {
        return unknown("capacity-unavailable", "Capacity unavailable".into());
    }
    let percent = available as f64 * 100.0 / total as f64;
    let mut r = json!({"status": "ok", "detail": format!("{percent:.1}% available; minimum {minimum}%"), "available": available, "total": total});
    if percent < minimum {
        (r["status"], r["reason"]) = ("warn".into(), "below-minimum".into());
    }
    r
}

/// Windows has no inodes; its disk probe is a later task (serverq.md § 7).
#[cfg(windows)]
fn statvfs(_: &str) -> Result<[u64; 4], String> {
    Err(UNSUPPORTED.into())
}

/// `[total bytes, free bytes, inodes, free inodes]` as `shutil.disk_usage` and `os.statvfs` report them.
#[cfg(unix)]
fn statvfs(path: &str) -> Result<[u64; 4], String> {
    let c = std::ffi::CString::new(path).map_err(|_| "InvalidPath")?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(format!("{:?}", std::io::Error::last_os_error().kind()));
    }
    let block = s.f_frsize as u64;
    Ok([s.f_blocks as u64 * block, s.f_bavail as u64 * block, s.f_files as u64, s.f_favail as u64])
}

/// One profiled probe call (`probe.<program>`, serverq.md § 9).
fn command(argv: &[&str]) -> Result<String, String> {
    let start = Instant::now();
    let r = run_probe(argv);
    profile::record(&format!("probe.{}", argv[0]), start, r.as_ref().err().map(|e| profile::class(e)));
    r
}

/// Runs without a shell, 15 s timeout; spawn failure, nonzero exit or timeout is an error.
fn run_probe(argv: &[&str]) -> Result<String, String> {
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

/// Only Linux probes look a program up.
#[cfg(unix)]
fn which(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| fs::metadata(d.join(name)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)))
}

#[cfg(windows)]
fn which(_: &str) -> bool {
    false
}

fn failed_units(output: Result<String, String>) -> Value {
    match output {
        Ok(text) => {
            let units: Vec<&str> = text.lines().filter_map(|l| l.split_whitespace().next()).collect();
            let mut r = json!({"status": "ok", "detail": format!("{} failed system units", units.len()), "units": units});
            if !units.is_empty() {
                (r["status"], r["reason"]) = ("warn".into(), "failed-units".into());
            }
            r
        }
        Err(e) => unknown(COLLECTION, e),
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
    let mut r = json!({"status": "ok", "detail": detail, "values": shown});
    if !missing.is_empty() {
        (r["status"], r["reason"]) = ("warn".into(), "missing-journal-bounds".into());
    }
    r
}

/// Every container of the local system daemon, regardless of the caller's Docker context.
fn docker_logs(cli: bool, socket: bool, run: &mut dyn FnMut(&[&str]) -> Result<String, String>) -> Value {
    match (cli, socket) {
        (false, true) => unknown("docker-cli-missing", "Docker CLI unavailable".into()),
        (false, false) => json!({"status": "ok", "detail": "System Docker not installed"}),
        _ => docker_inspect(run).unwrap_or_else(|e| unknown(COLLECTION, format!("Docker log collection failed: {e}"))),
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
    let (status, reason) = if !unsupported.is_empty() { ("unknown", "unsupported-log-driver") } else if !deviations.is_empty() { ("warn", "missing-rotation") } else { ("ok", "") };
    let mut r = json!({"status": status, "detail": format!("{} containers inspected; {} lack explicit positive rotation; {} unsupported log drivers", ids.len(), deviations.len(), unsupported.len()),
        "missing_rotation": deviations, "unsupported": unsupported});
    if !reason.is_empty() {
        r["reason"] = reason.into();
    }
    Ok(r)
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
    let r = (|| -> std::io::Result<()> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(format!("{}\n", pretty(value)).as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        // Windows cannot open a folder as a file; NTFS journals the rename
        if cfg!(unix) {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
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
        #[cfg(unix)]
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
        #[cfg(unix)]
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

    /// A schema-1 report from before `catalog` and `reason`, with private detail in every field DashQ must not see.
    #[test]
    fn projection_exports_only_the_allowlist() {
        let home = scratch("projection");
        let secret = ["/srv/private", "db.service", &"c".repeat(64), "256M"];
        let report = json!({"schema": 1, "at": 200.5, "revision": "old", "filesystem": secret[0], "manual_rules": ["service-contract", "../x"],
            "results": {
                "disk-headroom": {"status": "warn", "detail": secret[0], "available": 5, "total": 100, "reason": "below-minimum"},
                "failed-services": {"status": "warn", "detail": "1 failed", "units": [secret[1]]},
                "bounded-docker-logs": {"status": "unknown", "detail": secret[2], "missing_rotation": [], "unsupported": [secret[2]], "reason": "free text"},
                "bounded-journal": {"status": "ok", "detail": "x", "values": {"SystemMaxUse": secret[3]}, "cached": true, "checked_at": 150.0},
                "/etc/passwd": {"status": "ok"}},
            "incidents": {"disk-headroom": {"first_seen": 1.0, "last_seen": 2.0, "occurrences": 2, "state": "verified", "verified_at": 3.0, "note": secret[0]},
                "failed-services": {"state": "bogus"}}});
        fs::write(home.join("report.json"), report.to_string()).unwrap();
        let p = projection(&home).unwrap();
        let text = p.to_string();
        for s in secret.iter().chain(&["../x", "passwd", "free text", "note"]) {
            assert!(!text.contains(s), "{s} leaked: {text}");
        }
        assert_eq!(p["rules"]["disk-headroom"], json!({"status": "warn", "observed_at": 200.5, "reason": "below-minimum", "evidence": {"available": 5, "total": 100}}));
        assert_eq!(p["rules"]["failed-services"]["evidence"], json!({"units": 1}));
        assert_eq!((&p["rules"]["bounded-docker-logs"]["status"], &p["rules"]["bounded-docker-logs"]["reason"]), (&json!("unknown"), &Value::Null));
        assert_eq!(p["rules"]["bounded-journal"]["observed_at"], 150.0, "a cached result keeps its original observation time");
        assert_eq!(p["rules"]["service-contract"]["status"], "manual");
        assert_eq!(p["incidents"], json!({"disk-headroom": {"first_seen": 1.0, "last_seen": 2.0, "occurrences": 2, "verified_at": 3.0, "state": "verified"}}));
        assert_eq!(p["catalog"], Value::Null, "an old report of another revision has no known catalog");
        let id = p["installation"].as_str().unwrap().to_string();
        assert_eq!(projection(&home).unwrap()["installation"], id.as_str(), "stable identity");
        fs::remove_file(home.join("installation")).unwrap();
        assert_ne!(projection(&home).unwrap()["installation"], id.as_str(), "a lost identity is a new installation");
        fs::write(home.join("installation"), "host-name").unwrap();
        assert!(projection(&home).is_err(), "a corrupt identity is kept, never replaced");
    }

    #[test]
    fn every_non_ok_result_has_a_known_reason() {
        let ids = ["a".repeat(64)];
        for r in [capacity(100, 5, 10.0), capacity(0, 0, 10.0), failed_units(Ok("a.service x\n".into())), failed_units(Err("e".into())), journal_policy(""), unsupported(),
            docker_logs(false, true, &mut |_| unreachable!()), docker(vec![Ok(&ids[0]), Ok(r#"{"Type": "fluentd", "Config": {}}"#)], &mut vec![]),
            docker(vec![Ok(&ids[0]), Ok(r#"{"Type": "local", "Config": {}}"#)], &mut vec![])] {
            assert!(REASONS.contains(&r["reason"].as_str().unwrap()), "{r}");
        }
        assert!(capacity(100, 50, 10.0).get("reason").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_unit_is_the_task_restart_runs() {
        let u = unit(&coreq::Tool::new("serverq", "ServerQ", "taskqorg/serverq", VERSION));
        assert_eq!(u.load[1], ["schtasks", "/Run", "/TN", TASK]);
        assert_eq!(u.stop, [["schtasks", "/End", "/TN", TASK]]);
    }

    #[test]
    fn embedded_rules_drive_the_checks() {
        let rules: Obj = serde_json::from_slice(RULES).unwrap();
        for (k, field) in [("disk-headroom", "minimum_percent"), ("inode-headroom", "minimum_percent"), ("bounded-journal", "ttl_seconds")] {
            assert!(rules[k][field].is_number(), "{k}.{field}");
        }
    }
}
