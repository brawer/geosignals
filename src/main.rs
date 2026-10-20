// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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
        /// OSM PBF file to read, or "-" for standard input.
        #[arg(long)]
        input: PathBuf,
        /// GeoTIFF file to write.
        #[arg(long)]
        output: PathBuf,
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
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::NodeDensity {
            input,
            output,
            workdir,
            threads,
            run_size,
            sample_dir,
            sample_rate,
        } => {
            let threads = threads.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            });
            let cfg = osmsignals::node_density::Config {
                output,
                workdir,
                threads,
                run_size,
                sample_dir,
                sample_rate,
            };
            let summary = if input.as_os_str() == "-" {
                osmsignals::node_density::run(
                    std::io::BufReader::with_capacity(1 << 20, std::io::stdin()),
                    &cfg,
                )?
            } else {
                let f = std::fs::File::open(&input)
                    .with_context(|| format!("opening {}", input.display()))?;
                osmsignals::node_density::run(std::io::BufReader::with_capacity(1 << 20, f), &cfg)?
            };
            eprintln!("{summary:#?}");
        }
    }
    Ok(())
}
