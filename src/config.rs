//! User-level configuration persisted across sessions.
//!
//! Currently stores the ignore list for noisy repository discovery errors
//! (stale worktree markers, broken submodules) so the Review view can hide
//! them once the user decides they are not actionable.

use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const CONFIG_DIR: &str = "latte-lens";
const CONFIG_FILE: &str = "config.json";

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct ConfigFile {
    ignored_error_paths: Vec<PathBuf>,
}

/// Absolute paths of discovery errors the user has dismissed.
pub(crate) type IgnoredErrorPaths = HashSet<PathBuf>;

fn config_dir() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME or USERPROFILE must be set for config storage"))?;
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".config"));
    Ok(config_home.join(CONFIG_DIR))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

/// Load the ignored error paths. Missing or unreadable config yields an empty
/// set so a corrupt file never blocks startup.
pub(crate) fn load_ignored_error_paths() -> IgnoredErrorPaths {
    let Ok(path) = config_path() else {
        return HashSet::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    match serde_json::from_str::<ConfigFile>(&content) {
        Ok(config) => config.ignored_error_paths.into_iter().collect(),
        Err(_) => HashSet::new(),
    }
}

/// Persist the ignored error paths, creating the config directory on demand.
pub(crate) fn save_ignored_error_paths(paths: &IgnoredErrorPaths) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create config directory {}", parent.display()))?;
    }
    let mut sorted: Vec<PathBuf> = paths.iter().cloned().collect();
    sorted.sort();
    let config = ConfigFile {
        ignored_error_paths: sorted,
    };
    let json = serde_json::to_string_pretty(&config).context("cannot serialize config")?;
    std::fs::write(&path, json)
        .with_context(|| format!("cannot write config file {}", path.display()))?;
    Ok(())
}

/// Best-effort canonicalization used before comparing against discovery error
/// paths, which are themselves canonicalized where possible.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
