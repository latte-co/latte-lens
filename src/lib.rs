#[cfg(any(feature = "agent-observability", test))]
pub mod agent;
pub mod app;
mod clipboard;
pub mod config;
mod content_safety;
mod diff;
mod folding;
pub mod git;
mod lsp;
mod lsp_process;
pub mod navigation;
pub mod preview;
pub mod repo_graph;
mod runtime;
mod search;
mod system_preview;
mod text_layout;
pub mod theme;
pub mod tree;
pub mod ui;

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared test utilities for environment isolation.
    //!
    //! Env-var tests mutate process-global state, so they must serialize on a
    //! single lock. Poison is tolerated so one assertion failure does not
    //! cascade into lock-poison failures in sibling tests.

    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Restores environment variables on drop.
    pub struct EnvironmentGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvironmentGuard {
        pub fn apply(vars: &[(&'static str, Option<std::ffi::OsString>)]) -> Self {
            let saved = vars
                .iter()
                .map(|(key, _)| (*key, std::env::var_os(key)))
                .collect();
            for (key, value) in vars {
                // SAFETY: tests are single-threaded for env mutation.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
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
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }
}
