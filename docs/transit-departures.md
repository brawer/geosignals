<!--
SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
SPDX-License-Identifier: MIT
-->

# Public transport departures

This data package tells how often public transport departs from each
place on Earth, by bus, tram, metro, train, ferry or cable car. The
numbers come from the published timetables (GTFS feeds) of transit
agencies, as listed by [Transitous](https://transitous.org). They are a
signal of how well a place is served by scheduled public transport.
They don't count passengers, and they don't say how far a departure
goes.

This page is for data analysts who consider using the file. It covers
what the file contains, how it is computed, its artifacts and biases,
and its licenses.

There is no planet-wide release yet. The examples below come from a
test run on three feeds:

- the Swiss national feed;
- the German national feed from [gtfs.de](https://gtfs.de);
- the regional feed of [bodo](https://www.bodo.de), a transit
  association around Lake Constance.

Counts of catalog entries come from the Transitous catalog of
27 October 2026.

## The files

Each release has one Cloud-Optimized GeoTIFF, a data bill of materials
(`.cdx.json`), a run report (`.report.json`), a `datapackage.json`,
and a [STAC](running.md#stac-catalog) item and collection:

| File | What it holds |
| --- | --- |
| `transit-departures.2026-10-26.tiff` | The raster |
| `transit-departures.2026-10-26.cdx.json` | Every catalog and feed that went in, with its URL, SHA-256, license and attribution |
| `transit-departures.2026-10-26.report.json` | Each feed's status, median week, departures and duplicates, and the sources that were skipped, with reasons |
| `datapackage.json` | The GeoTIFF and BOM as a Frictionless data package, with the license |
| `transit-departures.2026-10-26.stac.json` | The GeoTIFF and BOM as a STAC item, with the license and projection |
| `collection.json` | The STAC collection, linking to the items of every release in the directory |

The date in each name is the day of the run. A dated file never
changes; the next release gets new files. On the three test feeds, the
GeoTIFF is 20 MB.

## The sources

**Catalog.** [Transitous](https://github.com/public-transport/transitous)
is a community-run, worldwide transit router. Its repository lists
the GTFS feeds it uses, one JSON file per country or region, under
CC0. Some entries point into two other catalogs, which we also read:

- the [Transitland Atlas](https://github.com/transitland/transitland-atlas)
  (CC BY 4.0);
- the [Mobility Database](https://mobilitydatabase.org).

All three are downloaded on each run. The data BOM records the git
commit each catalog was at.

**Feeds.** The catalog of 27 October 2026 lists 1,483 static GTFS
feeds that resolve through Transitous and the Atlas, in 128 regions.
Another 633 point into the Mobility Database. France (446), Poland
(139), Hungary (86) and Canada (98) have the most entries. One national
feed can cover a whole country, though: Germany has 9 entries and
Switzerland 3.

We also add feeds that Transitous doesn't list, whose licenses we
checked ourselves, from
[`src/transit/checked.json`](../src/transit/checked.json). So far this
is one feed: the Department for Transport's
[Bus Open Data Service](https://www.bus-data.dft.gov.uk) for England,
under the Open Government Licence. Transitous has this data too, but
merged with National Rail timetables whose terms don't allow us to
republish them.

The same file can add a feed in a format other than GTFS. So far this
is Network Rail's [SCHEDULE
feed](https://publicdatafeeds.networkrail.co.uk) for the national
rail network of Great Britain, which Transitous does not list on its
own: a full CIF extract of every train, rail replacement bus and ship.
It needs a free account at publicdatafeeds.networkrail.co.uk, set as
`NETWORK_RAIL_USERNAME` and `NETWORK_RAIL_PASSWORD`; without them the
feed is skipped. Network Rail's data feeds licence has not been checked
yet, so the feed is only used with `--licenses any` for now. See
[Timetables in CIF](#timetables-in-cif).

The same file also settles the license of some feeds that Transitous
does list. The Swiss national feeds from
[opentransportdata.swiss](https://opentransportdata.swiss) come with
terms of use rather than an open license. opentransportdata.swiss has
authorised us to redistribute the derived raster without imposing any
restrictions, such as attribution, on its users; geosignals is on its
list of [authorised
databases](https://opentransportdata.swiss/authorised-databases/).

**What we leave out.**

- Sources Transitous marks as skipped (289 entries), usually because
  the feed is broken or superseded.
- Feeds that need credentials, an API key or a POST request (186
  entries). We never use Transitous's own keys.
- Feeds that are not static GTFS: GTFS Realtime, GBFS bike sharing,
  NeTEx.
- Feeds whose timetable ended more than 90 days before the run date.
  A feed that has expired probably has a newer version that we couldn't
  get.

Every skipped source is listed in the run report with its reason.

**Downloads.** Each run asks every publisher whether its feed has
changed, and downloads it if so. When a publisher is down, we use the
last copy we fetched, however old, as long as its timetable has not
expired. The report marks such feeds as `stale`, with the error.

## What a pixel means

A pixel counts **departures per day**: every time a scheduled vehicle
leaves a stop in that pixel, averaged over the seven days of a typical
week.

- **Departures.** Every stop of a trip counts, except its last one and
  stops marked `pickup_type = 1` (no boarding). A train that stops at
  ten stations counts nine departures, one at each station but the
  last.
- **Frequency-based trips.** Trips that run "every 10 minutes from 6:00
  to 20:00" in `frequencies.txt` are expanded into one trip per start
  time.
- **All modes.** All modes count the same: a bus, a tram, a ferry and
  a 400 m long train are one departure each. The files don't hold
  `route_type`, so modes can't be told apart.

### Timetables in CIF

Network Rail publishes its timetable in CIF, a file of fixed-width
records. A train is a basic schedule, which says from when to when and
on which weekdays it runs, followed by its calling points. Several
schedules can share a train's ID: on any one day a cancellation beats
a new short-term schedule, which beats an overlay, which beats the
permanent schedule.

- **Departures.** A calling point counts if passengers may board
  there: the train begins or stops there, picks up, or stops on
  request. Points where it only sets down, isn't advertised, or passes
  without stopping don't count, nor does its terminus. Freight and
  empty trains have no public calls and count nothing.
- **Times.** We use the public departure time, or the working time
  where the public one is missing. Like GTFS times, they are local and
  run on past midnight within a train.
- **Bank holidays.** The flag that says whether a train runs on bank
  holidays is ignored, since a median week rarely contains one.
- **Stations.** CIF names places by TIPLOC and has no coordinates. We
  place each TIPLOC by its railway station in
  [NaPTAN](https://www.gov.uk/government/publications/national-public-transport-access-node-schema),
  where a station's ATCO code is `9100` plus its TIPLOC. Big stations
  have several TIPLOCs, so a TIPLOC that NaPTAN lacks takes the
  position of another with the same three-letter station code. The
  run log names the TIPLOCs that remain unplaced; their departures are
  dropped.
- **Modes.** Trains, buses and ships are kept apart in the condensed
  form, but as for GTFS the output doesn't distinguish them.

### The median week

A feed's calendar usually spans weeks to a year, with holidays, school
breaks and timetable changes. For each feed, we:

1. count its trips on each day;
2. split the days into weeks from Monday to Sunday;
3. take the week whose total is the median.

The median skips holiday weeks, which have less service. It also skips
the thin tail at the end of a feed, where the next timetable is not
published yet. Weekends stay in, so a place with good weekday service
and none on Sundays shows 6/7 of its weekday rate.

Each feed gets its own median week, so different feeds can describe
different weeks. In the test run, the Swiss feed's median week began
on 14 September 2026, the German one on 22 June and bodo's on 11 May.
For places served by a single feed, this rarely matters. Where two
feeds meet, a summer timetable may sit next to a winter one. The
report lists each feed's median week.

### Duplicates

Neighbouring feeds often contain the same trips. National feeds include
buses from across the border, and regional feeds overlap with national
ones. Counting both copies would double the density exactly where
feeds overlap.

So we compare every pair of feeds with stops in the same 1 km tile
(zoom 15). A departure of the smaller feed matches if the larger feed
departs on the same weekday within one pixel (about 150 m) and one
minute of it. A trip whose departures match at least 80% (and at least
two of them) is dropped from the smaller feed on that weekday. The
copy in the largest feed survives.

In the test run:

- **bodo:** 619,483 of its 701,938 weekly departures were also in the
  German national feed, and were dropped.
- **Switzerland:** 681,349 of 18.9 million were dropped. Almost all
  were German operators near the border, such as Südbadenbus and
  SWEG. These are the same ones Transitous removes by hand.

To check for false matches, we shifted one feed's times by seven
minutes. That matched 0.05% of trips, against 3.8% without the shift.

The report lists, for each pair of feeds, how many departures were
dropped and from which agencies.

### Grid and encoding

The file uses the Web Mercator grid (EPSG:3857) and covers the world
up to about ±85° latitude.

- **Finest level.** Zoom-10 tiles of 256 × 256 pixels, so each pixel is
  a zoom-18 tile. That is about 153 m wide at the equator, 108 m at 45°
  and 76 m at 60°. This is fine enough to separate a station from the
  street next to it.
- **Overviews.** Ten overview levels follow, down to one tile for the
  world.
- **Values.** A pixel stores departures per day per km² of real Earth
  surface, so pixels can be compared across latitudes. The values are
  unsigned 16-bit integers on a log scale, with the same encoding as
  the other geosignals rasters:

```
v = 0                                          no departures
log2(departures per day per km²) = 0.01 · v − 15.01   otherwise
```

Each step of 100 doubles the density. One departure per day in a pixel
at the equator (0.0234 km²) is about 43 departures per day per km², or
v ≈ 2043. To get departures per day back, multiply the density by the
pixel's area.

The GeoTIFF carries `SCALE` and `OFFSET` metadata, so GDAL and QGIS
show log2(departures per day per km²) directly. It also carries a
raster attribute table with a histogram of the values, and a `FEEDS`
item with the number of feeds that went in.

### Overview pixels

Departures add up. An overview pixel holds the sum of the departures
below it, divided by its own area. Its density is therefore the
area-weighted average of its children's densities. So you can also
build coarser grids yourself by summing departures (density × area).
The overviews give the same result.

## Artifacts and biases

**Coverage follows open data, not transit.** The default run (`--licenses
open`) only uses feeds whose license allows republishing with
attribution (see below). Many feeds have no license in the catalog, and
those are left out even if the publisher would allow reuse. In the
catalog of 27 October 2026, 938 of the 1,484 feeds qualify, 150 of
them only through the Transitland Atlas's summary of their terms. By
region:

- **Mostly covered.** France (430 of 446), Poland (137 of 139), Italy
  (39 of 43), Estonia, Finland, Australia (28 of 31), the
  Netherlands, and Switzerland through the national feeds of
  opentransportdata.swiss (see above).
- **Partly covered.**
  - Germany (5 of 7, including the national DELFI feed).
  - The United States (111 of 341), including the New York MTA's buses,
    LIRR and Metro-North, LA Metro, WMATA, SFMTA, the MBTA, Denver RTD
    and Sound Transit. Not included are TriMet, NJ Transit and MARTA.
  - Canada (43 of 98).
  - Hungary (13 of 86).
  - Great Britain: buses, trams and ferries in England from BODS, but
    no trains, Scotland, Wales or Northern Ireland.
- **Missing.** Austria. Its catalog entries carry no license.

The Mobility Database entries come on top of these counts. A missing
country shows up as zero departures, which looks the same as having no
transit. Check the BOM before reading a zero as "no service".

**Only what is published as GTFS.** Informal transport is missing:
minibuses, shared taxis and many intercity coaches. Much of Africa,
South Asia and Latin America has little or no GTFS, so the file
understates transit there most. On-demand and flexible services (GTFS
Flex) have no fixed stops or times, so they count little or not at all.

**Departures, not capacity or reach.** A bus and a long train weigh the
same, and so do a trip that stops once and a trip that stops at every
corner. Corridors with closely spaced stops look denser than express
lines with the same number of trips. A terminus counts its departures,
but the end stop of each trip counts nothing.

**Stop positions.** Departures sit at the stop's coordinates. Big
stations list each platform as its own stop, so their departures spread
over several pixels. Stops without coordinates, or at (0, 0), take
their parent station's, and are dropped if there is none. Wrong coordinates in a feed
show up as isolated bright pixels, sometimes far from any line.

**Duplicates that we miss.** Two feeds may place the same stop more than
150 m apart, or round times differently. Then their copies of a trip
don't match and both count. Regional feeds that copy a national one
with different stop positions are the main risk. The opposite error,
dropping a trip that only looks like another, is rare (see above).

**Stale and changing feeds.** A publisher that is down for one run
still contributes its last timetable. A feed that disappears from the
catalog drops out entirely, and its area goes to zero in the next
release. Compare the reports of two releases before reading a change
in the raster as a change in service.

**Departures without times.** Stops whose time is left blank (allowed
in GTFS between timed stops) still count as departures. They are
ignored only by the duplicate check.

## Licenses and attribution

The file is published under the
[Open Database License (ODbL) 1.0](https://opendatacommons.org/licenses/odbl/1-0/),
like OpenStreetMap. We only use feeds whose license allows combining
them and republishing the result under ODbL with attribution:

- public domain dedications (CC0, PDDL);
- attribution licenses (CC BY 2.0–4.0, ODC-By, Etalab 2.0, NLOD, DL-DE
  BY and Zero, OGL UK and Canada, MIT, Apache);
- the ODbL itself.

Feeds under other share-alike licenses, NonCommercial or NoDerivatives
licenses are left out. A feed's license is decided in this order:

1. **Our own check.** For feeds in
   [`checked.json`](../src/transit/checked.json), the license whose
   terms we read ourselves, or a permission the publisher gave us.
   The page that states it goes into the BOM as an `evidence`
   reference.
2. **SPDX identifier.** Otherwise, the SPDX identifier that Transitous
   gives, or else the one in the Transitland Atlas.
3. **Atlas summary.** For a feed without a known license, the Atlas
   summary of its terms of use. Many agencies, especially in North
   America, publish feeds under their own terms instead of a standard
   license, and the Atlas records what those terms allow. We use the
   feed if the terms allow derived products and don't forbid
   commercial use, redistribution or require share-alike. Its
   attribution text, or else its attribution instructions, goes into
   the BOM.

The Mobility Database only gives a link to the license terms. We record
that link in the BOM, but it does not qualify a feed on its own. The
run report gives each feed's license, or the permission it was used
under.

The Atlas summary is a third party's reading of each agency's terms,
not a license. If you need certainty for one region, read the terms
linked from the BOM.

Attribution is per feed. The data BOM lists every feed that went in,
with its license, the attribution text its publisher asks for, and,
for a feed used by permission, a link to that permission. The
GeoTIFF's `Copyright` tag points to it. If you publish something based
on the file, credit "public transport data from N GTFS feeds", with N
from the `Copyright` tag. Link to the BOM, or reproduce its
attributions.

`--licenses any` uses every feed regardless of license. It is meant for
internal analysis only. Its files still carry ODbL metadata, but they
must not be published.

Whether bare counts of departures per place are protected by
copyright or database rights at all is a separate legal question; this
page is not legal advice.

## Reproducing the files

```sh
cargo run --release -- transit-departures \
  --cache s3://my-bucket/geosignals --output-dir transit
```

`--date 2026-10-26` names the run's day; it defaults to today (UTC).
`--only ch/` restricts a run to some regions, and may be given several
times. See [Running geosignals](running.md) for the container, the data
BOM, `datapackage.json` and scratch space.

**Catalogs.** Transitous and the Transitland Atlas are downloaded as
the tarballs GitHub serves for their `main` branch, and the Mobility
Database as its CSV export. They are cached like feeds, under
`catalogs/<name>/`, so an outage at GitHub falls back to the last copy.
`--transitous` and `--transitland-atlas` also take a local tarball or
checkout, and `--mobility-database` a local CSV file; `none` skips the
sources that need the Atlas or the Mobility Database.

**Logins.** A feed in `checked.json` with `credentials` needs our own
login, set as `<PREFIX>_USERNAME` and `<PREFIX>_PASSWORD`, such as
`NETWORK_RAIL_USERNAME` and `NETWORK_RAIL_PASSWORD`. The password goes
only to the feed's URL, not to where that redirects, and the BOM
records the URL we asked for, not the signed URL it redirected to.
Without the variables, the feed is skipped, except with `--offline`.

**Licenses.** `--licenses open` (the default) uses only feeds under a
license we may republish, as described in [Licenses and
attribution](#licenses-and-attribution). `--licenses any` uses all
feeds, for internal use only.

**Cache.** `--cache` is a directory or an S3-compatible bucket
(`s3://bucket/prefix`, configured with `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_REGION` and `AWS_ENDPOINT_URL`). For each
feed it keeps the last download, its `ETag`, `Last-Modified` and
SHA-256, and a condensed copy of its median week. A run asks each
publisher whether the feed changed (`If-None-Match`,
`If-Modified-Since`), retries network errors, tries mirrors such as
MobilityData's daily copy, and if all fail uses the cached copy,
however old, as long as its timetable ended at most
`--max-expired-days` (90) before `--date`. `--offline` uses only the
cache. The Swiss and German national feeds (160 and 230 MB) condense
to 11 and 59 MB; a planet-wide cache should need some 20–40 GB of raw
feeds plus a few GB of condensed ones.

**Memory.** The condensed feeds live in memory-mapped scratch files
in `--workdir`, so the kernel pages them in and out as needed; put
`--workdir` on disk, not on a RAM disk. The heap holds little more
than the feeds being condensed: 200–250 MB for the Swiss or German
national feed, among the largest anywhere, so `--threads` still bounds
memory. On the Swiss, German and bodo feeds, a warm run takes 16
seconds and 100 MB of heap, and still finishes within a 300 MB memory
limit; a cold run on 4 threads, which condenses all three at once,
takes a minute and 430 MB of heap, within a 1 GB limit. A planet-wide
run should fit on a 2 GB machine with 2–4 threads. Scratch disk needs
about 12 bytes per stop time of the median week, a few GB planet-wide,
plus the downloads in progress.
