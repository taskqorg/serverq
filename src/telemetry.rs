//! Check observations and CoreQ's shipper (serverq.md § 9); never changes an audit result.
use coreq::{enroll::{Backoff, Delivery, Wire}, telemetry::{Event, Store}};
use serde_json::{Value, json};
use std::{path::Path, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

#[derive(Default)]
pub struct Checks {
    store: Option<Store>,
}

// CoreQ exposes cursor initialization through ship; recording must never send a backlog inline.
struct RetentionOnly;
impl Wire for RetentionOnly {
    fn post(&mut self, _: &str, _: &str, _: Option<&str>, _: &Value) -> Result<(u16, Value), String> { Err("retention-only".into()) }
    fn now(&self) -> u64 { 0 }
    fn sleep(&mut self, _: u64) {}
    fn show(&mut self, _: &str, _: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A3/§ 9: reopening a cursorless store must protect unsent rows without delivering during a check.
    #[test]
    fn cursorless_restart_preserves_unshipped_records() {
        let home = crate::tests::scratch("telemetry-cursorless-restart");
        let event = || Event::observation(json!({"product": "serverq", "version": crate::VERSION,
            "kind": "check", "operation": "disk-headroom", "started_ms": 0,
            "trace": null, "span": null, "unknown": {"trace": "no-caller", "span": "no-caller"},
            "outcome": "failed", "error": "collection-failed"})).unwrap();
        // A crash after opening the database can leave either an empty store or an unsent backlog.
        for backlog in [0, 1] {
            let path = home.join(format!("backlog-{backlog}"));
            std::fs::create_dir(&path).unwrap();
            let store = Store::open(&path.join("telemetry.db")).unwrap();
            if backlog > 0 { store.append(&[event()]).unwrap(); }
            assert_eq!(store.shipper().unwrap()["state"], "off");
            drop(store);
            // Invented approved grant: initializing retention must not deliver its backlog.
            let connection = path.join("connection");
            std::fs::create_dir(&connection).unwrap();
            std::fs::write(connection.join("connection.json"), json!({"active": {
                "destination": "https://dash.example", "credential": "invented", "generation": 1,
                "schemas": [coreq::telemetry::OBSERVATION_SCHEMA]}}).to_string()).unwrap();
            let mut checks = Checks::default();
            let store = checks.store(&path).unwrap();
            // Historical receipt clocks exercise age pruning deterministically, without sleeps or SQL edits.
            store.import_history("fixture", None, "old", &(0..5).map(|_| event()).collect::<Vec<_>>(), &[0; 5]).unwrap();
            store.prune().unwrap();
            assert_eq!(store.export(0, 0, 10).unwrap()["events"].as_array().unwrap().len(), backlog + 5,
                "restart must preserve every unshipped observation through age pruning");
            assert_eq!(store.storage().unwrap()["pruned_count"], 0);
            assert_eq!(store.shipper().unwrap()["cursor"], 0, "initialization never acknowledges delivery");
            drop(checks);
            let mut checks = Checks::default();
            checks.record(&path, "disk-headroom", (SystemTime::now(), Duration::ZERO), &json!({"status": "unknown", "reason": "collection-failed"}));
            assert_eq!(checks.store(&path).unwrap().shipper().unwrap()["backlog_rows"], backlog + 6);
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    /// § 9/R1: stored observations distinguish deviations, cached results and failed collection.
    #[test]
    fn outcomes_and_unknown_quantities_survive_storage() {
        let home = crate::tests::scratch("telemetry-outcomes");
        let mut checks = Checks::default();
        let cases = [
            json!({"status": "ok", "available": 10}),
            json!({"status": "warn", "reason": "below-minimum", "available": 1, "cached": true}),
            json!({"status": "unknown", "reason": "collection-failed", "detail": "private raw failure"}),
        ];
        for result in &cases {
            let start = start();
            checks.record(&home, "disk-headroom", (start.0, start.1.elapsed()), result);
        }
        let rows = checks.store(&home).unwrap().export(0, 0, 10).unwrap();
        let observations: Vec<_> = rows["events"].as_array().unwrap().iter().map(|r| &r["observation"]).collect();
        assert_eq!(observations.len(), 3);
        for (o, (outcome, error)) in observations.iter().zip([("ok", "none"), ("failed", "below-minimum"), ("failed", "collection-failed")]) {
            assert_eq!((o["outcome"].as_str(), o["error"].as_str()), (Some(outcome), Some(error)));
        }
        assert_eq!(observations[0]["disk_bytes"], 10);
        assert_eq!(observations[1]["source"], "cached");
        assert!(observations[2]["disk_bytes"].is_null());
        assert_eq!(observations[2]["unknown"]["disk_bytes"], "collection-failed");
        assert!(!rows.to_string().contains("private raw failure"));
        drop(checks);
        std::fs::remove_dir_all(home).unwrap();
    }
}

pub fn start() -> (SystemTime, Instant) {
    (SystemTime::now(), Instant::now())
}

impl Checks {
    fn store(&mut self, home: &Path) -> Result<&Store, String> {
        if self.store.is_none() {
            let path = home.join("telemetry.db");
            let store = Store::open(&path)?;
            if store.shipper()?["cursor"].is_null() {
                let initialized = store.ship(&home.join("connection"), &mut Backoff::default(), &mut RetentionOnly);
                if store.shipper()?["cursor"].is_null() {
                    return Err(format!("shipper initialization failed: {initialized:?}"));
                }
            }
            self.store = Some(store);
        }
        Ok(self.store.as_ref().unwrap())
    }

    pub fn record(&mut self, home: &Path, operation: &str, start: (SystemTime, Duration), result: &Value) {
        let duration = u64::try_from(start.1.as_micros()).ok();
        let at = start.0.duration_since(UNIX_EPOCH).ok().and_then(|d| u64::try_from(d.as_millis()).ok());
        let ok = result["status"] == "ok";
        let reason = result["reason"].as_str().filter(|s| crate::REASONS.contains(s)).unwrap_or("unknown-result");
        let host = crate::hostname();
        let host = (!host.is_empty() && host.len() <= 128 && host.bytes().all(|c| c.is_ascii_alphanumeric() || b"._:-".contains(&c))).then_some(host);
        let mut value = json!({"product": "serverq", "version": crate::VERSION, "kind": "check", "operation": operation,
            "started_ms": at, "duration_us": duration, "outcome": if ok { "ok" } else { "failed" }, "error": if ok { "none" } else { reason },
            "host": host, "runtime": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH), "runtime_version": crate::VERSION,
            "trace": null, "span": null, "source": if result["cached"] == true { "cached" } else { "fresh" },
            "disk_bytes": if operation == "disk-headroom" { result["available"].clone() } else { Value::Null },
            "unknown": {"trace": "no-caller", "span": "no-caller", "disk_bytes": if result["status"] == "unknown" { reason } else { "not-collected" },
                "units": if result["status"] == "unknown" { reason } else { "not-collected" }}});
        for (field, reason) in [("host", "host-unavailable"), ("started_ms", "clock-unavailable"), ("duration_us", "duration-overflow")] {
            if value[field].is_null() { value["unknown"][field] = reason.into(); }
        }
        if !value["disk_bytes"].is_null() { value["unknown"].as_object_mut().unwrap().remove("disk_bytes"); }
        let record = (|| { self.store(home)?.append(&[Event::observation(value)?])?; Ok::<_, String>(()) })();
        if let Err(e) = record {
            eprintln!("serverq: telemetry unknown: {e}");
        }
    }

    /// Same loop and enrollment as snapshots; its independent backoff never moves a check or update.
    pub fn ship(&mut self, home: &Path, backoff: &mut Backoff, wire: &mut dyn Wire) -> Option<u64> {
        let connection = home.join("connection");
        if !home.join("telemetry.db").exists() {
            return None;
        }
        match coreq::enroll::active(&connection) {
            Ok(Some(active)) => {
                if !active["schemas"].as_array().is_some_and(|schemas| schemas.iter().any(|s| s == coreq::telemetry::OBSERVATION_SCHEMA)) {
                    eprintln!("serverq: telemetry unknown: observation schema not approved; reenroll to enable delivery");
                    return None;
                }
            }
            Ok(None) => return None,
            Err(e) => { eprintln!("serverq: telemetry unknown: {e}"); return Some(backoff.next(None)); }
        }
        let result = self.store(home).map_err(Delivery::Stopped).and_then(|store| store.ship(&connection, backoff, wire));
        match result {
            Ok(done) => {
                if done.sent > 0 || done.rejected > 0 {
                    println!("serverq: telemetry sent {} rejected {}", done.sent, done.rejected);
                }
                done.more.then_some(2)
            }
            Err(Delivery::Retry(seconds)) => Some(seconds),
            Err(Delivery::Stopped(e)) => { eprintln!("serverq: telemetry unknown: {e}"); Some(backoff.next(None)) }
        }
    }
}
