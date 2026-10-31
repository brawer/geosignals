// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Condenses a timetable in CIF, the format of Network Rail's SCHEDULE
//! feed for Great Britain; see [`super::condensed`].
//!
//! CIF is a file of 80-character records. A train is a basic schedule
//! (`BS`), which says from when to when and on which weekdays it runs,
//! followed by its calling points: origin (`LO`), intermediate points
//! (`LI`) and terminus (`LT`). Several schedules may share a train UID.
//! On any one day the one with the strongest short-term planning (STP)
//! indicator applies: a cancellation (`C`) beats a new schedule (`N`),
//! which beats an overlay (`O`), which beats the permanent one (`P`).
//! That is their alphabetical order.
//!
//! We count a calling point as a departure if passengers may board
//! there: its activity is `TB` (train begins), `T` (stops), `U` (stops
//! to pick up) or `R` (request stop), and not `N` (not advertised).
//! Its minute is the public departure time, or the working time where
//! the public one is missing. Times are local, like GTFS times, and
//! count on past midnight within a train.
//!
//! Trains, buses and ships with passengers count; freight and empty
//! trains have no public calls. The bank holiday flag is ignored.
//!
//! CIF has no coordinates. Calling points are named by TIPLOC, which
//! we look up in NaPTAN, where a station's ATCO code is `9100` and its
//! TIPLOC. A TIPLOC that NaPTAN lacks takes the place of another TIPLOC
//! with the same CRS code (the three-letter station code), since big
//! stations have several. Departures at TIPLOCs without either are
//! dropped.

use super::condensed;
use super::gtfs::{self, MAX_SPAN_DAYS, NO_TIME};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{BufRead, Read};
use std::path::Path;

/// Version of the CIF condensing method. Bumping it makes every run
/// re-condense CIF feeds.
pub const VERSION: u8 = 2;

/// Railway stations from NaPTAN: zoom-24 position by TIPLOC.
#[derive(Debug, Default)]
pub struct Stations {
    by_tiploc: HashMap<String, (u32, u32)>,
}

impl Stations {
    /// Reads NaPTAN's CSV export, as `naptan.api.dft.gov.uk` serves it.
    /// Columns are found by name, since NaPTAN adds columns over time.
    pub fn from_naptan(csv: impl Read) -> Result<Stations> {
        let mut r = csv::ReaderBuilder::new().flexible(true).from_reader(csv);
        let headers = r.headers().context("reading NaPTAN header")?.clone();
        let col = |name: &str| {
            headers
                .iter()
                .position(|h| h.trim_start_matches('\u{feff}').eq_ignore_ascii_case(name))
                .with_context(|| format!("NaPTAN has no column {name}"))
        };
        let (atco, lon, lat) = (col("ATCOCode")?, col("Longitude")?, col("Latitude")?);
        let mut by_tiploc = HashMap::new();
        let mut rec = csv::StringRecord::new();
        while r.read_record(&mut rec).context("reading NaPTAN")? {
            let Some(tiploc) = rec.get(atco).and_then(|a| a.strip_prefix("9100")) else {
                continue;
            };
            let coords = rec
                .get(lon)
                .and_then(|v| v.parse::<f64>().ok())
                .zip(rec.get(lat).and_then(|v| v.parse::<f64>().ok()));
            if let Some((lon, lat)) = coords.filter(|&c| c != (0.0, 0.0))
                && !tiploc.is_empty()
                && let Some(p) = condensed::station_xy(lon, lat)
            {
                by_tiploc.insert(tiploc.to_string(), p);
            }
        }
        Ok(Stations { by_tiploc })
    }

    pub fn len(&self) -> usize {
        self.by_tiploc.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_tiploc.is_empty()
    }
}

/// Condenses the CIF file at `path`, gzipped or not.
pub fn condense_path(path: &Path, stations: &Stations) -> Result<condensed::Feed> {
    let mut f = std::io::BufReader::with_capacity(
        1 << 20,
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?,
    );
    if f.fill_buf()?.starts_with(&[0x1f, 0x8b]) {
        let gz = flate2::read::MultiGzDecoder::new(f);
        condense(std::io::BufReader::with_capacity(1 << 20, gz), stations)
    } else {
        condense(f, stations)
    }
}

/// A basic schedule, while condensing.
struct Schedule {
    uid: [u8; 6],
    /// `C`, `N`, `O` or `P`; lower wins.
    stp: u8,
    from: i64,
    to: i64,
    /// Weekdays it runs on; bit 0 is Monday.
    weekdays: u8,
    agency: u32,
    /// GTFS route type, or `None` if no passengers travel on it.
    route_type: Option<u16>,
    /// Range of its departures in `Parser::stops`.
    start: u32,
    len: u32,
}

impl Schedule {
    fn runs(&self, day: i64) -> bool {
        (self.from..=self.to).contains(&day) && self.weekdays & (1 << gtfs::weekday(day)) != 0
    }

    /// Whether, if it applies on a day, passengers board it.
    fn counts(&self) -> bool {
        self.stp != b'C' && self.route_type.is_some() && self.len > 0
    }
}

/// A departure, before its TIPLOC is placed.
#[derive(Clone, Copy)]
struct RawStop {
    tiploc: u32,
    minute: u16,
}

#[derive(Default)]
struct Parser {
    schedules: Vec<Schedule>,
    stops: Vec<RawStop>,
    tiplocs: Vec<String>,
    tiploc_index: HashMap<String, u32>,
    /// CRS code by TIPLOC, from the `TI` records.
    crs: HashMap<String, String>,
    agencies: Vec<String>,
    agency_index: HashMap<String, u32>,
    /// Whether the last `BS` record started a schedule we keep.
    current: bool,
    /// Minute of the previous departure of the current schedule, and
    /// what we add for the midnights it passed.
    last_minute: u16,
    day_offset: u16,
}

/// Field of a record, by 0-based byte offset; empty past the line's end.
fn field(line: &str, start: usize, len: usize) -> &str {
    let end = (start + len).min(line.len());
    line.get(start.min(end)..end).unwrap_or("")
}

/// Day number of a CIF date `YYMMDD`. `999999` means open-ended.
fn parse_date(s: &str) -> Option<i64> {
    if s == "999999" {
        return Some(crate::date::days_from_civil(2099, 12, 31));
    }
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s[r].parse::<u32>().ok();
    let (y, m, d) = (n(0..2)?, n(2..4)?, n(4..6)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(crate::date::days_from_civil(2000 + i64::from(y), m, d))
}

/// Minute of day of a time `HHMM`, ignoring a trailing `H` (half minute).
fn parse_time(s: &str) -> Option<u16> {
    let s = s.get(..4)?;
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = (s[..2].parse::<u16>().ok()?, s[2..].parse::<u16>().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Whether passengers may board, given a calling point's activities:
/// up to six two-character codes.
fn boards(activity: &str) -> bool {
    let codes: Vec<&str> = activity
        .as_bytes()
        .chunks(2)
        .filter_map(|c| std::str::from_utf8(c).ok())
        .collect();
    !codes.contains(&"N ")
        && codes
            .iter()
            .any(|c| matches!(*c, "TB" | "T " | "U " | "R " | "T" | "U" | "R"))
}

impl Parser {
    fn line(&mut self, line: &str) -> Result<()> {
        match field(line, 0, 2) {
            "TI" => {
                let tiploc = field(line, 2, 7).trim();
                let crs = field(line, 53, 3).trim();
                if !tiploc.is_empty() && crs.len() == 3 {
                    self.crs.insert(tiploc.to_string(), crs.to_string());
                }
            }
            "BS" => self.basic_schedule(line),
            "BX" => {
                if self.current {
                    let atoc = field(line, 11, 2).trim();
                    let n = self.agencies.len() as u32;
                    let a = *self.agency_index.entry(atoc.to_string()).or_insert(n);
                    if a == n {
                        self.agencies.push(atoc.to_string());
                    }
                    self.schedules.last_mut().expect("current").agency = a;
                }
            }
            // Origin: departure time at 10 (working) and 15 (public).
            "LO" => self.call(line, 10, 15, None, 29),
            // Intermediate: departure at 15 and 29; a pass time at 20
            // means the train runs through without stopping.
            "LI" => self.call(line, 15, 29, Some(20), 42),
            // Terminus: where the train ends, not a departure.
            "LT" => self.current = false,
            _ => {}
        }
        Ok(())
    }

    fn basic_schedule(&mut self, line: &str) {
        self.current = false;
        // Transaction type: N(ew) and R(evise) in update files; a full
        // extract has only N. Deletions have no schedule to read.
        if field(line, 2, 1) == "D" {
            return;
        }
        let (Some(from), Some(to)) = (
            parse_date(field(line, 9, 6)),
            parse_date(field(line, 15, 6)),
        ) else {
            return;
        };
        let mut weekdays = 0u8;
        for (i, c) in field(line, 21, 7).bytes().enumerate() {
            if c == b'1' {
                weekdays |= 1 << i;
            }
        }
        let stp = match field(line, 79, 1).as_bytes().first() {
            Some(&c @ (b'C' | b'N' | b'O' | b'P')) => c,
            _ => b'P',
        };
        let route_type = match field(line, 29, 1) {
            "P" | "1" => Some(2), // passenger train
            "B" | "5" => Some(3), // bus
            "S" | "4" => Some(4), // ship
            _ => None,            // freight, trips, empty stock
        };
        let mut uid = [b' '; 6];
        for (u, b) in uid.iter_mut().zip(field(line, 3, 6).bytes()) {
            *u = b;
        }
        self.schedules.push(Schedule {
            uid,
            stp,
            from,
            to,
            weekdays,
            agency: 0,
            route_type,
            start: self.stops.len() as u32,
            len: 0,
        });
        self.current = route_type.is_some() && stp != b'C';
        self.last_minute = 0;
        self.day_offset = 0;
    }

    fn call(
        &mut self,
        line: &str,
        working: usize,
        public: usize,
        pass: Option<usize>,
        activity: usize,
    ) {
        if !self.current {
            return;
        }
        if pass.is_some_and(|p| !field(line, p, 5).trim().is_empty()) {
            return;
        }
        if !boards(field(line, activity, 12)) {
            return;
        }
        let minute = match field(line, public, 4) {
            "0000" | "" => parse_time(field(line, working, 5)),
            t => parse_time(t).or_else(|| parse_time(field(line, working, 5))),
        };
        let Some(minute) = minute else {
            return;
        };
        let sched = self.schedules.last_mut().expect("current");
        if sched.len > 0 && minute + self.day_offset < self.last_minute {
            self.day_offset += 1440;
        }
        self.last_minute = minute + self.day_offset;
        let tiploc = field(line, 2, 7).trim();
        let n = self.tiplocs.len() as u32;
        let t = *self.tiploc_index.entry(tiploc.to_string()).or_insert(n);
        if t == n {
            self.tiplocs.push(tiploc.to_string());
        }
        self.stops.push(RawStop {
            tiploc: t,
            minute: self.last_minute.min(NO_TIME - 1),
        });
        sched.len += 1;
    }
}

/// Condenses a CIF timetable.
pub fn condense(input: impl BufRead, stations: &Stations) -> Result<condensed::Feed> {
    let mut p = Parser {
        agencies: vec![String::new()],
        agency_index: HashMap::from([(String::new(), 0)]),
        ..Default::default()
    };
    let mut input = input;
    let mut buf = Vec::with_capacity(128);
    let mut line_no = 0u64;
    loop {
        buf.clear();
        if input.read_until(b'\n', &mut buf).context("reading CIF")? == 0 {
            break;
        }
        line_no += 1;
        // CIF is ASCII; anything else is replaced, not fatal.
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        p.line(line)
            .with_context(|| format!("CIF line {line_no}"))?;
    }

    // Place each TIPLOC: by itself, or by another one with its CRS code.
    let mut by_crs: HashMap<&str, (u32, u32)> = HashMap::new();
    for (tiploc, crs) in &p.crs {
        if let Some(&px) = stations.by_tiploc.get(tiploc) {
            by_crs.entry(crs.as_str()).or_insert(px);
        }
    }
    let positions: Vec<Option<(u32, u32)>> = p
        .tiplocs
        .iter()
        .map(|t| {
            stations
                .by_tiploc
                .get(t)
                .or_else(|| p.crs.get(t).and_then(|c| by_crs.get(c.as_str())))
                .copied()
        })
        .collect();
    let unplaced: Vec<&str> = p
        .tiplocs
        .iter()
        .zip(&positions)
        .filter(|(_, px)| px.is_none())
        .map(|(t, _)| t.as_str())
        .collect();
    if !unplaced.is_empty() {
        let mut shown = unplaced.clone();
        shown.sort_unstable();
        shown.truncate(20);
        eprintln!(
            "CIF: {} of {} TIPLOCs with departures are not in NaPTAN, such as {}",
            unplaced.len(),
            p.tiplocs.len(),
            shown.join(" ")
        );
    }

    // Schedules of one train, strongest STP indicator first.
    let mut order: Vec<u32> = (0..p.schedules.len() as u32).collect();
    order.sort_by_key(|&i| {
        let s = &p.schedules[i as usize];
        (s.uid, s.stp)
    });
    let groups: Vec<&[u32]> = order
        .chunk_by(|&a, &b| p.schedules[a as usize].uid == p.schedules[b as usize].uid)
        .filter(|g| g.iter().any(|&i| p.schedules[i as usize].counts()))
        .collect();
    let winner = |group: &[u32], day: i64| -> Option<u32> {
        group
            .iter()
            .copied()
            .find(|&i| p.schedules[i as usize].runs(day))
            .filter(|&i| p.schedules[i as usize].counts())
    };

    let counting = || p.schedules.iter().filter(|s| s.counts());
    let (Some(first_day), Some(last_day)) = (
        counting().map(|s| s.from).min(),
        counting().map(|s| s.to).max(),
    ) else {
        return Ok(condensed::Feed {
            condenser: u32::from(VERSION),
            agencies: p.agencies,
            ..Default::default()
        });
    };
    let days = (last_day.min(first_day + MAX_SPAN_DAYS - 1) - first_day + 1) as u32;

    // A station for each placed TIPLOC.
    let mut out = condensed::Feed {
        condenser: u32::from(VERSION),
        first_day,
        days,
        last_day,
        agencies: p.agencies.clone(),
        ..Default::default()
    };
    let station: Vec<Option<u32>> = p
        .tiplocs
        .iter()
        .zip(&positions)
        .map(|(t, pos)| {
            pos.map(|(x, y)| {
                out.stations.push(condensed::Station {
                    x,
                    y,
                    id: t.clone(),
                });
                out.stations.len() as u32 - 1
            })
        })
        .collect();

    // On each day, the schedule that wins in each group runs; that
    // gives each schedule its service.
    let mut services: HashMap<u32, condensed::Service> = HashMap::new();
    for group in &groups {
        let lo = group
            .iter()
            .map(|&i| p.schedules[i as usize].from)
            .min()
            .unwrap_or(first_day);
        let up = group
            .iter()
            .map(|&i| p.schedules[i as usize].to)
            .max()
            .unwrap_or(last_day);
        for day in lo.max(first_day)..=up.min(first_day + i64::from(days) - 1) {
            if let Some(w) = winner(group, day) {
                services
                    .entry(w)
                    .or_insert_with(|| condensed::Service::new(days))
                    .set((day - first_day) as u32);
            }
        }
    }
    let mut won: Vec<(u32, condensed::Service)> = services.into_iter().collect();
    won.sort_unstable_by_key(|(w, _)| *w);
    for (w, service) in won {
        let s = &p.schedules[w as usize];
        let deps: Vec<(u32, u16)> = p.stops[s.start as usize..(s.start + s.len) as usize]
            .iter()
            .filter_map(|r| Some((station[r.tiploc as usize]?, r.minute)))
            .collect();
        let Some(start) = deps.iter().map(|&(_, m)| m).min() else {
            continue;
        };
        out.services.push(service);
        out.patterns.push(condensed::Pattern {
            agency: s.agency,
            route_type: s.route_type.unwrap_or(2),
            stops: deps
                .iter()
                .map(|&(station, m)| condensed::PatternStop {
                    station,
                    offset: m - start,
                })
                .collect(),
        });
        out.trips.push(condensed::Trip {
            pattern: out.patterns.len() as u32 - 1,
            service: out.services.len() as u32 - 1,
            start,
        });
    }
    out.normalize();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::date::days_from_civil;
    use crate::tile;
    use gtfs::Condensed;

    /// Pads a record to 80 characters, with fields at their offsets.
    fn rec(fields: &[(usize, &str)]) -> String {
        let mut b = vec![b' '; 80];
        for (at, s) in fields {
            b[*at..*at + s.len()].copy_from_slice(s.as_bytes());
        }
        String::from_utf8(b).unwrap()
    }

    fn bs(uid: &str, from: &str, to: &str, days: &str, status: &str, stp: &str) -> String {
        rec(&[
            (0, "BSN"),
            (3, uid),
            (9, from),
            (15, to),
            (21, days),
            (29, status),
            (79, stp),
        ])
    }

    fn bx(atoc: &str) -> String {
        rec(&[(0, "BX"), (11, atoc)])
    }

    fn lo(tiploc: &str, working: &str, public: &str) -> String {
        rec(&[
            (0, "LO"),
            (2, tiploc),
            (10, working),
            (15, public),
            (29, "TB"),
        ])
    }

    fn li(tiploc: &str, dep: &str, public: &str, activity: &str) -> String {
        rec(&[
            (0, "LI"),
            (2, tiploc),
            (10, dep),
            (15, dep),
            (25, public),
            (29, public),
            (42, activity),
        ])
    }

    fn pass(tiploc: &str, time: &str) -> String {
        rec(&[
            (0, "LI"),
            (2, tiploc),
            (20, time),
            (25, "0000"),
            (29, "0000"),
        ])
    }

    fn lt(tiploc: &str, time: &str) -> String {
        rec(&[(0, "LT"), (2, tiploc), (10, time), (15, time), (25, "TF")])
    }

    const A: (f64, f64) = (-0.1135, 51.5031); // London Waterloo
    const B: (f64, f64) = (-0.1700, 51.4640); // Clapham Junction
    const C: (f64, f64) = (-0.3008, 51.4630); // Richmond

    fn stations() -> Stations {
        let mut csv = String::from("ATCOCode,CommonName,Longitude,Latitude,StopType\n");
        for (atco, (lon, lat)) in [("9100WATRLMN", A), ("9100CLPHMJC", B), ("9100RICHMND", C)] {
            csv.push_str(&format!("{atco},x,{lon},{lat},RLY\n"));
        }
        csv.push_str("490000254S,bus,-0.11,51.50,BCT\n");
        Stations::from_naptan(csv.as_bytes()).unwrap()
    }

    fn px((lon, lat): (f64, f64)) -> (u32, u32) {
        tile::pixel_xy(lon, lat).unwrap()
    }

    #[test]
    fn reads_naptan_rail_stations() {
        let s = stations();
        assert_eq!(s.len(), 3);
        let (x, y) = s.by_tiploc["CLPHMJC"];
        assert_eq!((x >> 6, y >> 6), px(B));
    }

    #[test]
    fn parses_fields() {
        assert_eq!(parse_date("260601"), Some(days_from_civil(2026, 6, 1)));
        assert_eq!(parse_date("261341"), None);
        assert_eq!(parse_time("2315H"), Some(23 * 60 + 15));
        assert_eq!(parse_time("    "), None);
        assert!(boards("TB"));
        assert!(boards("T           "));
        assert!(boards("U "));
        assert!(boards("R "));
        assert!(!boards("D "));
        assert!(!boards("TF"));
        assert!(!boards("T N "));
        assert!(!boards("            "));
    }

    /// June 2026: Monday the 1st to Tuesday the 30th.
    fn timetable() -> String {
        [
            rec(&[(0, "HD")]),
            // Station with a CRS code but a TIPLOC that NaPTAN lacks.
            rec(&[(0, "TI"), (2, "CLPHMJM"), (53, "CLJ")]),
            rec(&[(0, "TI"), (2, "CLPHMJC"), (53, "CLJ")]),
            // Weekdays: Waterloo → Clapham (other TIPLOC) → Richmond.
            bs("W00001", "260601", "260630", "1111100", "P", "P"),
            bx("SW"),
            lo("WATRLMN", "0800 ", "0800"),
            pass("VAUXHLM", "0803 "),
            li("CLPHMJM", "0806H", "0806", "T "),
            lt("RICHMND", "0820 "),
            // ...cancelled on Wednesday 10 June.
            bs("W00001", "260610", "260610", "0010000", " ", "C"),
            // ...and overlaid on Thursday 11 June, leaving later.
            bs("W00001", "260611", "260611", "0001000", "P", "O"),
            bx("SW"),
            lo("WATRLMN", "0810 ", "0810"),
            lt("RICHMND", "0830 "),
            // A night train past midnight, Saturdays only, with a
            // set-down-only stop at Clapham and an unknown TIPLOC.
            bs("N00002", "260601", "260630", "0000010", "1", "P"),
            lo("WATRLMN", "2350 ", "2350"),
            li("CLPHMJC", "2355 ", "2355", "D "),
            li("NOWHERE", "0005 ", "0005", "T "),
            li("RICHMND", "0015 ", "0000", "T "),
            lt("WATRLMN", "0040 "),
            // A freight train, which counts nothing.
            bs("F00003", "260601", "260630", "1111111", "F", "P"),
            lo("WATRLMN", "1200 ", "0000"),
            lt("RICHMND", "1230 "),
            rec(&[(0, "ZZ")]),
        ]
        .join("\n")
    }

    /// Days and (pixel, minute) of each trip's departures.
    type Deps = Vec<(u8, Vec<((u32, u32), u16)>)>;

    fn deps(c: &Condensed) -> Deps {
        let mut v: Vec<_> = c
            .trips
            .iter()
            .map(|t| {
                (
                    t.days,
                    c.trip_stops(t)
                        .iter()
                        .map(|s| ((s.x, s.y), s.minute))
                        .collect(),
                )
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn condenses_median_week() {
        let c = condense(timetable().as_bytes(), &stations())
            .unwrap()
            .median_week(None)
            .unwrap();
        assert_eq!(c.first_day, days_from_civil(2026, 6, 1));
        assert_eq!(c.last_day, days_from_civil(2026, 6, 30));
        // Full weeks start 1, 8, 15 and 22 June; the week of the 8th
        // has one train fewer (cancelled), the others tie, and the
        // median of four is the second smallest.
        assert_eq!(c.week_start, days_from_civil(2026, 6, 1));
        assert_eq!(c.agencies, ["", "SW"]);
        assert_eq!(
            deps(&c),
            [
                // Mon–Fri: Waterloo 08:00, Clapham (via its CRS) 08:06.
                (0b0001_1111, vec![(px(A), 480), (px(B), 486)]),
                // Saturday night: Waterloo 23:50, then Richmond at
                // 00:15 of the next day; Clapham is set-down only.
                (0b0010_0000, vec![(px(A), 1430), (px(C), 1455)]),
            ]
        );
        let weekday = c.trips.iter().find(|t| t.days == 0b0001_1111).unwrap();
        assert_eq!(weekday.route_type, 2);
        assert_eq!(c.agencies[weekday.agency as usize], "SW");
    }

    #[test]
    fn applies_cancellations_and_overlays() {
        // Only the week of 8 June is left, so it is the median.
        let tt = timetable().replace("260601260630", "260608260614");
        let c = condense(tt.as_bytes(), &stations())
            .unwrap()
            .median_week(None)
            .unwrap();
        assert_eq!(c.week_start, days_from_civil(2026, 6, 8));
        assert_eq!(
            deps(&c),
            [
                // The overlay runs on Thursday.
                (0b0000_1000, vec![(px(A), 490)]),
                // The permanent schedule on Mon, Tue and Fri.
                (0b0001_0011, vec![(px(A), 480), (px(B), 486)]),
                (0b0010_0000, vec![(px(A), 1430), (px(C), 1455)]),
            ]
        );
        assert_eq!(c.departures_per_week(), 1 + 2 * 3 + 2);
    }

    #[test]
    fn reads_gzip() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.cif.gz");
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        gz.write_all(timetable().as_bytes()).unwrap();
        gz.finish().unwrap();
        let c = condense_path(&path, &stations()).unwrap();
        assert_eq!(c.median_week(None).unwrap().trips.len(), 2);
    }

    #[test]
    fn empty_timetable() {
        let c = condense(rec(&[(0, "HD")]).as_bytes(), &stations()).unwrap();
        assert!(c.trips.is_empty());
    }
}
