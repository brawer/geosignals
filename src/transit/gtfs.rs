// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Condenses a GTFS feed to the departures of its median week.
//!
//! A feed's calendar usually spans weeks to a year, with holidays and
//! timetable changes. We count the trips that run on each day, split the
//! days into weeks from Monday to Sunday, and pick the week whose total
//! is the median. Holiday weeks and the thin tail at the end of a feed,
//! where the next timetable is not yet published, fall out that way,
//! and weekends stay in.
//!
//! For each trip running in that week we keep the days it runs on (a
//! bit per weekday), and for each stop where it departs the zoom-18
//! pixel and the minute of day. A stop is a departure unless it is the
//! trip's last one or has `pickup_type = 1`. Trips in `frequencies.txt`
//! are expanded into one trip per start time. This is all the signal
//! and the duplicate detection need, and it is about 10 times smaller
//! than the feed, so we cache it; see [`Condensed::encode`].

use crate::slab::{Record, Slab, Writer};
use crate::tile;
use anyhow::{Context, Result, bail, ensure};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

/// Version of the condensed format and of the condensing method.
/// Bumping it makes every run re-condense its feeds.
pub const VERSION: u8 = 1;

const MAGIC: &[u8; 8] = b"OSMSGTFS";

/// Minute value for departures without a time.
pub const NO_TIME: u16 = u16::MAX;

/// Longest calendar span we look at, in days, from the first day with
/// service. Some feeds claim to run until 2099.
const MAX_SPAN_DAYS: i64 = 371;

/// The median week of one feed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Condensed {
    /// Day number (days since 1970-01-01) of the Monday of the median week.
    pub week_start: i64,
    /// First and last day with any service, as day numbers.
    pub first_day: i64,
    pub last_day: i64,
    pub agencies: Vec<String>,
    pub trips: Slab<Trip>,
    /// Departures of all trips, one trip after the other.
    pub stops: Slab<Stop>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trip {
    /// Index into [`Condensed::agencies`].
    pub agency: u32,
    /// GTFS `route_type`, such as 3 for bus.
    pub route_type: u16,
    /// Days of the median week the trip runs on; bit 0 is Monday.
    pub days: u8,
    /// Range of the trip's departures in [`Condensed::stops`].
    pub start: u32,
    pub len: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stop {
    /// Zoom-18 pixel.
    pub x: u32,
    pub y: u32,
    /// Minutes after midnight of the service day, which may exceed
    /// 24 hours; or [`NO_TIME`].
    pub minute: u16,
}

// SAFETY: both are repr(C) with integer fields; `write` fills every
// byte, padding included, at the offsets of the fields.
unsafe impl Record for Trip {
    const SIZE: usize = 16;
    fn write(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&self.agency.to_ne_bytes());
        out[4..6].copy_from_slice(&self.route_type.to_ne_bytes());
        out[6] = self.days;
        out[7] = 0;
        out[8..12].copy_from_slice(&self.start.to_ne_bytes());
        out[12..16].copy_from_slice(&self.len.to_ne_bytes());
    }
}

unsafe impl Record for Stop {
    const SIZE: usize = 12;
    fn write(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&self.x.to_ne_bytes());
        out[4..8].copy_from_slice(&self.y.to_ne_bytes());
        out[8..10].copy_from_slice(&self.minute.to_ne_bytes());
        out[10..12].fill(0);
    }
}

impl Condensed {
    pub fn trip_stops(&self, trip: &Trip) -> &[Stop] {
        &self.stops[trip.start as usize..(trip.start + trip.len) as usize]
    }

    /// Departures in the median week.
    pub fn departures_per_week(&self) -> u64 {
        self.trips
            .iter()
            .map(|t| u64::from(t.len) * u64::from(t.days.count_ones()))
            .sum()
    }

    /// Serializes to a compact, zstd-compressed binary form.
    pub fn encode(&self) -> Result<Vec<u8>> {
        use std::io::Write;
        // Compressed as we go, so a big feed never sits uncompressed in memory.
        let mut z = zstd::Encoder::new(Vec::new(), 9)?;
        let mut b = Vec::with_capacity(1 << 16);
        b.extend_from_slice(MAGIC);
        b.push(VERSION);
        put_signed(&mut b, self.week_start);
        put_signed(&mut b, self.first_day);
        put_signed(&mut b, self.last_day);
        put_varint(&mut b, self.agencies.len() as u64);
        for a in &self.agencies {
            put_varint(&mut b, a.len() as u64);
            b.extend_from_slice(a.as_bytes());
            z.write_all(&b)?;
            b.clear();
        }
        put_varint(&mut b, self.trips.len() as u64);
        let (mut px, mut py) = (0i64, 0i64);
        for t in &self.trips {
            put_varint(&mut b, u64::from(t.agency));
            put_varint(&mut b, u64::from(t.route_type));
            b.push(t.days);
            put_varint(&mut b, u64::from(t.len));
            for s in self.trip_stops(t) {
                put_signed(&mut b, i64::from(s.x) - px);
                put_signed(&mut b, i64::from(s.y) - py);
                (px, py) = (i64::from(s.x), i64::from(s.y));
                put_varint(&mut b, (u64::from(s.minute) + 1) & 0xffff);
            }
            if b.len() >= (1 << 16) - 1024 {
                z.write_all(&b)?;
                b.clear();
            }
        }
        z.write_all(&b)?;
        Ok(z.finish()?)
    }

    pub fn decode(data: &[u8]) -> Result<Condensed> {
        Self::decode_in(data, None)
    }

    /// Like [`decode`](Self::decode), keeping trips and stops in
    /// memory-mapped files in `scratch`, if given.
    pub fn decode_in(data: &[u8], scratch: Option<&Path>) -> Result<Condensed> {
        let mut r = Reader {
            r: std::io::BufReader::with_capacity(1 << 16, zstd::stream::read::Decoder::new(data)?),
        };
        ensure!(r.bytes(8)? == MAGIC, "not a condensed GTFS feed");
        let version = r.byte()?;
        ensure!(
            version == VERSION,
            "condensed format version {version}, expected {VERSION}"
        );
        let mut c = Condensed {
            week_start: r.signed()?,
            first_day: r.signed()?,
            last_day: r.signed()?,
            ..Condensed::default()
        };
        for _ in 0..r.varint()? {
            let len = r.varint()? as usize;
            c.agencies.push(String::from_utf8(r.bytes(len)?)?);
        }
        let (mut px, mut py) = (0i64, 0i64);
        let max = (1i64 << tile::PIXEL_ZOOM) - 1;
        let (mut trips, mut stops) = (Writer::new(scratch)?, Writer::new(scratch)?);
        for _ in 0..r.varint()? {
            let agency = u32::try_from(r.varint()?)?;
            ensure!((agency as usize) < c.agencies.len().max(1), "bad agency");
            let route_type = u16::try_from(r.varint()?)?;
            let days = r.byte()?;
            let len = u32::try_from(r.varint()?)?;
            let start = u32::try_from(stops.len())?;
            for _ in 0..len {
                px += r.signed()?;
                py += r.signed()?;
                ensure!(
                    (0..=max).contains(&px) && (0..=max).contains(&py),
                    "bad pixel"
                );
                let m = r.varint()?;
                let minute = if m == 0 {
                    NO_TIME
                } else {
                    u16::try_from(m - 1)?
                };
                stops.push(Stop {
                    x: px as u32,
                    y: py as u32,
                    minute,
                })?;
            }
            trips.push(Trip {
                agency,
                route_type,
                days,
                start,
                len,
            })?;
        }
        ensure!(r.at_end()?, "trailing data");
        c.trips = trips.finish()?;
        c.stops = stops.finish()?;
        Ok(c)
    }
}

fn put_varint(b: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        b.push((v as u8) | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
}

fn put_signed(b: &mut Vec<u8>, v: i64) {
    put_varint(b, ((v << 1) ^ (v >> 63)) as u64);
}

/// Reads the decompressed stream, without holding all of it.
struct Reader<R> {
    r: std::io::BufReader<R>,
}

impl<R: Read> Reader<R> {
    fn bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        ensure!(n <= 1 << 16, "corrupt length {n}");
        let mut buf = vec![0; n];
        self.r.read_exact(&mut buf).context("truncated")?;
        Ok(buf)
    }
    fn byte(&mut self) -> Result<u8> {
        let mut b = [0u8];
        self.r.read_exact(&mut b).context("truncated")?;
        Ok(b[0])
    }
    fn at_end(&mut self) -> Result<bool> {
        use std::io::BufRead;
        Ok(self.r.fill_buf()?.is_empty())
    }
    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        bail!("varint too long")
    }
    fn signed(&mut self) -> Result<i64> {
        let v = self.varint()?;
        Ok((v >> 1) as i64 ^ -((v & 1) as i64))
    }
}

/// Weekday of a day number; 0 is Monday.
pub fn weekday(day: i64) -> u32 {
    (day + 3).rem_euclid(7) as u32
}

/// Parses a GTFS date such as `20260609` into a day number.
fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| crate::date::days_from_civil(y, m, d))
}

/// Parses a GTFS time such as `25:07:00` into seconds.
fn parse_time(s: &str) -> Option<u32> {
    let mut parts = s.trim().split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let sec: u32 = parts.next()?.parse().ok()?;
    (parts.next().is_none() && m < 60 && sec < 61).then(|| h * 3600 + m * 60 + sec)
}

/// A CSV file of the feed, with columns looked up by name.
struct Table<R: Read> {
    reader: csv::Reader<R>,
    columns: HashMap<String, usize>,
}

impl<R: Read> Table<R> {
    fn new(r: R) -> Result<Self> {
        let mut reader = csv::ReaderBuilder::new()
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(r);
        let columns = reader
            .headers()?
            .iter()
            .enumerate()
            .map(|(i, h)| (h.trim_start_matches('\u{feff}').trim().to_string(), i))
            .collect();
        Ok(Table { reader, columns })
    }

    fn col(&self, name: &str) -> Option<usize> {
        self.columns.get(name).copied()
    }

    fn require(&self, name: &str) -> Result<usize> {
        self.col(name).with_context(|| format!("no column {name}"))
    }

    /// Calls `f` with each row; rows that fail to parse are skipped.
    fn for_each(&mut self, mut f: impl FnMut(&csv::StringRecord)) -> Result<()> {
        let mut rec = csv::StringRecord::new();
        loop {
            match self.reader.read_record(&mut rec) {
                Ok(true) => f(&rec),
                Ok(false) => return Ok(()),
                Err(e) if e.is_io_error() => return Err(e.into()),
                Err(_) => {} // a malformed row
            }
        }
    }
}

fn field(rec: &csv::StringRecord, col: Option<usize>) -> &str {
    col.and_then(|c| rec.get(c)).unwrap_or("")
}

/// The files of a GTFS zip, found by name even inside a folder.
struct Feed {
    zip: zip::ZipArchive<std::fs::File>,
    names: HashMap<String, usize>,
}

impl Feed {
    fn open(path: &Path) -> Result<Feed> {
        let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let zip =
            zip::ZipArchive::new(f).with_context(|| format!("{} is not a zip", path.display()))?;
        let mut names = HashMap::new();
        for i in 0..zip.len() {
            let Some(Ok(name)) = zip.name_for_index(i) else {
                continue;
            };
            let base = name
                .rsplit('/')
                .next()
                .unwrap_or(&name)
                .to_ascii_lowercase();
            if !name.contains("__MACOSX") {
                names.entry(base).or_insert(i);
            }
        }
        Ok(Feed { zip, names })
    }

    fn has(&self, name: &str) -> bool {
        self.names.contains_key(name)
    }

    fn table(
        &mut self,
        name: &str,
    ) -> Result<Option<Table<zip::read::ZipFile<'_, std::fs::File>>>> {
        let Some(&i) = self.names.get(name) else {
            return Ok(None);
        };
        let file = self.zip.by_index(i)?;
        Ok(Some(
            Table::new(file).with_context(|| format!("reading {name}"))?,
        ))
    }
}

struct CalendarRule {
    weekdays: u8,
    start: i64,
    end: i64,
}

impl CalendarRule {
    fn active(&self, day: i64) -> bool {
        (self.start..=self.end).contains(&day) && self.weekdays & (1 << weekday(day)) != 0
    }
}

/// Condenses the GTFS zip at `path`.
pub fn condense(path: &Path) -> Result<Condensed> {
    condense_in(path, None)
}

/// Like [`condense`], keeping the large arrays in memory-mapped files
/// in `scratch`, if given, instead of on the heap.
pub fn condense_in(path: &Path, scratch: Option<&Path>) -> Result<Condensed> {
    let mut feed = Feed::open(path)?;
    for name in ["trips.txt", "stop_times.txt", "stops.txt"] {
        ensure!(feed.has(name), "no {name}");
    }
    ensure!(
        feed.has("calendar.txt") || feed.has("calendar_dates.txt"),
        "neither calendar.txt nor calendar_dates.txt"
    );

    // Agencies and routes.
    let mut agencies = Vec::new();
    let mut agency_index: HashMap<String, u32> = HashMap::new();
    if let Some(mut t) = feed.table("agency.txt")? {
        let (id, name) = (t.col("agency_id"), t.col("agency_name"));
        t.for_each(|r| {
            if let std::collections::hash_map::Entry::Vacant(e) =
                agency_index.entry(field(r, id).to_string())
            {
                e.insert(agencies.len() as u32);
                agencies.push(field(r, name).to_string());
            }
        })?;
    }
    if agencies.is_empty() {
        agencies.push(String::new());
    }
    // route_id → (agency, route_type)
    let mut routes: HashMap<String, (u32, u16)> = HashMap::new();
    if let Some(mut t) = feed.table("routes.txt")? {
        let (id, agency, typ) = (t.col("route_id"), t.col("agency_id"), t.col("route_type"));
        t.for_each(|r| {
            let a = agency_index.get(field(r, agency)).copied().unwrap_or(0);
            let ty = field(r, typ).parse().unwrap_or(3);
            routes.insert(field(r, id).to_string(), (a, ty));
        })?;
    }

    // Services.
    let mut service_index: HashMap<String, u32> = HashMap::new();
    let mut intern = |s: &str| -> u32 {
        let n = service_index.len() as u32;
        *service_index.entry(s.to_string()).or_insert(n)
    };
    let mut rules: HashMap<u32, CalendarRule> = HashMap::new();
    if let Some(mut t) = feed.table("calendar.txt")? {
        let id = t.require("service_id")?;
        let days: Vec<Option<usize>> = [
            "monday",
            "tuesday",
            "wednesday",
            "thursday",
            "friday",
            "saturday",
            "sunday",
        ]
        .iter()
        .map(|d| t.col(d))
        .collect();
        let (start, end) = (t.col("start_date"), t.col("end_date"));
        t.for_each(|r| {
            let (Some(s), Some(e)) = (parse_date(field(r, start)), parse_date(field(r, end)))
            else {
                return;
            };
            let mut weekdays = 0u8;
            for (i, c) in days.iter().enumerate() {
                if field(r, *c) == "1" {
                    weekdays |= 1 << i;
                }
            }
            let svc = intern(field(r, Some(id)));
            rules.insert(
                svc,
                CalendarRule {
                    weekdays,
                    start: s,
                    end: e,
                },
            );
        })?;
    }
    // Some feeds have ten million of these, so they are packed into
    // eight bytes each; see `Exception`.
    let mut exceptions: Vec<Exception> = Vec::new();
    if let Some(mut t) = feed.table("calendar_dates.txt")? {
        let (id, date, typ) = (
            t.require("service_id")?,
            t.col("date"),
            t.col("exception_type"),
        );
        t.for_each(|r| {
            let Some(day) = parse_date(field(r, date)) else {
                return;
            };
            let added = match field(r, typ) {
                "1" => true,
                "2" => false,
                _ => return,
            };
            let svc = intern(field(r, Some(id)));
            exceptions.push(Exception::new(svc, day, added));
        })?;
        // For a service and day listed twice, the first entry counts.
        exceptions.sort_by_key(|e| e.0 >> 1); // stable
        exceptions.dedup_by_key(|e| e.0 >> 1);
    }

    // Trips.
    struct TripInfo {
        service: u32,
        agency: u32,
        route_type: u16,
    }
    let mut trip_index: HashMap<String, u32> = HashMap::new();
    let mut trips: Vec<TripInfo> = Vec::new();
    {
        let mut t = feed.table("trips.txt")?.expect("checked above");
        let (id, svc, route) = (
            t.require("trip_id")?,
            t.require("service_id")?,
            t.col("route_id"),
        );
        t.for_each(|r| {
            let Some(&service) = service_index.get(field(r, Some(svc))) else {
                return; // service without any days
            };
            let (agency, route_type) = routes.get(field(r, route)).copied().unwrap_or((0, 3));
            if let std::collections::hash_map::Entry::Vacant(e) =
                trip_index.entry(field(r, Some(id)).to_string())
            {
                e.insert(trips.len() as u32);
                trips.push(TripInfo {
                    service,
                    agency,
                    route_type,
                });
            }
        })?;
    }

    // Frequency-based trips: trip → [(start secs, end secs, headway secs)].
    let mut frequencies: HashMap<u32, Vec<(u32, u32, u32)>> = HashMap::new();
    if let Some(mut t) = feed.table("frequencies.txt")? {
        let (id, start, end, headway) = (
            t.col("trip_id"),
            t.col("start_time"),
            t.col("end_time"),
            t.col("headway_secs"),
        );
        t.for_each(|r| {
            let (Some(&trip), Some(s), Some(e), Ok(h)) = (
                trip_index.get(field(r, id)),
                parse_time(field(r, start)),
                parse_time(field(r, end)),
                field(r, headway).parse::<u32>(),
            ) else {
                return;
            };
            if h > 0 && e > s {
                frequencies.entry(trip).or_default().push((s, e, h));
            }
        })?;
    }

    // Trips per day over the calendar, to find the median week.
    let n_services = service_index.len();
    let mut trips_per_service = vec![0u64; n_services];
    for t in &trips {
        trips_per_service[t.service as usize] += 1;
    }
    for (trip, windows) in &frequencies {
        let n: u64 = windows
            .iter()
            .map(|&(s, e, h)| u64::from((e - s).div_ceil(h)))
            .sum();
        trips_per_service[trips[*trip as usize].service as usize] += n.saturating_sub(1);
    }
    let first_day = rules
        .values()
        .filter(|r| r.weekdays != 0)
        .map(|r| r.start)
        .chain(exceptions.iter().filter(|e| e.added()).map(|e| e.day()))
        .min();
    let last_day = rules
        .values()
        .filter(|r| r.weekdays != 0)
        .map(|r| r.end)
        .chain(exceptions.iter().filter(|e| e.added()).map(|e| e.day()))
        .max();
    let (Some(first_day), Some(last_day)) = (first_day, last_day) else {
        return Ok(Condensed {
            agencies,
            ..Condensed::default()
        });
    };
    let hi = last_day.min(first_day + MAX_SPAN_DAYS - 1);
    let span = (hi - first_day + 1).max(0) as usize;
    let mut per_day = vec![0i64; span];
    for (&svc, rule) in &rules {
        let n = trips_per_service[svc as usize] as i64;
        if n == 0 {
            continue;
        }
        for day in rule.start.max(first_day)..=rule.end.min(hi) {
            if rule.active(day) {
                per_day[(day - first_day) as usize] += n;
            }
        }
    }
    for (svc, day, added) in exceptions.iter().map(Exception::get) {
        if !(first_day..=hi).contains(&day) {
            continue;
        }
        let by_rule = rules.get(&svc).is_some_and(|r| r.active(day));
        let n = trips_per_service[svc as usize] as i64;
        let i = (day - first_day) as usize;
        match (added, by_rule) {
            (true, false) => per_day[i] += n,
            (false, true) => per_day[i] -= n,
            _ => {}
        }
    }
    let week_start = median_week(first_day, &per_day);

    // Which days of that week each service runs on.
    let mut service_days = vec![0u8; n_services];
    for (&svc, rule) in &rules {
        for i in 0..7 {
            if rule.active(week_start + i) {
                service_days[svc as usize] |= 1 << i;
            }
        }
    }
    for (svc, day, added) in exceptions.iter().map(Exception::get) {
        let i = day - week_start;
        if (0..7).contains(&i) {
            if added {
                service_days[svc as usize] |= 1 << i;
            } else {
                service_days[svc as usize] &= !(1 << i);
            }
        }
    }

    // Stop locations, falling back to the parent station's.
    let mut stop_index: HashMap<String, u32> = HashMap::new();
    let mut pixels: Vec<(u32, u32)> = Vec::new();
    {
        let mut parents: Vec<(String, String)> = Vec::new();
        let mut t = feed.table("stops.txt")?.expect("checked above");
        let (id, lat, lon, parent) = (
            t.require("stop_id")?,
            t.col("stop_lat"),
            t.col("stop_lon"),
            t.col("parent_station"),
        );
        t.for_each(|r| {
            let key = field(r, Some(id)).to_string();
            let coords = field(r, lat)
                .parse::<f64>()
                .ok()
                .zip(field(r, lon).parse::<f64>().ok());
            match coords.and_then(|(lat, lon)| tile::pixel_xy(lon, lat)) {
                Some(p) if coords != Some((0.0, 0.0)) => {
                    stop_index.insert(key, pixels.len() as u32);
                    pixels.push(p);
                }
                _ => {
                    let p = field(r, parent);
                    if !p.is_empty() {
                        parents.push((key, p.to_string()));
                    }
                }
            }
        })?;
        for (stop, parent) in parents {
            if let Some(&p) = stop_index.get(&parent) {
                stop_index.insert(stop, p);
            }
        }
    }

    // Stop times of the trips that run in the median week. A big feed
    // has tens of millions, so they go to a scratch file, if given.
    let mut events = Writer::new(scratch)?;
    {
        let mut t = feed.table("stop_times.txt")?.expect("checked above");
        let (trip, stop, seq, dep, arr, pickup) = (
            t.require("trip_id")?,
            t.require("stop_id")?,
            t.col("stop_sequence"),
            t.col("departure_time"),
            t.col("arrival_time"),
            t.col("pickup_type"),
        );
        let mut result = Ok(());
        t.for_each(|r| {
            if result.is_err() {
                return;
            }
            let Some(&ti) = trip_index.get(field(r, Some(trip))) else {
                return;
            };
            if service_days[trips[ti as usize].service as usize] == 0 {
                return;
            }
            let Some(&si) = stop_index.get(field(r, Some(stop))) else {
                return;
            };
            let time = parse_time(field(r, dep)).or_else(|| parse_time(field(r, arr)));
            let minute = time.map_or(NO_TIME, |s| (s / 60).min(u32::from(NO_TIME) - 1) as u16);
            result = events.push(Event {
                trip: ti,
                seq: field(r, seq).parse().unwrap_or(0),
                stop: si,
                minute,
                pickup: u8::from(field(r, pickup) != "1"),
            });
        })?;
        result?;
    }
    drop(stop_index);
    drop(trip_index);
    let mut events = events.finish()?;
    events
        .as_mut_slice()
        .sort_unstable_by_key(|e| (e.trip, e.seq));

    let mut out_trips = Writer::new(scratch)?;
    let mut out_stops = Writer::new(scratch)?;
    let mut template: Vec<Stop> = Vec::new();
    for group in events.chunk_by(|a, b| a.trip == b.trip) {
        let info = &trips[group[0].trip as usize];
        let days = service_days[info.service as usize];
        // The last stop is where the trip ends, not a departure.
        template.clear();
        template.extend(
            group[..group.len() - 1]
                .iter()
                .filter(|e| e.pickup != 0)
                .map(|e| {
                    let (x, y) = pixels[e.stop as usize];
                    Stop {
                        x,
                        y,
                        minute: e.minute,
                    }
                }),
        );
        if template.is_empty() {
            continue;
        }
        let mut push = |stops: &mut dyn Iterator<Item = Stop>| -> Result<()> {
            let start = u32::try_from(out_stops.len())?;
            for s in stops {
                out_stops.push(s)?;
            }
            out_trips.push(Trip {
                agency: info.agency,
                route_type: info.route_type,
                days,
                start,
                len: u32::try_from(out_stops.len())? - start,
            })
        };
        match frequencies.get(&group[0].trip) {
            None => push(&mut template.iter().copied())?,
            Some(windows) => {
                let base = template
                    .iter()
                    .map(|s| s.minute)
                    .find(|&m| m != NO_TIME)
                    .unwrap_or(0) as i64;
                for &(s, e, h) in windows {
                    let mut t = s;
                    while t < e {
                        let shift = i64::from(t / 60) - base;
                        push(&mut template.iter().map(|st| Stop {
                            minute: if st.minute == NO_TIME {
                                NO_TIME
                            } else {
                                (i64::from(st.minute) + shift).clamp(0, i64::from(NO_TIME) - 1)
                                    as u16
                            },
                            ..*st
                        }))?;
                        t += h;
                    }
                }
            }
        }
    }
    Ok(Condensed {
        week_start,
        first_day,
        last_day,
        agencies,
        trips: out_trips.finish()?,
        stops: out_stops.finish()?,
    })
}

/// An entry of `calendar_dates.txt`: service, day and whether service
/// is added (or removed) that day, packed as `service << 33 | (day +
/// 2³¹) << 1 | added`.
#[derive(Clone, Copy)]
struct Exception(u64);

impl Exception {
    fn new(service: u32, day: i64, added: bool) -> Self {
        // parse_date only gives years 0 to 9999, which fit.
        let day = (day + (1 << 31)) as u64 & 0xffff_ffff;
        Exception(u64::from(service) << 33 | day << 1 | u64::from(added))
    }
    fn service(&self) -> u32 {
        (self.0 >> 33) as u32
    }
    fn day(&self) -> i64 {
        ((self.0 >> 1) & 0xffff_ffff) as i64 - (1 << 31)
    }
    fn added(&self) -> bool {
        self.0 & 1 != 0
    }
    fn get(&self) -> (u32, i64, bool) {
        (self.service(), self.day(), self.added())
    }
}

/// A stop time of a trip in the median week, while condensing.
#[repr(C)]
#[derive(Clone, Copy)]
struct Event {
    trip: u32,
    seq: u32,
    /// Index into the stop pixels.
    stop: u32,
    minute: u16,
    /// 1 if passengers can board here.
    pickup: u8,
}

// SAFETY: repr(C) with integer fields; `write` fills all 16 bytes.
unsafe impl Record for Event {
    const SIZE: usize = 16;
    fn write(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&self.trip.to_ne_bytes());
        out[4..8].copy_from_slice(&self.seq.to_ne_bytes());
        out[8..12].copy_from_slice(&self.stop.to_ne_bytes());
        out[12..14].copy_from_slice(&self.minute.to_ne_bytes());
        out[14] = self.pickup;
        out[15] = 0;
    }
}

/// Monday of the median week, given the number of trips on each day
/// from `first_day` on. Only weeks that lie wholly inside the calendar
/// and have service count, unless there are none.
fn median_week(first_day: i64, per_day: &[i64]) -> i64 {
    let monday = |d: i64| d - i64::from(weekday(d));
    let last = first_day + per_day.len() as i64 - 1;
    let total = |m: i64| -> i64 {
        (m..m + 7)
            .filter(|d| (first_day..=last).contains(d))
            .map(|d| per_day[(d - first_day) as usize])
            .sum()
    };
    let mut first_full = monday(first_day);
    if first_full < first_day {
        first_full += 7;
    }
    let mut weeks: Vec<(i64, i64)> = (0..)
        .map(|i| first_full + 7 * i)
        .take_while(|m| m + 6 <= last)
        .map(|m| (total(m), m))
        .filter(|&(t, _)| t > 0)
        .collect();
    if weeks.is_empty() {
        // Less than a full week of service: take the busiest partial week.
        return (0..)
            .map(|i| monday(first_day) + 7 * i)
            .take_while(|&m| m <= last)
            .max_by_key(|&m| (total(m), -m))
            .unwrap_or(monday(first_day));
    }
    weeks.sort_unstable();
    weeks[(weeks.len() - 1) / 2].1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::date::days_from_civil;
    use std::io::Write;

    /// Writes a GTFS zip with the given files.
    pub fn write_zip(path: &Path, files: &[(&str, &str)]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, content) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(content.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn weekdays() {
        assert_eq!(weekday(0), 3, "1970-01-01 was a Thursday");
        assert_eq!(weekday(days_from_civil(2026, 6, 8)), 0, "Monday");
        assert_eq!(weekday(days_from_civil(2026, 6, 14)), 6, "Sunday");
    }

    #[test]
    fn parses() {
        assert_eq!(parse_date("20260609"), Some(days_from_civil(2026, 6, 9)));
        assert_eq!(parse_date("2026069"), None);
        assert_eq!(parse_date("20261309"), None);
        assert_eq!(parse_time("25:07:30"), Some(25 * 3600 + 7 * 60 + 30));
        assert_eq!(parse_time(" 7:05:00"), Some(7 * 3600 + 5 * 60));
        assert_eq!(parse_time("7:65:00"), None);
        assert_eq!(parse_time(""), None);
    }

    #[test]
    fn picks_median_week() {
        let monday = days_from_civil(2026, 6, 1);
        // Starts on a Wednesday; four full weeks with 10, 3 (holiday),
        // 10 and 12 trips a day, then a thin tail.
        let first = monday + 2;
        let mut per_day = vec![10i64; 5]; // Wed..Sun of the partial week
        for t in [10, 3, 10, 12] {
            per_day.extend([t; 7]);
        }
        per_day.extend([1; 3]);
        let w = median_week(first, &per_day);
        assert_eq!(weekday(w), 0);
        assert_eq!(
            w,
            monday + 7,
            "the first week of 10, ties go to the earlier"
        );
        // Less than a week of service.
        assert_eq!(median_week(monday + 3, &[5, 5, 5]), monday);
    }

    #[test]
    fn packs_exceptions() {
        for (svc, day, added) in [
            (0, 0, true),
            (7, -719_528, false),
            (u32::MAX >> 1, 2_932_896, true),
        ] {
            assert_eq!(Exception::new(svc, day, added).get(), (svc, day, added));
        }
        // The first entry for a service and day wins.
        let mut e = vec![
            Exception::new(2, 20_000, false),
            Exception::new(1, 20_001, true),
            Exception::new(2, 20_000, true),
        ];
        e.sort_by_key(|e| e.0 >> 1);
        e.dedup_by_key(|e| e.0 >> 1);
        let got: Vec<_> = e.iter().map(Exception::get).collect();
        assert_eq!(got, [(1, 20_001, true), (2, 20_000, false)]);
    }

    #[test]
    fn encode_round_trip() {
        let c = Condensed {
            week_start: 20_000,
            first_day: 19_990,
            last_day: 20_400,
            agencies: vec!["SBB".into(), "Südbadenbus".into()],
            trips: vec![
                Trip {
                    agency: 1,
                    route_type: 3,
                    days: 0b0011111,
                    start: 0,
                    len: 2,
                },
                Trip {
                    agency: 0,
                    route_type: 2,
                    days: 0b1100000,
                    start: 2,
                    len: 1,
                },
            ]
            .into(),
            stops: vec![
                Stop {
                    x: 137_000,
                    y: 91_000,
                    minute: 300,
                },
                Stop {
                    x: 136_998,
                    y: 91_005,
                    minute: NO_TIME,
                },
                Stop {
                    x: 0,
                    y: (1 << 18) - 1,
                    minute: 1500,
                },
            ]
            .into(),
        };
        let data = c.encode().unwrap();
        assert_eq!(Condensed::decode(&data).unwrap(), c);
        assert_eq!(c.departures_per_week(), 2 * 5 + 2);
        let mut bad = zstd::decode_all(data.as_slice()).unwrap();
        bad[8] = VERSION + 1;
        let bad = zstd::encode_all(bad.as_slice(), 1).unwrap();
        assert!(Condensed::decode(&bad).is_err());
    }

    fn sample_feed(dir: &Path, extra: &[(&str, &str)]) -> std::path::PathBuf {
        let path = dir.join("feed.zip");
        let mut files = vec![
            (
                "agency.txt",
                "agency_id,agency_name\nA,Alpha Bus\nB,Beta Rail\n",
            ),
            (
                "routes.txt",
                "route_id,agency_id,route_type\nr1,A,3\nr2,B,2\n",
            ),
            // Weekdays all June, weekends all June; a holiday on Thursday 4 June.
            (
                "calendar.txt",
                "\u{feff}service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\n\
                 wd,1,1,1,1,1,0,0,20260601,20260630\n\
                 we,0,0,0,0,0,1,1,20260601,20260630\n",
            ),
            (
                "calendar_dates.txt",
                "service_id,date,exception_type\nwd,20260604,2\nwe,20260604,1\n",
            ),
            (
                "trips.txt",
                "route_id,service_id,trip_id\nr1,wd,t1\nr2,we,t2\nr1,wd,f1\nr1,nosuch,t3\n",
            ),
            (
                "stops.txt",
                "stop_id,stop_lat,stop_lon,parent_station\n\
                 P,47.3782,8.5402,\ns1,,,P\ns2,47.40,8.55,\ns3,47.41,8.56,\n",
            ),
            (
                "stop_times.txt",
                "trip_id,arrival_time,departure_time,stop_id,stop_sequence,pickup_type\n\
                 t1,08:00:00,08:00:00,s1,1,0\n\
                 t1,08:10:00,08:10:00,s3,3,0\n\
                 t1,08:05:00,08:05:00,s2,2,1\n\
                 t2,25:00:00,25:00:00,s2,1,\n\
                 t2,25:30:00,25:30:00,s3,2,\n\
                 f1,06:00:00,06:00:00,s2,1,\n\
                 f1,06:20:00,06:20:00,s3,2,\n",
            ),
        ];
        files.extend_from_slice(extra);
        write_zip(&path, &files);
        path
    }

    #[test]
    fn condenses_feed() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample_feed(
            dir.path(),
            &[(
                "frequencies.txt",
                "trip_id,start_time,end_time,headway_secs\nf1,06:00:00,07:00:00,1200\n",
            )],
        );
        let c = condense(&path).unwrap();
        assert_eq!(c.agencies, ["Alpha Bus", "Beta Rail"]);
        assert_eq!(c.first_day, days_from_civil(2026, 6, 1));
        assert_eq!(c.last_day, days_from_civil(2026, 6, 30));
        // The holiday week (1–7 June) has fewer weekday trips, and the
        // last full week is 22–28 June; weeks of 8, 15 and 22 June tie.
        assert_eq!(c.week_start, days_from_civil(2026, 6, 8));

        let zh = tile::pixel_xy(8.5402, 47.3782).unwrap();
        let s2 = tile::pixel_xy(8.55, 47.40).unwrap();
        // t1: s1 (at the parent's location), s2 has no pickup, s3 ends the trip.
        let t1 = &c.trips[0];
        assert_eq!((t1.agency, t1.route_type, t1.days), (0, 3, 0b0011111));
        assert_eq!(
            c.trip_stops(t1),
            [Stop {
                x: zh.0,
                y: zh.1,
                minute: 480
            }]
        );
        // t2 runs on weekends after midnight.
        let t2 = &c.trips[1];
        assert_eq!((t2.agency, t2.route_type, t2.days), (1, 2, 0b1100000));
        assert_eq!(
            c.trip_stops(t2),
            [Stop {
                x: s2.0,
                y: s2.1,
                minute: 1500
            }]
        );
        // f1 runs every 20 minutes from 6 to 7.
        let starts: Vec<u16> = c.trips[2..]
            .iter()
            .map(|t| c.trip_stops(t)[0].minute)
            .collect();
        assert_eq!(starts, [360, 380, 400]);
        assert_eq!(c.trips.len(), 5);
        assert_eq!(c.departures_per_week(), 5 + 2 + 3 * 5);
    }

    #[test]
    fn rejects_broken_feeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.zip");
        std::fs::write(&path, b"<html>not found</html>").unwrap();
        assert!(condense(&path).is_err());
        write_zip(&path, &[("stops.txt", "stop_id\n")]);
        assert!(
            condense(&path)
                .unwrap_err()
                .to_string()
                .contains("trips.txt")
        );
    }
}
