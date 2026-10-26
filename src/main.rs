// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Ranking signals for OpenStreetMap")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Computes OSM node density as a Cloud-Optimized GeoTIFF.
    NodeDensity {
        /// OSM PBF to read: a file, an http(s) URL, or "-" for standard
        /// input. Defaults to the most recent weekly planet dump, which
        /// is streamed over HTTPS without storing it.
        #[arg(long, default_value = osmsignals::http::PLANET_URL)]
        input: String,
        /// Directory for the GeoTIFF, which gets named after the date of
        /// the input data, such as osm-node-density.2026-10-19.tiff.
        #[arg(long)]
        output_dir: PathBuf,
        /// Working directory for temporary files, which can take several
        /// times the size of the input. Each signal uses its own
        /// subdirectory and cleans up leftovers of interrupted runs.
        #[arg(long, alias = "tmpdir", default_value_os_t = std::env::temp_dir())]
        workdir: PathBuf,
        /// Worker threads; defaults to the number of CPUs.
        #[arg(long)]
        threads: Option<usize>,
        /// Keys buffered per thread before spilling a sorted run to disk.
        #[arg(long, default_value_t = 128 << 20)]
        run_size: usize,
        /// Write raw counts of sampled tiles into this directory.
        #[arg(long)]
        sample_dir: Option<PathBuf>,
        /// Sample one in this many tiles at zoom 6 and deeper.
        #[arg(long, default_value_t = 64)]
        sample_rate: u64,
        /// Read only the first few blocks of the input, to check that
        /// the pipeline works end to end without processing the planet.
        #[arg(long)]
        smoke_test: bool,
        /// Identifies this run in its scheduler, such as a Kubernetes
        /// job name; recorded in the data bill of materials.
        #[arg(long, default_value = "")]
        run_id: String,
    },
    /// Republishes Nominatim's secondary importance raster as a
    /// Cloud-Optimized GeoTIFF in EPSG:4326.
    NominatimSecondaryImportance {
        /// The gzipped SQL dump to read: a file or an http(s) URL.
        /// Defaults to the one published by nominatim.org.
        #[arg(long, default_value = osmsignals::nominatim::URL)]
        input: String,
        /// Directory for the GeoTIFF, which gets named after the date the
        /// input was last modified, such as
        /// nominatim-secondary-importance.2024-05-04.tiff. Use a different
        /// directory than for other signals, since each writes its own
        /// datapackage.json.
        #[arg(long)]
        output_dir: PathBuf,
        /// Working directory for temporary files, about as big as the
        /// output. Each signal uses its own subdirectory.
        #[arg(long, alias = "tmpdir", default_value_os_t = std::env::temp_dir())]
        workdir: PathBuf,
        /// Worker threads; defaults to the number of CPUs.
        #[arg(long)]
        threads: Option<usize>,
        /// Identifies this run in its scheduler, such as a Kubernetes
        /// job name; recorded in the data bill of materials.
        #[arg(long, default_value = "")]
        run_id: String,
    },
}

fn default_threads(threads: Option<usize>) -> usize {
    threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    })
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::NodeDensity {
            input,
            output_dir,
            workdir,
            threads,
            run_size,
            sample_dir,
            sample_rate,
            smoke_test,
            run_id,
        } => {
            let threads = default_threads(threads);
            let (reader, source) = open(&input)?;
            let cfg = osmsignals::node_density::Config {
                input: osmsignals::bom::Input::new(&source),
                run_id,
                output_dir,
                workdir,
                threads,
                run_size,
                sample_dir,
                sample_rate,
                max_blocks: smoke_test.then_some(5),
            };
            let input = std::io::BufReader::with_capacity(1 << 20, reader);
            let summary = osmsignals::node_density::run(input, &cfg)?;
            eprintln!("{summary:#?}");
        }
        Command::NominatimSecondaryImportance {
            input,
            output_dir,
            workdir,
            threads,
            run_id,
        } => {
            let (reader, source, timestamp) = open_dated(&input)?;
            let cfg = osmsignals::nominatim::Config {
                input: osmsignals::bom::Input::new(&source).from(&osmsignals::bom::NOMINATIM),
                timestamp,
                run_id,
                output_dir,
                workdir,
                threads: default_threads(threads),
            };
            let summary = osmsignals::nominatim::run(reader, &cfg)?;
            eprintln!("{summary:#?}");
        }
    }
    Ok(())
}

/// Like [`open`], but also returns when the input was last modified:
/// for a URL, its Last-Modified header; for a file, its mtime.
fn open_dated(input: &str) -> Result<(Box<dyn Read + Send>, String, i64)> {
    use osmsignals::date;
    if input.starts_with("https://") || input.starts_with("http://") {
        let reader = osmsignals::http::HttpReader::open(osmsignals::http::agent(), input)?;
        eprintln!("reading {}", reader.url());
        let modified = reader
            .last_modified()
            .with_context(|| format!("{} has no Last-Modified header", reader.url()))?;
        let timestamp = date::parse_http_date(modified)?;
        let url = reader.url().to_string();
        return Ok((Box::new(reader), url, timestamp));
    }
    anyhow::ensure!(
        input != "-",
        "standard input has no date; pass a file or URL"
    );
    let timestamp = date::mtime(std::path::Path::new(input))?;
    let (reader, source) = open(input)?;
    Ok((reader, source, timestamp))
}

/// Opens the input, and returns it with where it actually comes from:
/// for a URL, the one it redirected to.
fn open(input: &str) -> Result<(Box<dyn Read + Send>, String)> {
    if input == "-" {
        return Ok((Box::new(std::io::stdin()), input.into()));
    }
    if input.starts_with("https://") || input.starts_with("http://") {
        let reader = osmsignals::http::HttpReader::open(osmsignals::http::agent(), input)?;
        eprintln!("reading {}", reader.url());
        let url = reader.url().to_string();
        return Ok((Box::new(reader), url));
    }
    let f = std::fs::File::open(input).with_context(|| format!("opening {input}"))?;
    Ok((Box::new(f), input.into()))
}
