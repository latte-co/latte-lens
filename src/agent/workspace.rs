use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use super::{
    BoundedVec, IdentityKeyer, SensitiveWorkspaceLocator, WorkspaceHint, WorkspaceSelector,
};

/// Canonical workspace identity shared by Lens and the Hook CLI.
///
/// Raw paths remain inside this boundary. Only keyed hints are returned to the
/// observability core, receiver registry, metadata index, and UI runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedWorkspace {
    canonical_root: PathBuf,
    primary: WorkspaceHint,
    selected: WorkspaceSelector,
}

impl ResolvedWorkspace {
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub const fn primary(&self) -> &WorkspaceHint {
        &self.primary
    }

    pub const fn selector(&self) -> &WorkspaceSelector {
        &self.selected
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceResolutionError {
    Unavailable,
    NotDirectory,
    IdentityRejected,
}

impl fmt::Display for WorkspaceResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "workspace resolution failed: {self:?}")
    }
}

impl Error for WorkspaceResolutionError {}

/// Resolve a launch directory to a stable workspace.
///
/// The canonical launch directory is the workspace. Lens startup and Hook
/// emission use the same function, so only Code Agents launched from exactly
/// the directory selected by Lens share a workspace identity.
pub fn resolve_workspace(
    launch_directory: &Path,
    identity: &dyn IdentityKeyer,
) -> Result<ResolvedWorkspace, WorkspaceResolutionError> {
    let launch = launch_directory
        .canonicalize()
        .map_err(|_| WorkspaceResolutionError::Unavailable)?;
    if !launch.is_dir() {
        return Err(WorkspaceResolutionError::NotDirectory);
    }

    let primary = hint_for_path(identity, &launch)?;
    let selected = WorkspaceSelector::new(
        BoundedVec::try_from_vec(vec![primary.clone()])
            .expect("one exact workspace is always bounded"),
    );

    Ok(ResolvedWorkspace {
        canonical_root: launch,
        primary,
        selected,
    })
}

fn hint_for_path(
    identity: &dyn IdentityKeyer,
    path: &Path,
) -> Result<WorkspaceHint, WorkspaceResolutionError> {
    let bytes = path_identity_bytes(path);
    identity
        .workspace_hint(SensitiveWorkspaceLocator::new(&bytes))
        .map_err(|_| WorkspaceResolutionError::IdentityRejected)
}

#[cfg(unix)]
fn path_identity_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn path_identity_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::agent::{HmacIdentityKeyer, SensitiveId};

    fn identity() -> HmacIdentityKeyer {
        HmacIdentityKeyer::new(SensitiveId::new(&[7; 32])).expect("identity")
    }

    #[test]
    fn repository_subdirectories_are_distinct_exact_workspaces() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::create_dir(directory.path().join(".git")).expect("git marker");
        let first = directory.path().join("crates/first/src");
        let second = directory.path().join("crates/second");
        fs::create_dir_all(&first).expect("first");
        fs::create_dir_all(&second).expect("second");

        let first = resolve_workspace(&first, &identity()).expect("first resolution");
        let second = resolve_workspace(&second, &identity()).expect("second resolution");

        assert_ne!(first.primary(), second.primary());
        assert_eq!(
            first.canonical_root(),
            directory
                .path()
                .join("crates/first/src")
                .canonicalize()
                .expect("canonical root")
        );
        assert_eq!(first.selector().workspaces(), &[first.primary().clone()]);
    }

    #[test]
    fn repeated_resolution_of_one_directory_is_stable() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first = resolve_workspace(directory.path(), &identity()).expect("first");
        let second = resolve_workspace(directory.path(), &identity()).expect("second");

        assert_eq!(first.primary(), second.primary());
        assert_eq!(
            first.canonical_root(),
            directory.path().canonicalize().expect("canonical root")
        );
    }

    #[test]
    fn files_are_not_workspaces() {
        let directory = tempfile::tempdir().expect("workspace");
        let file = directory.path().join("file");
        fs::write(&file, b"content").expect("file");
        assert_eq!(
            resolve_workspace(&file, &identity()),
            Err(WorkspaceResolutionError::NotDirectory)
        );
    }
}
