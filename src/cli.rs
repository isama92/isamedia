use std::path::PathBuf;

use clap::Parser;

/// Terminal client for Jellyfin, Radarr and Sonarr.
#[derive(Debug, Parser)]
#[command(name = "isamedia", version, about)]
pub struct Cli {
    /// Config file path
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Debug log file path (enables debug logging)
    #[arg(short, long, value_name = "FILE")]
    pub debug: Option<PathBuf>,
}
