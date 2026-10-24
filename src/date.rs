// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Dates in UTC: output file names, RFC 3339, and HTTP dates.

use anyhow::{Context, Result, bail, ensure};

/// File name for data as of `timestamp` (seconds since the Unix epoch),
/// such as `osm-node-density.2026-10-19.tiff` for prefix
/// `osm-node-density`. The date is in UTC.
pub fn dated_name(prefix: &str, timestamp: i64) -> Result<String> {
    let (y, m, d) = civil_date(timestamp.div_euclid(86_400));
    ensure!(
        (1970..=9999).contains(&y),
        "implausible timestamp {timestamp}"
    );
    Ok(format!("{prefix}.{y:04}-{m:02}-{d:02}.tiff"))
}

/// Year, month and day of a day number counted from 1970-01-01, after
/// Howard Hinnant's `civil_from_days`.
pub fn civil_date(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Inverse of [`civil_date`], after Hinnant's `days_from_civil`.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// RFC 3339 form of `timestamp`, in UTC.
pub fn rfc3339(timestamp: i64) -> String {
    let (y, m, d) = civil_date(timestamp.div_euclid(86_400));
    let s = timestamp.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// Parses an HTTP date such as `Sat, 04 May 2024 08:42:14 GMT`, the
/// only format RFC 9110 lets servers send, into seconds since the epoch.
pub fn parse_http_date(s: &str) -> Result<i64> {
    let parse = || -> Option<i64> {
        let (_weekday, rest) = s.trim().split_once(", ")?;
        let f: Vec<&str> = rest.split(' ').collect();
        let [day, month, year, time, "GMT"] = f[..] else {
            return None;
        };
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let m = MONTHS.iter().position(|&n| n == month)? as u32 + 1;
        let d: u32 = day.parse().ok()?;
        let y: i64 = year.parse().ok()?;
        let t: Vec<i64> = time
            .split(':')
            .map(|v| v.parse().ok())
            .collect::<Option<_>>()?;
        let [hh, mm, ss] = t[..] else { return None };
        if !(1..=31).contains(&d) || !(0..24).contains(&hh) || !(0..60).contains(&mm) {
            return None;
        }
        if !(0..=60).contains(&ss) {
            return None;
        }
        Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss)
    };
    match parse() {
        Some(t) => Ok(t),
        None => bail!("not an HTTP date: {s:?}"),
    }
}

/// Modification time of a file, in whole seconds since the epoch.
pub fn mtime(path: &std::path::Path) -> Result<i64> {
    let t = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .with_context(|| format!("reading modification time of {}", path.display()))?;
    Ok(match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs_f64().ceil() as i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dated_names() {
        let name = |t| dated_name("osm-node-density", t).unwrap();
        assert_eq!(name(0), "osm-node-density.1970-01-01.tiff");
        assert_eq!(name(951_782_400), "osm-node-density.2000-02-29.tiff");
        assert_eq!(name(1_760_918_399), "osm-node-density.2025-10-19.tiff");
        assert_eq!(name(1_760_918_400), "osm-node-density.2025-10-20.tiff");
        assert!(dated_name("x", -1).is_err());
        assert!(dated_name("x", i64::MAX / 2).is_err());
    }

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_769_501_462), "2026-01-27T08:11:02Z");
    }

    #[test]
    fn civil_round_trip() {
        for days in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_date(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn http_dates() {
        let t = parse_http_date("Sat, 04 May 2024 08:42:14 GMT").unwrap();
        assert_eq!(rfc3339(t), "2024-05-04T08:42:14Z");
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT").unwrap(), 0);
        for bad in [
            "",
            "2024-05-04T08:42:14Z",
            "Sat, 04 May 2024 08:42:14 UTC",
            "Sat, 04 Mai 2024 08:42:14 GMT",
            "Sat, 04 May 2024 25:42:14 GMT",
            "Sat, 4 May 2024 08:42 GMT",
        ] {
            assert!(parse_http_date(bad).is_err(), "{bad}");
        }
    }
}
