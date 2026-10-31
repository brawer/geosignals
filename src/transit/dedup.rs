// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Finds trips that two feeds both contain, so they count only once.
//!
//! National feeds often include buses from across the border, and
//! regional feeds overlap with national ones. We compare every pair of
//! feeds that have stations in the same zoom-15 tile (about 1 km), on
//! the days both calendars cover. For such a pair, a departure of the
//! smaller feed (by departures on an average day) matches on a day if
//! the larger feed departs that day within one pixel (about 150 m) and
//! one minute of it. A trip of the smaller feed whose departures match
//! at least 80% (and at least two) is a duplicate on that day, and we
//! drop it there. So the copy in the largest feed survives, whatever
//! order we look at pairs in.
//!
//! Comparing on dates, not weekdays, keeps a summer timetable from
//! matching a winter one, and a holiday from matching a normal day.
//! Feeds whose calendars share no day are not compared: no day has
//! both, so nothing counts twice.
//!
//! Only the larger feed's stations near the smaller feed, and the trips
//! through them, are looked at, so a pair costs about as much as the
//! area the two feeds share.
//!
//! Comparing two feeds' independent services this way rarely matches:
//! shifting one feed's times by seven minutes, Switzerland against
//! Germany matched 0.05% of trips, against 3.8% unshifted.

use super::condensed::{Feed, NO_TIME, Service};
use anyhow::Result;
use std::collections::{HashMap, HashSet};

/// Share of a trip's departures that must match, in percent.
const MATCH_PERCENT: usize = 80;

/// Zoom level of the tiles for finding feeds that overlap.
const CELL_SHIFT: u32 = 3; // zoom 18 - 3 = 15

/// What we need to know of a feed to find the feeds it overlaps.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outline {
    /// Zoom-15 tiles with stations, sorted.
    pub cells: Vec<u64>,
    /// Departures on an average day; the larger feed's copy survives.
    pub size: f64,
}

impl Outline {
    pub fn new(feed: &Feed) -> Outline {
        let mut cells: Vec<u64> = feed
            .stations
            .iter()
            .map(|s| {
                let (x, y) = s.pixel();
                cell(x, y)
            })
            .collect();
        cells.sort_unstable();
        cells.dedup();
        Outline {
            cells,
            size: feed.departures_per_day(),
        }
    }
}

/// Duplicates that one pair of feeds has.
#[derive(Clone, Debug, PartialEq)]
pub struct Overlap {
    /// Index of the feed whose trips get dropped.
    pub smaller: usize,
    pub larger: usize,
    /// Days both calendars cover.
    pub days: u32,
    /// Departures of `smaller` that are duplicates, over those days.
    pub departures: u64,
    /// The same, by agency of `smaller`.
    pub by_agency: Vec<(String, u64)>,
}

/// Trips of a feed, numbered as [`Feed::all_trips`] does, sorted, with
/// the calendar days on which they are duplicates.
pub type Dropped = Vec<(u32, Service)>;

fn cell(x: u32, y: u32) -> u64 {
    (u64::from(x >> CELL_SHIFT) << 32) | u64::from(y >> CELL_SHIFT)
}

const MINUTES: u16 = 0x2000;

fn key(x: u32, y: u32, minute: u16) -> u64 {
    (u64::from(x) << 31) | (u64::from(y) << 13) | u64::from(minute % MINUTES)
}

/// Days as a bitmap, wide enough for any calendar.
type Days = [u64; 6];

const _: () = assert!(64 * 6 >= super::condensed::MAX_DAYS as usize);

/// Finds the duplicates among feeds. `outlines` describe the feeds,
/// and `load` decodes one on demand, so only the feeds being compared
/// are in memory. Returns, for each feed, its trips that are
/// duplicates of a larger feed's, and the overlaps found.
pub fn find(
    outlines: &[&Outline],
    load: impl Fn(usize) -> Result<Feed> + Sync,
    threads: usize,
) -> Result<(Vec<Dropped>, Vec<Overlap>)> {
    let mut by_cell: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, o) in outlines.iter().enumerate() {
        for &c in &o.cells {
            by_cell.entry(c).or_default().push(i);
        }
    }
    let smaller = |a: usize, b: usize| {
        // Ties go by position in the feed list.
        let (sa, sb) = (outlines[a].size, outlines[b].size);
        sa < sb || (sa == sb && a < b)
    };
    let mut pairs: HashSet<(usize, usize)> = HashSet::new();
    for ids in by_cell.values() {
        for (n, &a) in ids.iter().enumerate() {
            for &b in &ids[n + 1..] {
                pairs.insert(if smaller(a, b) { (a, b) } else { (b, a) });
            }
        }
    }
    drop(by_cell);
    // Work in batches that share the larger feed, which is the costly
    // one to load.
    let mut pairs: Vec<(usize, usize)> = pairs.into_iter().collect();
    pairs.sort_unstable_by_key(|&(s, l)| (l, s));
    let batches: Vec<&[(usize, usize)]> = pairs
        .chunk_by(|a, b| a.1 == b.1)
        .flat_map(|g| g.chunks(8))
        .collect();

    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<Result<Vec<(usize, Dropped, Overlap)>>> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.max(1))
            .map(|_| {
                s.spawn(|| -> Result<Vec<(usize, Dropped, Overlap)>> {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(batch) = batches.get(i) else {
                            return Ok(out);
                        };
                        let large_index = batch[0].1;
                        let large = load(large_index)?;
                        let large_cells: HashSet<u64> =
                            outlines[large_index].cells.iter().copied().collect();
                        for &(small_index, _) in batch.iter() {
                            let small = load(small_index)?;
                            let (dropped, overlap) = compare(&small, &large, &large_cells);
                            if overlap.departures > 0 {
                                out.push((
                                    small_index,
                                    dropped,
                                    Overlap {
                                        smaller: small_index,
                                        larger: large_index,
                                        ..overlap
                                    },
                                ));
                            }
                        }
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("dedup worker panicked"))
            .collect()
    });

    let mut dropped: Vec<HashMap<u32, Service>> = vec![HashMap::new(); outlines.len()];
    let mut overlaps = Vec::new();
    for r in results {
        for (feed, trips, overlap) in r? {
            for (t, days) in trips {
                match dropped[feed].entry(t) {
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(days);
                    }
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        for (a, b) in e.get_mut().0.iter_mut().zip(&days.0) {
                            *a |= b;
                        }
                    }
                }
            }
            overlaps.push(overlap);
        }
    }
    let dropped = dropped
        .into_iter()
        .map(|m| {
            let mut v: Dropped = m.into_iter().collect();
            v.sort_unstable_by_key(|(t, _)| *t);
            v
        })
        .collect();
    overlaps.sort_by(|a, b| {
        b.departures
            .cmp(&a.departures)
            .then(a.smaller.cmp(&b.smaller))
    });
    Ok((dropped, overlaps))
}

/// The days of `s` from day `from` of its calendar, `len` of them.
fn window(s: &Service, from: i64, len: u32) -> Days {
    let mut out = Days::default();
    for d in 0..len {
        let day = from + i64::from(d);
        if day >= 0 && s.runs(day as u32) {
            out[(d / 64) as usize] |= 1 << (d % 64);
        }
    }
    out
}

/// Compares `small` against `large` on the days both cover; returns the
/// trips of `small` with the days on which they are duplicates.
/// `smaller` and `larger` in the result are left for the caller.
fn compare(small: &Feed, large: &Feed, large_cells: &HashSet<u64>) -> (Dropped, Overlap) {
    let mut overlap = Overlap {
        smaller: 0,
        larger: 0,
        days: 0,
        departures: 0,
        by_agency: Vec::new(),
    };
    let lo = small.first_day.max(large.first_day);
    let hi = (small.first_day + i64::from(small.days)).min(large.first_day + i64::from(large.days));
    if lo >= hi {
        return (Vec::new(), overlap);
    }
    overlap.days = (hi - lo) as u32;
    // Days count from `lo` in both feeds' services below.
    let (small_from, large_from) = (lo - small.first_day, lo - large.first_day);

    // Tiles of `small`, grown by one so neighbours across an edge count.
    let near_small: HashSet<u64> = small
        .stations
        .iter()
        .map(|s| {
            let (x, y) = s.pixel();
            cell(x, y)
        })
        .collect::<HashSet<u64>>()
        .into_iter()
        .flat_map(|c| {
            let (cx, cy) = ((c >> 32) as i64, (c & 0xffff_ffff) as i64);
            (-1..=1).flat_map(move |dx| (-1..=1).map(move |dy| (cx + dx, cy + dy)))
        })
        .filter(|&(x, y)| x >= 0 && y >= 0)
        .map(|(x, y)| ((x as u64) << 32) | y as u64)
        .collect();

    // Departures of `large` near `small`, as (key, service).
    let near: Vec<bool> = large
        .stations
        .iter()
        .map(|s| {
            let (x, y) = s.pixel();
            near_small.contains(&cell(x, y))
        })
        .collect();
    let touches: Vec<bool> = large
        .patterns
        .iter()
        .map(|p| p.stops.iter().any(|s| near[s.station as usize]))
        .collect();
    let mut entries: Vec<(u64, u32)> = Vec::new();
    for t in large.all_trips() {
        if !touches[t.pattern as usize] {
            continue;
        }
        for s in &large.patterns[t.pattern as usize].stops {
            if s.offset != NO_TIME && near[s.station as usize] {
                let (x, y) = large.stations[s.station as usize].pixel();
                let minute = (u32::from(t.start) + u32::from(s.offset)).min(u32::from(NO_TIME) - 1);
                entries.push((key(x, y, minute as u16), t.service));
            }
        }
    }
    if entries.is_empty() {
        return (Vec::new(), overlap);
    }
    entries.sort_unstable();
    entries.dedup();
    let mut large_days: HashMap<u32, Days> = HashMap::new();

    // Trips of `small`.
    let in_large: Vec<bool> = small
        .stations
        .iter()
        .map(|s| {
            let (x, y) = s.pixel();
            large_cells.contains(&cell(x, y))
        })
        .collect();
    let candidate: Vec<bool> = small
        .patterns
        .iter()
        .map(|p| {
            p.stops.iter().any(|s| in_large[s.station as usize])
                && p.stops.iter().filter(|s| s.offset != NO_TIME).count() >= 2
        })
        .collect();
    let own_days: Vec<Days> = small
        .services
        .iter()
        .map(|s| window(s, small_from, overlap.days))
        .collect();
    let mut dropped = Vec::new();
    let mut by_agency: HashMap<u32, u64> = HashMap::new();
    let mut hits = vec![0u16; overlap.days as usize];
    let mut services = Vec::new();
    for (i, t) in small.all_trips().enumerate() {
        let own = &own_days[t.service as usize];
        if !candidate[t.pattern as usize] || own.iter().all(|&w| w == 0) {
            continue;
        }
        let p = &small.patterns[t.pattern as usize];
        hits.fill(0);
        let mut timed = 0usize;
        for s in p.stops.iter().filter(|s| s.offset != NO_TIME) {
            timed += 1;
            let (x, y) = small.stations[s.station as usize].pixel();
            let minute =
                (u32::from(t.start) + u32::from(s.offset)).min(u32::from(NO_TIME) - 1) as u16;
            services.clear();
            for nx in x.saturating_sub(1)..=x.saturating_add(1) {
                for ny in y.saturating_sub(1)..=y.saturating_add(1) {
                    let lo_m = minute.saturating_sub(1);
                    let hi_m = (minute + 1).min(MINUTES - 1);
                    if lo_m > hi_m {
                        continue;
                    }
                    let from = entries.partition_point(|e| e.0 < key(nx, ny, lo_m));
                    for e in &entries[from..] {
                        if e.0 > key(nx, ny, hi_m) {
                            break;
                        }
                        services.push(e.1);
                    }
                }
            }
            services.sort_unstable();
            services.dedup();
            let mut matched = Days::default();
            for &svc in &services {
                let d = large_days.entry(svc).or_insert_with(|| {
                    window(&large.services[svc as usize], large_from, overlap.days)
                });
                for (m, w) in matched.iter_mut().zip(d.iter()) {
                    *m |= w;
                }
            }
            for (w, (m, o)) in matched.iter().zip(own).enumerate() {
                let mut bits = m & o;
                while bits != 0 {
                    let b = bits.trailing_zeros() as usize;
                    hits[w * 64 + b] += 1;
                    bits &= bits - 1;
                }
            }
        }
        let mut days = Service::new(small.days);
        let mut n = 0u64;
        for (d, &h) in hits.iter().enumerate() {
            if usize::from(h) * 100 >= timed * MATCH_PERCENT && h > 0 {
                days.set(small_from as u32 + d as u32);
                n += 1;
            }
        }
        if n > 0 {
            let departures = n * p.stops.len() as u64;
            overlap.departures += departures;
            *by_agency.entry(p.agency).or_default() += departures;
            dropped.push((i as u32, days));
        }
    }
    let mut by_agency: Vec<(String, u64)> = by_agency
        .into_iter()
        .map(|(a, n)| {
            (
                small.agencies.get(a as usize).cloned().unwrap_or_default(),
                n,
            )
        })
        .collect();
    by_agency.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    overlap.by_agency = by_agency;
    (dropped, overlap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transit::condensed::{Pattern, PatternStop, Station, Trip};

    /// A trip: agency, days of the calendar it runs on, and its
    /// departures as (zoom-18 x, y, minute).
    type TestTrip<'a> = (u32, &'a [u32], &'a [(u32, u32, u16)]);

    /// A feed whose calendar starts on `first_day` and lasts 14 days.
    fn feed(first_day: i64, agencies: &[&str], trips: &[TestTrip]) -> Feed {
        let days = 14;
        let mut f = Feed {
            first_day,
            days,
            last_day: first_day + i64::from(days) - 1,
            agencies: agencies.iter().map(|a| a.to_string()).collect(),
            ..Feed::default()
        };
        for &(agency, on, stops) in trips {
            let mut service = Service::new(days);
            for &d in on {
                service.set(d);
            }
            f.services.push(service);
            let start = stops[0].2;
            let mut pattern = Pattern {
                agency,
                route_type: 3,
                stops: Vec::new(),
            };
            for &(x, y, minute) in stops {
                f.stations.push(Station {
                    x: x << 6,
                    y: y << 6,
                    id: format!("{x},{y}"),
                });
                pattern.stops.push(PatternStop {
                    station: f.stations.len() as u32 - 1,
                    offset: minute - start,
                });
            }
            f.patterns.push(pattern);
            f.trips.push(Trip {
                pattern: f.patterns.len() as u32 - 1,
                service: f.services.len() as u32 - 1,
                start,
            });
        }
        f.normalize();
        f
    }

    fn run(feeds: &[Feed]) -> (Vec<Dropped>, Vec<Overlap>) {
        let outlines: Vec<Outline> = feeds.iter().map(Outline::new).collect();
        let refs: Vec<&Outline> = outlines.iter().collect();
        find(&refs, |i| Ok(feeds[i].clone()), 2).unwrap()
    }

    /// Days of a dropped trip, as calendar day numbers.
    fn days(s: &Service) -> Vec<u32> {
        (0..14).filter(|&d| s.runs(d)).collect()
    }

    const A: &[(u32, u32, u16)] = &[(1000, 1000, 480), (1010, 1000, 485), (1020, 1000, 490)];
    const ALL: &[u32] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
    const WEEKDAYS: &[u32] = &[0, 1, 2, 3, 4, 7, 8, 9, 10, 11];

    #[test]
    fn drops_copy_from_smaller_feed() {
        // The national feed has the regional bus, one pixel and one
        // minute off, plus a lot of its own service far away.
        let shifted: Vec<(u32, u32, u16)> = A.iter().map(|&(x, y, m)| (x + 1, y, m + 1)).collect();
        let far: Vec<(u32, u32, u16)> = (0..20).map(|i| (90_000 + i, 90_000, 600)).collect();
        let national = feed(
            0,
            &["Rail", "Regio Bus"],
            &[(0, ALL, &far), (1, WEEKDAYS, &shifted)],
        );
        // The regional feed runs the bus every day; the national feed
        // only knows the weekday runs.
        let regional = feed(
            0,
            &["Regio Bus"],
            &[
                (0, ALL, A),
                (0, ALL, &[(1000, 1000, 700), (1010, 1000, 705)]),
            ],
        );
        let (dropped, overlaps) = run(&[national, regional.clone()]);
        assert!(
            dropped[0].is_empty(),
            "nothing dropped from the larger feed"
        );
        assert_eq!(dropped[1].len(), 1);
        let (t, d) = &dropped[1][0];
        assert_eq!(
            regional.patterns[regional.trips[*t as usize].pattern as usize]
                .stops
                .len(),
            3
        );
        assert_eq!(days(d), WEEKDAYS, "weekday runs of the first trip");
        assert_eq!(overlaps.len(), 1);
        let o = &overlaps[0];
        assert_eq!((o.smaller, o.larger, o.days), (1, 0, 14));
        assert_eq!(o.departures, 10 * 3);
        assert_eq!(o.by_agency, [("Regio Bus".to_string(), 30)]);

        // Without its duplicates, the regional feed loses the first
        // trip's ten runs that the national feed has, at 3 stops each.
        let all: u64 = regional.totals().values.iter().sum();
        let without: u64 = regional.totals_without(&dropped[1]).values.iter().sum();
        assert_eq!(all - without, 10 * 3);
    }

    #[test]
    fn compares_only_shared_days() {
        // The same trips, but the calendars overlap on days 7 to 13 of
        // the regional one only.
        let national = feed(
            7,
            &["N"],
            &[(0, ALL, A), (0, ALL, &[(5000, 5000, 1), (5001, 5000, 2)])],
        );
        let regional = feed(0, &["R"], &[(0, ALL, A)]);
        let (dropped, overlaps) = run(&[national, regional]);
        assert_eq!(days(&dropped[1][0].1), [7, 8, 9, 10, 11, 12, 13]);
        assert_eq!(overlaps[0].days, 7);
        assert_eq!(overlaps[0].departures, 7 * 3);

        // Calendars without a common day are not compared.
        let later = feed(14, &["N"], &[(0, ALL, A), (0, ALL, A)]);
        let regional = feed(0, &["R"], &[(0, ALL, A)]);
        let (dropped, overlaps) = run(&[later, regional]);
        assert!(dropped.iter().all(|d| d.is_empty()));
        assert!(overlaps.is_empty());
    }

    #[test]
    fn keeps_different_services() {
        // Same stops, seven minutes apart: a different service.
        let later: Vec<(u32, u32, u16)> = A.iter().map(|&(x, y, m)| (x, y, m + 7)).collect();
        let a = feed(0, &["A"], &[(0, ALL, A)]);
        let b = feed(
            0,
            &["B"],
            &[(0, ALL, &later), (0, &[0], &[(5, 5, 5), (6, 6, 6)])],
        );
        let (dropped, overlaps) = run(&[a.clone(), b]);
        assert!(dropped.iter().all(|d| d.is_empty()));
        assert!(overlaps.is_empty());
        // Partly matching (2 of 3 departures) is not enough either.
        let partly: Vec<(u32, u32, u16)> = vec![A[0], A[1], (1020, 1000, 499)];
        let c = feed(
            0,
            &["C"],
            &[(0, ALL, &partly), (0, &[0], &[(5, 5, 5), (6, 6, 6)])],
        );
        assert!(run(&[a, c]).0.iter().all(|d| d.is_empty()));
    }

    #[test]
    fn far_apart_feeds_are_not_compared() {
        let a = feed(0, &["A"], &[(0, &[0], A)]);
        let b = feed(
            0,
            &["B"],
            &[(0, &[0], &[(200_000, 200_000, 480), (200_001, 200_000, 485)])],
        );
        assert!(run(&[a, b]).1.is_empty());
    }
}
