use std::fs;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The daemon shuts itself down after this long with nothing playing and no
/// client activity, unless overridden by `Config::idle_timeout_secs` — see
/// [`Config::idle_timeout`].
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 20 * 60;

/// On-disk config: server URL + username, plus optional daemon tuning.
/// **Never the password** — that lives in the OS keyring (see
/// [`Credentials::load`]), a deliberate improvement over every existing
/// maraetai client, none of which use a hardware/OS-backed secret store on
/// the platforms where one exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub server_url: String,
    pub username: String,
    /// Seconds of no playback + no client activity before the daemon exits
    /// on its own. Absent/`None` means [`DEFAULT_IDLE_TIMEOUT_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
    /// The audio output device to use, by name — as reported by `maraetaid
    /// --list-devices`. Absent/`None` means the system default. Matched
    /// case-insensitively/by substring if the exact name has drifted (see
    /// the daemon's `match_output_device`), so a config written against an
    /// older device list usually still works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_device: Option<String>,
}

impl Config {
    pub fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_timeout_secs.unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS))
    }
}

/// The OS keyring service name under which the password is stored, keyed by
/// username as the keyring "account" — so switching accounts on the same
/// machine keeps each password separate.
const KEYRING_SERVICE: &str = "maraetai";

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "maraetai").ok_or(Error::NoConfigDir)
}

/// Path to `~/.config/maraetai/config.toml` (or the platform equivalent).
pub fn config_path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

impl Config {
    /// Loads the config file. Returns [`Error::ConfigMissing`] if it doesn't
    /// exist yet — callers should treat that as "run `maraetai login`", not a
    /// fatal startup error, so the daemon can still come up and expose MPRIS
    /// in a clearly-unconfigured state rather than crash-looping.
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(Error::ConfigMissing(path.to_path_buf()));
        }
        let raw = fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| Error::ConfigParse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Writes the config file, creating its parent directory if needed. Only
    /// server URL + username are ever written here — see the module doc.
    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|source| Error::ConfigWrite {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let raw = toml::to_string_pretty(self)?;
        fs::write(path, raw).map_err(|source| Error::ConfigWrite {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Config + the password fetched from the OS keyring — what a caller actually
/// needs to authenticate against `maraetai-service`. Kept separate from
/// [`Config`] so the password is never accidentally serialized to disk (it
/// simply isn't a field on the type that gets written).
#[derive(Clone)]
pub struct Credentials {
    pub server_url: String,
    pub username: String,
    pub password: String,
}

impl Credentials {
    /// Loads the config file, then looks up the matching password in the OS
    /// keyring. Fails closed — [`Error::PasswordMissing`] — rather than
    /// falling back to any plaintext storage if the keyring entry is absent.
    pub fn load() -> Result<Self> {
        let config = Config::load()?;
        let password = keyring_entry(&config.username)?
            .get_password()
            .map_err(|e| match e {
                keyring::Error::NoEntry => Error::PasswordMissing {
                    username: config.username.clone(),
                },
                other => Error::Keyring(other),
            })?;
        Ok(Self {
            server_url: config.server_url,
            username: config.username,
            password,
        })
    }

    /// Saves the config file and stores the password in the OS keyring. This
    /// is the `maraetai login` flow. Preserves `idle_timeout_secs`/
    /// `output_device` from an existing config, if any, rather than
    /// resetting them — `login` changes credentials, not daemon tuning a
    /// user may have already customized.
    pub fn save(server_url: String, username: String, password: &str) -> Result<()> {
        let existing = Config::load().ok();
        let idle_timeout_secs = existing.as_ref().and_then(|c| c.idle_timeout_secs);
        let output_device = existing.and_then(|c| c.output_device);
        Config {
            server_url,
            username: username.clone(),
            idle_timeout_secs,
            output_device,
        }
        .save()?;
        keyring_entry(&username)?.set_password(password)?;
        Ok(())
    }
}

fn keyring_entry(username: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(KEYRING_SERVICE, username)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config {
            server_url: "https://music.example.com".into(),
            username: "alice".into(),
            idle_timeout_secs: Some(600),
            output_device: Some("USB DAC".into()),
        };
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(cfg, loaded);
    }

    #[test]
    fn missing_file_is_a_distinct_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        assert!(matches!(
            Config::load_from(&path),
            Err(Error::ConfigMissing(_))
        ));
    }

    #[test]
    fn never_serializes_a_password_field() {
        // Guards the security property in the module doc: Config has no
        // password field at all, so there is no way for one to leak into the
        // on-disk TOML even by future accident — this test fails to compile
        // (not just fails at runtime) if a `password` field is ever added
        // without updating this guard.
        let cfg = Config {
            server_url: "https://music.example.com".into(),
            username: "alice".into(),
            idle_timeout_secs: None,
            output_device: None,
        };
        let raw = toml::to_string(&cfg).unwrap();
        assert!(!raw.contains("password"));
    }

    #[test]
    fn idle_timeout_falls_back_to_default_when_unset() {
        let cfg = Config {
            server_url: "https://music.example.com".into(),
            username: "alice".into(),
            idle_timeout_secs: None,
            output_device: None,
        };
        assert_eq!(cfg.idle_timeout(), std::time::Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS));
    }

    #[test]
    fn idle_timeout_uses_configured_value() {
        let cfg = Config {
            server_url: "https://music.example.com".into(),
            username: "alice".into(),
            idle_timeout_secs: Some(60),
            output_device: None,
        };
        assert_eq!(cfg.idle_timeout(), std::time::Duration::from_secs(60));
    }

    #[test]
    fn absent_idle_timeout_field_deserializes_as_none() {
        // A config file written before this field existed must still load.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "server_url = \"https://music.example.com\"\nusername = \"alice\"\n").unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.idle_timeout_secs, None);
    }
}
