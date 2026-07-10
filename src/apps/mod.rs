pub mod auto_search;
pub mod delete_prompt;
pub mod downloads;
pub mod jellyfin;
pub mod radarr;
pub mod settings;
pub mod sonarr;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::app::MediaApp;
use crate::config::Config;
use crate::event::{AppSender, Event};

/// All apps shown in the top tab bar, in tab order. Adding a new app means
/// writing its module and registering it here (a placeholder can implement
/// `status() -> AppStatus::ComingSoon`); the shell needs no changes.
pub fn build_apps(
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    tx: mpsc::UnboundedSender<Event>,
) -> Vec<Box<dyn MediaApp>> {
    vec![
        Box::new(jellyfin::JellyfinApp::new(
            config.clone(),
            config_path.clone(),
            AppSender::new("jellyfin", tx.clone()),
        )),
        Box::new(radarr::RadarrApp::new(
            config.clone(),
            config_path.clone(),
            AppSender::new("radarr", tx.clone()),
        )),
        Box::new(sonarr::SonarrApp::new(
            config.clone(),
            config_path.clone(),
            AppSender::new("sonarr", tx.clone()),
        )),
        // Owns the config to read/write settings; the backend credential forms
        // connect to a server and write the keyring, so it needs a sender to
        // report those async results back.
        Box::new(settings::SettingsApp::new(
            config,
            config_path,
            AppSender::new("settings", tx),
        )),
    ]
}
