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
    },
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
        } => {
            let threads = threads.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            });
            let cfg = osmsignals::node_density::Config {
                output_dir,
                workdir,
                threads,
                run_size,
                sample_dir,
                sample_rate,
                max_blocks: smoke_test.then_some(5),
            };
            let input = std::io::BufReader::with_capacity(1 << 20, open(&input)?);
            let summary = osmsignals::node_density::run(input, &cfg)?;
            eprintln!("{summary:#?}");
        }
    }
    Ok(())
}

fn open(input: &str) -> Result<Box<dyn Read + Send>> {
    if input == "-" {
        return Ok(Box::new(std::io::stdin()));
    }
    if input.starts_with("https://") || input.starts_with("http://") {
        let reader = osmsignals::http::HttpReader::open(osmsignals::http::agent(), input)?;
        eprintln!("reading {}", reader.url());
        return Ok(Box::new(reader));
    }
    let f = std::fs::File::open(input).with_context(|| format!("opening {input}"))?;
    Ok(Box::new(f))
}
