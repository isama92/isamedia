pub mod coming_soon;
pub mod jellyfin;
pub mod settings;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::app::MediaApp;
use crate::config::Config;
use crate::event::{AppSender, Event};

/// All apps shown in the top tab bar, in tab order. Adding a new app (Sonarr,
/// Radarr) means writing its module and swapping its ComingSoonApp entry here;
/// the shell needs no changes.
pub fn build_apps(
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    tx: mpsc::UnboundedSender<Event>,
) -> Vec<Box<dyn MediaApp>> {
    vec![
        Box::new(jellyfin::JellyfinApp::new(
            config.clone(),
            config_path.clone(),
            AppSender::new("jellyfin", tx),
        )),
        Box::new(coming_soon::ComingSoonApp::new("radarr", "Radarr")),
        Box::new(coming_soon::ComingSoonApp::new("sonarr", "Sonarr")),
        // Purely local: owns the config to read/write settings, needs no sender.
        Box::new(settings::SettingsApp::new(config, config_path)),
    ]
}
