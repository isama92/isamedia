mod app;
mod apps;
mod cli;
mod config;
mod event;
mod jellyfin;
mod player;
mod secrets;
mod shell;
mod ui;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;

use crate::cli::Cli;
use crate::config::Config;
use crate::shell::Shell;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let _log_guard = init_logging(cli.debug.as_deref())?;

    let config_path = match cli.config {
        Some(path) => path,
        None => Config::default_path()?,
    };
    let config = Config::load_or_init(&config_path)?;
    // Set the palette before the first draw so the initial frame is themed.
    crate::ui::theme::init(config.theme);
    let config = Arc::new(Mutex::new(config));

    let (tx, rx) = mpsc::unbounded_channel();
    event::spawn_input_thread(tx.clone());
    event::spawn_tick_task(tx.clone());

    let apps = apps::build_apps(config.clone(), config_path.clone(), tx);
    let mut shell = Shell::new(apps, config, config_path, rx);

    // init() puts the terminal in raw mode + alternate screen and installs a
    // panic hook that restores it, so a crash never leaves the terminal broken.
    let mut terminal = ratatui::init();
    let result = shell.run(&mut terminal).await;
    ratatui::restore();
    result
}

fn init_logging(
    path: Option<&std::path::Path>,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    let Some(path) = path else {
        return Ok(None);
    };
    // The log names the server, media titles and every (redacted) mpv
    // command; keep it owner-only like the config file.
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    // mode() only applies on creation; also tighten a log file that already
    // existed with looser permissions (through the handle, no path race).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let (writer, guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(writer)
        .init();
    tracing::info!("enabled debug logging");
    Ok(Some(guard))
}
