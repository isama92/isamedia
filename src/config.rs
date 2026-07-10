use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub last_app: Option<String>,
    pub jellyfin: JellyfinConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JellyfinConfig {
    pub host: String,
    pub username: String,
    pub password: String,
    pub device: String,
    pub device_id: String,
    pub token: String,
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
        std::fs::write(path, raw)
            .with_context(|| format!("writing config file {}", path.display()))?;
        // The file holds credentials; keep it private to the user.
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
            jellyfin: JellyfinConfig {
                host: "https://demo.jellyfin.org".into(),
                skip_segments: vec!["Intro".into(), "Outro".into()],
                ..JellyfinConfig::default()
            },
        };
        let raw = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.last_app.as_deref(), Some("jellyfin"));
        assert_eq!(parsed.jellyfin.host, config.jellyfin.host);
        assert_eq!(parsed.jellyfin.skip_segments, config.jellyfin.skip_segments);
    }

    #[test]
    fn parses_partial_config() {
        let parsed: Config = toml::from_str("[jellyfin]\nhost = \"http://x\"\n").unwrap();
        assert_eq!(parsed.jellyfin.host, "http://x");
        assert!(parsed.jellyfin.token.is_empty());
        assert!(parsed.last_app.is_none());
    }

}
