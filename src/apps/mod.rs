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

/// Every app tab's id and label, in the built-in order. The single source of
/// tab identity for the Settings reorder screen and the persisted
/// `Config::tab_order`, so both can name and label a tab (even a hidden one)
/// without building it. `build_apps` constructs the same tabs in this order,
/// which `tab_catalog_matches_build_apps` guards; each id matches the app's
/// `id()`. Add a new app here as well as in `build_apps`.
pub const TAB_CATALOG: &[(&str, &str)] = &[
    ("jellyfin", "Jellyfin"),
    ("radarr", "Radarr"),
    ("sonarr", "Sonarr"),
    ("settings", "Settings"),
];

/// Resolve a persisted tab order into display positions over `known_ids`.
///
/// Returns a permutation of `0..known_ids.len()`: the `saved` ids first, in the
/// order given (skipping any that no longer name a known tab, and any
/// duplicate), then every remaining known id appended in its original order.
/// This tolerance is what keeps `Config::tab_order` forward and backward
/// compatible: a saved order that predates a new app still shows that app (at
/// the end), and an id dropped from a build simply disappears.
pub fn order_indices(known_ids: &[&str], saved: &[String]) -> Vec<usize> {
    let mut order = Vec::with_capacity(known_ids.len());
    for id in saved {
        if let Some(i) = known_ids.iter().position(|known| known == id)
            && !order.contains(&i)
        {
            order.push(i);
        }
    }
    for i in 0..known_ids.len() {
        if !order.contains(&i) {
            order.push(i);
        }
    }
    order
}

/// All apps shown in the top tab bar, in tab order. Adding a new app means
/// writing its module and registering it here (and in `TAB_CATALOG`); the shell
/// needs no changes.
pub fn build_apps(
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    tx: mpsc::UnboundedSender<Event>,
) -> Vec<Box<dyn MediaApp>> {
    // Bumped by the Settings tab whenever it re-authenticates Jellyfin, so the
    // running Jellyfin tab reconnects with the freshly stored token instead of
    // dropping to the login form (which would make the user type the password
    // twice). Shared between exactly those two apps.
    let jellyfin_reauth = Arc::new(std::sync::atomic::AtomicU64::new(0));
    vec![
        Box::new(jellyfin::JellyfinApp::new(
            config.clone(),
            config_path.clone(),
            AppSender::new("jellyfin", tx.clone()),
            jellyfin_reauth.clone(),
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
            jellyfin_reauth,
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known_ids() -> Vec<&'static str> {
        TAB_CATALOG.iter().map(|&(id, _)| id).collect()
    }

    #[test]
    fn tab_catalog_matches_build_apps() {
        // The catalog is a hand-maintained mirror of `build_apps`; if the two
        // ever drift, the reorder screen would name a tab that does not exist
        // (or miss one). Building with a default config touches no network.
        let config = Arc::new(Mutex::new(Config::default()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let apps = build_apps(
            config,
            PathBuf::from("/nonexistent/isamedia/config.toml"),
            tx,
        );
        let built: Vec<(&str, &str)> = apps.iter().map(|app| (app.id(), app.title())).collect();
        assert_eq!(built, TAB_CATALOG.to_vec());
    }

    #[test]
    fn order_indices_defaults_to_build_order() {
        assert_eq!(order_indices(&known_ids(), &[]), vec![0, 1, 2, 3]);
    }

    #[test]
    fn order_indices_honours_saved_then_appends_missing() {
        // Settings pulled to the front, Sonarr dropped from the saved list: the
        // missing id is appended in its built-in position.
        let saved = vec!["settings".to_string(), "radarr".to_string()];
        assert_eq!(order_indices(&known_ids(), &saved), vec![3, 1, 0, 2]);
    }

    #[test]
    fn order_indices_ignores_unknown_and_duplicate_ids() {
        let saved = vec![
            "sonarr".to_string(),
            "ghost".to_string(),  // no such tab: skipped
            "sonarr".to_string(), // duplicate: ignored
            "jellyfin".to_string(),
        ];
        assert_eq!(order_indices(&known_ids(), &saved), vec![2, 0, 1, 3]);
    }
}
