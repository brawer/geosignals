<!--
SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
SPDX-License-Identifier: MIT
-->

# Nominatim secondary importance

This data package republishes a raster that
[Nominatim](https://nominatim.org), the geocoder of OpenStreetMap, uses
to rank search results. For each place on Earth, it tells how important
the town or municipality around it is, as measured by how often
Wikipedia links to that town's article. It is a signal of how
well-known an area is. It does not measure population, activity or
how many people search for a place.

This page is for data analysts who consider using the file. It covers
what the file contains, how Nominatim ranks search results and where
the raster fits in, how the raster was made, its artifacts and biases,
and its license. The numbers come from the file of 4 May 2024, which
is still the current one on 28 October 2026.

## The files

Each release has one Cloud-Optimized GeoTIFF, a data bill of materials
(`.cdx.json`) and a `datapackage.json`:

| File | What it holds |
| --- | --- |
| `nominatim-secondary-importance.2024-05-04.tiff` | The raster (9 MB) |
| `nominatim-secondary-importance.2024-05-04.cdx.json` | The input's URL, date, SHA-256 and SHA-512, and the hashes of the GeoTIFF |
| `datapackage.json` | The above as a Frictionless data package, with the license |

The date in each name is the `Last-Modified` date of Nominatim's file,
not the day of our run. As long as Nominatim does not publish a new
raster, every run gives a file with the same name and the same values.

## The source

Nominatim publishes the raster as
[`wikimedia-secondary-importance.sql.gz`](https://nominatim.org/data/wikimedia-secondary-importance.sql.gz)
(7.7 MB), a dump of a PostGIS raster table that Nominatim servers load
into their database. It is made by
[osm-search/secondary-importance](https://github.com/osm-search/secondary-importance)
from a planet-wide Nominatim database. We convert it to a GeoTIFF and
add overviews, but don't change any value, so a pixel reads the same
as in Nominatim's database.

The raster combines two things, both as they were in spring 2024:

- **OpenStreetMap areas:** the boundaries of municipalities, towns,
  villages, suburbs and similar places.
- **Wikipedia importance:** a score for each Wikipedia article, from
  [osm-search/wikipedia-wikidata](https://github.com/osm-search/wikipedia-wikidata).
  It counts the links to an article from other articles, in 39
  language editions, and takes the logarithm. The most-linked article
  (United States) gets 1, and the scale is logarithmic: an article
  with 1% as many links gets 0.70. Nominatim publishes these scores as
  [`wikimedia-importance.csv.gz`](https://nominatim.org/data/wikimedia-importance.csv.gz),
  which it updates more often than the raster; the copy of November
  2025 is the newest.

## How Nominatim ranks search results

A search such as "Springfield" or "Bahnhofstrasse 12, Zürich" finds
many candidates. Nominatim orders them by two numbers: how well a
candidate matches the query, and how important the candidate is. The
raster plays only a small part in the second number. This section
follows Nominatim's source code as of October 2026.

### How well a result matches

Nominatim first splits the query into words, and tries different ways
of reading them: as a name, a street, a house number, a postcode, a
city, a country or a type of place such as "cafe". Each reading gets a
**penalty**, from 0 for the most likely reading upwards. For example, a
word that only matches the start of a name, a house number without a
street, a postcode that doesn't match the found place, or words read in
an unusual order all add to it. Most penalties are steps of 0.1 to 1.

Nominatim runs its database searches in order of penalty, and stops
when the remaining ones can't beat the results found so far. Each
result gets an **accuracy**, which starts with the penalty of its
search. Then Nominatim compares the result's name and address with the
words of the query, and adds a little for every query word that doesn't
appear in it. Lower accuracy is better.

### How important a place is

Every place in Nominatim's database has an **importance** between 0 and
1. It is computed once, when the place is imported or changed, by the
function
[`compute_importance`](https://github.com/osm-search/Nominatim/blob/master/lib-sql/functions/importance.sql):

1. **Wikipedia tag.** If the place has a `wikipedia` tag of the form
   `de:Zürich`, and that article (or a redirect to it) is known, the
   importance is the article's score. Next it tries `wikipedia:de`,
   `wikipedia:fr` and the like for 40 languages.
2. **Wikidata tag.** Otherwise, if the place has a `wikidata` tag whose
   item has a known article, the importance is that article's score.
3. **Default by rank.** Otherwise, the importance comes from the
   place's search rank *r*, a number from 1 (continents) to 30 (shops,
   buildings, addresses) that Nominatim
   [assigns by type](https://nominatim.org/release-docs/latest/customize/Ranking/):

   ```
   importance = 0.40001 − r / 75 + v / 655350000
   ```

   Here *v* is the value of this raster at the place's centroid.

Some places borrow importance from another one. A municipality's
boundary in OpenStreetMap is often accompanied by a point that marks
its centre and carries the Wikipedia tags. Nominatim merges the two,
and the boundary takes the higher importance of the two.

Here are some typical values:

| Place | Importance |
| --- | ---: |
| United States, from Wikipedia | 1 |
| A large city, from Wikipedia | 0.7–0.85 |
| A small town, from Wikipedia | 0.4–0.6 |
| A country without a Wikipedia match (*r* = 4) | 0.347 |
| A city without a Wikipedia match (*r* = 16) | 0.187 |
| A village without a Wikipedia match (*r* = 19) | 0.147 |
| A street (*r* = 26) | 0.053 |
| A shop or an address (*r* = 30) | 0.00001 |

Shops, restaurants and most other points of interest almost never have
a Wikipedia article. For them, only the rank and the raster count.

### Putting both together

Each result's **ranking** is its accuracy minus its importance. The
results are sorted by ranking, from low to high, so a good match beats
a well-known place only if the difference in accuracy is larger than
the difference in importance. Nominatim also drops results whose
ranking is more than 0.5 behind the best one, and is a little stricter
with results that are smaller places (a higher search rank) than the
best one.

Two exceptions:

- **Searches near a place.** For a query such as "cafe near Zürich
  Hauptbahnhof", Nominatim orders the cafés by their distance to the
  station, and importance doesn't count.
- **Reverse geocoding.** Looking up the address of a coordinate uses
  distance and address rank only. Importance plays no part.

### What the raster does in ranking

The raster adds at most *v* / 655350000 to the importance, which is
less than 0.0001 for every pixel in the file. By design, it is a
tie-breaker. The next step in the default by rank is 1/75 ≈ 0.013.
Two Wikipedia scores differ by less than 0.0001 only if the articles'
link counts are within about 0.15% of each other. Accuracy rarely
differs by less than 0.01. So in practice the raster only decides
between results that match the query equally well, have the same rank
and have no Wikipedia article.

That is common, though. Take two villages called "Neudorf", both without
a Wikipedia article. Both get 0.40001 − 19/75 ≈ 0.1467. The one in a
pixel with *v* = 26,215 gets 0.0000400 on top, the other, in a pixel with
*v* = 18,023, gets 0.0000275. So the first one is listed first. The
same goes for streets, shops and addresses with the same name. The raster
then puts the result in the better-known town first.

If you want to use the raster as Nominatim does, you need the place's
tags and rank. Read *v* at the place's centroid from this file, and
follow the three steps above. If you use the raster for something else,
such as a feature in a model, there is no need to scale it down: use
*v* / 65536 directly.

## What a pixel means

A pixel holds the **highest importance of the areas that cover it**.
Only areas count whose search rank is below 25 (cities, towns, villages,
suburbs, neighbourhoods) and whose address rank is 13 or more. Larger
units, such as countries, states, provinces and counties, are not
painted at all. Where several areas overlap, such as a city and one of
its suburbs, the pixel takes the higher of the two, usually the city.
Where no area covers a place, the pixel is 0. Place nodes, such as a
village mapped as a single point, also count, as small circles around
the point (see [How the raster was made](#how-the-raster-was-made)).

An area's importance comes from its Wikipedia article, as described
above. Without a match, it is a default for its rank. The defaults
follow an older Nominatim formula, 0.75001 − *r* / 40, not today's
0.40001 − *r* / 75. In the file:

- **Default values.** 37% of painted pixels hold a default. The most
  common one, 0.40001 (rank 14, a municipality), covers 14% of painted
  pixels. 0.27501 (rank 19, a village) and 0.35001 (rank 16, a city)
  follow, with 7% each.
- **Wikipedia values.** The other 63% hold a Wikipedia score, with
  8,500 distinct values in all.

Across painted pixels, the median importance is 0.36. A few samples:

| Place | Importance |
| --- | ---: |
| Paris, London | 0.83 |
| Manhattan | 0.82 |
| Berlin-Mitte | 0.79 |
| Tokyo | 0.76 |
| Zürich | 0.69 |
| Lagos | 0.60 |
| Springfield, Illinois | 0.57 |
| Lauterbrunnen, a Swiss village | 0.48 |
| The Nevada desert, rural Kansas | 0 |

The highest value in the file, 0.85 (*v* = 55,416), is not in a
capital. It is a mapping error. It sits in four pixels around 3.01° S,
44.84° W (x 6150–6151, y 8465–8467), in the rural municipality of
São João Batista in Maranhão, Brazil. The pixels around them hold the
municipality's 0.43. The hamlet at that spot, Olinda dos Aranhas,
scores 0.15 in today's Nominatim.

The cause is a place node that existed from 23 January to 7 June 2024,
so it was in the database when the file was made on 4 May 2024:
[node 11542167247](https://www.openstreetmap.org/node/11542167247),
"Povoado Olinda", tagged `place=neighbourhood`. A new mapper had tagged
it with `wikidata=Q155` and `wikipedia=pt:Brasil`, the article on the
whole country. Nominatim therefore gave this hamlet of 620 people the
importance of Brazil, which is higher than that of Paris. The same
mapper deleted the node in June 2024, and the hamlet has since been
mapped again without the error.

The four pixels are exactly those whose centres lie within 0.02° of the
node. That is the radius Nominatim uses for a place of this rank when
it matches places by location. So the published file also paints place
nodes, as small circles, even though the scripts in the repository
only paint polygons (see [How the raster was made](#how-the-raster-was-made)).

Single pixels or small patches far above their surroundings are worth
treating with suspicion, since one wrong `wikipedia` or `wikidata` tag
is enough to cause them.

### Grid and encoding

The file keeps Nominatim's grid: plain longitude and latitude
(EPSG:4326), from −180° to 180° and from 90° to −90°, with 16,384 × 16,384
cells. That is not Web Mercator, unlike the other geosignals rasters.

- **Pixel size.** A pixel is 360/16384° wide and 180/16384° tall. At the
  equator, that is about 2.4 km wide and 1.2 km tall. At 45° it is
  1.7 × 1.2 km, at 60° 1.2 × 1.2 km. (The secondary-importance project's
  README speaks of 250 m pixels, but the published file is coarser.)
- **Finding a pixel.** The pixel holding a location is:

  ```
  x = floor((lon + 180) / 360 · 16384)
  y = floor((90 − lat) / 180 · 16384)
  ```

- **Values.** Pixels are unsigned 16-bit integers, as in Nominatim's
  database:

  ```
  v = 0      no area (also declared as the NoData value)
  v > 0      importance = v / 65536
  ```

The GeoTIFF carries `SCALE` and `OFFSET` metadata, so GDAL and QGIS
show importance between 0 and 1 directly. It also carries a raster
attribute table with a histogram of the values, with one row for each
value that occurs.

### Overview pixels

Six overview levels halve the resolution, down to one 256 × 256 tile
for the world. An overview pixel holds the **largest** of the four
pixels below it, just as Nominatim keeps the highest importance where
areas overlap. Overviews therefore show where the important places are,
but they overstate the importance of the surrounding countryside. An
overview pixel that touches a big city takes the city's value. For
averages over an area, read the full-resolution pixels and aggregate
them yourself.

## How the raster was made

The scripts of
[osm-search/secondary-importance](https://github.com/osm-search/secondary-importance)
run inside a planet-wide Nominatim database. They split the world into
tiles. For each tile, they take every polygon with a search rank below
25, compute its importance, and rasterize it. Where polygons overlap,
the highest value wins. The published scripts paint only polygons.
The published file, however, also paints place nodes as small circles:
the four pixels of the Maranhão peak described above match a circle of
0.02° around a `place=neighbourhood` node exactly. That radius is
Nominatim's `reverse_place_diameter` for places of search rank 20 and
above. The file was apparently made with a variant of the scripts that
also differs in the default values (see
[What a pixel means](#what-a-pixel-means)).

An area's value is its importance × ((address rank + 2) / 15) × 65536,
rounded. The second factor uses integer division, so it is 0 for
address ranks below 13 and 1 for ranks 13 to 27. This is what leaves out
countries, states and counties.

Nominatim has not published a new raster since May 2024. It is not
clear when it will.

## Artifacts and biases

**Boundaries, not people.** Each pixel takes the importance of the
whole municipality around it. A farm at the edge of a large
municipality gets the same value as its town hall. In countries with
large municipalities, such as Brazil, China or Australia, whole regions
take the value of a single town. In countries with small ones, the
values change every few kilometres.

**Coverage follows OpenStreetMap boundaries.** About 17% of the Earth's
surface is painted, while land makes up 29%. Where OpenStreetMap has no
municipal boundaries, or they have a rank outside the painted range,
the raster is 0, even in inhabited areas. Germany and France are fully
painted, and so is most of eastern China and interior Brazil. About a
third of Kansas and 4% of Nevada are painted. Unincorporated land in the
United States, deserts and the open sea are 0. A zero means "no
painted area", not "unimportant".

**Wikipedia's view of the world.** Wikipedia importance measures how
often editors link to an article. Places in countries with large
Wikipedia editions, and places with historical, political or tourist
significance, score higher than places of the same size elsewhere. The
39 languages that are counted cover Europe and East Asia better than
Africa and South Asia: there is Hindi, but neither Bengali nor Swahili,
for example.

**Tags decide everything.** An area only gets a Wikipedia score if
someone tagged it with `wikipedia` or `wikidata` in OpenStreetMap. A
wrong tag, such as a village tagged with the article about its region,
gives the village the region's importance. A missing tag gives the
default for its rank, even for a well-known town. Many of the sharpest
jumps between neighbouring municipalities are tagging differences, not
differences in importance.

**Defaults are not comparable to Wikipedia scores.** A rank default
says nothing about the place itself. A municipality without a tag
gets 0.40001, more than many small towns with a Wikipedia article. Use
the histogram to separate the default values from the rest if this
matters for your analysis.

**Old formula.** The defaults follow an older Nominatim formula.
Today's Nominatim gives places without an article much lower defaults,
so the raster's defaults are higher than the importance the same areas
would get today.

**Age.** The file reflects OpenStreetMap boundaries and Wikipedia links
from spring 2024. New municipal mergers, new boundaries and newly
tagged articles are missing.

## License and attribution

The file is published under the
[Open Database License (ODbL) 1.0](https://opendatacommons.org/licenses/odbl/1-0/),
like Nominatim's original and OpenStreetMap itself. Credit
"© OpenStreetMap contributors", as the file's `Copyright` tag does. It
adds that the importance comes from Nominatim, based on Wikipedia
links.

Wikipedia's text is under CC BY-SA, but the raster holds no text, only
scores derived from counting links. Whether such counts are protected
by copyright or database rights at all is a separate legal question;
this page is not legal advice.

## Reproducing the files

```sh
cargo run --release -- nominatim-secondary-importance --output-dir nominatim
```

This downloads the 7.7 MB file from nominatim.org and takes about 20
seconds. `--input` reads a local copy instead. See the
[README](../README.md#nominatim-secondary-importance) for the options.
