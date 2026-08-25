//! User-level state persisted across sessions.
//!
//! Stores the ignore list for noisy repository discovery errors (stale
//! worktree markers, broken submodules) so the Review view can hide them
//! once the user decides they are not actionable.
//!
//! This is **state**, not configuration: it changes as the user dismisses
//! errors, so it lives under the Lens state root
//! (`LATTE_LENS_STATE_DIR` → `LATTE_HOME/lens/state` → `~/.latte/lens/state`)
//! rather than the JSONC user config file.

use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const STATE_FILE: &str = "ignored-error-paths.json";

/// Absolute paths of discovery errors the user has dismissed.
pub(crate) type IgnoredErrorPaths = HashSet<PathBuf>;

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct StateFile {
    ignored_error_paths: Vec<PathBuf>,
}

/// Resolve the Lens state root following the same convention as
/// `agent::metadata::resolve_state_root_from_environment`:
/// `LATTE_LENS_STATE_DIR` → `LATTE_HOME/lens/state` → `~/.latte/lens/state`.
fn state_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("LATTE_LENS_STATE_DIR") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            anyhow::bail!("LATTE_LENS_STATE_DIR must be an absolute path");
        }
        return Ok(path);
    }
    if let Some(home) = env::var_os("LATTE_HOME") {
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            anyhow::bail!("LATTE_HOME must be an absolute path");
        }
        return Ok(home.join("lens").join("state"));
    }
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .ok_or_else(|| anyhow::anyhow!("HOME or USERPROFILE must be set for state storage"))?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        anyhow::bail!("HOME or USERPROFILE must be an absolute path");
    }
    Ok(home.join(".latte").join("lens").join("state"))
}

fn state_path() -> Result<PathBuf> {
    Ok(state_root()?.join(STATE_FILE))
}

/// Load the ignored error paths. Missing or unreadable state yields an empty
/// set so a corrupt file never blocks startup.
pub(crate) fn load_ignored_error_paths() -> IgnoredErrorPaths {
    let Ok(path) = state_path() else {
        return HashSet::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    match serde_json::from_str::<StateFile>(&content) {
        Ok(state) => state.ignored_error_paths.into_iter().collect(),
        Err(_) => HashSet::new(),
    }
}

/// Persist the ignored error paths, creating the state directory on demand.
pub(crate) fn save_ignored_error_paths(paths: &IgnoredErrorPaths) -> Result<()> {
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create state directory {}", parent.display()))?;
    }
    let mut sorted: Vec<PathBuf> = paths.iter().cloned().collect();
    sorted.sort();
    let state = StateFile {
        ignored_error_paths: sorted,
    };
    let json = serde_json::to_string_pretty(&state).context("cannot serialize state")?;
    std::fs::write(&path, json)
        .with_context(|| format!("cannot write state file {}", path.display()))?;
    Ok(())
}

/// Best-effort canonicalization used before comparing against discovery error
/// paths, which are themselves canonicalized where possible.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Unified ignore check: an error path is ignored when any ignored entry is
/// equal to or a prefix of it (so ignoring a repository/directory also hides
/// every error underneath). The error path is canonicalized first because
/// discovery error paths are not always canonical while ignored paths are.
pub(crate) fn is_ignored_error_path(error_path: &Path, ignored_paths: &HashSet<PathBuf>) -> bool {
    let normalized = normalize_path(error_path);
    ignored_paths
        .iter()
        .any(|ignored| normalized.starts_with(ignored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env-var tests must run serially since they mutate process-global state.
    /// Poison is tolerated so one assertion failure does not cascade into
    /// lock-poison failures in sibling tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn state_root_follows_latte_lens_state_dir() {
        let _env = lock_env();
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("custom-state");
        let _guard = EnvironmentGuard::apply(&[
            (
                "LATTE_LENS_STATE_DIR",
                Some(state_dir.clone().into_os_string()),
            ),
            ("LATTE_HOME", None),
            ("HOME", Some(temp.path().to_owned().into_os_string())),
        ]);
        assert_eq!(state_root().unwrap(), state_dir);
    }

    #[test]
    fn state_root_falls_back_to_latte_home() {
        let _env = lock_env();
        let temp = tempfile::tempdir().unwrap();
        let latte_home = temp.path().join("latte-home");
        let _guard = EnvironmentGuard::apply(&[
            ("LATTE_LENS_STATE_DIR", None),
            ("LATTE_HOME", Some(latte_home.clone().into_os_string())),
            ("HOME", Some(temp.path().to_owned().into_os_string())),
        ]);
        assert_eq!(state_root().unwrap(), latte_home.join("lens").join("state"));
    }

    #[test]
    fn state_root_defaults_to_home_latte() {
        let _env = lock_env();
        let temp = tempfile::tempdir().unwrap();
        let _guard = EnvironmentGuard::apply(&[
            ("LATTE_LENS_STATE_DIR", None),
            ("LATTE_HOME", None),
            ("HOME", Some(temp.path().to_owned().into_os_string())),
        ]);
        assert_eq!(
            state_root().unwrap(),
            temp.path().join(".latte").join("lens").join("state")
        );
    }

    #[test]
    fn state_root_rejects_relative_env_paths() {
        let _env = lock_env();
        let temp = tempfile::tempdir().unwrap();
        let _guard = EnvironmentGuard::apply(&[
            ("LATTE_LENS_STATE_DIR", Some("relative/path".into())),
            ("LATTE_HOME", None),
            ("HOME", Some(temp.path().to_owned().into_os_string())),
        ]);
        assert!(state_root().is_err());
    }

    #[test]
    fn state_root_rejects_relative_home() {
        let _env = lock_env();
        let _guard = EnvironmentGuard::apply(&[
            ("LATTE_LENS_STATE_DIR", None),
            ("LATTE_HOME", None),
            ("HOME", Some("relative/home".into())),
            ("USERPROFILE", None),
        ]);
        assert!(state_root().is_err());
    }

    #[test]
    fn state_root_rejects_empty_home() {
        let _env = lock_env();
        let _guard = EnvironmentGuard::apply(&[
            ("LATTE_LENS_STATE_DIR", None),
            ("LATTE_HOME", None),
            ("HOME", Some("".into())),
            ("USERPROFILE", None),
        ]);
        assert!(state_root().is_err());
    }

    #[test]
    fn round_trip_save_and_load() {
        let _env = lock_env();
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let _guard = EnvironmentGuard::apply(&[(
            "LATTE_LENS_STATE_DIR",
            Some(state_dir.clone().into_os_string()),
        )]);

        let mut paths = HashSet::new();
        paths.insert(PathBuf::from("/repo/a"));
        paths.insert(PathBuf::from("/repo/b"));
        save_ignored_error_paths(&paths).unwrap();

        let loaded = load_ignored_error_paths();
        assert_eq!(loaded, paths);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let _env = lock_env();
        let temp = tempfile::tempdir().unwrap();
        let _guard = EnvironmentGuard::apply(&[(
            "LATTE_LENS_STATE_DIR",
            Some(temp.path().join("missing").into_os_string()),
        )]);
        assert!(load_ignored_error_paths().is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty() {
        let _env = lock_env();
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join(STATE_FILE), "not json {").unwrap();
        let _guard =
            EnvironmentGuard::apply(&[("LATTE_LENS_STATE_DIR", Some(state_dir.into_os_string()))]);
        assert!(load_ignored_error_paths().is_empty());
    }

    #[test]
    fn ignore_check_uses_prefix_matching() {
        let mut ignored = HashSet::new();
        ignored.insert(PathBuf::from("/repo/group"));
        assert!(is_ignored_error_path(
            Path::new("/repo/group/repo-a/error"),
            &ignored
        ));
        assert!(is_ignored_error_path(Path::new("/repo/group"), &ignored));
        assert!(!is_ignored_error_path(
            Path::new("/repo/other/error"),
            &ignored
        ));
    }

    /// Restores environment variables on drop.
    struct EnvironmentGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvironmentGuard {
        fn apply(vars: &[(&'static str, Option<std::ffi::OsString>)]) -> Self {
            let saved = vars
                .iter()
                .map(|(key, _)| (*key, env::var_os(key)))
                .collect();
            for (key, value) in vars {
                // SAFETY: tests are single-threaded for env mutation.
                unsafe {
                    match value {
                        Some(v) => env::set_var(key, v),
                        None => env::remove_var(key),
                    }
                }
            }
            Self { saved }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                // SAFETY: tests are single-threaded for env mutation.
                unsafe {
                    match value {
                        Some(v) => env::set_var(key, v),
                        None => env::remove_var(key),
                    }
                }
            }
        }
    }
}
