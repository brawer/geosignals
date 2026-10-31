// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! The weekly history of a feed, for the 12-month average.
//!
//! Many feeds publish only the next few weeks, so a year of departures
//! has to be collected run by run. For each week (Monday to Sunday),
//! the history keeps the departures per zoom-18 pixel, after removing
//! duplicates, and how many days of the week a timetable covered. A
//! later run replaces a week when its timetable covers at least as many
//! days of it, so the latest timetable wins.
//!
//! The signal is the departures on an average day over the 52 weeks up
//! to the run. A week counts if some timetable covered it, or if it
//! lies between the first run that tracked the feed and this run: then
//! no timetable means no service, which counts as zero. Weeks before we
//! started tracking a feed, which no timetable covered, are unknown and
//! left out.

use super::condensed::{Feed, Totals};
use super::gtfs::weekday;
use crate::tile;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::io::Read;

const MAGIC: &[u8; 8] = b"GSTRHIST";
const VERSION: u8 = 1;

/// Weeks in the average.
pub const WEEKS: i64 = 52;

/// Departures at the pixels of one week.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Week {
    /// Days of the week a timetable covered, 1 to 7.
    pub days: u8,
    /// Departures by Hilbert key of the zoom-18 pixel, sorted by key.
    pub pixels: Vec<(u64, u64)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Monday (as a day number) of the week of the first run that
    /// tracked the feed.
    pub first_tracked: i64,
    /// Weeks by the day number of their Monday.
    pub weeks: BTreeMap<i64, Week>,
}

/// Monday of the week of `day`.
pub fn monday(day: i64) -> i64 {
    day - i64::from(weekday(day))
}

impl History {
    /// An empty history, for a feed first tracked on `day`.
    pub fn new(day: i64) -> History {
        History {
            first_tracked: monday(day),
            weeks: BTreeMap::new(),
        }
    }

    /// Adds the weeks of a feed's calendar, as of a run on `day`, with
    /// `totals` from [`Feed::totals_without`], and forgets weeks too
    /// old to count.
    pub fn update(&mut self, feed: &Feed, totals: &Totals, day: i64) {
        self.first_tracked = self.first_tracked.min(monday(day));
        let pixels: Vec<u64> = feed
            .stations
            .iter()
            .map(|s| {
                let (x, y) = s.pixel();
                tile::pixel_key(x, y)
            })
            .collect();
        for (w, &days) in feed.covered_days().iter().enumerate() {
            let start = totals.first_monday + 7 * w as i64;
            if days == 0 || self.weeks.get(&start).is_some_and(|old| old.days > days) {
                continue;
            }
            // Stations are sorted by pixel key, so equal keys are adjacent.
            let mut week = Week {
                days,
                pixels: Vec::new(),
            };
            for (i, &key) in pixels.iter().enumerate() {
                let n = totals.values[i * totals.weeks + w];
                if n == 0 {
                    continue;
                }
                match week.pixels.last_mut() {
                    Some(last) if last.0 == key => last.1 += n,
                    _ => week.pixels.push((key, n)),
                }
            }
            self.weeks.insert(start, week);
        }
        let oldest = monday(day) - 7 * (WEEKS - 1);
        self.weeks.retain(|&m, _| m >= oldest);
    }

    /// Departures on an average day of the 52 weeks up to `day`, by
    /// pixel key, sorted; and the number of weeks that counted.
    pub fn average(&self, day: i64) -> (Vec<(u64, f64)>, usize) {
        let last = monday(day);
        let mut days = 0u64;
        let mut weeks = 0;
        let mut sums: BTreeMap<u64, u64> = BTreeMap::new();
        for i in 0..WEEKS {
            let m = last - 7 * i;
            match self.weeks.get(&m) {
                Some(w) => {
                    days += u64::from(w.days);
                    weeks += 1;
                    for &(k, n) in &w.pixels {
                        *sums.entry(k).or_default() += n;
                    }
                }
                None if m >= self.first_tracked => {
                    days += 7;
                    weeks += 1;
                }
                None => {}
            }
        }
        if days == 0 {
            return (Vec::new(), 0);
        }
        let avg = sums
            .into_iter()
            .map(|(k, n)| (k, n as f64 / days as f64))
            .collect();
        (avg, weeks)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.push(VERSION);
        put_signed(&mut b, self.first_tracked);
        put_varint(&mut b, self.weeks.len() as u64);
        for (&m, w) in &self.weeks {
            put_signed(&mut b, m);
            b.push(w.days);
            put_varint(&mut b, w.pixels.len() as u64);
            let mut prev = 0u64;
            for &(k, n) in &w.pixels {
                put_varint(&mut b, k - prev);
                put_varint(&mut b, n);
                prev = k;
            }
        }
        Ok(zstd::bulk::compress(&b, 9)?)
    }

    pub fn decode(data: &[u8]) -> Result<History> {
        let mut r = zstd::stream::read::Decoder::new(data)?;
        let mut head = [0u8; 9];
        r.read_exact(&mut head).context("truncated history")?;
        ensure!(&head[..8] == MAGIC, "not a transit history");
        ensure!(head[8] == VERSION, "history version {}", head[8]);
        let mut r = Reader(std::io::BufReader::new(r));
        let mut h = History {
            first_tracked: r.signed()?,
            weeks: BTreeMap::new(),
        };
        for _ in 0..r.varint()? {
            let m = r.signed()?;
            let days = r.byte()?;
            ensure!((1..=7).contains(&days), "bad week");
            let n = r.varint()?;
            let mut pixels = Vec::new();
            let mut key = 0u64;
            for _ in 0..n {
                key = key.checked_add(r.varint()?).context("bad pixel")?;
                pixels.push((key, r.varint()?));
            }
            h.weeks.insert(m, Week { days, pixels });
        }
        ensure!(r.at_end()?, "trailing data in history");
        Ok(h)
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

struct Reader<R>(std::io::BufReader<R>);

impl<R: Read> Reader<R> {
    fn byte(&mut self) -> Result<u8> {
        let mut b = [0u8];
        self.0.read_exact(&mut b).context("truncated history")?;
        Ok(b[0])
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
    fn at_end(&mut self) -> Result<bool> {
        use std::io::BufRead;
        Ok(self.0.fill_buf()?.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::date::days_from_civil;
    use crate::transit::condensed::{Pattern, PatternStop, Service, Station, Trip};

    /// One bus a day from one stop, on `days` days from `first_day`.
    fn feed(first_day: i64, days: u32) -> Feed {
        let mut service = Service::new(days);
        for d in 0..days {
            service.set(d);
        }
        let mut f = Feed {
            first_day,
            days,
            last_day: first_day + i64::from(days) - 1,
            agencies: vec!["A".into()],
            stations: vec![
                Station {
                    x: 100 << 6,
                    y: 200 << 6,
                    id: "a".into(),
                },
                Station {
                    x: (100 << 6) + 1,
                    y: 200 << 6,
                    id: "a2".into(),
                },
            ],
            patterns: vec![Pattern {
                agency: 0,
                route_type: 3,
                stops: vec![
                    PatternStop {
                        station: 0,
                        offset: 0,
                    },
                    PatternStop {
                        station: 1,
                        offset: 1,
                    },
                ],
            }],
            trips: vec![Trip {
                pattern: 0,
                service: 0,
                start: 480,
            }],
            services: vec![service],
            ..Feed::default()
        };
        f.normalize();
        f
    }

    #[test]
    fn averages_over_tracked_weeks() {
        let monday = days_from_civil(2026, 6, 1);
        let key = tile::pixel_key(100, 200);
        // A feed for two weeks from Wednesday; both stations share a
        // pixel, so it has 2 departures a day there.
        let f = feed(monday + 2, 14);
        let mut h = History::new(monday + 3);
        h.update(&f, &f.totals(), monday + 3);
        assert_eq!(h.weeks.len(), 3);
        assert_eq!(h.weeks[&monday].days, 5);
        assert_eq!(h.weeks[&monday].pixels, [(key, 10)]);
        assert_eq!(h.weeks[&(monday + 14)].days, 2);
        let (avg, weeks) = h.average(monday + 3);
        assert_eq!((avg, weeks), (vec![(key, 2.0)], 1));

        // Three weeks later, the feed has expired: the two weeks in
        // between count as covered, the third as zero.
        let later = monday + 21;
        let (avg, weeks) = h.average(later);
        assert_eq!(weeks, 4);
        assert_eq!(avg, [(key, 28.0 / (5 + 7 + 2 + 7) as f64)]);

        let data = h.encode().unwrap();
        assert_eq!(History::decode(&data).unwrap(), h);
    }

    #[test]
    fn keeps_the_fuller_week() {
        let monday = days_from_civil(2026, 6, 1);
        let mut h = History::new(monday);
        h.update(&feed(monday, 7), &feed(monday, 7).totals(), monday);
        // A new version covers only Friday to Sunday of that week.
        let short = feed(monday + 4, 3);
        h.update(&short, &short.totals(), monday + 4);
        assert_eq!(h.weeks[&monday].days, 7);
        // Old weeks are forgotten.
        let much_later = monday + 7 * WEEKS;
        h.update(&short, &short.totals(), much_later);
        assert!(h.weeks.is_empty());
        assert_eq!(h.first_tracked, monday);
    }

    #[test]
    fn rejects_garbage() {
        assert!(History::decode(b"nope").is_err());
        let z = zstd::bulk::compress(b"GSTRHIST\x02", 1).unwrap();
        assert!(History::decode(&z).is_err());
    }
}
