//! Local date/time formatting and parsing for the file list's time filter.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

struct Zone {
    offset: UtcOffset,
    /// IANA name such as "Asia/Kuala_Lumpur", when it can be found.
    name: Option<String>,
    /// Where the zone came from: "system", "--tz" or "AXTAIL_TZ".
    source: &'static str,
}

static ZONE: OnceLock<Zone> = OnceLock::new();

/// Picks the time zone used for every date shown or typed: `--tz`, else
/// the `AXTAIL_TZ` variable, else this machine's zone. Call once at
/// startup, before any thread is spawned (the system lookup is only
/// reliable then).
pub fn init(tz: Option<&str>) -> Result<(), String> {
    let env = std::env::var("AXTAIL_TZ")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let zone = match (tz, env.as_deref()) {
        (Some(v), _) => Zone {
            offset: parse_offset(v)?,
            name: None,
            source: "--tz",
        },
        (None, Some(v)) => Zone {
            offset: parse_offset(v).map_err(|e| format!("AXTAIL_TZ: {e}"))?,
            name: None,
            source: "AXTAIL_TZ",
        },
        (None, None) => Zone {
            offset: UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC),
            name: system_zone_name(),
            source: "system",
        },
    };
    let _ = ZONE.set(zone);
    Ok(())
}

fn zone() -> &'static Zone {
    ZONE.get_or_init(|| Zone {
        offset: UtcOffset::UTC,
        name: None,
        source: "system",
    })
}

fn offset() -> UtcOffset {
    zone().offset
}

/// Parses "UTC", "local", "+08:00", "+8", "-05:30", "UTC+8", "GMT-3".
pub fn parse_offset(text: &str) -> Result<UtcOffset, String> {
    let bad = || format!("invalid time zone '{text}' (use UTC, local, or an offset like +08:00)");
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    if lower == "local" {
        return Ok(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
    }
    let rest = lower
        .strip_prefix("utc")
        .or_else(|| lower.strip_prefix("gmt"))
        .unwrap_or(&lower);
    if rest.is_empty() || rest == "z" {
        return Ok(UtcOffset::UTC);
    }
    let (sign, digits) = match rest.as_bytes()[0] {
        b'+' => (1i8, &rest[1..]),
        b'-' => (-1i8, &rest[1..]),
        _ => return Err(bad()),
    };
    let (h, m) = match digits.split_once(':') {
        Some((h, m)) => (h, m),
        None if digits.len() == 4 => digits.split_at(2),
        None => (digits, "0"),
    };
    let h: i8 = h.parse().map_err(|_| bad())?;
    let m: i8 = m.parse().map_err(|_| bad())?;
    if !(0..=14).contains(&h) || !(0..60).contains(&m) {
        return Err(bad());
    }
    UtcOffset::from_hms(sign * h, sign * m, 0).map_err(|_| bad())
}

/// Best-effort IANA zone name of this machine (Unix only).
fn system_zone_name() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':');
        if tz.contains('/') && !tz.starts_with('/') {
            return Some(tz.to_string());
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let target = target.to_string_lossy();
        if let Some((_, name)) = target.split_once("zoneinfo/") {
            return Some(name.to_string());
        }
    }
    std::fs::read_to_string("/etc/timezone")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// "UTC+08:00" style label for the zone in use.
pub fn offset_label() -> String {
    let o = offset();
    if o.is_utc() {
        return "UTC".into();
    }
    let (h, m, _) = o.as_hms();
    let sign = if o.is_negative() { '-' } else { '+' };
    format!("UTC{sign}{:02}:{:02}", h.unsigned_abs(), m.unsigned_abs())
}

/// "UTC+08:00 Asia/Kuala_Lumpur", or just the offset when no name is known.
pub fn zone_label() -> String {
    match &zone().name {
        Some(n) if n != "UTC" && n != "Etc/UTC" => format!("{} {n}", offset_label()),
        _ => offset_label(),
    }
}

/// Where the zone came from, for the help screen.
pub fn zone_source() -> &'static str {
    zone().source
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

/// Formats as local "YYYY-MM-DD HH:MM:SS".
pub fn format_secs(secs: i64) -> String {
    match OffsetDateTime::from_unix_timestamp(secs) {
        Ok(t) => {
            let t = t.to_offset(offset());
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                t.year(),
                t.month() as u8,
                t.day(),
                t.hour(),
                t.minute(),
                t.second()
            )
        }
        Err(_) => "-".into(),
    }
}

/// Seconds east of UTC of the zone in use.
pub fn offset_secs() -> i64 {
    offset().whole_seconds() as i64
}

/// The local year of a timestamp.
pub fn year(secs: i64) -> i32 {
    OffsetDateTime::from_unix_timestamp(secs).map_or(1970, |t| t.to_offset(offset()).year())
}

/// Unix seconds of a calendar date and time written in a zone `off` seconds
/// east of UTC. Fast path for parsing log timestamps; None when out of range.
pub fn civil_to_unix(y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64, off: i64) -> Option<i64> {
    if !(1970..=9999).contains(&y)
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&d)
        || !(0..24).contains(&h)
        || !(0..60).contains(&mi)
        || !(0..=60).contains(&s)
    {
        return None;
    }
    // Days since 1970-01-01 (proleptic Gregorian).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s - off)
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
    fn offsets() {
        let hm = |s: &str| parse_offset(s).map(|o| (o.whole_hours(), o.minutes_past_hour()));
        assert_eq!(hm("UTC"), Ok((0, 0)));
        assert_eq!(hm("z"), Ok((0, 0)));
        assert_eq!(hm("+08:00"), Ok((8, 0)));
        assert_eq!(hm("+8"), Ok((8, 0)));
        assert_eq!(hm("UTC+8"), Ok((8, 0)));
        assert_eq!(hm("-05:30"), Ok((-5, -30)));
        assert_eq!(hm("+0530"), Ok((5, 30)));
        for bad in ["8", "+15", "+8:60", "Asia/Kuala_Lumpur", "utc+x"] {
            assert!(parse_offset(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_to_unix(1970, 1, 1, 0, 0, 0, 0), Some(0));
        assert_eq!(
            civil_to_unix(2026, 11, 22, 13, 5, 9, 0),
            Some(1_795_352_709)
        );
        assert_eq!(
            civil_to_unix(2026, 11, 22, 13, 5, 9, 8 * 3600),
            Some(1_795_352_709 - 8 * 3600)
        );
        assert_eq!(civil_to_unix(2024, 2, 29, 0, 0, 0, 0), Some(1_709_164_800));
        assert_eq!(civil_to_unix(2026, 13, 1, 0, 0, 0, 0), None);
        assert_eq!(
            format_secs(parse("2026-11-22 13:05", false).unwrap() + 9),
            "2026-11-22 13:05:09"
        );
    }

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
