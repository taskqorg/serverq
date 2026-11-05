//! Read-only inventory, explicit scan roots and typed wire metadata (serverq.md § 11).
use serde_json::{Value, json};
use cap_std::fs::{Dir, Metadata};
use cap_fs_ext::{DirExt, MetadataExt};
use std::{collections::{HashMap, HashSet}, fs, path::{Path, PathBuf}, time::{Duration, Instant}};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

const DEPTH: usize = 4;
const FOLDERS: usize = 100;
const ENTRIES: usize = 100_000;
const SECONDS: u64 = 15;
const WALK_DEPTH: usize = 128;

fn item(entity: &str, identity: impl Into<Value>) -> Value {
    let fields: &[&str] = match entity {
        "host" => &["status", "os", "os_version", "uptime_s", "load_1_milli", "load_5_milli", "load_15_milli", "cpu_count", "cpu_used_milli", "ram_total_bytes", "ram_used_bytes", "swap_total_bytes", "swap_used_bytes"],
        "mount" => &["status", "disk_total_bytes", "disk_used_bytes", "disk_free_bytes"],
        "folder" => &["status", "disk_used_bytes"],
        _ => &["status", "cpu_used_milli", "ram_used_bytes", "disk_used_bytes"],
    };
    let mut row = json!({"entity": entity, "identity": identity.into(), "unknown": {}});
    for field in fields { missing(&mut row, field, "not-collected"); }
    row
}

fn missing(row: &mut Value, field: &str, reason: &str) {
    row[field] = Value::Null;
    row["unknown"][field] = reason.into();
}

fn put(row: &mut Value, field: &str, value: impl Into<Value>) {
    row[field] = value.into();
    row["unknown"].as_object_mut().unwrap().remove(field);
}

fn code(text: &str) -> bool {
    !text.is_empty() && text.len() <= 128 && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

fn milli(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0 && value * 1000.0 < u64::MAX as f64).then(|| (value * 1000.0).round() as u64)
}

fn collection(reason: Option<&str>) -> Value {
    reason.map_or_else(|| json!({"status": "ok"}), |r| json!({"status": "unknown", "reason": r}))
}

/// No arguments, executable paths or environments: refresh only the measurements and cwd for attribution.
fn process_refresh() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing().without_tasks().with_cpu().with_memory().with_cwd(UpdateKind::Always)
}

pub fn collect(home: &Path, status: &str) -> Value {
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok();
    let trace = crate::sha256(format!("{}-{}", at.map_or(0, |d| d.as_nanos()), std::process::id()).as_bytes())[..32].to_string();
    let start = Instant::now();
    let mut system = System::new_with_specifics(RefreshKind::nothing()
        .with_cpu(CpuRefreshKind::nothing().with_cpu_usage()).with_memory(MemoryRefreshKind::everything()));
    // macOS sysinfo's process refresh reads KERN_PROCARGS2 even when cmd/environ are disabled.
    if !cfg!(target_os = "macos") { system.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh()); }
    let sampled_pids: HashMap<_, _> = system.processes().values().filter_map(|p| process_cpu_ns(p.pid().as_u32()).map(|cpu| (p.pid(), (p.start_time(), cpu, Instant::now())))).collect();
    let cpu_before = cpu_counters();
    let before = if cfg!(windows) { Err("not-required") } else { services() };
    let sample_start = Instant::now();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    system.refresh_cpu_usage();
    if !cfg!(target_os = "macos") { system.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh()); }
    let after = services();
    let elapsed = sample_start.elapsed().as_secs_f64();
    let usage: HashMap<_, _> = system.processes().values().filter_map(|p| {
        let (start, before, clock) = sampled_pids.get(&p.pid())?;
        if *start != p.start_time() { return None; }
        process_cpu_ns(p.pid().as_u32())?.checked_sub(*before).and_then(|n| milli(n as f64 / 1e9 / clock.elapsed().as_secs_f64())).map(|cpu| (p.pid(), cpu))
    }).collect();
    let mut host = item("host", "host");
    put(&mut host, "status", status);
    put(&mut host, "os", std::env::consts::OS);
    if let Some(version) = System::os_version() {
        let version = version.replace(" (", ".").trim_end_matches(')').to_string();
        if code(&version) { put(&mut host, "os_version", version); }
    }
    let uptime = System::uptime();
    if uptime > 0 { put(&mut host, "uptime_s", uptime); }
    if !system.cpus().is_empty() {
        put(&mut host, "cpu_count", system.cpus().len() as u64);
        if let Some(cpu) = cpu_before.zip(cpu_counters()).and_then(|(a, b)| {
            let total = b.0.checked_sub(a.0)?;
            if total == 0 { return None; }
            let idle = b.1.checked_sub(a.1)?;
            let used = total.checked_sub(idle)?;
            milli(used as f64 / total as f64 * system.cpus().len() as f64)
        }) {
            put(&mut host, "cpu_used_milli", cpu);
        } else { missing(&mut host, "cpu_used_milli", "collection-failed"); }
    }
    if system.total_memory() > 0 {
        put(&mut host, "ram_total_bytes", system.total_memory());
        put(&mut host, "ram_used_bytes", system.used_memory());
        put(&mut host, "swap_total_bytes", system.total_swap());
        put(&mut host, "swap_used_bytes", system.used_swap());
    }
    memory_validity(&mut host);
    if cfg!(windows) {
        for field in ["load_1_milli", "load_5_milli", "load_15_milli"] { missing(&mut host, field, "unsupported-os"); }
    } else {
        load(&mut host);
    }
    let mut entities = vec![host];
    let mut collections = json!({});
    match mounts() {
        Ok(mut rows) => { collections["mounts"] = collection(None); entities.append(&mut rows); }
        Err(reason) => collections["mounts"] = collection(Some(reason)),
    }
    match after {
        Ok(rows) => {
            collections["services"] = collection(None);
            let mut shared_pids = HashSet::new();
            let mut seen_pids = HashSet::new();
            for row in &rows {
                if let Some(pid) = row["pid"].as_u64().filter(|p| *p != 0) {
                    if !seen_pids.insert(pid) { shared_pids.insert(pid); }
                }
            }
            for source in rows {
                let mut row = item("service", source["identity"].clone());
                if source["status"].as_str().is_some_and(code) { put(&mut row, "status", source["status"].clone()); }
                if let Some(pid) = source["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) {
                    if shared_pids.contains(&(pid as u64)) {
                        for field in ["cpu_used_milli", "ram_used_bytes"] { missing(&mut row, field, "shared-process"); }
                    } else if cfg!(target_os = "macos") {
                        service_accounting(&mut row, &source, before.as_ref().ok(), elapsed);
                    } else {
                        workload(&mut row, &system, &usage, pid);
                    }
                } else {
                    service_accounting(&mut row, &source, before.as_ref().ok(), elapsed);
                }
                missing(&mut row, "disk_used_bytes", "not-attributable");
                entities.push(row);
            }
        }
        Err(reason) => collections["services"] = collection(Some(reason)),
    }
    match containers() {
        Ok(rows) => {
            collections["containers"] = collection(None);
            for source in rows {
                let mut row = item("container", source["identity"].as_str().unwrap_or("").trim_start_matches('/'));
                if source["status"].as_str().is_some_and(code) { put(&mut row, "status", source["status"].clone()); }
                if source["status"] == "exited" || source["status"] == "created" || source["status"] == "dead" {
                    put(&mut row, "cpu_used_milli", 0);
                    put(&mut row, "ram_used_bytes", 0);
                } else if let Some(pid) = source["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) {
                    workload(&mut row, &system, &usage, pid);
                }
                if let Some(n) = source["disk_used_bytes"].as_u64() { put(&mut row, "disk_used_bytes", n); }
                entities.push(row);
            }
        }
        Err(reason) => collections["containers"] = collection(Some(reason)),
    }
    let roots = roots(home);
    let scan_roots = roots.as_ref().cloned().unwrap_or_default();
    let scan = std::thread::spawn(move || scan(&scan_roots, ENTRIES, Duration::from_secs(SECONDS))).join();
    let mut scan = scan.unwrap_or_else(|_| json!({"status": "unknown", "reason": "worker-failed", "entities": []}));
    if let Err(reason) = &roots {
        scan["status"] = "unknown".into();
        scan["reason"] = (*reason).into();
    }
    let projects = scan["entities"].as_array_mut().unwrap();
    for row in projects.iter_mut().filter(|r| r["entity"] == "project_root") {
        let path = Path::new(row["identity"].as_str().unwrap());
        let mut cpu = Some(0u64);
        let mut ram = Some(0u64);
        let mut complete = !system.processes().is_empty();
        for process in system.processes().values() {
            match process.cwd() {
                Some(cwd) if inside(cwd, path) => {
                    cpu = cpu.zip(usage.get(&process.pid())).and_then(|(n, next)| n.checked_add(*next));
                    ram = ram.filter(|_| process.memory() > 0).and_then(|n| n.checked_add(process.memory()));
                }
                None => complete = false,
                _ => {}
            }
        }
        if complete {
            if let Some(cpu) = cpu { put(row, "cpu_used_milli", cpu); } else { missing(row, "cpu_used_milli", "no-baseline"); }
            if let Some(ram) = ram { put(row, "ram_used_bytes", ram); }
        } else {
            let reason = if cfg!(target_os = "macos") { "unsupported-os" } else { "attribution-incomplete" };
            missing(row, "cpu_used_milli", reason);
            missing(row, "ram_used_bytes", reason);
        }
    }
    entities.append(projects);
    scan.as_object_mut().unwrap().remove("entities");
    collections["projects"] = match roots { Ok(_) => collection(None), Err(reason) => collection(Some(reason)) };
    for (name, entity) in [("mounts", "mount"), ("services", "service"), ("containers", "container"), ("projects", "project_root")] {
        collections[name]["count"] = if collections[name]["status"] == "ok" { json!(entities.iter().filter(|r| r["entity"] == entity).count()) } else { Value::Null };
    }
    json!({"at_ms": at.and_then(|d| u64::try_from(d.as_millis()).ok()), "trace": trace,
        "duration_us": start.elapsed().as_micros() as u64, "collections": collections, "scan": scan, "entities": entities})
}

fn roots(home: &Path) -> Result<Vec<PathBuf>, &'static str> {
    let bytes = fs::read(home.join("inventory.json")).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "not-configured" } else { "config-unreadable" })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid-config")?;
    let roots = value.as_array().ok_or("invalid-config")?;
    let mut result = vec![];
    for value in roots {
        let path = PathBuf::from(value.as_str().ok_or("invalid-config")?);
        if !path.is_absolute() || path.components().any(|p| matches!(p, std::path::Component::ParentDir | std::path::Component::CurDir)) {
            return Err("invalid-config");
        }
        let path: PathBuf = path.components().collect();
        if !result.contains(&path) { result.push(path); }
    }
    Ok(result)
}

fn service_accounting(row: &mut Value, source: &Value, before: Option<&Vec<Value>>, elapsed: f64) {
    for field in ["cpu_used_milli", "ram_used_bytes"] { missing(row, field, "accounting-unavailable"); }
    if source["pid"] == 0 {
        put(row, "cpu_used_milli", 0);
        put(row, "ram_used_bytes", 0);
        return;
    }
    if let Some(ram) = source["ram"].as_u64().filter(|v| *v != u64::MAX) { put(row, "ram_used_bytes", ram); }
    if let (Some(new), Some(old)) = (source["cpu_ns"].as_u64().filter(|v| *v != u64::MAX),
        before.and_then(|rows| rows.iter().find(|r| r["identity"] == source["identity"] && r["pid"] == source["pid"])).and_then(|r| r["cpu_ns"].as_u64())) {
        if let Some(cpu) = new.checked_sub(old).and_then(|n| milli(n as f64 / 1e9 / elapsed)) { put(row, "cpu_used_milli", cpu); }
    }
}

fn workload(row: &mut Value, system: &System, usage: &HashMap<sysinfo::Pid, u64>, pid: u32) {
    if pid == 0 {
        put(row, "cpu_used_milli", 0);
        put(row, "ram_used_bytes", 0);
        return;
    }
    let mut pids = HashSet::from([sysinfo::Pid::from_u32(pid)]);
    loop {
        let before = pids.len();
        for process in system.processes().values() {
            if process.parent().is_some_and(|p| pids.contains(&p)) { pids.insert(process.pid()); }
        }
        if pids.len() == before { break; }
    }
    if system.process(sysinfo::Pid::from_u32(pid)).is_none() {
        missing(row, "cpu_used_milli", "process-unavailable");
        missing(row, "ram_used_bytes", "process-unavailable");
        return;
    }
    let mut cpu = Some(0u64);
    let mut ram = Some(0u64);
    for pid in pids {
        if let Some(process) = system.process(pid) {
            cpu = cpu.zip(usage.get(&pid)).and_then(|(n, next)| n.checked_add(*next));
            // Sysinfo exposes zero on denied memory reads; a live process's zero is not proof of no use.
            ram = ram.filter(|_| process.memory() > 0).and_then(|n| n.checked_add(process.memory()));
        }
    }
    if let Some(cpu) = cpu { put(row, "cpu_used_milli", cpu); } else { missing(row, "cpu_used_milli", "no-baseline"); }
    if let Some(ram) = ram { put(row, "ram_used_bytes", ram); }
}

fn services() -> Result<Vec<Value>, &'static str> {
    if cfg!(target_os = "linux") {
        let output = crate::run_probe(&["systemctl", "show", "--all", "--no-pager", "--property=Id,ActiveState,CPUUsageNSec,MemoryCurrent", "*.service"]).map_err(|_| "collection-failed")?;
        let mut rows = vec![];
        for block in output.split("\n\n") {
            let mut row = json!({});
            for line in block.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    match key {
                        "Id" => row["identity"] = value.into(),
                        "ActiveState" => row["status"] = value.into(),
                        "CPUUsageNSec" => row["cpu_ns"] = value.parse::<u64>().ok().into(),
                        "MemoryCurrent" => row["ram"] = value.parse::<u64>().ok().into(),
                        _ => {}
                    }
                }
            }
            if row["identity"].is_string() { rows.push(row); }
        }
        if rows.is_empty() { return Err("collection-failed"); }
        Ok(rows)
    } else if cfg!(windows) {
        windows_services()
    } else if cfg!(target_os = "macos") {
        let output = crate::run_probe(&["launchctl", "list"]).map_err(|_| "collection-failed")?;
        let mut rows = vec![];
        for line in output.lines().skip(1) {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() != 3 { return Err("collection-failed"); }
            let pid = if fields[0] == "-" { 0 } else { fields[0].parse::<u32>().map_err(|_| "collection-failed")? };
            let mut row = json!({"identity": fields[2], "status": if pid == 0 { "inactive" } else { "running" }, "pid": pid});
            macos_accounting(&mut row, pid);
            rows.push(row);
        }
        Ok(rows)
    } else { Err("unsupported-os") }
}

fn containers() -> Result<Vec<Value>, &'static str> {
    if !cfg!(target_os = "linux") { return Err("unsupported-os"); }
    match fs::metadata("/var/run/docker.sock") {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(io_reason(e)),
    }
    let ids = crate::run_probe(&["docker", "--host", "unix:///var/run/docker.sock", "container", "ls", "--all", "--quiet", "--no-trunc"]).map_err(|_| "collection-failed")?;
    let ids: Vec<_> = ids.split_whitespace().collect();
    if ids.iter().any(|id| id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit())) { return Err("collection-failed"); }
    let mut result = vec![];
    for batch in ids.chunks(64) {
        let mut argv = vec!["docker", "--host", "unix:///var/run/docker.sock", "container", "inspect", "--size", "--format",
            r#"{"identity":{{json .Name}},"status":{{json .State.Status}},"disk_used_bytes":{{json .SizeRw}},"pid":{{json .State.Pid}}}"#];
        argv.extend_from_slice(batch);
        let output = crate::run_probe(&argv).map_err(|_| "collection-failed")?;
        for line in output.lines() { result.push(serde_json::from_str(line).map_err(|_| "collection-failed")?); }
    }
    Ok(result)
}

#[cfg(unix)]
fn link(meta: &Metadata) -> bool { meta.file_type().is_symlink() }
#[cfg(windows)]
fn link(meta: &Metadata) -> bool {
    use cap_std::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}

#[cfg(unix)]
fn allocated(meta: &Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
    if seen.insert((meta.dev(), meta.ino())) { cap_std::fs::MetadataExt::blocks(meta).saturating_mul(512) } else { 0 }
}
#[cfg(windows)]
fn allocated(meta: &Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
    if meta.is_file() && seen.insert((meta.dev(), meta.ino())) { meta.len() } else { 0 }
}

#[cfg(unix)]
fn same_device(meta: &Metadata, root: &Metadata) -> bool {
    meta.dev() == root.dev()
}
#[cfg(windows)]
fn same_device(meta: &Metadata, root: &Metadata) -> bool { meta.dev() == root.dev() }

struct Walk {
    deadline: Instant,
    remaining: usize,
    seen: HashSet<(u64, u64)>,
    folders: Vec<Value>,
}

impl Walk {
    fn tree(&mut self, dir: &Dir, path: &Path, depth: usize, device: &Metadata) -> Result<u64, &'static str> {
        if depth > WALK_DEPTH { return Err("scan-depth-limit"); }
        if Instant::now() >= self.deadline { return Err("scan-time-limit"); }
        if depth == 0 {
            if self.remaining == 0 { return Err("scan-entry-limit"); }
            self.remaining -= 1;
        }
        let meta = dir.dir_metadata().map_err(io_reason)?;
        if link(&meta) || !same_device(&meta, device) { return Ok(0); }
        let mut size = allocated(&meta, &mut self.seen);
        if !meta.is_dir() { return Ok(size); }
        let mut error = None;
        for child in dir.entries().map_err(io_reason)? {
            let child = match child { Ok(c) => c, Err(e) => { error = Some(io_reason(e)); continue; } };
            if Instant::now() >= self.deadline { error = Some("scan-time-limit"); break; }
            if self.remaining == 0 { error = Some("scan-entry-limit"); break; }
            self.remaining -= 1;
            let name = child.file_name();
            let child_meta = match dir.symlink_metadata(&name) { Ok(m) => m, Err(e) => { error = Some(io_reason(e)); continue; } };
            if link(&child_meta) || !same_device(&child_meta, device) { continue; }
            let result = if child_meta.is_dir() {
                // Single-component no-follow opens keep races and ancestor links inside the explicit capability.
                dir.open_dir_nofollow(&name).map_err(io_reason).and_then(|child_dir| self.tree(&child_dir, &path.join(&name), depth + 1, device))
            } else {
                Ok(allocated(&child_meta, &mut self.seen))
            };
            match result {
                Ok(n) => size = size.checked_add(n).ok_or("size-overflow")?,
                Err(reason) => {
                    error = Some(reason);
                    if ["scan-time-limit", "scan-entry-limit"].contains(&reason) { break; }
                }
            }
        }
        if depth > 0 && depth <= DEPTH {
            let mut row = item("folder", path.to_string_lossy().into_owned());
            if let Some(reason) = error {
                missing(&mut row, "status", reason);
                missing(&mut row, "disk_used_bytes", reason);
            } else {
                put(&mut row, "status", "ok");
                put(&mut row, "disk_used_bytes", size);
            }
            self.folders.push(row);
            self.folders.sort_by(|a, b| b["disk_used_bytes"].as_u64().cmp(&a["disk_used_bytes"].as_u64()).then_with(|| a["identity"].as_str().cmp(&b["identity"].as_str())));
            self.folders.truncate(FOLDERS);
        }
        error.map_or(Ok(size), Err)
    }
}

fn io_reason(error: std::io::Error) -> &'static str {
    if error.kind() == std::io::ErrorKind::PermissionDenied { "access-denied" } else { "collection-failed" }
}

fn scan(roots: &[PathBuf], entries: usize, time: Duration) -> Value {
    let start = Instant::now();
    let cpu = cpu_us();
    let priority = background();
    let mut entities = vec![];
    let mut reason = priority.err();
    let mut walk = Walk { deadline: start + time, remaining: entries, seen: HashSet::new(), folders: vec![] };
    for root in roots {
        walk.seen.clear();
        let mut row = item("project_root", root.to_string_lossy().into_owned());
        let size = if priority.is_err() { Err("priority-unavailable") } else {
            open_root(root).and_then(|dir| { let meta = dir.dir_metadata().map_err(io_reason)?; walk.tree(&dir, root, 0, &meta) })
        };
        match size {
            Ok(n) => { put(&mut row, "status", "ok"); put(&mut row, "disk_used_bytes", n); }
            Err(r) => { missing(&mut row, "status", r); missing(&mut row, "disk_used_bytes", r); reason = Some(r); }
        }
        entities.push(row);
    }
    entities.append(&mut walk.folders);
    let duration = start.elapsed().as_micros() as u64;
    let cpu = cpu.zip(cpu_us()).and_then(|(a, b)| b.checked_sub(a));
    json!({"status": if reason.is_some() { "unknown" } else { "ok" }, "reason": reason,
        "priority": if priority.is_ok() { "background" } else { "unknown" }, "duration_us": duration, "cpu_us": cpu,
        "depth": DEPTH, "traversal_depth": WALK_DEPTH, "count": FOLDERS, "entry_limit": entries, "entries": entries - walk.remaining, "seconds": time.as_secs(), "entities": entities})
}

fn open_root(path: &Path) -> Result<Dir, &'static str> {
    let anchor = path.ancestors().last().ok_or("invalid-root")?;
    let mut dir = Dir::open_ambient_dir(anchor, cap_std::ambient_authority()).map_err(io_reason)?;
    for component in path.strip_prefix(anchor).map_err(|_| "invalid-root")?.components() {
        dir = dir.open_dir_nofollow(component).map_err(io_reason)?;
    }
    Ok(dir)
}

fn inside(path: &Path, root: &Path) -> bool {
    #[cfg(windows)] {
        // Compare components, so a root prefix never matches a sibling such as repo-other.
        let path: Vec<_> = path.components().map(|p| p.as_os_str().to_string_lossy().to_lowercase()).collect();
        let root: Vec<_> = root.components().map(|p| p.as_os_str().to_string_lossy().to_lowercase()).collect();
        path.starts_with(&root)
    }
    #[cfg(not(windows))] { path.starts_with(root) }
}

fn mount_row(path: String, capacity: Option<(u64, u64, u64)>) -> Value {
    let mut row = item("mount", path);
    if let Some((total, used, free)) = capacity.filter(|(total, used, free)| *total > 0 && used <= total && free <= total) {
        put(&mut row, "status", "ok");
        put(&mut row, "disk_total_bytes", total);
        put(&mut row, "disk_used_bytes", used);
        put(&mut row, "disk_free_bytes", free);
    } else {
        for field in ["status", "disk_total_bytes", "disk_used_bytes", "disk_free_bytes"] { missing(&mut row, field, "capacity-unavailable"); }
    }
    row
}

#[cfg(target_os = "linux")]
fn mounts() -> Result<Vec<Value>, &'static str> {
    let text = fs::read_to_string("/proc/self/mountinfo").map_err(io_reason)?;
    let mut rows = vec![];
    for line in text.lines() {
        let path = line.split_whitespace().nth(4).ok_or("collection-failed")?;
        // mountinfo escapes just whitespace and backslash; decode backslash last to avoid double decoding.
        let path = path.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\");
        let c = std::ffi::CString::new(path.as_str()).map_err(|_| "collection-failed")?;
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        let capacity = if unsafe { libc::statvfs(c.as_ptr(), &mut stats) } == 0 {
            let block = stats.f_frsize as u64;
            (stats.f_blocks as u64).checked_mul(block).zip((stats.f_bfree as u64).checked_mul(block))
                .and_then(|(total, free)| total.checked_sub(free).map(|used| (total, used)))
                .zip((stats.f_bavail as u64).checked_mul(block)).map(|((total, used), available)| (total, used, available))
        } else { None };
        rows.push(mount_row(path, capacity));
    }
    if rows.is_empty() { Err("collection-failed") } else { Ok(rows) }
}
#[cfg(target_os = "macos")]
fn mounts() -> Result<Vec<Value>, &'static str> {
    let mut buffer: *mut libc::statfs = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut buffer, libc::MNT_WAIT) };
    if count <= 0 || buffer.is_null() { return Err("collection-failed"); }
    let mut rows = vec![];
    for stat in unsafe { std::slice::from_raw_parts(buffer, count as usize) } {
        let path = unsafe { std::ffi::CStr::from_ptr(stat.f_mntonname.as_ptr()) }.to_string_lossy().into_owned();
        let block = stat.f_bsize as u64;
        let capacity = stat.f_blocks.checked_mul(block).zip(stat.f_bfree.checked_mul(block))
            .and_then(|(total, free)| total.checked_sub(free).map(|used| (total, used)))
            .zip(stat.f_bavail.checked_mul(block)).map(|((total, used), available)| (total, used, available));
        rows.push(mount_row(path, capacity));
    }
    Ok(rows)
}
#[cfg(windows)]
fn mounts() -> Result<Vec<Value>, &'static str> {
    let mut disks = sysinfo::Disks::new_with_refreshed_list_specifics(sysinfo::DiskRefreshKind::nothing());
    if disks.is_empty() { return Err("collection-failed"); }
    Ok(disks.list_mut().iter_mut().map(|disk| {
        let capacity = if disk.refresh_specifics(sysinfo::DiskRefreshKind::nothing().with_storage()) {
            disk.total_space().checked_sub(disk.available_space()).map(|used| (disk.total_space(), used, disk.available_space()))
        } else { None };
        mount_row(disk.mount_point().to_string_lossy().into_owned(), capacity)
    }).collect())
}

#[cfg(target_os = "linux")]
fn process_cpu_ns(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, counters) = stat.rsplit_once(") ")?;
    let counters: Vec<_> = counters.split_whitespace().collect();
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks <= 0 { return None; }
    let ticks_used = counters.get(11)?.parse::<u64>().ok()?.checked_add(counters.get(12)?.parse::<u64>().ok()?)?;
    u64::try_from(ticks_used as u128 * 1_000_000_000 / ticks as u128).ok()
}
#[cfg(windows)]
fn process_cpu_ns(pid: u32) -> Option<u64> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
        fn GetProcessTimes(process: *mut std::ffi::c_void, created: *mut u64, exited: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }
    unsafe {
        let process = OpenProcess(0x1000, 0, pid);
        if process.is_null() { return None; }
        let (mut created, mut exited, mut kernel, mut user) = (0, 0, 0, 0);
        let ok = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        CloseHandle(process);
        if ok == 0 { None } else { kernel.checked_add(user)?.checked_mul(100) }
    }
}
#[cfg(target_os = "macos")]
fn process_cpu_ns(_: u32) -> Option<u64> { None }

#[cfg(target_os = "linux")]
fn cpu_counters() -> Option<(u64, u64)> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let fields: Vec<u64> = text.lines().next()?.split_whitespace().skip(1).take(8).map(str::parse).collect::<Result<_, _>>().ok()?;
    if fields.len() < 4 { return None; }
    let total = fields.iter().try_fold(0u64, |a, b| a.checked_add(*b))?;
    Some((total, fields[3].checked_add(*fields.get(4).unwrap_or(&0))?))
}

#[cfg(target_os = "linux")]
fn memory_validity(host: &mut Value) {
    let mem = fs::read_to_string("/proc/meminfo").ok();
    let quantity = |name: &str| mem.as_ref()?.lines().find(|l| l.starts_with(name))?.split_whitespace().nth(1)?.parse::<u64>().ok()?.checked_mul(1024);
    for (total, used, a, b) in [("ram_total_bytes", "ram_used_bytes", "MemTotal:", "MemAvailable:"), ("swap_total_bytes", "swap_used_bytes", "SwapTotal:", "SwapFree:")] {
        if let Some((total_value, used_value)) = quantity(a).zip(quantity(b)).and_then(|(t, f)| t.checked_sub(f).map(|u| (t, u))) {
            put(host, total, total_value);
            put(host, used, used_value);
        } else {
            missing(host, total, "collection-failed");
            missing(host, used, "collection-failed");
        }
    }
}
#[cfg(windows)]
fn memory_validity(host: &mut Value) {
    #[repr(C)]
    struct Performance { cb: u32, commit_total: usize, commit_limit: usize, commit_peak: usize, physical_total: usize,
        physical_available: usize, system_cache: usize, kernel_total: usize, kernel_paged: usize, kernel_nonpaged: usize,
        page_size: usize, handles: u32, processes: u32, threads: u32 }
    #[link(name = "kernel32")]
    unsafe extern "system" { fn K32GetPerformanceInfo(info: *mut Performance, size: u32) -> i32; }
    let mut perf: Performance = unsafe { std::mem::zeroed() };
    perf.cb = std::mem::size_of::<Performance>() as u32;
    let quantities = if unsafe { K32GetPerformanceInfo(&mut perf, perf.cb) } != 0 {
        (perf.page_size as u64).checked_mul(perf.commit_limit.saturating_sub(perf.physical_total) as u64)
            .zip((perf.page_size as u64).checked_mul(perf.commit_total.saturating_sub(perf.physical_total) as u64))
    } else { None };
    if let Some((total, used)) = quantities {
        put(host, "swap_total_bytes", total);
        put(host, "swap_used_bytes", used);
    } else {
        missing(host, "swap_total_bytes", "collection-failed");
        missing(host, "swap_used_bytes", "collection-failed");
    }
}
#[cfg(target_os = "macos")]
fn memory_validity(host: &mut Value) {
    #[repr(C)]
    struct Swap { total: u64, available: u64, used: u64, page: u32, encrypted: i32 }
    let mut swap: Swap = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<Swap>();
    if unsafe { libc::sysctlbyname(c"vm.swapusage".as_ptr(), (&mut swap as *mut Swap).cast(), &mut size, std::ptr::null_mut(), 0) } == 0 && size == std::mem::size_of::<Swap>() {
        put(host, "swap_total_bytes", swap.total);
        put(host, "swap_used_bytes", swap.used);
    } else {
        missing(host, "swap_total_bytes", "collection-failed");
        missing(host, "swap_used_bytes", "collection-failed");
    }
    if host["ram_used_bytes"] == 0 { missing(host, "ram_used_bytes", "collection-failed"); }
}
#[cfg(windows)]
fn cpu_counters() -> Option<(u64, u64)> {
    #[link(name = "kernel32")]
    unsafe extern "system" { fn GetSystemTimes(idle: *mut u64, kernel: *mut u64, user: *mut u64) -> i32; }
    let (mut idle, mut kernel, mut user) = (0, 0, 0);
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 { None } else { Some((kernel.checked_add(user)?, idle)) }
}
#[cfg(target_os = "macos")]
fn cpu_counters() -> Option<(u64, u64)> {
    unsafe extern "C" {
        fn mach_host_self() -> u32;
        fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    }
    let mut ticks = [0u32; 4];
    let mut count = 4;
    if unsafe { host_statistics(mach_host_self(), 3, ticks.as_mut_ptr().cast(), &mut count) } != 0 || count != 4 { return None; }
    Some((ticks.iter().map(|n| *n as u64).sum(), ticks[2] as u64))
}

#[cfg(unix)]
fn load(host: &mut Value) {
    let mut values = [0.0; 3];
    let valid = unsafe { libc::getloadavg(values.as_mut_ptr(), 3) } == 3;
    for (field, n) in ["load_1_milli", "load_5_milli", "load_15_milli"].into_iter().zip(values) {
        if let Some(n) = valid.then(|| milli(n)).flatten() { put(host, field, n); } else { missing(host, field, "collection-failed"); }
    }
}
#[cfg(windows)]
fn load(_: &mut Value) {}

#[cfg(not(windows))]
fn windows_services() -> Result<Vec<Value>, &'static str> { Err("unsupported-os") }
#[cfg(windows)]
fn windows_services() -> Result<Vec<Value>, &'static str> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Service { name: *const u16, display: *const u16, state: [u32; 9] }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenSCManagerW(machine: *const u16, database: *const u16, access: u32) -> *mut c_void;
        fn EnumServicesStatusExW(manager: *mut c_void, level: u32, kind: u32, state: u32, buffer: *mut u8, size: u32,
            needed: *mut u32, returned: *mut u32, resume: *mut u32, group: *const u16) -> i32;
        fn CloseServiceHandle(handle: *mut c_void) -> i32;
    }
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), 4);
        if manager.is_null() { return Err("access-denied"); }
        // 256 KiB is the API maximum page; resume walks further pages without truncating the list.
        let mut buffer = vec![0u64; 256 * 1024 / 8];
        let (mut needed, mut count, mut resume) = (0, 0, 0);
        let mut rows = vec![];
        loop {
            let ok = EnumServicesStatusExW(manager, 0, 0x30, 3, buffer.as_mut_ptr().cast(), (buffer.len() * 8) as u32,
                &mut needed, &mut count, &mut resume, std::ptr::null());
            let error = std::io::Error::last_os_error().raw_os_error();
            if ok == 0 && error != Some(234) { CloseServiceHandle(manager); return Err("collection-failed"); }
            for service in std::slice::from_raw_parts(buffer.as_ptr().cast::<Service>(), count as usize) {
                let len = (0..256).take_while(|i| *service.name.add(*i) != 0).count();
                let name = String::from_utf16(std::slice::from_raw_parts(service.name, len)).map_err(|_| "collection-failed");
                let name = match name { Ok(name) => name, Err(e) => { CloseServiceHandle(manager); return Err(e); } };
                let status = match service.state[1] { 1 => "stopped", 2 => "start-pending", 3 => "stop-pending", 4 => "running", 5 => "continue-pending", 6 => "pause-pending", 7 => "paused", _ => "unknown" };
                rows.push(json!({"identity": name, "status": status, "pid": service.state[7]}));
            }
            if ok != 0 { break; }
        }
        CloseServiceHandle(manager);
        Ok(rows)
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_accounting(_: &mut Value, _: u32) {}
#[cfg(target_os = "macos")]
fn macos_accounting(row: &mut Value, pid: u32) {
    #[repr(C)]
    struct Task { virtual_bytes: u64, resident_bytes: u64, user_ns: u64, system_ns: u64, threads_user: u64, threads_system: u64, counters: [i32; 12] }
    #[link(name = "proc")]
    unsafe extern "C" { fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut std::ffi::c_void, size: i32) -> i32; }
    let mut task: Task = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<Task>() as i32;
    if pid != 0 && unsafe { proc_pidinfo(pid as i32, 4, 0, (&mut task as *mut Task).cast(), size) } == size {
        row["ram"] = task.resident_bytes.into();
        row["cpu_ns"] = task.user_ns.checked_add(task.system_ns).into();
    }
}

#[cfg(target_os = "linux")]
fn background() -> Result<(), &'static str> {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, 19) } == 0 { Ok(()) } else { Err("priority-unavailable") }
}
#[cfg(target_os = "macos")]
fn background() -> Result<(), &'static str> {
    unsafe extern "C" { fn pthread_set_qos_class_self_np(class: u32, priority: i32) -> i32; }
    if unsafe { pthread_set_qos_class_self_np(0x09, 0) } == 0 { Ok(()) } else { Err("priority-unavailable") }
}
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetThreadPriority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
    fn GetThreadTimes(thread: *mut std::ffi::c_void, created: *mut u64, exited: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
}
#[cfg(windows)]
fn background() -> Result<(), &'static str> {
    if unsafe { SetThreadPriority(-2isize as *mut _, 0x00010000) } != 0 { Ok(()) } else { Err("priority-unavailable") }
}
#[cfg(unix)]
fn cpu_us() -> Option<u64> {
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    (unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } == 0)
        .then(|| time.tv_sec as u64 * 1_000_000 + time.tv_nsec as u64 / 1000)
}
#[cfg(windows)]
fn cpu_us() -> Option<u64> {
    let (mut created, mut exited, mut kernel, mut user) = (0, 0, 0, 0);
    (unsafe { GetThreadTimes(-2isize as *mut _, &mut created, &mut exited, &mut kernel, &mut user) } != 0)
        .then(|| kernel.saturating_add(user) / 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A4/R1: metadata totals, top/depth bounds, links and exhausted scans never fabricate a complete total.
    #[test]
    fn inventory_scan_metadata_bounds_and_incomplete_totals() {
        let home = fs::canonicalize(crate::tests::scratch("inventory-walk")).unwrap();
        let root = home.join("root");
        fs::create_dir_all(root.join("one/two/three/four/five")).unwrap();
        fs::write(root.join("one/data"), vec![0; 16384]).unwrap();
        fs::hard_link(root.join("one/data"), root.join("one/alias")).unwrap();
        for n in 0..FOLDERS + 5 { fs::create_dir(root.join(format!("empty-{n}"))).unwrap(); }
        #[cfg(unix)] {
            std::os::unix::fs::symlink(&home, root.join("loop")).unwrap();
            assert!(open_root(&root.join("loop/root")).is_err(), "a symlink ancestor must never expand scope");
        }
        #[cfg(windows)] {
            assert!(inside(Path::new(r"c:\REPO\child"), Path::new(r"C:\repo")));
            assert!(!inside(Path::new(r"C:\repo-other"), Path::new(r"C:\repo")));
        }
        let roots = vec![root.clone()];
        let run = |entries, duration| { let roots = roots.clone(); std::thread::spawn(move || scan(&roots, entries, duration)).join().unwrap() };
        let good = run(ENTRIES, Duration::from_secs(SECONDS));
        assert_eq!(good["priority"], "background");
        assert_eq!(good["status"], "ok");
        let rows = good["entities"].as_array().unwrap();
        assert!(rows[0]["disk_used_bytes"].as_u64().unwrap() >= 16384);
        assert!(good["cpu_us"].is_u64());
        assert_eq!(rows.iter().filter(|r| r["entity"] == "folder").count(), FOLDERS);
        #[cfg(windows)] assert_eq!(rows[0]["disk_used_bytes"], 16384, "hard links count once");
        assert!(!rows.iter().any(|r| r["identity"].as_str().unwrap().ends_with("five")));
        for bad in [run(1, Duration::from_secs(SECONDS)), run(ENTRIES, Duration::ZERO)] {
            assert_eq!(bad["status"], "unknown");
            assert!(bad["entities"][0]["disk_used_bytes"].is_null());
            assert!(bad["entities"][0]["unknown"]["disk_used_bytes"].is_string());
        }
        fs::write(home.join("inventory.json"), b"broken").unwrap();
        assert_eq!(super::roots(&home), Err("invalid-config"));
        assert_eq!(fs::read(home.join("inventory.json")).unwrap(), b"broken");
        fs::remove_dir_all(home).unwrap();
    }
}
