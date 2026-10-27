// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Finds trips that two feeds both contain, so they count only once.
//!
//! National feeds often include buses from across the border, and
//! regional feeds overlap with national ones. We compare every pair of
//! feeds that have stops in the same zoom-15 tile (about 1 km). For such
//! a pair, a departure of the smaller feed (by departures per week)
//! matches if the larger feed departs on the same weekday within one
//! pixel (about 150 m) and one minute of it. A trip of the smaller feed
//! whose departures match at least 80% (and at least two) is a
//! duplicate on that weekday, and we drop it there. So the copy in the
//! largest feed survives, whatever order we look at pairs in.
//!
//! Comparing two feeds' independent services this way rarely matches:
//! shifting one feed's times by seven minutes, Switzerland against
//! Germany matched 0.05% of trips, against 3.8% unshifted.

use super::gtfs::{Condensed, NO_TIME};
use std::collections::{HashMap, HashSet};

/// Share of a trip's departures that must match, in percent.
const MATCH_PERCENT: usize = 80;

/// Zoom level of the tiles for finding feeds that overlap.
const CELL_SHIFT: u32 = 3; // zoom 18 - 3 = 15

/// Duplicates that one pair of feeds has.
#[derive(Clone, Debug, PartialEq)]
pub struct Overlap {
    /// Index of the feed whose trips get dropped.
    pub smaller: usize,
    pub larger: usize,
    /// Departures per week of `smaller` that are duplicates.
    pub departures: u64,
    /// The same, by agency of `smaller`.
    pub by_agency: Vec<(String, u64)>,
}

/// Trips of a feed, by index, with the days they are duplicates on.
type Dropped = Vec<(usize, u8)>;

fn cell(x: u32, y: u32) -> u64 {
    (u64::from(x >> CELL_SHIFT) << 32) | u64::from(y >> CELL_SHIFT)
}

fn key(x: u32, y: u32, day: u32, minute: u16) -> u64 {
    (u64::from(x) << 34)
        | (u64::from(y) << 16)
        | (u64::from(day) << 13)
        | u64::from(minute & 0x1fff)
}

/// Returns, for each feed and trip, the days on which the trip is a
/// duplicate of a larger feed's, and the overlaps found.
pub fn find(feeds: &[&Condensed], threads: usize) -> (Vec<Vec<u8>>, Vec<Overlap>) {
    let cells: Vec<HashSet<u64>> = feeds
        .iter()
        .map(|f| f.stops.iter().map(|s| cell(s.x, s.y)).collect())
        .collect();
    let mut by_cell: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, cs) in cells.iter().enumerate() {
        for &c in cs {
            by_cell.entry(c).or_default().push(i);
        }
    }
    let size: Vec<u64> = feeds.iter().map(|f| f.departures_per_week()).collect();
    let mut pairs: HashSet<(usize, usize)> = HashSet::new();
    for ids in by_cell.values() {
        for (n, &a) in ids.iter().enumerate() {
            for &b in &ids[n + 1..] {
                // (smaller, larger); ties go by position in the feed list.
                let p = if (size[a], a) < (size[b], b) {
                    (a, b)
                } else {
                    (b, a)
                };
                pairs.insert(p);
            }
        }
    }
    let mut pairs: Vec<(usize, usize)> = pairs.into_iter().collect();
    pairs.sort_unstable();

    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<(usize, Dropped, Overlap)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.max(1))
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(small, large)) = pairs.get(i) else {
                            return out;
                        };
                        let (dropped, overlap) =
                            compare(feeds[small], &cells[small], feeds[large], &cells[large]);
                        if overlap.0 > 0 {
                            out.push((
                                small,
                                dropped,
                                Overlap {
                                    smaller: small,
                                    larger: large,
                                    departures: overlap.0,
                                    by_agency: overlap.1,
                                },
                            ));
                        }
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("dedup worker panicked"))
            .collect()
    });

    let mut dropped: Vec<Vec<u8>> = feeds.iter().map(|f| vec![0u8; f.trips.len()]).collect();
    let mut overlaps = Vec::new();
    for (feed, trips, overlap) in results {
        for (t, days) in trips {
            dropped[feed][t] |= days;
        }
        overlaps.push(overlap);
    }
    overlaps.sort_by(|a, b| {
        b.departures
            .cmp(&a.departures)
            .then(a.smaller.cmp(&b.smaller))
    });
    (dropped, overlaps)
}

/// Compares `small` against `large`; returns the trips of `small` with
/// the days on which they are duplicates, plus the duplicate
/// departures per week, in total and by agency.
#[allow(clippy::type_complexity)]
fn compare(
    small: &Condensed,
    small_cells: &HashSet<u64>,
    large: &Condensed,
    large_cells: &HashSet<u64>,
) -> (Vec<(usize, u8)>, (u64, Vec<(String, u64)>)) {
    // Cells of `small`, grown by one so neighbours across a cell edge count.
    let near_small: HashSet<u64> = small_cells
        .iter()
        .flat_map(|&c| {
            let (cx, cy) = ((c >> 32) as i64, (c & 0xffff_ffff) as i64);
            (-1..=1).flat_map(move |dx| (-1..=1).map(move |dy| (cx + dx, cy + dy)))
        })
        .filter(|&(x, y)| x >= 0 && y >= 0)
        .map(|(x, y)| ((x as u64) << 32) | y as u64)
        .collect();
    let mut keys: HashSet<u64> = HashSet::new();
    for t in &large.trips {
        for s in large.trip_stops(t) {
            if s.minute == NO_TIME || !near_small.contains(&cell(s.x, s.y)) {
                continue;
            }
            for d in 0..7 {
                if t.days & (1 << d) != 0 {
                    keys.insert(key(s.x, s.y, d, s.minute));
                }
            }
        }
    }
    if keys.is_empty() {
        return (Vec::new(), (0, Vec::new()));
    }
    let hit = |x: u32, y: u32, d: u32, m: u16| -> bool {
        for dx in [-1i64, 0, 1] {
            for dy in [-1i64, 0, 1] {
                for dm in [-1i64, 0, 1] {
                    let (nx, ny, nm) = (x as i64 + dx, y as i64 + dy, m as i64 + dm);
                    if nx < 0 || ny < 0 || !(0..0x2000).contains(&nm) {
                        continue;
                    }
                    if keys.contains(&key(nx as u32, ny as u32, d, nm as u16)) {
                        return true;
                    }
                }
            }
        }
        false
    };
    let mut dropped = Vec::new();
    let mut total = 0u64;
    let mut by_agency: HashMap<u32, u64> = HashMap::new();
    for (i, t) in small.trips.iter().enumerate() {
        let stops = small.trip_stops(t);
        if !stops.iter().any(|s| large_cells.contains(&cell(s.x, s.y))) {
            continue;
        }
        let timed = stops.iter().filter(|s| s.minute != NO_TIME).count();
        if timed < 2 {
            continue;
        }
        let mut days = 0u8;
        for d in 0..7 {
            if t.days & (1 << d) == 0 {
                continue;
            }
            let hits = stops
                .iter()
                .filter(|s| s.minute != NO_TIME && hit(s.x, s.y, d, s.minute))
                .count();
            if hits * 100 >= timed * MATCH_PERCENT {
                days |= 1 << d;
            }
        }
        if days != 0 {
            dropped.push((i, days));
            let n = u64::from(days.count_ones()) * u64::from(t.len);
            total += n;
            *by_agency.entry(t.agency).or_default() += n;
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
    (dropped, (total, by_agency))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transit::gtfs::{Stop, Trip};

    /// A feed with one trip per entry: (agency, days, [(x, y, minute)]).
    type TestTrip<'a> = (u32, u8, &'a [(u32, u32, u16)]);

    fn feed(agencies: &[&str], trips: &[TestTrip]) -> Condensed {
        let mut c = Condensed {
            agencies: agencies.iter().map(|a| a.to_string()).collect(),
            ..Condensed::default()
        };
        for &(agency, days, stops) in trips {
            let start = c.stops.len() as u32;
            c.stops
                .extend(stops.iter().map(|&(x, y, minute)| Stop { x, y, minute }));
            c.trips.push(Trip {
                agency,
                route_type: 3,
                days,
                start,
                len: stops.len() as u32,
            });
        }
        c
    }

    const A: &[(u32, u32, u16)] = &[(1000, 1000, 480), (1010, 1000, 485), (1020, 1000, 490)];

    #[test]
    fn drops_copy_from_smaller_feed() {
        // The national feed has the regional bus, one pixel and one
        // minute off, plus a lot of its own service far away.
        let shifted: Vec<(u32, u32, u16)> = A.iter().map(|&(x, y, m)| (x + 1, y, m + 1)).collect();
        let national = feed(
            &["Rail", "Regio Bus"],
            &[
                (0, 0b1111111, &[(90_000, 90_000, 600); 20]),
                (1, 0b0011111, &shifted),
            ],
        );
        // The regional feed runs the bus every day; the national feed
        // only knows the weekday runs.
        let regional = feed(
            &["Regio Bus"],
            &[
                (0, 0b1111111, A),
                (0, 0b1111111, &[(1000, 1000, 700), (1010, 1000, 705)]),
            ],
        );
        let (dropped, overlaps) = find(&[&national, &regional], 2);
        assert_eq!(dropped[0], [0, 0], "nothing dropped from the larger feed");
        assert_eq!(dropped[1], [0b0011111, 0], "weekday runs of the first trip");
        assert_eq!(overlaps.len(), 1);
        assert_eq!((overlaps[0].smaller, overlaps[0].larger), (1, 0));
        assert_eq!(overlaps[0].departures, 5 * 3);
        assert_eq!(overlaps[0].by_agency, [("Regio Bus".to_string(), 15)]);
    }

    #[test]
    fn keeps_different_services() {
        // Same stops, seven minutes apart: a different service.
        let later: Vec<(u32, u32, u16)> = A.iter().map(|&(x, y, m)| (x, y, m + 7)).collect();
        let a = feed(&["A"], &[(0, 0b1111111, A)]);
        let b = feed(
            &["B"],
            &[(0, 0b1111111, &later), (0, 1, &[(5, 5, 5), (6, 6, 6)])],
        );
        let (dropped, overlaps) = find(&[&a, &b], 1);
        assert_eq!(dropped, [vec![0], vec![0, 0]]);
        assert!(overlaps.is_empty());
        // Partly matching (2 of 3 departures) is not enough either.
        let partly: Vec<(u32, u32, u16)> = vec![A[0], A[1], (1020, 1000, 499)];
        let c = feed(&["C"], &[(0, 0b1111111, &partly), (0, 1, &[(5, 5, 5)])]);
        assert_eq!(find(&[&a, &c], 1).0, [vec![0], vec![0, 0]]);
    }

    #[test]
    fn far_apart_feeds_are_not_compared() {
        let a = feed(&["A"], &[(0, 1, A)]);
        let b = feed(
            &["B"],
            &[(0, 1, &[(200_000, 200_000, 480), (200_001, 200_000, 485)])],
        );
        assert!(find(&[&a, &b], 1).1.is_empty());
    }
}
