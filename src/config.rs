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

    /// Load the config, creating it on first run. When no isamedia config
    /// exists yet, credentials are imported from an existing jfsh config so
    /// the user does not have to log in again.
    pub fn load_or_init(path: &Path) -> Result<Self> {
        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading config file {}", path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("parsing config file {}", path.display()))?
        } else if let Some(imported) = import_jfsh_config() {
            tracing::info!("imported existing jfsh config");
            Config {
                jellyfin: imported,
                ..Config::default()
            }
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

/// Subset of jfsh's YAML config (viper writes lowercase keys).
#[derive(Debug, Deserialize)]
struct JfshConfig {
    #[serde(default)]
    host: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    device: String,
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    skip_segments: Vec<String>,
}

fn import_jfsh_config() -> Option<JellyfinConfig> {
    let path = jfsh_config_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let jfsh: JfshConfig = serde_yaml::from_str(&raw)
        .map_err(|err| tracing::warn!(%err, "found jfsh config but failed to parse it"))
        .ok()?;
    Some(JellyfinConfig {
        host: jfsh.host,
        username: jfsh.username,
        password: jfsh.password,
        device: jfsh.device,
        device_id: jfsh.device_id,
        token: jfsh.token,
        user_id: jfsh.user_id,
        skip_segments: jfsh.skip_segments,
    })
}

fn jfsh_config_path() -> Option<PathBuf> {
    // jfsh uses adrg/xdg: $XDG_CONFIG_HOME/jfsh/jfsh.yaml. On Windows adrg/xdg
    // maps XDG_CONFIG_HOME to %LOCALAPPDATA%.
    #[cfg(unix)]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    Some(base.join("jfsh").join("jfsh.yaml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut config = Config::default();
        config.last_app = Some("jellyfin".into());
        config.jellyfin.host = "https://demo.jellyfin.org".into();
        config.jellyfin.skip_segments = vec!["Intro".into(), "Outro".into()];
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

    #[test]
    fn imports_jfsh_yaml() {
        let raw = "host: https://jf.example.com\nusername: me\npassword: hunter2\ndevice: box\ndevice_id: abc-123\ntoken: tok\nuser_id: uid\nskip_segments:\n  - Intro\nclient_version: 0.1.0\n";
        let jfsh: JfshConfig = serde_yaml::from_str(raw).unwrap();
        assert_eq!(jfsh.host, "https://jf.example.com");
        assert_eq!(jfsh.device_id, "abc-123");
        assert_eq!(jfsh.skip_segments, vec!["Intro"]);
    }
}
