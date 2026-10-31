// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Condensed transit feed, format 2: everything a run needs from a
//! GTFS or CIF feed, including its whole calendar.
//!
//! A feed becomes a table of stations, a list of trip patterns (the
//! stations a trip departs from, with minutes after its first
//! departure), the trips (pattern, service, start minute) and the
//! services (a bitmap of the days each one runs). Most trips share
//! their pattern with many others, so this is several times smaller
//! than storing every departure: the Swiss national feed takes 3.3 MB
//! and the German one 8.1 MB, against 11 and 59 MB for format 1, which
//! held only the median week.
//!
//! Patterns hold departures only: the condenser leaves out a trip's
//! last stop and stops where boarding is not allowed.
//!
//! Stations are sorted by the Hilbert key of their zoom-18 pixel, so a
//! station table can be merge-joined with another or searched by area.
//! Each keeps its position at zoom 24 (about 2 m) and its stop ID.
//!
//! The file is a magic number and the format version, then sections
//! that are each their own zstd frame behind a 4-byte length: header,
//! weekly totals, stations, patterns, trips, frequencies, services. A
//! run that only needs the inputs' ETags, or the departures per station
//! and week, reads the first one or two frames and stops.
//!
//! The header records each input file: its URL and where that
//! redirected to, `ETag`, `Last-Modified`, SHA-256, SHA-512, size, and
//! when we first downloaded it. It is the cache's record of the feed;
//! see [`super::fetch`].

use super::gtfs;
use crate::slab::Writer;
use crate::tile;
use anyhow::{Context, Result, bail, ensure};
use std::io::Read;
use std::path::Path;

/// Version of this file format.
pub const FORMAT: u8 = 2;

const MAGIC: &[u8; 8] = b"OSMSGTFS";

/// Minute value for departures without a time.
pub const NO_TIME: u16 = u16::MAX;

/// Most days a calendar may cover, counted from its first day.
pub const MAX_DAYS: u32 = 371;

/// Zoom level of station positions.
pub const STATION_ZOOM: u8 = 24;

/// Bits between a station position and its zoom-18 pixel.
const STATION_SHIFT: u8 = STATION_ZOOM - tile::PIXEL_ZOOM;

/// Zstandard level for the sections. A file is written only when its
/// feed changes, but read on every run, and reading costs the same at
/// any level. Level 19 makes the Swiss and German feeds 8 to 10%
/// smaller than level 9, for 6 and 12 more seconds of compressing.
const ZSTD_LEVEL: i32 = 19;

/// Largest string or byte field we accept when decoding.
const MAX_FIELD: usize = 1 << 16;

/// A downloaded file the feed was condensed from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Input {
    /// The URL we asked for.
    pub url: String,
    /// Where that redirected to.
    pub final_url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub sha256: [u8; 32],
    pub sha512: Vec<u8>,
    pub bytes: u64,
    /// When we first downloaded this content, in seconds since 1970.
    pub first_seen: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Station {
    /// Zoom-24 Web Mercator position.
    pub x: u32,
    pub y: u32,
    /// The feed's stop ID, or TIPLOC for CIF.
    pub id: String,
}

impl Station {
    /// Zoom-18 pixel.
    pub fn pixel(&self) -> (u32, u32) {
        (self.x >> STATION_SHIFT, self.y >> STATION_SHIFT)
    }

    fn sort_key(&self) -> (u64, u32, u32) {
        let (px, py) = self.pixel();
        (tile::pixel_key(px, py), self.x, self.y)
    }
}

/// Zoom-24 position of a location, or `None` outside Web Mercator.
pub fn station_xy(lon: f64, lat: f64) -> Option<(u32, u32)> {
    if !(-tile::MAX_LAT..=tile::MAX_LAT).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let n = (1u64 << STATION_ZOOM) as f64;
    let max = (1u32 << STATION_ZOOM) - 1;
    let x = (lon + 180.0) / 360.0 * n;
    let y = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n;
    Some(((x as u32).min(max), (y.max(0.0) as u32).min(max)))
}

/// A departure in a pattern.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PatternStop {
    /// Index into [`Feed::stations`].
    pub station: u32,
    /// Minutes after the trip's start, or [`NO_TIME`].
    pub offset: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Pattern {
    /// Index into [`Feed::agencies`].
    pub agency: u32,
    /// GTFS `route_type`, such as 3 for bus.
    pub route_type: u16,
    pub stops: Vec<PatternStop>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Trip {
    /// Index into [`Feed::patterns`].
    pub pattern: u32,
    /// Index into [`Feed::services`].
    pub service: u32,
    /// Minutes after midnight of the service day, which may exceed
    /// 24 hours.
    pub start: u16,
}

/// Trips that start every `headway` seconds from `start` until
/// before `end`, as in GTFS `frequencies.txt`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Frequency {
    pub pattern: u32,
    pub service: u32,
    /// Seconds after midnight of the service day.
    pub start: u32,
    pub end: u32,
    pub headway: u32,
}

impl Frequency {
    /// Number of trips.
    pub fn count(&self) -> u32 {
        if self.headway == 0 || self.end <= self.start {
            0
        } else {
            (self.end - self.start).div_ceil(self.headway)
        }
    }
}

/// Days a service runs on, bit `d` for [`Feed::first_day`] + `d`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Service(pub Vec<u8>);

impl Service {
    pub fn new(days: u32) -> Self {
        Service(vec![0; days.div_ceil(8) as usize])
    }
    pub fn set(&mut self, day: u32) {
        self.0[(day / 8) as usize] |= 1 << (day % 8);
    }
    pub fn clear(&mut self, day: u32) {
        self.0[(day / 8) as usize] &= !(1 << (day % 8));
    }
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&b| b == 0)
    }
    pub fn runs(&self, day: u32) -> bool {
        self.0
            .get((day / 8) as usize)
            .is_some_and(|b| b & (1 << (day % 8)) != 0)
    }
}

/// A condensed feed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Feed {
    /// Version of the condenser that made the file. When it changes,
    /// the feed is condensed again from its raw inputs.
    pub condenser: u32,
    pub inputs: Vec<Input>,
    /// Day number (days since 1970-01-01) of the calendar's first day.
    pub first_day: i64,
    /// Days the calendar covers, at most [`MAX_DAYS`].
    pub days: u32,
    /// Last day with service, as the feed claims it; this may lie
    /// beyond the [`MAX_DAYS`] we keep.
    pub last_day: i64,
    pub agencies: Vec<String>,
    /// Sorted by the Hilbert key of their pixel; see [`Feed::normalize`].
    pub stations: Vec<Station>,
    pub patterns: Vec<Pattern>,
    pub trips: Vec<Trip>,
    pub frequencies: Vec<Frequency>,
    pub services: Vec<Service>,
}

/// Departures at each station in each week of the calendar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Day number of the Monday of the first week.
    pub first_monday: i64,
    pub weeks: usize,
    /// `weeks` values per station, in station order.
    pub values: Vec<u64>,
}

impl Totals {
    pub fn station(&self, i: usize) -> &[u64] {
        &self.values[i * self.weeks..(i + 1) * self.weeks]
    }
}

impl Feed {
    /// Trips and frequency trips that run on each day of the calendar.
    pub fn trips_per_day(&self) -> Vec<i64> {
        let mut per_service = vec![0i64; self.services.len()];
        for t in &self.trips {
            per_service[t.service as usize] += 1;
        }
        for f in &self.frequencies {
            per_service[f.service as usize] += i64::from(f.count());
        }
        let mut per_day = vec![0i64; self.days as usize];
        for (s, &n) in self.services.iter().zip(&per_service) {
            if n > 0 {
                for (d, v) in per_day.iter_mut().enumerate() {
                    if s.runs(d as u32) {
                        *v += n;
                    }
                }
            }
        }
        per_day
    }

    /// The departures of the median week, as format 1 held them.
    /// Large arrays go to memory-mapped files in `scratch`, if given.
    pub fn median_week(&self, scratch: Option<&Path>) -> Result<gtfs::Condensed> {
        let mut c = gtfs::Condensed {
            first_day: self.first_day,
            last_day: self.last_day,
            agencies: self.agencies.clone(),
            ..Default::default()
        };
        if self.days == 0 {
            return Ok(c);
        }
        c.week_start = gtfs::median_week(self.first_day, &self.trips_per_day());
        // Days of the median week that a service runs on.
        let week = |s: &Service| -> u8 {
            let mut bits = 0u8;
            for i in 0..7 {
                let d = c.week_start + i - self.first_day;
                if d >= 0 && s.runs(d as u32) {
                    bits |= 1 << i;
                }
            }
            bits
        };
        let days: Vec<u8> = self.services.iter().map(week).collect();
        let (mut trips, mut stops) = (Writer::new(scratch)?, Writer::new(scratch)?);
        for t in self.all_trips() {
            let d = days[t.service as usize];
            if d == 0 {
                continue;
            }
            let p = &self.patterns[t.pattern as usize];
            let first = u32::try_from(stops.len())?;
            for s in &p.stops {
                let (x, y) = self.stations[s.station as usize].pixel();
                let minute = if s.offset == NO_TIME {
                    NO_TIME
                } else {
                    (u32::from(t.start) + u32::from(s.offset)).min(u32::from(NO_TIME) - 1) as u16
                };
                stops.push(gtfs::Stop { x, y, minute })?;
            }
            trips.push(gtfs::Trip {
                agency: p.agency,
                route_type: p.route_type,
                days: d,
                start: first,
                len: u32::try_from(p.stops.len())?,
            })?;
        }
        c.trips = trips.finish()?;
        c.stops = stops.finish()?;
        Ok(c)
    }

    /// Every trip, with frequency-based ones expanded to one per start
    /// time: first [`Feed::trips`], then each frequency's trips in order.
    pub fn all_trips(&self) -> impl Iterator<Item = Trip> + '_ {
        self.trips
            .iter()
            .copied()
            .chain(self.frequencies.iter().flat_map(|f| {
                (0..f.count()).map(move |i| Trip {
                    pattern: f.pattern,
                    service: f.service,
                    start: ((f.start + i * f.headway) / 60).min(u32::from(NO_TIME) - 1) as u16,
                })
            }))
    }

    /// Departures on an average day of the calendar.
    pub fn departures_per_day(&self) -> f64 {
        if self.days == 0 {
            return 0.0;
        }
        let per_service: Vec<u64> = self
            .services
            .iter()
            .map(|s| s.0.iter().map(|b| u64::from(b.count_ones())).sum())
            .collect();
        let total: u64 = self
            .all_trips()
            .map(|t| {
                per_service[t.service as usize]
                    * self.patterns[t.pattern as usize].stops.len() as u64
            })
            .sum();
        total as f64 / f64::from(self.days)
    }

    /// Index of the calendar's first day within its week; 0 is Monday.
    fn first_weekday(&self) -> u32 {
        gtfs::weekday(self.first_day)
    }

    /// Number of weeks, Monday to Sunday, that the calendar touches.
    pub fn weeks(&self) -> usize {
        if self.days == 0 {
            0
        } else {
            (self.first_weekday() + self.days).div_ceil(7) as usize
        }
    }

    /// Sorts stations by their Hilbert key and renumbers the patterns'
    /// references; drops unused stations, patterns and services; sorts
    /// trips and frequencies.
    pub fn normalize(&mut self) {
        // Stations.
        let mut used = vec![false; self.stations.len()];
        for p in &self.patterns {
            for s in &p.stops {
                used[s.station as usize] = true;
            }
        }
        let mut order: Vec<usize> = (0..self.stations.len()).filter(|&i| used[i]).collect();
        order.sort_by(|&a, &b| {
            let (sa, sb) = (&self.stations[a], &self.stations[b]);
            sa.sort_key()
                .cmp(&sb.sort_key())
                .then_with(|| sa.id.cmp(&sb.id))
        });
        let mut remap = vec![u32::MAX; self.stations.len()];
        for (new, &old) in order.iter().enumerate() {
            remap[old] = new as u32;
        }
        let mut stations = std::mem::take(&mut self.stations);
        self.stations = order
            .iter()
            .map(|&i| std::mem::take(&mut stations[i]))
            .collect();
        for p in &mut self.patterns {
            for s in &mut p.stops {
                s.station = remap[s.station as usize];
            }
        }

        // Patterns: merge equal ones and drop unused ones.
        let mut used = vec![false; self.patterns.len()];
        for t in &self.trips {
            used[t.pattern as usize] = true;
        }
        for f in &self.frequencies {
            used[f.pattern as usize] = true;
        }
        let mut index = std::collections::HashMap::new();
        let mut remap = vec![u32::MAX; self.patterns.len()];
        let mut patterns = Vec::new();
        for (i, p) in std::mem::take(&mut self.patterns).into_iter().enumerate() {
            if used[i] {
                let n = *index.entry(p.clone()).or_insert_with(|| {
                    patterns.push(p);
                    patterns.len() as u32 - 1
                });
                remap[i] = n;
            }
        }
        self.patterns = patterns;

        // Services: the same.
        let mut used = vec![false; self.services.len()];
        for t in &self.trips {
            used[t.service as usize] = true;
        }
        for f in &self.frequencies {
            used[f.service as usize] = true;
        }
        let mut index = std::collections::HashMap::new();
        let mut sremap = vec![u32::MAX; self.services.len()];
        let mut services = Vec::new();
        for (i, s) in std::mem::take(&mut self.services).into_iter().enumerate() {
            if used[i] {
                let n = *index.entry(s.clone()).or_insert_with(|| {
                    services.push(s);
                    services.len() as u32 - 1
                });
                sremap[i] = n;
            }
        }
        self.services = services;

        for t in &mut self.trips {
            t.pattern = remap[t.pattern as usize];
            t.service = sremap[t.service as usize];
        }
        for f in &mut self.frequencies {
            f.pattern = remap[f.pattern as usize];
            f.service = sremap[f.service as usize];
        }
        self.trips.sort_unstable();
        self.frequencies.sort_unstable();
    }

    /// Like [`totals`](Self::totals), without the duplicates in
    /// `dropped`: trips as [`all_trips`](Self::all_trips) numbers them,
    /// sorted, with the calendar days they are duplicates on.
    pub fn totals_without(&self, dropped: &[(u32, Service)]) -> Totals {
        let mut t = self.totals();
        self.subtract(&mut t, dropped);
        t
    }

    /// Takes the `dropped` trips out of `t`, which holds this feed's
    /// [`totals`](Self::totals), such as the ones stored with it.
    pub fn subtract(&self, t: &mut Totals, dropped: &[(u32, Service)]) {
        if dropped.is_empty() {
            return;
        }
        let offset = self.first_weekday();
        let mut dropped = dropped.iter().peekable();
        for (i, trip) in self.all_trips().enumerate() {
            let Some((_, days)) = dropped.next_if(|(n, _)| *n as usize == i) else {
                if dropped.peek().is_none() {
                    break;
                }
                continue;
            };
            let own = &self.services[trip.service as usize];
            for d in (0..self.days).filter(|&d| days.runs(d) && own.runs(d)) {
                let w = ((d + offset) / 7) as usize;
                for s in &self.patterns[trip.pattern as usize].stops {
                    let v = &mut t.values[s.station as usize * t.weeks + w];
                    *v = v.saturating_sub(1);
                }
            }
        }
    }

    /// Days of each week, as [`totals`](Self::totals) numbers the
    /// weeks, that the calendar covers.
    pub fn covered_days(&self) -> Vec<u8> {
        let offset = self.first_weekday();
        let mut out = vec![0u8; self.weeks()];
        for d in 0..self.days {
            out[((d + offset) / 7) as usize] += 1;
        }
        out
    }

    /// Departures at each station in each week the calendar touches.
    pub fn totals(&self) -> Totals {
        let weeks = self.weeks();
        let offset = self.first_weekday();
        // Days per week that each service runs on.
        let per_service: Vec<Vec<u8>> = self
            .services
            .iter()
            .map(|s| {
                let mut w = vec![0u8; weeks];
                for d in 0..self.days {
                    if s.runs(d) {
                        w[((d + offset) / 7) as usize] += 1;
                    }
                }
                w
            })
            .collect();
        // Trips per week of each pattern.
        let mut per_pattern = vec![0u64; self.patterns.len() * weeks];
        let mut add = |pattern: u32, service: u32, n: u64| {
            let p = &mut per_pattern[pattern as usize * weeks..][..weeks];
            for (v, &d) in p.iter_mut().zip(&per_service[service as usize]) {
                *v += n * u64::from(d);
            }
        };
        for t in &self.trips {
            add(t.pattern, t.service, 1);
        }
        for f in &self.frequencies {
            add(f.pattern, f.service, u64::from(f.count()));
        }
        let mut values = vec![0u64; self.stations.len() * weeks];
        for (i, p) in self.patterns.iter().enumerate() {
            let src = &per_pattern[i * weeks..][..weeks];
            for s in &p.stops {
                let dst = &mut values[s.station as usize * weeks..][..weeks];
                for (d, v) in dst.iter_mut().zip(src) {
                    *d += v;
                }
            }
        }
        Totals {
            first_monday: self.first_day - i64::from(offset),
            weeks,
            values,
        }
    }

    /// Serializes to the format described in the module docs. The feed
    /// must be normalized.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.check()?;
        ensure!(
            self.stations
                .windows(2)
                .all(|w| w[0].sort_key() <= w[1].sort_key()),
            "stations are not sorted; call normalize() first"
        );
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(FORMAT);

        let mut b = Vec::new();
        put_varint(&mut b, u64::from(self.condenser));
        put_signed(&mut b, self.first_day);
        put_varint(&mut b, u64::from(self.days));
        put_signed(&mut b, self.last_day);
        put_varint(&mut b, self.inputs.len() as u64);
        for i in &self.inputs {
            put_str(&mut b, &i.url);
            put_str(&mut b, &i.final_url);
            put_str(&mut b, i.etag.as_deref().unwrap_or(""));
            put_str(&mut b, i.last_modified.as_deref().unwrap_or(""));
            b.extend_from_slice(&i.sha256);
            put_bytes(&mut b, &i.sha512);
            put_varint(&mut b, i.bytes);
            put_signed(&mut b, i.first_seen);
        }
        put_varint(&mut b, self.agencies.len() as u64);
        for a in &self.agencies {
            put_str(&mut b, a);
        }
        put_section(&mut out, &b)?;

        let totals = self.totals();
        b.clear();
        put_varint(&mut b, totals.weeks as u64);
        put_varint(&mut b, self.stations.len() as u64);
        for i in 0..self.stations.len() {
            let mut prev = 0i64;
            for &v in totals.station(i) {
                put_signed(&mut b, v as i64 - prev);
                prev = v as i64;
            }
        }
        put_section(&mut out, &b)?;

        b.clear();
        put_varint(&mut b, self.stations.len() as u64);
        let (mut px, mut py) = (0i64, 0i64);
        for s in &self.stations {
            put_signed(&mut b, i64::from(s.x) - px);
            put_signed(&mut b, i64::from(s.y) - py);
            (px, py) = (i64::from(s.x), i64::from(s.y));
        }
        // IDs share prefixes with the previous one, often a lot.
        let mut prev: &[u8] = b"";
        for s in &self.stations {
            let id = s.id.as_bytes();
            let common = id.iter().zip(prev).take_while(|(a, b)| a == b).count();
            put_varint(&mut b, common as u64);
            put_bytes(&mut b, &id[common..]);
            prev = id;
        }
        put_section(&mut out, &b)?;

        b.clear();
        put_varint(&mut b, self.patterns.len() as u64);
        for p in &self.patterns {
            put_varint(&mut b, u64::from(p.agency));
            put_varint(&mut b, u64::from(p.route_type));
            put_varint(&mut b, p.stops.len() as u64);
            let (mut station, mut offset) = (0i64, 0i64);
            for s in &p.stops {
                put_signed(&mut b, i64::from(s.station) - station);
                station = i64::from(s.station);
                if s.offset == NO_TIME {
                    put_varint(&mut b, 0);
                } else {
                    put_varint(&mut b, zigzag(i64::from(s.offset) - offset) + 1);
                    offset = i64::from(s.offset);
                }
            }
        }
        put_section(&mut out, &b)?;

        b.clear();
        put_varint(&mut b, self.trips.len() as u64);
        let mut prev = Trip::default();
        for t in &self.trips {
            put_varint(&mut b, u64::from(t.pattern - prev.pattern));
            if t.pattern != prev.pattern {
                prev = Trip::default();
            }
            put_signed(&mut b, i64::from(t.service) - i64::from(prev.service));
            put_signed(&mut b, i64::from(t.start) - i64::from(prev.start));
            prev = *t;
        }
        put_section(&mut out, &b)?;

        b.clear();
        put_varint(&mut b, self.frequencies.len() as u64);
        for f in &self.frequencies {
            put_varint(&mut b, u64::from(f.pattern));
            put_varint(&mut b, u64::from(f.service));
            put_varint(&mut b, u64::from(f.start));
            put_varint(&mut b, u64::from(f.end - f.start));
            put_varint(&mut b, u64::from(f.headway));
        }
        put_section(&mut out, &b)?;

        b.clear();
        put_varint(&mut b, self.services.len() as u64);
        for s in &self.services {
            b.extend_from_slice(&s.0);
        }
        put_section(&mut out, &b)?;
        Ok(out)
    }

    /// Checks that all indices are in range.
    fn check(&self) -> Result<()> {
        ensure!(self.days <= MAX_DAYS, "calendar of {} days", self.days);
        let bytes = self.days.div_ceil(8) as usize;
        for s in &self.services {
            ensure!(s.0.len() == bytes, "service bitmap of {} bytes", s.0.len());
        }
        for p in &self.patterns {
            ensure!(
                (p.agency as usize) < self.agencies.len().max(1),
                "bad agency"
            );
            for s in &p.stops {
                ensure!((s.station as usize) < self.stations.len(), "bad station");
            }
        }
        let (np, ns) = (self.patterns.len(), self.services.len());
        for t in &self.trips {
            ensure!((t.pattern as usize) < np, "bad pattern");
            ensure!((t.service as usize) < ns, "bad service");
        }
        for f in &self.frequencies {
            ensure!((f.pattern as usize) < np, "bad pattern");
            ensure!((f.service as usize) < ns, "bad service");
            ensure!(f.start <= f.end, "frequency ends before it starts");
        }
        ensure!(
            self.trips.is_sorted() && self.frequencies.is_sorted(),
            "trips are not sorted; call normalize() first"
        );
        Ok(())
    }

    pub fn decode(data: &[u8]) -> Result<Feed> {
        Ok(Self::decode_with_totals(data)?.0)
    }

    /// Decodes a feed and the weekly totals stored with it, which equal
    /// [`totals`](Self::totals) without computing them again.
    pub fn decode_with_totals(data: &[u8]) -> Result<(Feed, Totals)> {
        let mut r = Sections::new(data)?;
        let mut f = Feed::default();
        r.next(|r| {
            read_header(r, &mut f)?;
            for _ in 0..r.varint()? {
                f.agencies.push(r.string()?);
            }
            Ok(())
        })?;
        let mut totals = Totals::default();
        r.next(|r| {
            totals = read_totals(r)?;
            Ok(())
        })?;
        r.next(|r| {
            let n = r.count()?;
            let (mut x, mut y) = (0i64, 0i64);
            for _ in 0..n {
                x += r.signed()?;
                y += r.signed()?;
                f.stations.push(Station {
                    x: u32::try_from(x)?,
                    y: u32::try_from(y)?,
                    id: String::new(),
                });
            }
            let mut prev: Vec<u8> = Vec::new();
            for s in &mut f.stations {
                let common = usize::try_from(r.varint()?)?;
                ensure!(common <= prev.len(), "bad station ID");
                let len = r.len()?;
                prev.truncate(common);
                prev.extend_from_slice(&r.bytes(len)?);
                s.id = String::from_utf8(prev.clone())?;
            }
            Ok(())
        })?;
        r.next(|r| {
            for _ in 0..r.count()? {
                let agency = u32::try_from(r.varint()?)?;
                let route_type = u16::try_from(r.varint()?)?;
                let n = r.count()?;
                let mut stops = Vec::with_capacity(n);
                let (mut station, mut offset) = (0i64, 0i64);
                for _ in 0..n {
                    station += r.signed()?;
                    let o = r.varint()?;
                    let off = if o == 0 {
                        NO_TIME
                    } else {
                        offset += unzigzag(o - 1);
                        u16::try_from(offset)
                            .ok()
                            .filter(|&v| v != NO_TIME)
                            .context("bad offset")?
                    };
                    stops.push(PatternStop {
                        station: u32::try_from(station)?,
                        offset: off,
                    });
                }
                f.patterns.push(Pattern {
                    agency,
                    route_type,
                    stops,
                });
            }
            Ok(())
        })?;
        r.next(|r| {
            let n = r.count()?;
            f.trips.reserve(n);
            let mut prev = Trip::default();
            for _ in 0..n {
                let dp = u32::try_from(r.varint()?)?;
                let pattern = prev.pattern.checked_add(dp).context("bad pattern")?;
                if dp != 0 {
                    prev = Trip::default();
                }
                let t = Trip {
                    pattern,
                    service: u32::try_from(i64::from(prev.service) + r.signed()?)?,
                    start: u16::try_from(i64::from(prev.start) + r.signed()?)?,
                };
                f.trips.push(t);
                prev = t;
            }
            Ok(())
        })?;
        r.next(|r| {
            for _ in 0..r.count()? {
                let pattern = u32::try_from(r.varint()?)?;
                let service = u32::try_from(r.varint()?)?;
                let start = u32::try_from(r.varint()?)?;
                let end = start
                    .checked_add(u32::try_from(r.varint()?)?)
                    .context("bad frequency")?;
                let headway = u32::try_from(r.varint()?)?;
                f.frequencies.push(Frequency {
                    pattern,
                    service,
                    start,
                    end,
                    headway,
                });
            }
            Ok(())
        })?;
        r.next(|r| {
            let bytes = f.days.div_ceil(8) as usize;
            for _ in 0..r.count()? {
                f.services.push(Service(r.bytes(bytes)?));
            }
            Ok(())
        })?;
        ensure!(r.at_end(), "trailing data");
        f.check()?;
        ensure!(
            totals.weeks == f.weeks() && totals.values.len() == f.stations.len() * f.weeks(),
            "totals do not match the calendar"
        );
        totals.first_monday = f.first_day - i64::from(f.first_weekday());
        Ok((f, totals))
    }
}

/// Reads only the condenser version and the inputs of an encoded
/// feed, which is all a run needs to tell whether the file is current.
pub fn decode_inputs(data: &[u8]) -> Result<(u32, Vec<Input>)> {
    let mut r = Sections::new(data)?;
    let mut f = Feed::default();
    r.next_partial(|r| read_header(r, &mut f))?;
    Ok((f.condenser, f.inputs))
}

/// Reads the start of the header, up to and including the inputs.
fn read_header(r: &mut Reader, f: &mut Feed) -> Result<()> {
    f.condenser = u32::try_from(r.varint()?)?;
    f.first_day = r.signed()?;
    f.days = u32::try_from(r.varint()?)?;
    f.last_day = r.signed()?;
    for _ in 0..r.varint()? {
        let url = r.string()?;
        let final_url = r.string()?;
        let etag = Some(r.string()?).filter(|s| !s.is_empty());
        let last_modified = Some(r.string()?).filter(|s| !s.is_empty());
        let sha256 = r.bytes(32)?.try_into().unwrap();
        let len = r.len()?;
        let sha512 = r.bytes(len)?;
        f.inputs.push(Input {
            url,
            final_url,
            etag,
            last_modified,
            sha256,
            sha512,
            bytes: r.varint()?,
            first_seen: r.signed()?,
        });
    }
    Ok(())
}

/// Reads only the header and the weekly totals of an encoded feed.
pub fn decode_totals(data: &[u8]) -> Result<(i64, Totals)> {
    let mut r = Sections::new(data)?;
    let mut first_day = 0;
    r.next_partial(|r| {
        r.varint()?;
        first_day = r.signed()?;
        Ok(())
    })?;
    let mut totals = Totals::default();
    r.next(|r| {
        totals = read_totals(r)?;
        Ok(())
    })?;
    totals.first_monday = first_day - i64::from(gtfs::weekday(first_day));
    Ok((first_day, totals))
}

fn read_totals(r: &mut Reader) -> Result<Totals> {
    let weeks = r.count()?;
    let n = r.count()?;
    let mut values = Vec::with_capacity(n.saturating_mul(weeks).min(1 << 24));
    for _ in 0..n {
        let mut v = 0i64;
        for _ in 0..weeks {
            v += r.signed()?;
            values.push(u64::try_from(v)?);
        }
    }
    Ok(Totals {
        first_monday: 0,
        weeks,
        values,
    })
}

fn put_section(out: &mut Vec<u8>, b: &[u8]) -> Result<()> {
    let z = zstd::bulk::compress(b, ZSTD_LEVEL)?;
    out.extend_from_slice(&u32::try_from(z.len())?.to_le_bytes());
    out.extend_from_slice(&z);
    Ok(())
}

fn put_varint(b: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        b.push((v as u8) | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

fn put_signed(b: &mut Vec<u8>, v: i64) {
    put_varint(b, zigzag(v));
}

fn put_bytes(b: &mut Vec<u8>, s: &[u8]) {
    put_varint(b, s.len() as u64);
    b.extend_from_slice(s);
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    put_bytes(b, s.as_bytes());
}

/// The sections of an encoded feed, decompressed one at a time.
struct Sections<'a> {
    rest: &'a [u8],
}

impl<'a> Sections<'a> {
    fn new(data: &'a [u8]) -> Result<Self> {
        ensure!(
            data.len() > MAGIC.len() && &data[..MAGIC.len()] == MAGIC,
            "not a condensed transit feed"
        );
        let version = data[MAGIC.len()];
        ensure!(
            version == FORMAT,
            "condensed format version {version}, expected {FORMAT}"
        );
        Ok(Sections {
            rest: &data[MAGIC.len() + 1..],
        })
    }

    fn next(&mut self, read: impl FnOnce(&mut Reader) -> Result<()>) -> Result<()> {
        self.read(true, read)
    }

    /// Like [`next`](Self::next), for a reader that stops early.
    fn next_partial(&mut self, read: impl FnOnce(&mut Reader) -> Result<()>) -> Result<()> {
        self.read(false, read)
    }

    fn read(&mut self, whole: bool, read: impl FnOnce(&mut Reader) -> Result<()>) -> Result<()> {
        ensure!(self.rest.len() >= 4, "truncated");
        let len = u32::from_le_bytes(self.rest[..4].try_into().unwrap()) as usize;
        ensure!(self.rest.len() >= 4 + len, "truncated");
        let frame = &self.rest[4..4 + len];
        self.rest = &self.rest[4 + len..];
        let mut r = Reader {
            r: std::io::BufReader::with_capacity(1 << 16, zstd::stream::read::Decoder::new(frame)?),
        };
        read(&mut r)?;
        ensure!(!whole || r.at_end()?, "trailing data in section");
        Ok(())
    }

    fn at_end(&self) -> bool {
        self.rest.is_empty()
    }
}

struct Reader<'a> {
    r: std::io::BufReader<zstd::stream::read::Decoder<'a, std::io::BufReader<&'a [u8]>>>,
}

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        ensure!(n <= MAX_FIELD, "corrupt length {n}");
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
        Ok(unzigzag(self.varint()?))
    }
    /// A length of a byte field.
    fn len(&mut self) -> Result<usize> {
        let n = usize::try_from(self.varint()?)?;
        ensure!(n <= MAX_FIELD, "corrupt length {n}");
        Ok(n)
    }
    /// A number of records; each takes at least a byte, so a corrupt
    /// count cannot make us reserve much memory.
    fn count(&mut self) -> Result<usize> {
        Ok(usize::try_from(self.varint()?)?)
    }
    fn string(&mut self) -> Result<String> {
        let n = self.len()?;
        Ok(String::from_utf8(self.bytes(n)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(lon: f64, lat: f64, id: &str) -> Station {
        let (x, y) = station_xy(lon, lat).unwrap();
        Station {
            x,
            y,
            id: id.into(),
        }
    }

    fn service(days: u32, on: &[u32]) -> Service {
        let mut s = Service::new(days);
        for &d in on {
            s.set(d);
        }
        s
    }

    /// Two lines in Zürich, starting on a Wednesday.
    fn sample() -> Feed {
        let days = 10;
        let mut f = Feed {
            condenser: 7,
            inputs: vec![Input {
                url: "https://example.org/gtfs.zip".into(),
                final_url: "https://cdn.example.org/gtfs.zip".into(),
                etag: Some("\"abc\"".into()),
                last_modified: None,
                sha256: [1; 32],
                sha512: vec![2; 64],
                bytes: 12345,
                first_seen: 1_790_000_000,
            }],
            // 2026-06-03, a Wednesday.
            first_day: crate::date::days_from_civil(2026, 6, 3),
            last_day: crate::date::days_from_civil(2026, 6, 12),
            days,
            agencies: vec!["VBZ".into(), "SBB".into()],
            stations: vec![
                station(8.5402, 47.3782, "8503000"),
                station(8.5487, 47.3667, "8503003"),
                station(8.5170, 47.3910, "8503006"),
                station(8.5170, 47.3910, "8503006:0:1"),
                station(0.0, 0.0, "unused"),
            ],
            patterns: vec![
                Pattern {
                    agency: 0,
                    route_type: 0,
                    stops: vec![
                        PatternStop {
                            station: 0,
                            offset: 0,
                        },
                        PatternStop {
                            station: 1,
                            offset: NO_TIME,
                        },
                        PatternStop {
                            station: 2,
                            offset: 7,
                        },
                    ],
                },
                Pattern {
                    agency: 1,
                    route_type: 2,
                    stops: vec![
                        PatternStop {
                            station: 3,
                            offset: 0,
                        },
                        PatternStop {
                            station: 0,
                            offset: 6,
                        },
                    ],
                },
                // A copy of the first one.
                Pattern {
                    agency: 0,
                    route_type: 0,
                    stops: vec![
                        PatternStop {
                            station: 0,
                            offset: 0,
                        },
                        PatternStop {
                            station: 1,
                            offset: NO_TIME,
                        },
                        PatternStop {
                            station: 2,
                            offset: 7,
                        },
                    ],
                },
            ],
            trips: vec![
                Trip {
                    pattern: 2,
                    service: 0,
                    start: 25 * 60 + 3,
                },
                Trip {
                    pattern: 0,
                    service: 0,
                    start: 8 * 60,
                },
                Trip {
                    pattern: 1,
                    service: 2,
                    start: 9 * 60,
                },
            ],
            frequencies: vec![Frequency {
                pattern: 1,
                service: 1,
                start: 6 * 3600,
                end: 7 * 3600,
                headway: 900,
            }],
            services: vec![
                // Every day.
                service(days, &(0..days).collect::<Vec<_>>()),
                // Saturdays: days 3 and 10 would be, but 10 is past the end.
                service(days, &[3]),
                // Equal to the first.
                service(days, &(0..days).collect::<Vec<_>>()),
            ],
        };
        f.normalize();
        f
    }

    #[test]
    fn normalize_sorts_and_merges() {
        let f = sample();
        assert_eq!(f.stations.len(), 4, "unused station dropped");
        assert!(
            f.stations
                .windows(2)
                .all(|w| w[0].sort_key() <= w[1].sort_key())
        );
        assert_eq!(f.patterns.len(), 2, "equal patterns merged");
        assert_eq!(f.services.len(), 2, "equal services merged");
        assert_eq!(f.trips.len(), 3);
        assert!(f.trips.is_sorted());
        // The VBZ pattern still starts at Zürich HB.
        let vbz = f.patterns.iter().find(|p| p.agency == 0).unwrap();
        assert_eq!(f.stations[vbz.stops[0].station as usize].id, "8503000");
    }

    #[test]
    fn round_trip() {
        let f = sample();
        let data = f.encode().unwrap();
        assert_eq!(Feed::decode(&data).unwrap(), f);
        assert_eq!(decode_inputs(&data).unwrap(), (7, f.inputs.clone()));
    }

    #[test]
    fn totals_by_week() {
        let f = sample();
        // Wednesday to the next Friday: two weeks, of 5 and 5 days.
        assert_eq!(f.weeks(), 2);
        let t = f.totals();
        assert_eq!(t.first_monday, f.first_day - 2);
        let at = |id: &str| {
            let i = f.stations.iter().position(|s| s.id == id).unwrap();
            t.station(i).to_vec()
        };
        // Zürich HB: two VBZ trips daily, the SBB trip daily at its
        // second stop, and four frequency trips on the one Saturday.
        assert_eq!(at("8503000"), [5 * 3 + 4, 5 * 3]);
        // Stadelhofen: VBZ trips only.
        assert_eq!(at("8503003"), [10, 10]);
        // The SBB platform.
        assert_eq!(at("8503006:0:1"), [5 + 4, 5]);
        let (_, decoded) = decode_totals(&f.encode().unwrap()).unwrap();
        assert_eq!(decoded, t);
        let (_, stored) = Feed::decode_with_totals(&f.encode().unwrap()).unwrap();
        assert_eq!(stored, t);
    }

    #[test]
    fn frequency_count() {
        let f = |start, end, headway| {
            Frequency {
                start,
                end,
                headway,
                ..Default::default()
            }
            .count()
        };
        assert_eq!(f(0, 3600, 900), 4);
        assert_eq!(f(0, 3601, 900), 5);
        assert_eq!(f(0, 0, 900), 0);
        assert_eq!(f(0, 3600, 0), 0);
    }

    #[test]
    fn rejects_bad_input() {
        let data = sample().encode().unwrap();
        assert!(Feed::decode(&data[..data.len() - 1]).is_err());
        let mut v1 = data.clone();
        v1[8] = 1;
        let err = Feed::decode(&v1).unwrap_err().to_string();
        assert!(err.contains("version 1"), "{err}");
        assert!(Feed::decode(b"not a feed at all").is_err());
        let mut unsorted = sample();
        unsorted.stations.swap(0, 3);
        assert!(unsorted.encode().is_err());
    }

    #[test]
    fn empty_feed() {
        let mut f = Feed::default();
        f.normalize();
        let data = f.encode().unwrap();
        assert_eq!(Feed::decode(&data).unwrap(), f);
        assert_eq!(f.totals().values.len(), 0);
    }

    #[test]
    fn station_positions() {
        let (x, y) = station_xy(8.5402, 47.3782).unwrap();
        let (px, py) = tile::pixel_xy(8.5402, 47.3782).unwrap();
        let s = Station {
            x,
            y,
            id: String::new(),
        };
        assert_eq!(s.pixel(), (px, py));
        assert!(station_xy(0.0, 89.0).is_none());
    }
}
