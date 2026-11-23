//! Local date/time formatting and parsing for the file list's time filter.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

static OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Reads the local time zone offset. Call once at startup, before any
/// thread is spawned (the platform lookup is only reliable then).
pub fn init() {
    OFFSET.get_or_init(|| UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
}

fn offset() -> UtcOffset {
    *OFFSET.get_or_init(|| UtcOffset::UTC)
}

/// "UTC+08:00" style label for the local zone.
pub fn zone_label() -> String {
    let o = offset();
    if o.is_utc() {
        return "UTC".into();
    }
    let (h, m, _) = o.as_hms();
    let sign = if o.is_negative() { '-' } else { '+' };
    format!("UTC{sign}{:02}:{:02}", h.unsigned_abs(), m.unsigned_abs())
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Formats as local "YYYY-MM-DD HH:MM".
pub fn format(secs: i64) -> String {
    match OffsetDateTime::from_unix_timestamp(secs) {
        Ok(t) => {
            let t = t.to_offset(offset());
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                t.year(),
                t.month() as u8,
                t.day(),
                t.hour(),
                t.minute()
            )
        }
        Err(_) => "-".into(),
    }
}

/// Parses a local "YYYY-MM-DD[ HH[:MM[:SS]]]" (also with 'T').
/// For an upper bound (`end` = true) the result is the last second covered
/// by the given precision, so "2026-11-22" as "To" means the whole day.
pub fn parse(text: &str, end: bool) -> Option<i64> {
    let text = text.trim();
    let (date, time) = match text.split_once([' ', 'T']) {
        Some((d, t)) => (d, t.trim()),
        None => (text, ""),
    };
    let mut d = date.split('-');
    let y: i32 = d.next()?.parse().ok()?;
    let mo: u8 = d.next()?.parse().ok()?;
    let day: u8 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1970..=9999).contains(&y) {
        return None;
    }
    let date = Date::from_calendar_date(y, Month::try_from(mo).ok()?, day).ok()?;
    let parts: Vec<&str> = if time.is_empty() {
        Vec::new()
    } else {
        time.split(':').collect()
    };
    if parts.len() > 3 {
        return None;
    }
    let mut hms = [0u8; 3];
    for (i, p) in parts.iter().enumerate() {
        hms[i] = p.parse().ok()?;
    }
    let t = Time::from_hms(hms[0], hms[1], hms[2]).ok()?;
    let start = PrimitiveDateTime::new(date, t)
        .assume_offset(offset())
        .unix_timestamp();
    let span = match parts.len() {
        0 => 86_400,
        1 => 3_600,
        2 => 60,
        _ => 1,
    };
    Some(if end { start + span - 1 } else { start })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_precision() {
        let s = parse("2026-11-22 13:05", false).unwrap();
        assert_eq!(format(s), "2026-11-22 13:05");
        assert_eq!(
            parse("2026-11-22", true).unwrap() - parse("2026-11-22", false).unwrap(),
            86_399
        );
        assert_eq!(
            parse("2026-11-22 13", true).unwrap() - parse("2026-11-22 13", false).unwrap(),
            3_599
        );
        assert_eq!(parse("2026-11-22T13:05:09", false).unwrap(), s + 9);
        for bad in [
            "",
            "2026",
            "2026-13-01",
            "2026-11-31",
            "2026-11-22 25",
            "2026-11-22 1:2:3:4",
            "x-1-1",
        ] {
            assert!(parse(bad, false).is_none(), "{bad}");
        }
    }
}
