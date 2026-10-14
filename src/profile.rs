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
/// Every error class `class` and the callers produce; a stored record with any other key is corrupt.
const ERRORS: [&str; 7] = ["timeout", "exit-nonzero", "not-found", "lock-busy", "invalid-state", "unknown-result", "error"];
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
    let old = match fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        Some(v) if v["schema"] == 1 && v["ops"].is_object() => v["ops"].as_object().cloned().unwrap_or_default(),
        _ => Obj::new(),
    };
    // rebuilt from valid records of named operations only: unknown names, fields and corrupt records are dropped
    let mut ops: Vec<(&str, Record)> = BUDGETS.iter().filter_map(|b| Some((b.0, old.get(b.0).and_then(parse)?))).collect();
    for s in &spans {
        let i = ops.iter().position(|o| o.0 == s.op).unwrap_or_else(|| {
            let name = BUDGETS.iter().find(|b| b.0 == s.op).map_or("", |b| b.0);
            ops.push((name, Record { count: 0, failed: 0, errors: Obj::new(), first: s.at, last: s.at, ms: vec![] }));
            ops.len() - 1
        });
        let r = &mut ops[i].1;
        // a saturated counter stays put; failed and error counts stay below it, so the record stays consistent
        if r.count < u64::MAX {
            r.count += 1;
            if let Some(c) = s.error {
                r.failed += 1;
                let n = r.errors.get(c).and_then(Value::as_u64).unwrap_or(0) + 1;
                r.errors.insert(c.into(), n.into());
            }
        }
        r.last = r.last.max(s.at);
        r.ms.push((s.ms.max(0.0) * 1000.0).round() / 1000.0);
        let extra = r.ms.len().saturating_sub(SAMPLES);
        r.ms.drain(..extra);
    }
    let ops: Obj = ops.into_iter().map(|(op, r)| (op.to_string(), json!({"count": r.count, "failed": r.failed, "errors": r.errors, "first": r.first, "last": r.last, "ms": r.ms}))).collect();
    let _ = save_json(&path, &json!({"schema": 1, "ops": ops}));
    if home.join("trace").exists() {
        let p = home.join("trace.jsonl");
        let old = fs::read_to_string(&p).unwrap_or_default();
        // old rows are rebuilt from known fields; a row with an unknown operation, error class or bad number is dropped
        let old = old.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|v| {
            let op = BUDGETS.iter().find(|b| v["op"] == b.0)?.0;
            let error = match &v["error"] {
                Value::Null => None,
                e => Some(*ERRORS.iter().find(|c| e == **c)?),
            };
            let (at, ms) = (v["at"].as_f64().filter(|x| x.is_finite() && *x >= 0.0)?, v["ms"].as_f64().filter(|x| x.is_finite() && *x >= 0.0)?);
            Some(Span { op: op.into(), at, ms, error })
        });
        let lines: Vec<String> = old.chain(spans).map(|s| json!({"op": s.op, "at": s.at, "ms": s.ms, "error": s.error}).to_string()).collect();
        let keep = lines[lines.len().saturating_sub(TRACE_LINES)..].join("\n") + "\n";
        let _ = coreq::write_atomic(&p, &keep);
    }
}

struct Record {
    count: u64,
    failed: u64,
    errors: Obj,
    first: f64,
    last: f64,
    ms: Vec<f64>,
}

/// A consistent record, or `None`: unknown error classes, error counts not summing to `failed`, more samples than calls
/// or than kept, and negative, non-finite or out-of-order values are all corrupt.
fn parse(r: &Value) -> Option<Record> {
    let ok = |x: f64| x.is_finite() && x >= 0.0;
    let ms: Vec<f64> = r["ms"].as_array()?.iter().map(Value::as_f64).collect::<Option<_>>()?;
    let errors = r["errors"].as_object()?.clone();
    let mut sum = 0u64;
    for (k, v) in &errors {
        sum = sum.checked_add(v.as_u64().filter(|n| *n > 0 && ERRORS.contains(&k.as_str()))?)?;
    }
    let rec = Record { count: r["count"].as_u64()?, failed: r["failed"].as_u64()?, errors, first: r["first"].as_f64()?, last: r["last"].as_f64()?, ms };
    let valid = rec.failed <= rec.count
        && sum == rec.failed
        && rec.ms.len() as u64 <= rec.count
        && rec.ms.len() <= SAMPLES
        && rec.ms.iter().all(|m| ok(*m))
        && ok(rec.first)
        && ok(rec.last)
        && rec.first <= rec.last;
    valid.then_some(rec)
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
                Some(rec) if rec.ms.is_empty() => unknown("no-samples"),
                Some(Record { count, failed, errors, first, last, mut ms }) => {
                    ms.sort_by(f64::total_cmp);
                    let (p50, p95, max) = (percentile(&ms, 0.5), percentile(&ms, 0.95), ms[ms.len() - 1]);
                    let rate = failed as f64 / count as f64;
                    let (slow, failing) = (p95 / p95_budget, rate / fail_budget);
                    let instability = if p50 > 0.0 { p95 / p50 } else { 1.0 };
                    (from, to) = (from.min(first), to.max(last));
                    let status = if failing > 1.0 { "failing" } else if slow > 1.0 { "over-budget" } else { "ok" };
                    let mut row = base.clone();
                    for (k, v) in [("status", json!(status)), ("count", count.into()), ("samples", ms.len().into()), ("failure_rate", rate.into()), ("p50_ms", p50.into()), ("p95_ms", p95.into()), ("max_ms", max.into()), ("instability", instability.into()), ("errors", errors.into())] {
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

        // inconsistent records read corrupt, and a merge keeps only named operations, known fields and error classes
        let good = json!({"count": 2, "failed": 1, "errors": {"timeout": 1}, "first": 1.0, "last": 2.0, "ms": [1.0, 2.0]});
        let mut cases = vec![];
        for (k, v) in [("count", json!(0)), ("ms", json!([-1.0])), ("errors", json!({"/secret/path": 1})), ("errors", json!({})), ("first", json!(3.0)), ("ms", json!(vec![1.0; SAMPLES + 1]))] {
            let mut r = good.clone();
            r[k] = v;
            if k == "ms" {
                r["count"] = (SAMPLES as u64 + 1).into();
            }
            cases.push(r);
        }
        for case in cases {
            let mut leaky = json!({"schema": 1, "ops": {"cli.rules": good, "probe.docker": case, "/home/u/x": good}});
            leaky["ops"]["cli.rules"]["note"] = "/home/u/x".into();
            fs::write(home.join("profile.json"), leaky.to_string()).unwrap();
            assert_eq!(row(&report(&home).unwrap(), "probe.docker")["reason"], "corrupt-record", "{case}");
            assert_eq!(row(&report(&home).unwrap(), "cli.rules")["status"], "failing");
            record("probe.statvfs", t, None);
            flush(&home);
            let kept = fs::read_to_string(home.join("profile.json")).unwrap();
            assert!(!kept.contains("/home/u/x") && !kept.contains("secret") && !kept.contains("probe.docker"), "{kept}");
            assert_eq!(row(&report(&home).unwrap(), "cli.rules")["count"], 2);
        }

        // a saturated counter neither panics nor wraps; foreign trace rows and fields are dropped on merge
        let mut max = good.clone();
        (max["count"], max["failed"], max["errors"]) = (u64::MAX.into(), u64::MAX.into(), json!({"timeout": u64::MAX}));
        fs::write(home.join("profile.json"), json!({"schema": 1, "ops": {"cli.rules": max}}).to_string()).unwrap();
        record("cli.rules", t, Some("timeout"));
        flush(&home);
        assert_eq!(row(&report(&home).unwrap(), "cli.rules")["count"], u64::MAX);
        let foreign = [r#"{"op":"cli.rules","at":1,"ms":1,"error":null,"note":"/home/u/x"}"#, r#"{"op":"/home/u/x","at":1,"ms":1,"error":null}"#, r#"{"op":"cli.rules","at":1,"ms":1,"error":"secret"}"#, r#"{"op":"cli.rules","at":-1,"ms":1,"error":null}"#, "/home/u/x"];
        fs::write(home.join("trace.jsonl"), foreign.join("\n")).unwrap();
        record("probe.statvfs", t, None);
        flush(&home);
        let trace = fs::read_to_string(home.join("trace.jsonl")).unwrap();
        assert_eq!(trace.lines().count(), 2, "{trace}");
        assert!(!trace.contains("/home/u/x") && !trace.contains("secret"), "{trace}");

        fs::write(home.join("profile.json"), "broken").unwrap();
        assert!(report(&home).unwrap_err().contains("invalid profile"));
        record("probe.statvfs", t, None);
        flush(&home);
        assert_eq!(row(&report(&home).unwrap(), "probe.statvfs")["count"], 1);
        fs::remove_dir_all(&home).unwrap();
    }
}
