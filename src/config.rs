use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ui::theme::Theme;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub last_app: Option<String>,
    // Declared before the `jellyfin` table: TOML puts bare keys before tables,
    // so a field placed after it would serialise into the wrong section.
    pub theme: Theme,
    pub jellyfin: JellyfinConfig,
}

/// Non-secret Jellyfin settings. The token and password live in the OS
/// keyring (see `crate::secrets`), never in this file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JellyfinConfig {
    pub host: String,
    pub username: String,
    pub device: String,
    pub device_id: String,
    pub user_id: String,
    pub skip_segments: Vec<String>,
}

impl Config {
    pub fn default_path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "isamedia")
            .context("could not determine config directory")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// Load the config, creating an empty one on first run (the login screen
    /// collects credentials).
    pub fn load_or_init(path: &Path) -> Result<Self> {
        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading config file {}", path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("parsing config file {}", path.display()))?
        } else {
            Config::default()
        };

        let jf = &mut config.jellyfin;
        if jf.device.is_empty() {
            jf.device = gethostname::gethostname().to_string_lossy().into_owned();
        }
        if jf.device_id.is_empty() {
            jf.device_id = uuid::Uuid::new_v4().to_string();
        }

        config.save(path)?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating config directory {}", dir.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serializing config")?;
        // No secrets in here, but the file still names your server and
        // account; create it owner-only from the start rather than chmodding
        // after the fact, so it is never world-readable even briefly.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .with_context(|| format!("writing config file {}", path.display()))?;
        use std::io::Write;
        file.write_all(raw.as_bytes())
            .with_context(|| format!("writing config file {}", path.display()))?;
        // The mode above only applies on creation; also tighten a config
        // file that already existed with looser permissions.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let config = Config {
            last_app: Some("jellyfin".into()),
            theme: Theme::SolarizedLight,
            jellyfin: JellyfinConfig {
                host: "https://demo.jellyfin.org".into(),
                skip_segments: vec!["Intro".into(), "Outro".into()],
                ..JellyfinConfig::default()
            },
        };
        let raw = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.last_app.as_deref(), Some("jellyfin"));
        assert_eq!(parsed.theme, Theme::SolarizedLight);
        assert_eq!(parsed.jellyfin.host, config.jellyfin.host);
        assert_eq!(parsed.jellyfin.skip_segments, config.jellyfin.skip_segments);
    }

    #[test]
    fn parses_partial_config() {
        let parsed: Config = toml::from_str("[jellyfin]\nhost = \"http://x\"\n").unwrap();
        assert_eq!(parsed.jellyfin.host, "http://x");
        assert!(parsed.jellyfin.user_id.is_empty());
        assert!(parsed.last_app.is_none());
        // A config with no theme key defaults, so old files load unchanged.
        assert_eq!(parsed.theme, Theme::Latte);
    }

    #[test]
    fn theme_serialises_kebab_case() {
        let config = Config {
            theme: Theme::SolarizedLight,
            ..Config::default()
        };
        let raw = toml::to_string_pretty(&config).unwrap();
        assert!(raw.contains("theme = \"solarized-light\""));
        let parsed: Config = toml::from_str("theme = \"latte\"\n").unwrap();
        assert_eq!(parsed.theme, Theme::Latte);
    }

    #[test]
    fn unknown_theme_is_rejected() {
        // The enum is strict: a hand-edited unknown value fails to parse like
        // any other malformed TOML, rather than silently resetting.
        assert!(toml::from_str::<Config>("theme = \"nord\"\n").is_err());
    }

    #[test]
    fn ignores_legacy_secret_fields() {
        // Configs written before secrets moved to the keyring may still have
        // token/password keys; they must parse (and be dropped on next save).
        let parsed: Config =
            toml::from_str("[jellyfin]\nhost = \"http://x\"\ntoken = \"t\"\npassword = \"p\"\n")
                .unwrap();
        assert_eq!(parsed.jellyfin.host, "http://x");
        let saved = toml::to_string_pretty(&parsed).unwrap();
        assert!(!saved.contains("token"));
        assert!(!saved.contains("password"));
    }
}
