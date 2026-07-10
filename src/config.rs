use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ui::theme::{Accent, Theme};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub last_app: Option<String>,
    // Declared before the `jellyfin` table: TOML puts bare keys before tables,
    // so a field placed after it would serialise into the wrong section.
    pub theme: Theme,
    // Only applied for themes that offer accents (Catppuccin Latte); persisted
    // regardless so switching back to such a theme restores the choice.
    pub accent: Accent,
    pub jellyfin: JellyfinConfig,
    pub sonarr: SonarrConfig,
    pub radarr: RadarrConfig,
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
    pub audio_language: Option<TrackPreference>,
    pub subtitle_language: Option<TrackPreference>,
}

/// A track choice, global or remembered. `Default` (audio) means don't set
/// alang and let mpv pick the file's default track; `Off` (subtitles) means
/// sid=no. Serialises as a plain string — "default", "off", or an ISO 639-2/B
/// code like "ita" — none of which collide, since neither sentinel is a
/// language code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrackPreference {
    Default,
    Off,
    #[serde(untagged)]
    Language(String),
}

/// Per-movie / per-series remembered mpv track switches, keyed by item id
/// (movies) or series id (episodes). A field left `None` falls through to
/// the global preference.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LanguageOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<TrackPreference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitles: Option<TrackPreference>,
}

/// Resolved preferences handed to the player; `None` means leave mpv alone.
#[derive(Debug, Clone, Default)]
pub struct LanguagePrefs {
    pub audio: Option<TrackPreference>,
    pub subtitles: Option<TrackPreference>,
}

/// The remembered track switches, persisted in `language_overrides.json`
/// next to the config file rather than inside it: this map grows with every
/// show the user retunes, and keeping it separate leaves `config.toml` small
/// and stable. It is machine state no human edits, so it uses compact JSON
/// (serde_json is already in the tree and parses several times faster than
/// the toml crate). Only the Jellyfin app reads or writes it, so it is
/// owned plainly there instead of living behind the shared config mutex.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LanguageOverrides {
    entries: BTreeMap<String, LanguageOverride>,
}

impl LanguageOverrides {
    /// The overrides file that belongs to `config_path`: a sibling, so a
    /// `--config` override relocates both together.
    pub fn path_for(config_path: &Path) -> PathBuf {
        config_path.with_file_name("language_overrides.json")
    }

    /// Load the overrides, treating a missing file as empty (first run) and
    /// a malformed one as empty with a logged error: this data only tunes
    /// track selection, so a corrupt file must not take the app down.
    pub fn load(path: &Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                tracing::error!(%err, path = %path.display(), "failed to read language overrides");
                return Self::default();
            }
        };
        serde_json::from_str(&raw)
            .map_err(
                |err| tracing::error!(%err, path = %path.display(), "failed to parse language overrides"),
            )
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let raw = serde_json::to_string(self).context("serializing language overrides")?;
        write_owner_only(path, &raw)
    }

    /// The override slot for a movie/series key, created on first write.
    pub fn entry(&mut self, key: String) -> &mut LanguageOverride {
        self.entries.entry(key).or_default()
    }

    /// Resolve what the player should ask mpv for, per field: a remembered
    /// override for this movie/show wins, otherwise the global preference.
    /// An override that only recorded a subtitle switch still uses the
    /// global audio preference.
    pub fn language_prefs(&self, key: &str, config: &JellyfinConfig) -> LanguagePrefs {
        let overrides = self.entries.get(key);
        LanguagePrefs {
            audio: overrides
                .and_then(|o| o.audio.clone())
                .or_else(|| config.audio_language.clone()),
            subtitles: overrides
                .and_then(|o| o.subtitles.clone())
                .or_else(|| config.subtitle_language.clone()),
        }
    }
}

/// Non-secret Sonarr settings. The API key lives in the OS keyring (see
/// `crate::secrets`), never in this file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SonarrConfig {
    pub host: String,
}

/// Non-secret Radarr settings. The API key lives in the OS keyring (see
/// `crate::secrets`), never in this file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RadarrConfig {
    pub host: String,
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
        let raw = toml::to_string_pretty(self).context("serializing config")?;
        write_owner_only(path, &raw)
    }
}

/// Write a settings file owner-only, creating its directory if needed. No
/// secrets go through here, but the files still name your server, account
/// and watched shows; create them 0600 from the start rather than chmodding
/// after the fact, so they are never world-readable even briefly.
fn write_owner_only(path: &Path, raw: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating config directory {}", dir.display()))?;
    }
    // Write to a sibling temp file, then rename it over the target, so a crash
    // or power loss mid-write can never leave a zero-length or partial
    // `config.toml` (which would block startup) or `language_overrides.json`
    // (which would be silently reset, losing per-show track choices). rename
    // replaces an existing file atomically on unix and via
    // MOVEFILE_REPLACE_EXISTING on Windows. The temp is a sibling so it shares
    // the target's volume (rename stays atomic) and lands with the temp's 0600
    // mode, which is why there is no post-write chmod: the old file is replaced
    // wholesale, so a pre-existing looser file is tightened for free with no
    // world-readable window. The pid tag keeps a leftover temp from a crashed
    // run from colliding with a live write.
    let mut tmp_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);

    if let Err(err) = write_tmp_owner_only(&tmp, raw) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("replacing config file {}", path.display()));
    }
    Ok(())
}

/// Create `tmp` fresh, owner-only, write `raw`, and flush it to disk before
/// the caller renames it into place.
fn write_tmp_owner_only(tmp: &Path, raw: &str) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(tmp)
        .with_context(|| format!("creating temp config file {}", tmp.display()))?;
    use std::io::Write;
    file.write_all(raw.as_bytes())
        .with_context(|| format!("writing temp config file {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing temp config file {}", tmp.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let config = Config {
            last_app: Some("jellyfin".into()),
            theme: Theme::SolarizedLight,
            accent: Accent::Mauve,
            jellyfin: JellyfinConfig {
                host: "https://demo.jellyfin.org".into(),
                skip_segments: vec!["Intro".into(), "Outro".into()],
                ..JellyfinConfig::default()
            },
            sonarr: SonarrConfig {
                host: "https://sonarr.example.com".into(),
            },
            radarr: RadarrConfig {
                host: "https://radarr.example.com".into(),
            },
        };
        let raw = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.last_app.as_deref(), Some("jellyfin"));
        assert_eq!(parsed.theme, Theme::SolarizedLight);
        assert_eq!(parsed.accent, Accent::Mauve);
        assert_eq!(parsed.jellyfin.host, config.jellyfin.host);
        assert_eq!(parsed.jellyfin.skip_segments, config.jellyfin.skip_segments);
        assert_eq!(parsed.sonarr.host, config.sonarr.host);
        assert_eq!(parsed.radarr.host, config.radarr.host);
    }

    #[test]
    fn write_is_atomic_and_owner_only() {
        let dir = std::env::temp_dir().join(format!("isamedia-test-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        let config = Config {
            jellyfin: JellyfinConfig {
                host: "https://demo.jellyfin.org".into(),
                ..JellyfinConfig::default()
            },
            ..Config::default()
        };

        config.save(&path).unwrap();
        // Writing again must replace the existing file, not fail.
        config.save(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.jellyfin.host, config.jellyfin.host);

        // The temp file is renamed into place, so nothing is left beside the
        // target.
        let leftover_tmp = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leftover_tmp, "temp file left behind after save");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_partial_config() {
        let parsed: Config = toml::from_str("[jellyfin]\nhost = \"http://x\"\n").unwrap();
        assert_eq!(parsed.jellyfin.host, "http://x");
        assert!(parsed.jellyfin.user_id.is_empty());
        assert!(parsed.sonarr.host.is_empty());
        assert!(parsed.radarr.host.is_empty());
        assert!(parsed.last_app.is_none());
        // A config with no theme/accent keys defaults, so old files load unchanged.
        assert_eq!(parsed.theme, Theme::Latte);
        assert_eq!(parsed.accent, Accent::Rosewater);
    }

    #[test]
    fn theme_serialises_kebab_case() {
        let config = Config {
            theme: Theme::SolarizedLight,
            accent: Accent::Sky,
            ..Config::default()
        };
        let raw = toml::to_string_pretty(&config).unwrap();
        assert!(raw.contains("theme = \"solarized-light\""));
        assert!(raw.contains("accent = \"sky\""));
        let parsed: Config = toml::from_str("theme = \"latte\"\naccent = \"lavender\"\n").unwrap();
        assert_eq!(parsed.theme, Theme::Latte);
        assert_eq!(parsed.accent, Accent::Lavender);
    }

    #[test]
    fn unknown_theme_is_rejected() {
        // The enums are strict: a hand-edited unknown value fails to parse like
        // any other malformed TOML, rather than silently resetting.
        assert!(toml::from_str::<Config>("theme = \"nord\"\n").is_err());
        assert!(toml::from_str::<Config>("accent = \"turquoise\"\n").is_err());
    }

    #[test]
    fn language_preferences_roundtrip() {
        let config = Config {
            jellyfin: JellyfinConfig {
                audio_language: Some(TrackPreference::Language("ita".into())),
                subtitle_language: Some(TrackPreference::Off),
                ..JellyfinConfig::default()
            },
            ..Config::default()
        };
        let raw = toml::to_string_pretty(&config).unwrap();
        // Sentinels and codes serialise as plain strings.
        assert!(raw.contains("audio_language = \"ita\""), "{raw}");
        assert!(raw.contains("subtitle_language = \"off\""), "{raw}");

        let parsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(
            parsed.jellyfin.audio_language,
            Some(TrackPreference::Language("ita".into()))
        );
        assert_eq!(
            parsed.jellyfin.subtitle_language,
            Some(TrackPreference::Off)
        );
    }

    #[test]
    fn default_config_omits_language_keys() {
        // Existing configs must stay byte-identical until a preference is set.
        let raw = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!raw.contains("language"), "{raw}");
        // And old files without the keys parse to unset.
        let parsed: Config = toml::from_str("[jellyfin]\nhost = \"http://x\"\n").unwrap();
        assert!(parsed.jellyfin.audio_language.is_none());
        assert!(parsed.jellyfin.subtitle_language.is_none());
    }

    #[test]
    fn overrides_roundtrip_as_compact_json() {
        let mut overrides = LanguageOverrides::default();
        overrides.entry("abc123".into()).audio = Some(TrackPreference::Default);
        overrides.entry("abc123".into()).subtitles = Some(TrackPreference::Language("eng".into()));
        let raw = serde_json::to_string(&overrides).unwrap();
        // The transparent map with the sentinel/code strings, nothing else.
        assert_eq!(raw, r#"{"abc123":{"audio":"default","subtitles":"eng"}}"#);

        let parsed: LanguageOverrides = serde_json::from_str(&raw).unwrap();
        let entry = &parsed.entries["abc123"];
        assert_eq!(entry.audio, Some(TrackPreference::Default));
        assert_eq!(
            entry.subtitles,
            Some(TrackPreference::Language("eng".into()))
        );
    }

    #[test]
    fn overrides_load_missing_or_malformed_as_empty() {
        let missing = std::env::temp_dir().join("isamedia-test-no-such-overrides.json");
        assert!(LanguageOverrides::load(&missing).entries.is_empty());

        let malformed = std::env::temp_dir().join(format!(
            "isamedia-test-bad-overrides-{}.json",
            std::process::id()
        ));
        std::fs::write(&malformed, "not { valid").unwrap();
        assert!(LanguageOverrides::load(&malformed).entries.is_empty());
        let _ = std::fs::remove_file(&malformed);
    }

    #[test]
    fn overrides_path_sits_next_to_the_config() {
        assert_eq!(
            LanguageOverrides::path_for(Path::new("/home/x/.config/isamedia/config.toml")),
            PathBuf::from("/home/x/.config/isamedia/language_overrides.json")
        );
    }

    #[test]
    fn language_prefs_precedence() {
        let jellyfin = JellyfinConfig {
            audio_language: Some(TrackPreference::Language("ita".into())),
            subtitle_language: Some(TrackPreference::Language("eng".into())),
            ..JellyfinConfig::default()
        };
        let mut overrides = LanguageOverrides::default();
        overrides.entry("show".into()).audio = Some(TrackPreference::Language("jpn".into()));

        // No override: globals apply.
        let prefs = overrides.language_prefs("movie", &jellyfin);
        assert_eq!(prefs.audio, Some(TrackPreference::Language("ita".into())));
        assert_eq!(
            prefs.subtitles,
            Some(TrackPreference::Language("eng".into()))
        );

        // Override wins per field; the unset subtitle field falls through.
        let prefs = overrides.language_prefs("show", &jellyfin);
        assert_eq!(prefs.audio, Some(TrackPreference::Language("jpn".into())));
        assert_eq!(
            prefs.subtitles,
            Some(TrackPreference::Language("eng".into()))
        );

        // No globals, no override: nothing.
        let prefs =
            LanguageOverrides::default().language_prefs("movie", &JellyfinConfig::default());
        assert!(prefs.audio.is_none());
        assert!(prefs.subtitles.is_none());
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
