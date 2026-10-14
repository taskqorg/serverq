//! Built-in profiling (serverq.md § 9): always-on counters per named operation, an opt-in trace and a ranked report.
use crate::{Obj, now, pretty, save_json};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::cell::RefCell;
use std::time::Instant;

/// Operation, p95 budget in ms, failure-rate budget. Only these names are stored, which bounds the file.
pub const BUDGETS: [(&str, f64, f64); 13] = [
    ("cli.check", 5000.0, 0.01),
    ("cli.projection", 500.0, 0.01),
    ("cli.update", 60000.0, 0.05),
    ("cli.rules", 5000.0, 0.01),
    ("cli.rollback", 5000.0, 0.01),
    ("cli.service", 5000.0, 0.01),
    ("cli.other", 5000.0, 0.01),
    ("service.check", 5000.0, 0.01),
    ("service.update", 60000.0, 0.05),
    ("probe.statvfs", 100.0, 0.01),
    ("probe.systemctl", 2000.0, 0.01),
    ("probe.systemd-analyze", 2000.0, 0.01),
    ("probe.docker", 5000.0, 0.01),
];
const SAMPLES: usize = 200;
const TRACE_LINES: usize = 1000;

struct Span {
    op: String,
    at: f64,
    ms: f64,
    error: Option<&'static str>,
}

// per thread: the CLI and the service loop are one thread each, and parallel tests stay apart
thread_local! {
    static SPANS: RefCell<Vec<Span>> = const { RefCell::new(Vec::new()) };
}

/// Records one finished call that started at `start`.
pub fn record(op: &str, start: Instant, error: Option<&'static str>) {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    SPANS.with_borrow_mut(|s| s.push(Span { op: op.into(), at: now(), ms, error }));
}

/// A short stable code for an error message; the message itself is never stored.
pub fn class(e: &str) -> &'static str {
    if e.contains("timeout") {
        "timeout"
    } else if e.contains("collection failed: exit") || e.contains("collection failed: signal") {
        "exit-nonzero"
    } else if e.contains("NotFound") {
        "not-found"
    } else if e.contains("already running") {
        "lock-busy"
    } else if e.contains("invalid") {
        "invalid-state"
    } else {
        "error"
    }
}

/// Drops the samples kept so far (selftest runs in a temporary install).
pub fn discard() {
    SPANS.with_borrow_mut(Vec::clear);
}

/// Merges this process's samples into `<home>/profile.json`; a missing `<home>` records nothing.
pub fn flush(home: &Path) {
    let mut spans = SPANS.with_borrow_mut(std::mem::take);
    spans.retain(|s| BUDGETS.iter().any(|b| b.0 == s.op));
    if spans.is_empty() || !home.is_dir() {
        return;
    }
    let path = home.join("profile.json");
    // ponytail: read-merge-write without a lock; a concurrent writer can lose a sample, add a lock if that ever matters
    let mut ops = match fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        Some(v) if v["schema"] == 1 && v["ops"].is_object() => v["ops"].as_object().cloned().unwrap_or_default(),
        _ => Obj::new(),
    };
    for s in &spans {
        let entry = ops.entry(s.op.clone()).or_insert(Value::Null);
        if parse(entry).is_none() {
            *entry = json!({"count": 0, "failed": 0, "errors": {}, "first": s.at, "ms": []});
        }
        entry["count"] = (entry["count"].as_u64().unwrap_or(0) + 1).into();
        entry["last"] = s.at.into();
        if let Some(c) = s.error {
            entry["failed"] = (entry["failed"].as_u64().unwrap_or(0) + 1).into();
            entry["errors"][c] = (entry["errors"][c].as_u64().unwrap_or(0) + 1).into();
        }
        if let Some(ms) = entry["ms"].as_array_mut() {
            ms.push(((s.ms * 1000.0).round() / 1000.0).into());
            let extra = ms.len().saturating_sub(SAMPLES);
            ms.drain(..extra);
        }
    }
    let _ = save_json(&path, &json!({"schema": 1, "ops": ops}));
    if home.join("trace").exists() {
        let p = home.join("trace.jsonl");
        let old = fs::read_to_string(&p).unwrap_or_default();
        let new = spans.iter().map(|s| json!({"op": s.op, "at": s.at, "ms": s.ms, "error": s.error}).to_string());
        let lines: Vec<String> = old.lines().map(String::from).chain(new).collect();
        let keep = lines[lines.len().saturating_sub(TRACE_LINES)..].join("\n") + "\n";
        let _ = coreq::write_atomic(&p, &keep);
    }
}

/// A well-formed record: `(count, failed, durations)`.
fn parse(r: &Value) -> Option<(u64, u64, Vec<f64>)> {
    let ms: Option<Vec<f64>> = r["ms"].as_array()?.iter().map(Value::as_f64).collect();
    let (count, failed) = (r["count"].as_u64()?, r["failed"].as_u64()?);
    (r["errors"].is_object() && r["first"].is_number() && failed <= count).then_some((count, failed, ms?))
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Every named operation, worst first; missing or malformed data reads unknown, never zero.
pub fn report(home: &Path) -> Result<Value, String> {
    let path = home.join("profile.json");
    let ops = match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Obj::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
        Ok(b) => match serde_json::from_slice::<Value>(&b) {
            Ok(v) if v["schema"] == 1 && v["ops"].is_object() => v["ops"].as_object().cloned().unwrap_or_default(),
            _ => return Err(format!("invalid profile {}; the next merge replaces it", path.display())),
        },
    };
    let (mut rows, mut from, mut to) = (vec![], f64::INFINITY, 0f64);
    for (op, p95_budget, fail_budget) in BUDGETS.iter() {
        let base = json!({"op": op, "budget_p95_ms": p95_budget, "budget_failure_rate": fail_budget});
        let unknown = |reason: &str| {
            let mut r = base.clone();
            r["status"] = "unknown".into();
            r["reason"] = reason.into();
            (f64::NEG_INFINITY, 0.0, r)
        };
        let row = match ops.get(*op) {
            None => unknown("no-samples"),
            Some(r) => match parse(r) {
                None => unknown("corrupt-record"),
                Some((_, _, ms)) if ms.is_empty() => unknown("no-samples"),
                Some((count, failed, mut ms)) => {
                    ms.sort_by(f64::total_cmp);
                    let (p50, p95, max) = (percentile(&ms, 0.5), percentile(&ms, 0.95), ms[ms.len() - 1]);
                    let rate = failed as f64 / count as f64;
                    let (slow, failing) = (p95 / p95_budget, rate / fail_budget);
                    let instability = if p50 > 0.0 { p95 / p50 } else { 1.0 };
                    from = from.min(r["first"].as_f64().unwrap_or(from));
                    to = to.max(r["last"].as_f64().unwrap_or(to));
                    let status = if failing > 1.0 { "failing" } else if slow > 1.0 { "over-budget" } else { "ok" };
                    let mut row = base.clone();
                    for (k, v) in [("status", json!(status)), ("count", count.into()), ("samples", ms.len().into()), ("failure_rate", rate.into()), ("p50_ms", p50.into()), ("p95_ms", p95.into()), ("max_ms", max.into()), ("instability", instability.into()), ("errors", r["errors"].clone())] {
                        row[k] = v;
                    }
                    (slow.max(failing), instability, row)
                }
            },
        };
        rows.push(row);
    }
    // unknown rows rank after measured ones: they have no evidence to rank by
    rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.total_cmp(&a.1)));
    let window = if to > 0.0 { json!({"from": from, "to": to}) } else { Value::Null };
    Ok(json!({"schema": 1, "window": window, "traces": home.join("trace").exists(), "operations": rows.into_iter().map(|r| r.2).collect::<Vec<_>>()}))
}

pub fn print(home: &Path) -> Result<i32, String> {
    println!("{}", pretty(&report(home)?));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("serverq-profile-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn row<'a>(r: &'a Value, op: &str) -> &'a Value {
        r["operations"].as_array().unwrap().iter().find(|o| o["op"] == op).unwrap()
    }

    /// Chain: profile store to ranked report (serverq.md § 9). Risks: an empty store or a corrupt record reading as zero
    /// or healthy, a failing operation ranked below a healthy one, an unbounded sample list, traces leaking text.
    #[test]
    fn store_to_report() {
        let home = scratch("chain");
        let empty = report(&home).unwrap();
        assert!(empty["window"].is_null());
        assert!(empty["operations"].as_array().unwrap().iter().all(|o| o["status"] == "unknown" && o["reason"] == "no-samples"));

        fs::write(home.join("trace"), "").unwrap();
        let t = Instant::now();
        for _ in 0..SAMPLES + 5 {
            record("probe.statvfs", t, None);
        }
        record("probe.docker", t, Some(class("docker collection failed: exit 1")));
        record("not.named", t, None);
        flush(&home);
        let stored: Value = serde_json::from_slice(&fs::read(home.join("profile.json")).unwrap()).unwrap();
        assert_eq!(stored["ops"]["probe.statvfs"]["ms"].as_array().unwrap().len(), SAMPLES);
        assert!(stored["ops"].get("not.named").is_none());
        let trace = fs::read_to_string(home.join("trace.jsonl")).unwrap();
        assert_eq!(trace.lines().count(), SAMPLES + 6);
        assert!(trace.contains("\"exit-nonzero\"") && !trace.contains("collection failed"));

        let r = report(&home).unwrap();
        assert_eq!(r["traces"], true);
        assert_eq!(r["operations"][0]["op"], "probe.docker");
        assert_eq!(r["operations"][0]["status"], "failing");
        assert_eq!(row(&r, "probe.statvfs")["count"], SAMPLES + 5);
        assert_eq!(row(&r, "cli.check")["reason"], "no-samples");

        let mut bad = stored.clone();
        bad["ops"]["probe.statvfs"]["ms"] = json!(["x"]);
        fs::write(home.join("profile.json"), bad.to_string()).unwrap();
        assert_eq!(row(&report(&home).unwrap(), "probe.statvfs")["reason"], "corrupt-record");
        record("probe.statvfs", t, None);
        flush(&home);
        assert_eq!(row(&report(&home).unwrap(), "probe.statvfs")["count"], 1);

        fs::write(home.join("profile.json"), "broken").unwrap();
        assert!(report(&home).unwrap_err().contains("invalid profile"));
        record("probe.statvfs", t, None);
        flush(&home);
        assert_eq!(row(&report(&home).unwrap(), "probe.statvfs")["count"], 1);
        fs::remove_dir_all(&home).unwrap();
    }
}
