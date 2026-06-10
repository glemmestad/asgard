//! CLI profile config (`~/.config/asgard/config.toml`) and the connection
//! resolver. Precedence is flag/env > selected profile > built-in default. The
//! `asgard` binary's clap globals already fold flags over their env vars, so the
//! values passed to `resolve` are "flag-or-env"; this layer adds the profile and
//! the default.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::render::Output;
use crate::CliError;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pat: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Fully-resolved connection + presentation settings for a remote command.
pub struct Resolved {
    pub url: String,
    pub pat: Option<String>,
    pub output: Output,
}

const DEFAULT_URL: &str = "http://localhost:8080";

/// The directory holding `config.toml` and the `keys/` cache. `$FRONTKEEP_CONFIG`
/// points at the config *file*; its parent is the directory. When neither env
/// var is set, prefer `<base>/frontkeep` but fall back to `<base>/asgard` if
/// only the legacy directory exists — that way an upgrade from the previous
/// binary keeps the user's profiles and PATs.
pub fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("FRONTKEEP_CONFIG") {
        return PathBuf::from(p)
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".config"));
    let new_dir = base.join("frontkeep");
    if new_dir.exists() {
        return new_dir;
    }
    let legacy = base.join("asgard");
    if legacy.exists() {
        return legacy;
    }
    new_dir
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("FRONTKEEP_CONFIG") {
        return PathBuf::from(p);
    }
    config_dir().join("config.toml")
}

pub fn keys_dir() -> PathBuf {
    config_dir().join("keys")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

pub fn load() -> Config {
    match std::fs::read_to_string(config_path()) {
        Ok(s) => toml::from_str(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

impl Config {
    /// Resolve effective settings. `flag_*` are already flag-or-env (clap folds
    /// them); this adds the selected profile then the defaults. An explicitly
    /// named profile that doesn't exist is an error; a missing `default_profile`
    /// just falls through to defaults.
    pub fn resolve(
        &self,
        flag_url: Option<String>,
        flag_pat: Option<String>,
        flag_profile: Option<String>,
        flag_output: Option<String>,
    ) -> Result<Resolved, CliError> {
        if let Some(n) = &flag_profile {
            if !self.profiles.contains_key(n) {
                return Err(CliError::Config(format!(
                    "profile '{n}' not found in {}",
                    config_path().display()
                )));
            }
        }
        let profile = flag_profile
            .or_else(|| self.default_profile.clone())
            .and_then(|n| self.profiles.get(&n).cloned())
            .unwrap_or_default();

        let url = flag_url
            .or(profile.url)
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        let pat = flag_pat.or(profile.pat);
        let output = match flag_output.or(profile.output) {
            Some(s) => s.parse()?,
            None => Output::Table,
        };
        Ok(Resolved { url, pat, output })
    }
}

/// Write a profile's `url`/`pat` into the config (creating the file `0600`),
/// making it the default when none is set. Returns the config path.
pub fn save_login(
    profile: &str,
    url: Option<&str>,
    pat: &str,
    make_default: bool,
) -> Result<PathBuf, CliError> {
    let mut cfg = load();
    let entry = cfg.profiles.entry(profile.to_string()).or_default();
    if let Some(u) = url {
        entry.url = Some(u.to_string());
    }
    entry.pat = Some(pat.to_string());
    if make_default || cfg.default_profile.is_none() {
        cfg.default_profile = Some(profile.to_string());
    }
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CliError::Io(e.to_string()))?;
    }
    let body = toml::to_string_pretty(&cfg).map_err(|e| CliError::Config(e.to_string()))?;
    std::fs::write(&path, body).map_err(|e| CliError::Io(e.to_string()))?;
    set_mode_600(&path);
    Ok(path)
}

/// Restrict a file to owner read/write (`0600`). Best-effort; no-op off Unix.
pub fn set_mode_600(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_profile() -> Config {
        let mut profiles = HashMap::new();
        profiles.insert(
            "prod".to_string(),
            Profile {
                url: Some("https://prod.example".into()),
                pat: Some("asg_pat_prod".into()),
                output: Some("json".into()),
            },
        );
        Config {
            default_profile: Some("prod".into()),
            profiles,
        }
    }

    #[test]
    fn flag_beats_profile_beats_default() {
        let cfg = cfg_with_profile();
        // Flag wins.
        let r = cfg
            .resolve(Some("https://flag.example".into()), None, None, None)
            .unwrap();
        assert_eq!(r.url, "https://flag.example");
        assert_eq!(r.pat.as_deref(), Some("asg_pat_prod")); // from default profile
        assert_eq!(r.output, Output::Json); // from default profile
    }

    #[test]
    fn falls_back_to_builtin_default_url() {
        let cfg = Config::default();
        let r = cfg.resolve(None, None, None, None).unwrap();
        assert_eq!(r.url, DEFAULT_URL);
        assert!(r.pat.is_none());
        assert_eq!(r.output, Output::Table);
    }

    #[test]
    fn unknown_explicit_profile_errors() {
        let cfg = cfg_with_profile();
        assert!(cfg.resolve(None, None, Some("nope".into()), None).is_err());
    }
}
