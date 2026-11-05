//! Human time: specq caveman/rules/indicators.md § Time; machine epochs stay unchanged.

pub fn countdown(at: Option<u64>, now: u64) -> String {
    let Some(at) = at.filter(|at| *at <= 253402300799) else { return "? unavailable".into() };
    let seconds = at.saturating_sub(now);
    let relative = match seconds {
        0 => "now".into(),
        86400.. => format!("{}d", seconds.div_ceil(86400)),
        18000.. => format!("{}h", seconds.div_ceil(3600)),
        _ => {
            let minutes = seconds.div_ceil(60);
            format!("{}:{:02}", minutes / 60, minutes % 60)
        }
    };
    format!("{relative} ({})", exact(at))
}

pub fn age(at: Option<u64>, now: u64) -> String {
    let Some(at) = at.filter(|at| *at <= 253402300799) else { return "? unavailable".into() };
    let seconds = now.saturating_sub(at);
    let relative = match seconds {
        86400.. => format!("{}d", seconds / 86400),
        3600.. => format!("{}h", seconds / 3600),
        60.. => format!("{}m", seconds / 60),
        _ => "<1m".into(),
    };
    format!("{relative} ago ({})", exact(at))
}

fn exact(at: u64) -> String {
    // Gregorian civil_from_days (Howard Hinnant): portable UTC without an extra dependency or OS tool.
    let days = at / 86400 + 719468;
    let era = days / 146097;
    let day = days % 146097;
    let year = (day - day / 1460 + day / 36524 - day / 146096) / 365;
    let day = day - (365 * year + year / 4 - year / 100);
    let month = (5 * day + 2) / 153;
    let day = day - (153 * month + 2) / 5 + 1;
    let year = year + era * 400 + u64::from(month >= 10);
    let month = if month < 10 { month + 3 } else { month - 9 };
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", at / 3600 % 24, at / 60 % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A4: ages floor, countdowns ceil at unit boundaries; future/unknown never look like zero age.
    #[test]
    fn human_time_boundaries() {
        for (seconds, want) in [(0, "<1m"), (59, "<1m"), (60, "1m"), (3599, "59m"), (3600, "1h"), (86399, "23h"), (86400, "1d")] {
            assert_eq!(age(Some(0), seconds), format!("{want} ago (1970-01-01 00:00 UTC)"));
        }
        assert_eq!(age(Some(60), 0), "<1m ago (1970-01-01 00:01 UTC)");
        assert_eq!(age(None, 0), "? unavailable");
        assert_eq!(age(Some(u64::MAX), 0), "? unavailable");
        for (seconds, want) in [(0, "now"), (1, "0:01"), (60, "0:01"), (61, "0:02"), (17940, "4:59"), (17999, "5:00"), (18000, "5h"), (18001, "6h"), (86399, "24h"), (86400, "1d"), (86401, "2d")] {
            assert_eq!(countdown(Some(seconds), 0), format!("{want} ({})", exact(seconds)));
        }
        assert_eq!(countdown(None, 0), "? unavailable");
        assert_eq!(countdown(Some(0), 1), "now (1970-01-01 00:00 UTC)");
        for (at, want) in [(951782400, "2000-02-29 00:00 UTC"), (1793880000, "2026-11-05 12:00 UTC"), (4107542400, "2100-03-01 00:00 UTC"), (253402300799, "9999-12-31 23:59 UTC")] {
            assert_eq!(exact(at), want);
        }
    }
}
