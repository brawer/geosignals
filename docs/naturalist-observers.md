<!--
SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
SPDX-License-Identifier: MIT
-->

# Naturalist observers

This data package tells how many different people reported wildlife from
each place on Earth, according to two citizen-science projects:
[iNaturalist](https://www.inaturalist.org) and
[eBird](https://ebird.org). It is meant as a signal of where people go
outdoors to look at nature: parks, nature reserves, trails, coasts and
birding spots. It is not a map of biodiversity.

This page is for data analysts who consider using the files. It covers
what the files contain, how they are computed, their artifacts and
biases, and their licenses. The numbers come from the run on GBIF's
snapshot of 1 October 2026.

## The files

Each release has three Cloud-Optimized GeoTIFFs, each with a data bill
of materials (`.cdx.json`), and one `datapackage.json` that lists them
with their licenses:

| File | Input records | License | Records used | Observers | Size |
| --- | --- | --- | ---: | ---: | ---: |
| `ebird-observers.2026-10-01.cc-by-4.0.tiff` | eBird, CC0 and CC BY | CC BY 4.0 | 1,776 M | 1.04 M | 32 MB |
| `inaturalist-observers.2026-10-01.cc-by-4.0.tiff` | iNaturalist, CC0 and CC BY | CC BY 4.0 | 28.0 M | 27,492 | 12 MB |
| `inaturalist-observers.2026-10-01.cc-by-nc-4.0.tiff` | iNaturalist, CC0, CC BY and CC BY-NC | CC BY-NC 4.0 | 155.8 M | 2.04 M | 34 MB |

The date in each name is the date of the GBIF snapshot. A dated file
never changes; the next monthly release gets new files.

The two iNaturalist files come from the same records with the same
processing. They differ only in which licenses go in, so the CC BY file
counts a subset of the people in the NonCommercial file. Comparing them
pixel by pixel shows what commercial users lose.

## The sources

Both datasets come from the monthly
[GBIF occurrence snapshot](https://registry.opendata.aws/gbif/) on
Amazon S3. It is public, needs no account, and holds every occurrence
that GBIF publishes, about 1.9 billion records for our two datasets
alone.

- **eBird.** The
  [EOD – eBird Observation Dataset](https://doi.org/10.15468/aomfnb), by
  the Cornell Lab of Ornithology. Birders submit checklists of the
  birds they saw or heard at one place and time. Each species on a
  checklist becomes one record, so one outing typically yields dozens
  of records with the same observer and location.
- **iNaturalist.** The
  [iNaturalist Research-grade Observations](https://doi.org/10.15468/ab3s5x)
  dataset. People upload photos or sounds of any organism, and the
  community identifies them. Only observations that reached "research
  grade" (a date, a location, evidence and an agreed identification)
  go to GBIF. Casual and unidentified observations are not included.

The files cover all years in the snapshot, not one season or year.
Most of the data is recent: both projects have grown steeply, so the
last few years dominate.

The two datasets are not equally fresh. iNaturalist sends its data to
GBIF continuously; the October 2026 snapshot has observations up to
the last weeks before it. The eBird dataset is
[released to GBIF once a year](https://www.gbif.org/dataset/4fa7b334-ce0d-4e88-aaae-2e0c138d049e),
and in the October 2026 snapshot it ends in December 2024. Its newest
observations are therefore 10 to 22 months old.

## What a pixel means

A pixel counts **distinct observers**, not records. Someone who
reported 300 birds from one park counts once there. This dampens the
effect of a few very active people: on iNaturalist, the top 1% of
observers made more than half of all observations.

An observer is whoever GBIF lists under `recordedby`. For eBird this
is an anonymous observer code such as `obsr123456`. For iNaturalist it
is the person's name or login. A record with several names (a group
outing, say) counts as one observer, the group. We keep observers only
as 64-bit hashes of these names, so the files hold no names.

The two datasets do not share observer identities. Someone who uses
both apps is counted once in each file. Do not add the eBird and
iNaturalist files if you want people, because those people would be
counted twice.

### Grid and encoding

The files use the Web Mercator grid (EPSG:3857) and cover the world up
to about ±85° latitude.

- **Finest level.** Zoom-8 tiles of 256 × 256 pixels, so each pixel is
  a zoom-16 tile. That is about 611 m wide at the equator, 430 m at
  45° and 306 m at 60°. Finer pixels would mostly show GPS noise and
  the paths of single people.
- **Overviews.** Eight overview levels follow, down to one tile for the
  world.
- **Values.** A pixel stores observers per km² of real Earth surface,
  so pixels can be compared across latitudes. The values are unsigned
  16-bit integers on a log scale:

```
v = 0                                        no observers
log2(observers per km²) = 0.01 · v − 15.01   otherwise
```

Each step of 100 doubles the density. One observer in a pixel at the
equator (0.37 km²) gives about 2.7 observers per km², or v ≈ 1643. To
get counts back, multiply the density by the pixel's area. The GeoTIFF
carries `SCALE` and `OFFSET` metadata, so GDAL and QGIS show
log2(observers per km²) directly. It also carries a raster attribute
table with a histogram of the values.

### Overview pixels

Overview pixels are not sums or averages of the pixels below them. Each
overview pixel counts the distinct observers of its whole area, counted
again from scratch. Take a birder who reported from two neighbouring
pixels. Each of the two pixels counts them once, and the parent pixel
counts them once too, not twice.

As a result, a parent's count is at least that of its busiest child,
and at most the sum of its children's counts. In density terms, a
parent is at most the average of its children's densities. It is
equal only where nobody's reports cross a pixel boundary. The more
people move around within an area, the further the parent falls below
that average. Large parks and birding regions where people visit many
spots show this most.

So use the overview at the zoom level you need. Don't build your own
overviews by resampling the finest level, because summing or averaging
would count observers more than once. The counts are exact at every
level, not estimated.

## How it is computed

1. **Reading.** For each of the ~10,000 Parquet files in the snapshot,
   we read the footer and the `datasetkey` column with HTTP range
   requests. Only for parts that hold one of our datasets do we also
   read `license`, `decimallatitude`, `decimallongitude`,
   `coordinateuncertaintyinmeters` and `recordedby`. A run reads about
   4 GB of the 300 GB snapshot.
2. **Filtering.** We drop records that fail any of these tests:
   - The record has a location. We dropped 341,439 without one.
   - The stated coordinate uncertainty is at most 10 km. We dropped
     12.5 M, almost all from iNaturalist (see below). Records without a
     stated uncertainty are kept.
   - The record has an observer. We dropped 28 without one.
   - The license is CC0, CC BY or CC BY-NC. In October this dropped no
     records at all: iNaturalist only passes observations under these
     three licenses to GBIF, so ShareAlike, NoDerivatives and
     unlicensed observations never reach the snapshot, and all eBird
     records are CC0 or CC BY. On iNaturalist itself, they make up about 4%
     of the observations with an open photo license.
3. **Counting.** Every remaining record becomes an (observer, pixel)
   pair. An external sort brings the pairs in order of observer, then
   pixel. Pixel keys follow a Hilbert curve, where dropping the last
   two bits of a key gives its parent pixel. Dropping repeated pairs
   then yields the distinct pairs of every zoom level in one pass. A
   full run takes under 20 minutes and about 1.2 GB of memory.

## Artifacts and biases

**Who goes where.** The files measure where naturalists go, not where
nature is.

- **Wealthy regions.** Coverage is heavily skewed toward North
  America, Europe, Japan, South Africa, Australia and New Zealand. On
  iNaturalist, 49% of observations are from the USA and Canada and 4%
  from all of Africa.
- **Places people can reach.** Within a region, observations follow
  roads, trails, railways and coasts. Faint ship tracks cross the
  oceans.
- **Cities.** Urban parks and green spaces are well covered.
- **Few observers.** A pixel with one or two observers says little.
  Many rural pixels have a single observer, often the one person who
  lives or works there.

**eBird hotspots.** Birders often file checklists at shared "hotspots",
which put a single point on a park, a lake or a reserve. These pixels
stand out sharply, while the area around them looks empty, even if
people walked all over it. Travelling checklists cover a route of up
to several kilometres but carry one point: eBird
[asks birders](https://support.ebird.org/en/support/solutions/articles/48001059718)
to plot the starting point, or a hotspot that best describes the route.
So a pixel may stand for a whole walk that started there.

**eBird sensitive species.** eBird keeps a list of
[sensitive species](https://www.audubon.org/magazine/ebird-now-censoring-sightings-325-risk-species),
such as some owls, parrots and raptors that suffer from poaching or
disturbance, and leaves their records out of its public outputs. In
GBIF's copy, they are missing entirely. For example, GBIF holds
[29,588 Gyrfalcon records](https://www.gbif.org/occurrence/search?taxon_key=8069880),
723 of them from iNaturalist, but
[none from eBird](https://www.gbif.org/occurrence/search?taxon_key=8069880&dataset_key=4fa7b334-ce0d-4e88-aaae-2e0c138d049e).
The same holds for the Helmeted Hornbill, the Philippine Eagle and the
Night Parrot (checked on 28 October 2026). This matters little for
observer counts, since the same birders report other species from the
same places.

**iNaturalist blurring.** iNaturalist moves about 6% of observations to
a random point in a 0.2° box, about 22 km wide. It does this when the
observer asks, or when the species is threatened. GBIF marks these
records with an uncertainty of 20–30 km, so our 10 km cut removes
them. The share varies by country, from about 1% in Russia to 7.5% in
the USA and 13% in Japan. Places with many threatened species, or with
privacy-minded observers, are therefore slightly undercounted.

**Location precision.** Phone GPS is usually good to tens of metres.
On iNaturalist, about 54% of records claim 100 m or better and 68%
claim 1 km or better. About 19% give no uncertainty and are kept. Some
records are placed at the centre of a town, a park or a grid square.
These artificial points show up as isolated bright pixels.

**The CC BY iNaturalist file is not a random sample.** Only about 16%
of iNaturalist data is CC0 or CC BY. Most people keep the default
CC BY-NC. Which people chose an open license varies a lot by country:
about 40% of observations in the United Kingdom, 33% in New Zealand,
14% in the USA and 10% in Mexico. With 27,492 observers for 28 M
records, the file is also dominated by a small number of very active
people. Expect it to look much sparser and more uneven than the
NonCommercial file.

**Time.** The files pool all years, so places that were popular ten
years ago still count. Growth also differs by region, as each project
spreads at its own pace.

**Name variants.** Observers are matched by the exact text of
`recordedby`. The same person entered under two spellings counts
twice, and two people who share a name count once. On eBird, with its
observer codes, this is rare. On iNaturalist it can happen, because
GBIF shows a person's display name where one is set.

## Licenses and attribution

Each record in GBIF carries its own license, chosen by the person who
submitted it. Each file only uses records whose licenses are compatible
with the license of the file:

- **CC BY 4.0 files** (eBird, and iNaturalist CC BY) may be used for
  any purpose, including commercial use, with attribution.
- **The CC BY-NC 4.0 file** may only be used for noncommercial
  purposes. That includes hobby, nonprofit and research use, not only
  academic work.

Records under ShareAlike or NoDerivatives licenses, and unlicensed
records, are never used. GBIF does not take them from iNaturalist in
the first place.

GBIF asks that each dataset be cited by its DOI. Each file's
`Copyright` tag, data BOM and `datapackage.json` give the attribution
to use:

- *Cornell Lab of Ornithology and eBird contributors, EOD – eBird
  Observation Dataset, https://doi.org/10.15468/aomfnb*
- *iNaturalist contributors, iNaturalist Research-grade Observations,
  https://doi.org/10.15468/ab3s5x*

Add the snapshot date, which is also in the file name. Whether bare
counts of people per place are protected by copyright or database
rights at all is a separate legal question; this page is not legal
advice.

## Reproducing the files

```sh
cargo run --release -- naturalist-observers --output-dir observers
```

`--snapshot 2026-10-01` reads a given snapshot instead of the latest,
and `--bucket` reads from one of GBIF's mirrors in other AWS regions.
See the [README](../README.md#naturalist-observers) for the options.
