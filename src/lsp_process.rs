//! Platform process-tree containment for explicitly trusted LSP executables.
#![allow(dead_code)]

use std::{
    io::{Read, Write},
    path::Path,
};

use anyhow::Result;

use crate::navigation::TrustedServer;

#[cfg(unix)]
#[path = "lsp_process_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "lsp_process_windows.rs"]
mod platform;

#[cfg(not(any(unix, windows)))]
compile_error!("Latte Lens LSP process containment requires Unix/macOS or Windows");

pub(crate) use platform::OwnedProcessTree;

pub(crate) struct ProcessIo {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Read + Send>,
}

pub(crate) struct SpawnedLanguageServer {
    pub tree: OwnedProcessTree,
    pub io: ProcessIo,
}

/// A Windows process was created inside its Job, but a later failure could not
/// prove that the Job is empty and the direct child is reaped. The manager must
/// retain this owner instead of letting handle unwinding masquerade as cleanup.
#[cfg(windows)]
pub(crate) struct RetainedSpawnFailure {
    original_error: String,
    cleanup_error: String,
    tree: Option<OwnedProcessTree>,
}

#[cfg(windows)]
impl RetainedSpawnFailure {
    pub(crate) fn new(
        original_error: impl Into<String>,
        cleanup_error: impl Into<String>,
        tree: OwnedProcessTree,
    ) -> Self {
        Self {
            original_error: original_error.into(),
            cleanup_error: cleanup_error.into(),
            tree: Some(tree),
        }
    }

    pub(crate) fn take_tree(&mut self) -> Option<OwnedProcessTree> {
        self.tree.take()
    }
}

#[cfg(windows)]
impl std::fmt::Debug for RetainedSpawnFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedSpawnFailure")
            .field("original_error", &self.original_error)
            .field("cleanup_error", &self.cleanup_error)
            .field("retained_tree", &self.tree.is_some())
            .finish()
    }
}

#[cfg(windows)]
impl std::fmt::Display for RetainedSpawnFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}; secondary cleanup failure: {}",
            self.original_error, self.cleanup_error
        )
    }
}

#[cfg(windows)]
impl std::error::Error for RetainedSpawnFailure {}

/// Spawn performs the final executable identity/workspace exclusion check in
/// the same platform function immediately before the OS process API.
pub(crate) fn spawn_language_server(
    server: &TrustedServer,
    workspace_root: &Path,
    server_root: &Path,
) -> Result<SpawnedLanguageServer> {
    platform::spawn(server, workspace_root, server_root)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum IoThreadKind {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IoThreadDone {
    pub kind: IoThreadKind,
    pub session_epoch: u64,
}
