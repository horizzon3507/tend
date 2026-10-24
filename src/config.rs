use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util::{ensure_parent, home};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupConfig {
    pub paths: Vec<String>,
    pub keep: usize,
    pub dest: String,
}

/// Same field names as the JSON already on disk, so an existing config keeps working.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub roots: Vec<String>,
    #[serde(rename = "maxDepth")]
    pub max_depth: usize,
    #[serde(rename = "staleDays")]
    pub stale_days: i64,
    /// paths that scanning and cleanup must never enter
    #[serde(default)]
    pub exclude: Vec<String>,
    pub backup: BackupConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: vec!["~".to_string()],
            max_depth: 4,
            stale_days: 180,
            exclude: Vec::new(),
            backup: BackupConfig {
                paths: [
                    "~/.bashrc",
                    "~/.bash_profile",
                    "~/.zshrc",
                    "~/.profile",
                    "~/.gitconfig",
                    "~/.gitignore_global",
                    "~/.tmux.conf",
                    "~/.config/fish",
                    "~/.config/nvim",
                    "~/.config/hypr",
                    "~/.config/kitty",
                    "~/.config/alacritty",
                    "~/.config/opencode",
                    "~/.ssh",
                    "~/dotfiles",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
                keep: 7,
                dest: "~/.local/share/tend/backups".to_string(),
            },
        }
    }
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
}

pub fn config_path() -> PathBuf {
    config_home().join("tend/config.json")
}

pub fn data_dir() -> PathBuf {
    data_home().join("tend")
}

pub struct Loaded {
    pub config: Config,
    pub created: bool,
}

pub fn load() -> Loaded {
    let path = config_path();
    if !path.exists() {
        ensure_parent(&path);
        let config = Config::default();
        if let Ok(text) = serde_json::to_string_pretty(&config) {
            let _ = std::fs::write(&path, format!("{text}\n"));
        }
        return Loaded {
            config,
            created: true,
        };
    }

    match std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Config>(&text).ok())
    {
        Some(config) => Loaded {
            config,
            created: false,
        },
        None => Loaded {
            config: Config::default(),
            created: false,
        },
    }
}

pub fn write_default() -> std::io::Result<()> {
    let path = config_path();
    ensure_parent(&path);
    let text = serde_json::to_string_pretty(&Config::default()).map_err(std::io::Error::other)?;
    std::fs::write(&path, format!("{text}\n"))
}

pub fn resolved_roots(config: &Config, override_roots: &[String]) -> Vec<PathBuf> {
    let list = if override_roots.is_empty() {
        config.roots.as_slice()
    } else {
        override_roots
    };
    list.iter()
        .map(|root| crate::util::expand_home(root))
        .collect()
}

pub fn backup_dest(config: &Config, override_dest: Option<&str>) -> PathBuf {
    crate::util::expand_home(override_dest.unwrap_or(&config.backup.dest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_the_documented_shape() {
        let config = Config::default();
        assert_eq!(config.roots, vec!["~".to_string()]);
        assert_eq!(config.max_depth, 4);
        assert_eq!(config.stale_days, 180);
        assert_eq!(config.backup.keep, 7);
        assert!(config.backup.dest.ends_with("tend/backups"));
    }

    #[test]
    fn config_round_trips_through_camel_case_json() {
        let config = Config::default();
        let text = serde_json::to_string(&config).unwrap();
        assert!(text.contains("\"maxDepth\""));
        assert!(text.contains("\"staleDays\""));
        let parsed: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.max_depth, config.max_depth);
        assert_eq!(parsed.backup.keep, config.backup.keep);
    }

    #[test]
    fn a_config_written_by_the_previous_version_is_still_readable() {
        let text = r#"{
            "roots": ["~", "/tmp"],
            "maxDepth": 3,
            "staleDays": 90,
            "backup": { "paths": ["~/.bashrc"], "keep": 3, "dest": "~/archives" }
        }"#;
        let config: Config = serde_json::from_str(text).unwrap();
        assert_eq!(config.roots.len(), 2);
        assert_eq!(config.max_depth, 3);
        assert_eq!(config.stale_days, 90);
        assert_eq!(config.backup.keep, 3);
        assert_eq!(config.backup.dest, "~/archives");
    }

    #[test]
    fn override_roots_win_over_the_config_file() {
        let config = Config::default();
        let roots = resolved_roots(&config, &["/tmp".to_string()]);
        assert_eq!(roots, vec![PathBuf::from("/tmp")]);
    }
}
