pub mod coming_soon;
pub mod jellyfin;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::app::MediaApp;
use crate::config::Config;
use crate::event::{AppSender, Event};

/// All apps shown in the top tab bar, in tab order. Adding a new app later
/// (Sonarr, Radarr) means writing its module and swapping its ComingSoonApp
/// entry here; the shell needs no changes.
pub fn build_apps(
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    tx: mpsc::UnboundedSender<Event>,
) -> Vec<Box<dyn MediaApp>> {
    vec![
        Box::new(jellyfin::JellyfinApp::new(
            config,
            config_path,
            AppSender::new("jellyfin", tx),
        )),
        Box::new(coming_soon::ComingSoonApp::new("sonarr", "Sonarr")),
        Box::new(coming_soon::ComingSoonApp::new("radarr", "Radarr")),
    ]
}
