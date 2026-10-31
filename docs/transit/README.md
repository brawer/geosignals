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
and its licenses. How the pipeline is built, and why, is in the
[technical design](TECHNICAL_DESIGN.md).

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
and a [STAC](../running.md#stac-catalog) item and collection:

| File | What it holds |
| --- | --- |
| `transit-departures.2026-10-26.tiff` | The raster |
| `transit-departures.2026-10-26.cdx.json` | Every catalog and feed that went in, with its URL, SHA-256, license and attribution |
| `transit-departures.2026-10-26.report.json` | Each feed's status, weeks counted, departures and duplicates, and the sources that were skipped, with reasons |
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

For Japan, we also read [gtfs-data.jp](https://gtfs-data.jp), the
national repository of GTFS feeds, mostly of municipal and community
buses. We take its list of feeds and their licenses from the copy that
the Transitland Atlas keeps. Where Transitous lists the same feed, we
use the gtfs-data.jp entry instead. As of 28 October 2026, it lists 566
static feeds: 456 under CC BY 4.0, 72 under CC0 and 38 without a
license.

**Feeds.** The catalog of 27 October 2026 lists 1,483 static GTFS
feeds that resolve through Transitous and the Atlas, in 128 regions.
Another 633 point into the Mobility Database. France (446), Poland
(139), Hungary (86) and Canada (98) have the most entries. One national
feed can cover a whole country, though: Germany has 9 entries and
Switzerland 3.

We also add feeds that Transitous doesn't list, whose licenses we
checked ourselves, from
[`src/transit/checked.json`](../../src/transit/checked.json). So far this
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
feed is skipped. Network Rail [releases its data
feeds](https://www.networkrail.co.uk/who-we-are/transparency-and-ethics/transparency/)
under the Open Government Licence. See [Timetables in
CIF](#timetables-in-cif).

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
  a 400 m long train are one departure each. The output doesn't tell
  modes apart.

### Timetables in CIF

Network Rail's timetable for Great Britain comes in CIF rather than
GTFS. It counts the same way, with a few differences:

- **Departures.** A train's stop counts if passengers may board there.
  Stops where it only sets down, isn't advertised, or passes without
  stopping don't count, nor does its terminus. Freight and empty trains
  count nothing. Where several versions of a train's schedule apply on
  a day, such as a cancellation or a short-term change, only the one in
  force counts.
- **Bank holidays.** The flag that says whether a train runs on bank
  holidays is ignored.
- **Stations.** CIF has no coordinates. Stations are placed by their
  entry in
  [NaPTAN](https://www.gov.uk/government/publications/national-public-transport-access-node-schema).
  Departures at the few stations that NaPTAN lacks are dropped, and
  the run log names them.

### The 12-month average

Each pixel shows the departures on an average day of the 52 weeks up
to the run. An average, rather than a typical week, keeps seasonal
service: a ski bus that runs four months a year counts for about a
third of its winter rate, and a summer ferry likewise. Holidays,
school breaks and timetable changes count with their real weight.

Many feeds publish only the next few weeks, so the year is collected
run by run, week by week (Monday to Sunday). Where a newer timetable
covers a week, it replaces the older one. Which weeks count:

- a week some timetable covered, with the days it covered;
- a week since we first tracked the feed that no timetable covered,
  as zero: the operator ran nothing, or published nothing in time;
- not a week before we tracked the feed that no timetable covered,
  since we don't know it.

There is no filter on weak weeks, since that would lift seasonal
service back to its in-season level. During a feed's first year, the
average covers fewer weeks; the report gives each feed's count.

### Duplicates

Neighbouring feeds often contain the same trips. National feeds include
buses from across the border, and regional feeds overlap with national
ones. Counting both copies would double the density exactly where
feeds overlap.

So we compare every pair of feeds with stations in the same 1 km tile
(zoom 15), on the days both calendars cover. The smaller feed is the
one with fewer departures on an average day. A departure of the
smaller feed matches on a date if the larger feed departs that date
within one pixel (about 150 m) and one minute of it. A trip whose
departures match at least 80% (and at least two of them) is a
duplicate on that date. The copy in the largest feed survives.

Comparing dates keeps a summer timetable from matching a winter one,
and a holiday from matching a normal day. Feeds whose calendars share
no day are not compared, since no day has both copies.

In the test run:

- **bodo:** 619,483 of its 701,938 weekly departures were also in the
  German national feed (measured when we still compared median
  weeks).
- **Switzerland:** about 675,000 of 18.9 million departures a week
  were duplicates on the 31 days it shares with the German feed.
  Almost all were German operators near the border, such as
  Südbadenbus and SWEG. These are the same ones Transitous removes
  by hand.

To check for false matches, we shifted one feed's times by seven
minutes. That matched 0.05% of trips, against 3.8% without the shift.

The report lists, for each pair of feeds, the days they share and how
many departures a week were duplicates, by agency.

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
  - Japan: 528 feeds from gtfs-data.jp, mostly municipal and community
    buses, plus a Tokyo rail feed and Hiroshima's trams. Most
    big-city bus and subway operators are missing, since their data
    needs a key from ODPT, and the JR feed has no license in the
    catalog.
  - Great Britain: buses, trams and ferries in England from BODS, and
    national rail from Network Rail when the run has a login for it, but
    no buses in Scotland or Wales, and nothing in Northern Ireland.
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
   [`checked.json`](../../src/transit/checked.json), the license whose
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

- `--date 2026-10-26` names the run's day. It defaults to today (UTC).
- `--only ch/` restricts a run to some regions, and may be given
  several times.
- `--licenses any` uses all feeds, regardless of license. Its output
  is for internal use only (see [Licenses and
  attribution](#licenses-and-attribution)).
- Network Rail's feed needs `NETWORK_RAIL_USERNAME` and
  `NETWORK_RAIL_PASSWORD`. Without them, it is skipped.

A run keeps each feed's download, a condensed copy and its weekly
history in the cache, so later runs download only what changed, and
fall back to the last copy when a publisher is down. A planet-wide run
should fit on a 2 GB machine. The [technical
design](TECHNICAL_DESIGN.md#running-the-pipeline) covers the cache,
memory, scratch disk and the other flags. [Running
geosignals](../running.md) covers the container, the data BOM and
`datapackage.json`.
