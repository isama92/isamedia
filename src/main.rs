mod app;
mod apps;
mod arr;
mod cli;
mod config;
mod event;
mod images;
mod jellyfin;
mod lang;
mod net;
mod player;
mod radarr;
mod secrets;
mod shell;
mod sonarr;
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
    crate::ui::theme::init_accent(config.accent);
    // Likewise the poster mode, so `Off` reserves no space on the first frame.
    crate::images::init(config.images);
    let image_mode = config.images;
    let config = Arc::new(Mutex::new(config));

    let (tx, rx) = mpsc::unbounded_channel();
    // Touches no stdio, so it is safe to start before the terminal is set up.
    event::spawn_tick_task(tx.clone());

    // init() puts the terminal in raw mode + alternate screen and installs a
    // panic hook that restores it, so a crash never leaves the terminal broken.
    // Moved ahead of the input thread for two reasons: the graphics query below
    // has to read stdin, which that thread owns once running; and a panic between
    // here and there previously had no restore hook installed.
    let mut terminal = ratatui::init();
    // Draw one frame before probing the terminal. The probe waits up to two
    // seconds on a terminal that never answers, and a blank alternate screen for
    // that long reads as a hang.
    terminal.draw(draw_startup_frame)?;
    let images = crate::images::Images::start(image_mode);
    event::spawn_input_thread(tx.clone());

    let apps = apps::build_apps(config.clone(), config_path.clone(), tx, images);
    let mut shell = Shell::new(apps, config, config_path, rx);

    let result = shell.run(&mut terminal).await;
    ratatui::restore();
    result
}

/// The one frame drawn before the terminal graphics probe, so a slow probe does
/// not look like a hang. Intentionally minimal: the shell owns all real chrome.
fn draw_startup_frame(frame: &mut ratatui::Frame) {
    use ratatui::text::Line;
    use ratatui::widgets::Widget;

    let area = frame.area();
    if area.height == 0 {
        return;
    }
    Line::styled("  Starting isamedia...", crate::ui::theme::dim()).render(
        ratatui::layout::Rect::new(area.x, area.y, area.width, 1),
        frame.buffer_mut(),
    );
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
