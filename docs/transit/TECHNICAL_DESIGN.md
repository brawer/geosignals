<!--
SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
SPDX-License-Identifier: MIT
-->

# Public transport departures: technical design

This document describes how the `transit-departures` pipeline is built:
its data structures, file formats, memory use and scaling, and the
alternatives we considered. It is for people who work on the code or
run it at scale. What the output means, its biases and its licenses
are in the [user documentation](README.md).

The numbers below come from the Swiss and German national GTFS feeds,
which are among the largest anywhere. The code lives in
[`src/transit/`](../../src/transit/).

## Contents

- [Goals and constraints](#goals-and-constraints)
- [Overview](#overview)
- [The condensed feed](#the-condensed-feed)
- [Condensing GTFS](#condensing-gtfs)
- [Condensing CIF](#condensing-cif)
- [Fetching and the cache](#fetching-and-the-cache)
- [Memory: mmap instead of heap](#memory-mmap-instead-of-heap)
- [Sorting: in place, not external](#sorting-in-place-not-external)
- [Finding duplicates](#finding-duplicates)
- [The 12-month average](#the-12-month-average)
- [The raster](#the-raster)
- [Parallelism](#parallelism)
- [Running the pipeline](#running-the-pipeline)
- [Measurements](#measurements)
- [Scaling to the planet](#scaling-to-the-planet)
- [Sketch: spreading departures to walking distance](#sketch-spreading-departures-to-walking-distance)
- [Lessons learned](#lessons-learned)
- [Open issues](#open-issues)

## Goals and constraints

- **Planet-wide.** The Transitous catalog lists about 2,100 static
  GTFS feeds. Most are small, but national feeds hold tens of millions
  of stop times: the German one has 31.7 million rows in
  `stop_times.txt` (1.9 GB uncompressed), the Swiss one 24.0 million.
- **Small machines.** A planet-wide run should fit on a 2 GB machine
  with a few threads. Memory, not CPU, is the binding constraint.
- **Unreliable sources.** Publishers go down, rename files and serve
  expired timetables. A run must still produce output, using the last
  good copy of each feed.
- **Cheap reruns.** Most feeds don't change from one run to the next.
  A run that downloads nothing new should take seconds per large feed,
  not minutes.
- **Methods change.** The way we count departures will keep evolving.
  Whatever the cache holds must let a new method run without
  downloading every feed again, and without losing history that only
  earlier runs could see.
- **A year of history.** Many feeds publish only the next few weeks.
  The signal is an average over 12 months, so each run must add to
  what earlier runs saw.

## Overview

A run has six steps. Each is one module.

```
catalog ──► fetch ──► condense ──► dedup ──► history ──► raster
(catalog)   (fetch)   (gtfs, cif)  (dedup)   (history)   (raster, cog)
```

1. **Catalog** ([`catalog.rs`](../../src/transit/catalog.rs),
   [`repo.rs`](../../src/transit/repo.rs)). This step reads the
   Transitous `feeds/*.json` files and resolves references into the
   Transitland Atlas and the Mobility Database. It adds the feeds of
   gtfs-data.jp, Japan's GTFS repository, from the Atlas's copy of
   its listing, and those of
   [`checked.json`](../../src/transit/checked.json). It drops feeds
   whose license we can't republish.
2. **Fetch** ([`fetch.rs`](../../src/transit/fetch.rs)). This step
   asks each publisher whether its feed changed, downloads it if so,
   and falls back to the cache otherwise.
3. **Condense** ([`gtfs.rs`](../../src/transit/gtfs.rs),
   [`cif.rs`](../../src/transit/cif.rs)). A new download becomes a
   *condensed feed*: a compact, versioned file with the feed's whole
   calendar (see [`condensed.rs`](../../src/transit/condensed.rs)).
   It goes into the cache, so an unchanged feed is never condensed
   twice.
4. **Dedup** ([`dedup.rs`](../../src/transit/dedup.rs)). This step
   finds trips that a larger feed also has, on the days both feeds
   cover.
5. **History** ([`history.rs`](../../src/transit/history.rs)). This
   step adds each feed's departures per pixel and week, without
   duplicates, to the history in the cache, and averages the past
   52 weeks.
6. **Raster** ([`mod.rs`](../../src/transit/mod.rs),
   [`raster.rs`](../../src/raster.rs)). This step sums the averages
   of all feeds per zoom-18 pixel and writes a Cloud-Optimized
   GeoTIFF with overviews, a data BOM, a run report and the
   catalog metadata.

Steps 2 and 3 run feed by feed on a thread pool. Step 4 needs all
feeds at once, but only their outlines stay in memory (see
[Finding duplicates](#finding-duplicates)). Steps 5 and 6 again work
feed by feed.

## The condensed feed

A condensed feed holds everything later steps need from a timetable,
and nothing else. Its format is *format 2*: the magic `OSMSGTFS`, a
format byte, then seven sections. Each section is its own zstd frame
(level 9) behind a 4-byte little-endian length.

| Section | What it holds |
| --- | --- |
| Header | Condenser version, calendar (first day, length, last day), inputs (URL, ETag, Last-Modified, SHA-256, SHA-512, first seen), agencies |
| Weekly totals | Departures per station and calendar week (Monday to Sunday) |
| Stations | Zoom-24 positions (about 2 m), front-coded stop IDs |
| Patterns | Agency, route type, and departures as (station, minutes after the first departure) |
| Trips | (pattern, service, start minute), sorted, delta-coded |
| Frequencies | (pattern, service, start, end, headway) for `frequencies.txt` |
| Services | A bitmap of calendar days per service, up to 371 days |

### Patterns

Most trips repeat. A bus line that runs every 10 minutes all day makes
the same stops at the same spacing a hundred times. So a trip is
stored as a pattern (the stations it departs from, with offsets in
minutes) plus a start time and a service. The Swiss feed has 1.52
million trips but only 125,079 patterns. The German one has 1.54
million trips and 268,551 patterns.

A pattern holds departures only. The condenser drops a trip's last
stop and stops where boarding isn't allowed, so no later step needs
to know those rules. Stops without a time keep the marker `NO_TIME`.
They count as departures, but dedup ignores them.

### Services as bitmaps

GTFS describes service days in two ways: weekday rules in
`calendar.txt`, and exceptions in `calendar_dates.txt`. Some feeds use
only exceptions, with ten million rows. The condenser resolves both
into one bitmap per service, a bit per day. Equal bitmaps are merged:
the Swiss feed ends up with 56,035 distinct services and the German
one with 1,865. The bitmaps make "does this trip run on day d" a bit
test. They also make "on which days do two trips both run" a bitwise
AND, which dedup relies on. The calendar is capped at 371 days
(`MAX_DAYS`), slightly more than a year, so a service is at most
47 bytes.

### Stations in Hilbert order

Stations are sorted by the Hilbert key of their zoom-18 pixel, the
same key the raster uses. Neighbouring stations are therefore near
each other in the file. That has three benefits:

- Positions delta-code to small numbers.
- Stop IDs of neighbours often share prefixes, which front coding
  removes.
- Two feeds' station tables can be merge-joined or searched by area
  without building a spatial index.

### Weekly totals up front

The second section holds the departures at each station in each week,
delta-coded along the weeks. It duplicates what the other sections
imply, so it costs space: 0.56 MB of the Swiss file and 1.24 MB of the
German one. Because it comes right after the header, a reader that
only needs per-station, per-week counts can stop after two frames.
Nothing in the run reads it that way yet. It is there for quick
checks of feed changes and lightweight reports, without decoding
millions of trips.

Delta coding along the weeks saves about a third for the Swiss feed,
whose calendar spans a year of similar weeks. It costs about 9% for
the German feed, which spanned only 6 weeks. We kept it, since long
calendars dominate the planet-wide total.

### Sizes

| Section | Swiss feed | German feed |
| --- | ---: | ---: |
| Header | 0.01 MB | 0.00 MB |
| Weekly totals | 0.56 MB | 1.24 MB |
| Stations | 0.30 MB | 2.56 MB |
| Patterns | 0.89 MB | 3.52 MB |
| Trips | 1.48 MB | 1.52 MB |
| Frequencies | 0.01 MB | 0.00 MB |
| Services | 0.44 MB | 0.01 MB |
| **Total** | **3.68 MB** | **8.86 MB** |
| *Raw zip* | *169 MB* | *242 MB* |

The German feed has 426,285 stations, about six times as many as the
Swiss one, so its stations and patterns dominate. The Swiss feed has
many more distinct services, probably because its calendar spans a
year, with many holiday variations.

### Versioning

The header records which condenser made the file. Each condenser has
its own version (`gtfs::VERSION`, `cif::VERSION`), and a run compares
it before using a cached file (see [the cache](#fetching-and-the-cache)).
Bumping it makes the next run condense every feed of that kind again,
from the raw download that the cache keeps. The format byte guards the
layout itself. A decoder rejects any other version and checks every
index, so a damaged or foreign file fails cleanly and the feed is
condensed again.

### Alternatives considered

- **Every departure, one week.** Format 1 stored each departure of the
  feed's median week as (pixel, minute). It took 11 MB for Switzerland
  and 59 MB for Germany, three to seven times more than format 2, while
  holding one week instead of a year. It also baked one method, the
  median week, into the cache.
- **One total per station.** Storing only one departures-per-day
  figure per station would cost less than weekly totals, but it can't
  serve a 12-month average or any seasonal view. Weekly totals cost
  0.6 MB for Switzerland and 1.6 MB for Germany in a prototype (1.2 MB
  in the final format).
- **A separate file for totals.** Keeping the totals in their own
  object, or even as a GeoTIFF, would double the number of cache
  objects and allow them to drift apart. Sections in one file give the
  same partial reads.
- **Week-major totals.** Laying out totals week by week rather than
  station by station compresses a little better for feeds with few
  weeks, and worse for feeds with many. Station-major order also
  matches how dedup and the history use them.

## Condensing GTFS

[`gtfs::condense_in`](../../src/transit/gtfs.rs) reads the zip in one
pass per table, streaming each CSV file. It never holds a whole table
in memory.

1. **Agencies, routes, calendar.** These are small. Agencies and
   services are interned into dense numbers.
2. **Calendar dates.** These can have ten million rows, so each row is
   packed into one `u64` (service, day, added), sorted, and
   deduplicated.
3. **Trips** map to (service, agency, route type). Trips whose service
   never runs are dropped here, which skips their stop times later.
4. **Stops** become zoom-24 positions. A stop without coordinates, or
   at (0, 0), takes its parent station's position.
5. **Stop times** are the bulk. Each row becomes a 16-byte `Event`
   (trip, sequence, stop, minute, pickup flag), appended to a
   memory-mapped scratch file (see [Memory](#memory-mmap-instead-of-heap)).
   The string-keyed lookup tables for trips and stops are dropped as
   soon as this pass ends.
6. The events are **sorted** by (trip, sequence) in place (see
   [Sorting](#sorting-in-place-not-external)).
7. Each trip's run of events becomes a **pattern**, interned in a hash
   map, plus a trip or a set of frequency windows.
8. `normalize` sorts stations by Hilbert key, renumbers patterns,
   merges equal patterns and services, drops unused ones, and sorts
   trips. Sorted trips make the trip section delta-code well.

Downloading from a local server and condensing both feeds takes
56 seconds on one thread, and 31 seconds on four, where the slower
feed sets the pace. That is about one second per million stop times
per core.

## Condensing CIF

Network Rail's timetable for Great Britain comes as CIF, a file of
fixed-width records. [`cif.rs`](../../src/transit/cif.rs) parses it
line by line. Each train ID (UID) can have several schedules. On any
day, the one with the strongest planning indicator wins: a
cancellation beats a new schedule, which beats an overlay, which beats
the permanent one.

The condenser resolves this up front. For each schedule, its service
is the set of days on which it is the winner. The output is an
ordinary condensed feed, so no later step knows about CIF.

- **Departures.** A calling point counts if its activity is `TB`
  (train begins), `T` (stops), `U` (stops to pick up) or `R` (request
  stop), and not `N` (not advertised).
- **Times.** The public departure time is used, or the working time
  where the public one is missing.
- **Stations.** Each TIPLOC becomes a station. NaPTAN gives a railway
  station's position under the ATCO code `9100` plus its TIPLOC. Big
  stations have several TIPLOCs, so a TIPLOC that NaPTAN lacks takes
  the position of another with the same CRS (three-letter) code.
  Departures at TIPLOCs with neither are dropped, and the log names
  them.

Since the placement depends on the NaPTAN file too, the condensed
header records the hashes of both inputs, and a new NaPTAN file means
condensing again.

## Fetching and the cache

The cache is a key-value store ([`store.rs`](../../src/store.rs)):
either a local directory, or an S3-compatible bucket with path-style
URLs and SigV4 signing. Everything sits under `transit/`, so other
signals can share the cache under their own prefixes. Every object has
a fixed key:

```
transit/<feed id>/condensed.bin         the condensed feed
transit/<feed id>/gtfs.zip              the last download, as is (cif.gz for CIF)
transit/<feed id>/history.bin           weekly departures per pixel
transit/shared/<name>/<file>            NaPTAN and each catalog, fetched once per run
transit/shared/<name>/<name>.json       its URL, ETag, Last-Modified, hashes, fetch times
transit/index.json                      every feed's ETag, Last-Modified and hashes
```

The header of `condensed.bin` is the cache's record of a feed. It
holds the condenser version and, for each input file, the URL asked
for and where it redirected to, the `ETag`, `Last-Modified`, SHA-256,
SHA-512, size, and when we first downloaded it. A CIF header lists the
NaPTAN file too. Shared inputs have no condensed file, so they keep
the same record in a small JSON file next to them, which works the same
in S3 and in a local directory.

A run asks each publisher whether the feed changed (`If-None-Match`,
`If-Modified-Since`). If not, or if the download hashes to the same
SHA-256, it loads `condensed.bin` and never opens the zip. If the
publisher fails, the run retries transient errors with backoff, tries
mirrors, and finally uses the cached copy however old, marked `stale`
in the report. If the header names another condenser version or
other inputs, the run condenses the raw file again and overwrites
`condensed.bin`.

Once every feed is fetched, the run writes `index.json`. The next run
makes its conditional requests from it, so it never loads the stale
`condensed.bin` of a feed that changed. The gain is modest, since
unchanged feeds are read in full for the raster anyway, but there are
thousands of feeds. The index is only a hint. Where it disagrees with
a header, say after a crash, the run asks the publisher again with
what the header says. Without an index, it reads the headers.

Fixed keys have two consequences:

- **A method change rebuilds in place.** A new condenser version
  overwrites `condensed.bin`. Old and new code should therefore not
  share a cache, or each would keep rebuilding what the other wrote.
- **The raw download is kept,** so a new condenser can run without
  asking any publisher. That costs most of the cache's size: 169 MB and
  242 MB for the two feeds here, against 3.7 MB and 8.9 MB condensed.

In the measured run, the cache held 174 MB for Switzerland and 254 MB
for Germany. All but 4.7 MB and 11 MB of that was the raw zips.

## Memory: mmap instead of heap

Two kinds of large data would not fit on the heap of a small machine:

- the stop times of a large feed while it is condensed (16 bytes each,
  508 MB for Germany);
- the condensed feeds of all feeds at once, which dedup needs to look
  at in any order (about a GB planet-wide).

Both live in memory-mapped scratch files in `--workdir`.

- **[`slab.rs`](../../src/slab.rs)** holds arrays of fixed-size
  records. A `Writer` appends records through a buffered writer to an
  unnamed temporary file. `finish` maps the file and returns a `Slab`
  that reads and writes like a slice. Records are `repr(C)` with only
  integer fields, so any bit pattern is valid, and the unsafe cast is
  confined to one place.
- **`Fetched::data`** holds a feed's encoded condensed file, written to
  an unlinked temporary file and mapped read-only. A step that needs
  the feed decodes it from the map, uses it, and drops it.

Why this works: mapped pages are file-backed. The kernel can drop them
when memory gets tight and read them back from disk when needed. They
don't count against the heap, and on Linux they are reclaimable under
a cgroup memory limit, unlike anonymous memory. Unnamed files vanish
when the process exits, even after a crash, so nothing leaks into
`--workdir`.

The scratch directory must be on a real disk. On a RAM disk (tmpfs),
the "file" is memory again, and the scheme gains nothing.

In the measured runs (see [Measurements](#measurements)), condensing
peaks at 491 MB of mapped, file-backed memory, the German stop times,
next to at most 335 MB of anonymous memory on one thread. A warm run
maps only 19 MB.

### Alternatives considered

- **Everything on the heap.** This is the simplest option and fine for
  a laptop. But the German stop times alone take half a GB, and
  holding every decoded feed for dedup would take many GB planet-wide.
  A decoded German feed takes 100 MB of heap, against 8.9 MB encoded.
- **Decoded feeds in the map.** Mapping a decoded, fixed-layout
  representation would save decode time (170 ms for the Swiss feed,
  270 ms for the German one). But it needs a stable in-memory layout
  for variable-length patterns and strings, and would take ten times
  the disk. Decoding is cheap enough.
- **A database (SQLite, RocksDB).** This would add a dependency and
  an on-disk format to manage, for access patterns that are just
  "append, sort, scan".

## Sorting: in place, not external

Condensing needs the stop times grouped by trip and ordered by
sequence, but `stop_times.txt` is not guaranteed to be in that order.
The events are sorted in place, with `sort_unstable_by_key` on the
memory-mapped slab.

The repository also has a real external sort,
[`extsort.rs`](../../src/extsort.rs): sorted runs spilled to
zstd-compressed files, merged with a heap. The OSM node density and
iNaturalist signals use it. The transit pipeline doesn't, because:

- One feed's events are bounded. The largest known feed has about
  32 million stop times, 508 MB, which fits in the page cache of any
  machine we target. The sort then runs at memory speed, and the
  mapping only spares the heap.
- An in-place sort needs no extra disk and no merge pass, and the
  result is a slice that the pattern step can walk with `chunk_by`.
- `extsort` counts `u64` keys. Events carry a payload, so we would
  need a second, record-carrying variant.

If a feed ever has many times more stop times than the machine has
RAM, the in-place sort would start thrashing, since quicksort touches
pages all over the array. That is the point to switch to an external
merge sort. A cheaper first step would be a two-pass sort: bucket the
events by trip range into a few files, then sort each bucket in place.

Elsewhere, the pipeline sorts small things: stations by Hilbert key
in `normalize`, dedup's per-pair departure index, and the final
pixel list for the raster. All of these fit in memory.

## Finding duplicates

National feeds include buses from across the border, and regional
feeds copy national ones. [`dedup.rs`](../../src/transit/dedup.rs)
finds trips that a smaller feed shares with a larger one, so that only
the larger feed's copy counts.

### Which pairs to compare

Each feed has an `Outline`: the sorted list of zoom-15 tiles (about
1 km) that contain one of its stations, and its departures on an
average day. Outlines are small and stay in memory for all feeds.
Building a map from tile to feeds and listing the feeds that share a
tile gives the candidate pairs. Two feeds that share no tile are never
compared.

This is a one-level spatial hash, which works like a quadtree or S2
index cut at one level. It costs one hash lookup per tile, and its
size is the number of distinct tiles with stations.

Pairs are grouped by their larger feed, in batches of 8, so a worker
decodes the large feed once and compares several small feeds against
it.

### Comparing a pair

For a pair, only the days both calendars cover count. Within those
days:

1. Take the smaller feed's tiles, grown by one tile in every
   direction. Then take the larger feed's stations in those tiles,
   and the patterns and trips through them. This is "the larger feed
   near the smaller one". Its cost depends on the area the feeds
   share, not on the size of the larger feed.
2. Index those departures as sorted `(x, y, minute)` keys with their
   service.
3. For each departure of each trip in the smaller feed, look up the
   3 × 3 pixels around it and the minutes ±1. The union of the matched
   services' day bitmaps, ANDed with the trip's own days, says on which
   days that departure has a match.
4. A trip is a duplicate on a day if at least 80% of its timed
   departures (and at least two) match on that day. The result is,
   per trip, a bitmap of days on which it is dropped.

A feed's drops from several larger feeds are ORed together. The
largest feed keeps everything, so the largest copy survives whatever
order pairs are processed in.

**Swiss vs. German feed:** The calendars share 31 days. Comparing
them takes about 1.7 seconds and finds 674,772 duplicate departures a
week, mostly German operators near the border, such as Südbadenbus.
Shifting one feed's times by seven minutes matches 0.05% of trips,
against 3.8% unshifted, so false matches are rare.

### Alternatives considered

- **Bounding boxes, R-trees.** A national feed with a few
  cross-border buses has a bounding box that covers half a continent,
  so boxes would pair it with every feed nearby. Tiles follow the
  actual stations. An R-tree over boxes has the same problem and more
  code.
- **S2 or a multi-level quadtree.** At about 2,100 feeds, one level
  of 1 km tiles is enough, and pair finding should take well under a
  second. The cost
  is in comparing the pairs, which tiles don't change.
- **Comparing weekdays instead of dates.** Projecting both feeds onto
  a typical week would let feeds with disjoint calendars match. But it
  also lets a summer timetable match a winter one, and a holiday match
  a normal day. Comparing dates is exact. When two calendars share no
  day, no day has both copies, so nothing counts twice.
- **Comparing median weeks.** In format 1 we compared each feed's
  median week. With exact dates, that fails: the Swiss median week lay
  outside the 31 days shared with the German feed, so nothing was
  dropped. This was one reason to move to the 12-month average.
- **Whole-feed precedence.** Dropping a whole regional feed when a
  national one exists is simple, but loses trips that only the
  regional feed has. Per-trip, per-day matching keeps them.

## The 12-month average

Each pixel's value is the departures on an average day over the 52
weeks up to the run day. Since many feeds publish only a few weeks
ahead, each run adds to a per-feed history.

### The history file

[`history.rs`](../../src/transit/history.rs) keeps
`transit/<feed>/history.bin`, a zstd-compressed file containing:

- the Monday of the week when we first tracked the feed;
- for each week (keyed by its Monday): the days of that week a
  timetable covered (1 to 7), and the departures per zoom-18 pixel,
  as sorted (Hilbert key, count) pairs, delta-coded.

A run computes the feed's weekly totals without duplicates
(`Feed::totals_without`). It sums the stations of each pixel and
writes every week of the current calendar into the history. A week is
replaced only if the new timetable covers at least as many of its
days, so a newer full week beats an older one, and a feed's thin last
days don't overwrite a full week. Weeks older than 52 weeks are
pruned. The file is written back only if it changed.

### Counting weeks

For the 52 weeks up to the run:

- A week that some timetable covered counts, with the days it
  covered. This includes weeks before we started tracking the feed,
  if the first timetable we saw reached back that far.
- A week after we started tracking that no timetable covered counts
  as 7 days of zero. The feed expired, or its next version came late,
  and either way nothing ran according to us.
- A week before we started tracking, which no timetable covered, is
  unknown and left out.

The average is the sum of departures over the counted weeks divided
by the sum of their days. The report gives each feed's number of
counted weeks. In the measured run on 20 June 2026, the Swiss feed
counted 28 weeks, since its calendar starts in December 2025. The
German feed counted 4.

### Sizes

After one run, `history.bin` takes 1.0 MB for the Swiss feed (53
weeks, 1.9 million pixel-weeks) and 2.2 MB for the German feed (6
weeks, 1.7 million pixel-weeks). Similar weeks compress well, so the
Swiss file costs about 0.5 bytes per pixel-week. Extrapolating, a full
year of the German feed should take 8–20 MB.

### Alternatives considered

- **The median week.** Format 1 picked the week with the median
  number of departures. That is robust against holidays, but it
  removes seasonal service: a ski bus counts either fully or not at
  all. It also gives dedup the wrong days to compare (see above).
- **Recomputing from old condensed files.** Keeping every condensed
  version and averaging them on each run would avoid a new file. But
  it would keep megabytes per version instead of the weeks we need,
  and the history would be lost whenever condensed files are pruned.
- **Filtering weak weeks.** Dropping weeks with little service would
  make a feed's tail end look better. It would also lift seasonal
  service back to its peak, which is what the average is meant to
  avoid.
- **Storing departures per station rather than per pixel.** Station
  IDs and positions change between feed versions. Pixels are stable,
  and they are what the raster needs.

## The raster

All feeds' averages are summed per pixel in a hash map. Then they are
converted to thousandths of a departure per day, so a bus that runs
one day a month still shows. Finally they are sorted by Hilbert key.
[`raster.rs`](../../src/raster.rs) builds the pyramid from that
sorted stream. Each tile is converted to log-scale densities by
`encode::pixels_scaled` (with scale 1/1000), compressed on worker
threads, and spooled per zoom level by
[`cog.rs`](../../src/cog.rs), which then writes the Cloud-Optimized
GeoTIFF. Overview pixels sum their children's departures before
dividing by area, so they stay exact.

For the Swiss and German feeds, the output has 341,708 non-empty
pixels and takes 20 MB.

## Parallelism

Every per-feed step uses the same pattern: `--threads` scoped worker
threads take the next feed index from an atomic counter. Results are
sorted back into feed order, so output doesn't depend on scheduling.

- **Fetch and condense:** one feed per worker. Memory scales with
  the number of large feeds being condensed at once, so `--threads`
  is the memory knob.
- **Dedup:** one batch of up to 8 pairs, sharing their larger feed,
  per worker.
- **History:** one feed per worker. Each worker decodes a feed,
  computes totals, and reads and writes its history.
- **Raster:** tiles are compressed on workers, connected by bounded
  channels, and a collector writes them in order.

## Running the pipeline

The basic command and the main flags are in the [user
documentation](README.md#reproducing-the-files). This section covers
what matters for operating it.

- **Catalogs.** By default, Transitous and the Transitland Atlas are
  downloaded as the tarballs that GitHub serves for their `main`
  branch, and the Mobility Database as its CSV export. They are cached
  like feeds. `--transitous` and `--transitland-atlas` also take a
  local tarball or checkout, and `--mobility-database` a local CSV
  file. `none` skips the sources that need the Atlas or the Mobility
  Database.
- **Logins.** A feed in `checked.json` with `credentials` needs our own
  login, set as `<PREFIX>_USERNAME` and `<PREFIX>_PASSWORD`, such as
  `NETWORK_RAIL_USERNAME`. The password goes only to the feed's URL,
  not to where that redirects. The BOM records the URL we asked for,
  not the signed URL it redirected to. Without the variables, the feed
  is skipped, except with `--offline`.
- **Cache.** `--cache` is a directory or `s3://bucket/prefix`,
  configured with `CACHE_S3_ACCESS_KEY_ID`,
  `CACHE_S3_SECRET_ACCESS_KEY`, `CACHE_S3_REGION` (default `auto`) and
  `CACHE_S3_ENDPOINT_URL`. The standard `AWS_*` variables are not
  read, since the cache need not be on Amazon. `--offline` uses only the cache.
  A cached feed is used however old, as long as its timetable ended at
  most `--max-expired-days` (default 90) before `--date`.
- **Scratch disk.** `--workdir` holds downloads in progress, the stop
  times of the feeds being condensed (16 bytes each), and the
  condensed feeds of the run (about a GB planet-wide). It must be on
  disk, not on a RAM disk (see
  [Memory](#memory-mmap-instead-of-heap)).
- **Threads and memory.** `--threads` sets how many feeds are fetched
  and condensed at once, and so bounds memory. Each large feed takes up
  to about 350 MB of heap while it is condensed (see
  [Measurements](#measurements)). A planet-wide run should fit on a
  2 GB machine with 2–4 threads.

## Measurements

An end-to-end run on the Swiss and German national feeds, on
20 June 2026, on a 4-core machine with 16 GB. "Cold" means an empty
cache, so both feeds are downloaded from a local server and condensed.
"Warm" means the next run, with nothing changed. Anonymous memory is
mostly heap. Mapped memory is file-backed, mostly the stop-time
events.

| Run | Wall time | Anonymous memory (peak) | Mapped memory (peak) |
| --- | ---: | ---: | ---: |
| Cold, 1 thread | 88 s | 335 MB | 491 MB |
| Cold, 4 threads | 42 s | 528 MB | 491 MB |
| Warm, 1 thread | 33 s | 256 MB | 19 MB |
| Warm, 4 threads | 11 s | 462 MB | 19 MB |

Other figures from the same feeds:

| What | Swiss feed | German feed |
| --- | ---: | ---: |
| Rows in `stop_times.txt` | 24.0 M | 31.7 M |
| Stations / patterns / trips | 65,667 / 125,079 / 1.52 M | 426,285 / 268,551 / 1.54 M |
| Raw zip | 169 MB | 242 MB |
| Condensed file | 3.7 MB | 8.9 MB |
| Decoding it | 170 ms, 50 MB heap | 270 ms, 100 MB heap |
| Weekly totals | 215 ms, 28 MB (53 weeks) | 57 ms, 19 MB (6 weeks) |
| `history.bin` after one run | 1.0 MB | 2.2 MB |
| Departures on an average day | 2.60 M | 12.36 M |

Dedup between the two takes about 1.7 seconds. On a warm run, most
of the time goes to decoding and totalling the feeds, twice (once for
dedup and once for the history), and to building the raster.

## Scaling to the planet

These are estimates. No planet-wide run has been made yet.

- **Feeds.** About 2,100 catalog entries, of which 938 have a license
  we can use by default. Most are city or regional feeds, far smaller
  than the two measured here.
- **Cache.** Raw feeds are about 20–40 GB, and condensed feeds about
  1 GB. Histories grow to 52 weeks per feed, perhaps a few hundred MB
  in total.
- **Memory.** Peak heap is set by the largest feeds condensed or
  decoded at the same time, about 250–350 MB each. With 2–4 threads,
  a run should fit in 2 GB. Condensed feeds stay on scratch disk,
  about 1 GB.
- **Time.** A cold run condenses every feed once, at about one second
  per million stop times per core, so downloading is likely to take
  longer than condensing. Later runs re-condense only changed feeds. Dedup
  cost depends on the overlap area, not the number of feeds. Pair
  finding is linear in the number of tiles.
- **Hot spots.** Countries with many overlapping regional feeds
  (France, Poland) produce the most pairs. A national feed that
  overlaps 100 regional ones is decoded once per batch of 8, about 13
  times, at roughly 0.3 seconds each.

## Sketch: spreading departures to walking distance

The raster puts every departure on the pixel of its stop. A stop serves
the area around it, though, and how far that area reaches depends on
the street network, barriers such as rivers and motorways, and slopes.
This section sketches how we could spread departures to the cells
within walking distance. None of it is built yet.

**What to compute.** There are two possible meanings, and they need
different overviews:

- *Accessibility:* for each cell, the departures reachable within, say,
  10 minutes on foot, weighted by a decay such as
  `exp(−t / 5 min)`. A departure counts in many cells, so values don't
  add up, and overviews would take a mean rather than a sum.
- *Catchment:* each stop's departures are split among the cells that
  reach it, so the totals stay as they are and overviews still sum.

Accessibility is probably what analysts want. It could be a second
raster next to the current one.

**Inputs.**

- *Walking network* from the OSM planet: ways that allow walking, such
  as footways, paths, steps, pedestrian streets and residential roads,
  but not motorways or `foot=no`. Bridges and tunnels need their
  `layer`, so they don't connect to what lies above or below.
- *Terrain* from [Mapterhorn](https://mapterhorn.com)'s open terrain
  tiles, sampled at the graph's nodes. Walking speed per edge would
  follow Tobler's hiking function, which is slower uphill, slightly
  faster on gentle downhills, and direction-dependent. Steps get a
  fixed speed. Edges on bridges and in tunnels ignore terrain.
- *Sources*: the pixels with departures, as the pipeline already
  computes them, about 340,000 for Switzerland and Germany. Working
  per pixel rather than per stop bounds the work by area.

**Partitioning.** Work goes by zoom-10 tile (about 39 km at the
equator, less towards the poles), with a halo as wide as the maximum
walking distance, about 1 km. Tiles are independent, so they spread
over threads, or over machines.

1. *Graph extraction* is one pass over the planet PBF, which already
   has a reader in [`pbf.rs`](../../src/pbf.rs). Each walkable edge is
   keyed by the tiles whose halo it touches and written to sorted runs
   on disk, as [`extsort.rs`](../../src/extsort.rs) does for keys.
   The result is one compact graph file per tile, with elevations
   sampled once. It is cached per planet snapshot, and only rebuilt
   when OSM changes, not on every transit run.
2. *Spreading* loads one tile's graph and the source pixels in the
   tile and its halo. It snaps each source to its nearest graph nodes,
   adding the straight-line distance at walking speed. Then it runs a
   Dijkstra search bounded by the time limit from each source, and
   adds the decayed departures to the tile's 256 × 256 grid along the
   edges reached. Cells off the network take the time of the nearest
   edge plus the straight-line rest.
3. The tiles feed the existing raster writer, which builds overviews
   as before, with a mean instead of a sum for accessibility.

**Cost.** A 10-minute search reaches a few thousand nodes in a city. At
some millions of source pixels planet-wide, that is on the order of
10¹⁰ edge relaxations, a few CPU hours, and it parallelises perfectly.
A dense tile's graph has a few hundred thousand nodes, tens of MB, so
memory stays per tile. Two cheaper variants, if needed:

- *Fewer sources:* group source pixels on the same graph node, or
  coarsen to zoom-17 sources and accept 300 m of blur.
- *Reuse the searches:* the walking network changes far more slowly
  than timetables. The cells each source pixel reaches, with their
  walking times, can be cached per OSM snapshot. A transit run then
  only weights those lists by the day's departures, which takes
  seconds per tile.

## Lessons learned

Format 1 cached only each feed's median week, as individual departures.
Moving to format 2 taught us:

- **Cache the input in a compact form, not a derived result.** Format
  1 baked one method into the cache. The 12-month average, comparing
  feeds on exact dates, and seasonal analyses all needed the full
  calendar. Format 2 keeps it, and is still three to seven times
  smaller.
- **Exploit the structure before compressing.** Most of the size
  reduction came from patterns and service bitmaps, not from zstd. A
  timetable is highly repetitive in a way that a general compressor
  sees only partly.
- **Measure on real national feeds early.** Synthetic tests passed
  with the median-week approach. Only the Swiss and German feeds
  showed that their median weeks didn't overlap, which made dedup on
  dates a no-op, and that showed the median week was the wrong model.
- **Sort spatially once, when writing.** Hilbert-sorted stations made
  the files smaller, and made spatial lookups easy without a separate
  index.
- **Make files readable in parts.** Separate zstd frames per section
  cost almost nothing (the whole-file compressed size differs by under
  0.3%), and let a reader skip what it doesn't need.
- **Keep the full calendar, and pay for it in memory.** Condensing
  with the whole calendar raised peak resident memory from about 270
  to 500 MB for the Swiss feed, and from 540 to 770 MB for the German
  one. Part of the increase is mapped, reclaimable memory, but the
  heap grew too. `--threads` remains the knob.

## Open issues

- **Decoding twice.** A warm run decodes each feed for dedup and
  again for the history. Keeping the weekly totals from the first pass
  would halve that.
- **History growth.** Histories are not yet measured after a full
  year. If they grow large, coarser storage for old weeks (such as
  zoom-17 pixels) is an option.
- **Very large feeds.** If a feed's stop times ever exceed RAM by a
  wide margin, the in-place sort needs to become external (see
  [Sorting](#sorting-in-place-not-external)).
