#![allow(dead_code)]

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

pub struct TestRepo {
    _temp_dir: TempDir,
    root: PathBuf,
}

impl TestRepo {
    pub fn new() -> Self {
        Self::create(None)
    }

    pub fn with_root_name(name: impl AsRef<OsStr>) -> Self {
        Self::create(Some(name.as_ref()))
    }

    fn create(root_name: Option<&OsStr>) -> Self {
        let temp_dir = tempfile::tempdir().expect("create temporary repository");
        let root = root_name.map_or_else(
            || temp_dir.path().to_path_buf(),
            |name| temp_dir.path().join(name),
        );
        fs::create_dir_all(&root).expect("create repository root");
        let output = Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init", "--quiet"])
            .current_dir(&root)
            .output()
            .expect("run git init");
        assert_command_succeeded("git init", &output);

        let repo = Self {
            _temp_dir: temp_dir,
            root,
        };
        repo.git(&["config", "user.name", "Latte Lens Tests"]);
        repo.git(&["config", "user.email", "lattelens@example.invalid"]);
        repo
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write(&self, relative: &str, contents: impl AsRef<[u8]>) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent directories");
        }
        fs::write(path, contents).expect("write fixture file");
    }

    pub fn commit_all(&self, message: &str) {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "-m", message]);
    }

    pub fn git(&self, args: &[&str]) -> Output {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap_or_else(|error| panic!("run git {}: {error}", args.join(" ")));
        assert_command_succeeded(&format!("git {}", args.join(" ")), &output);
        output
    }

    pub fn status_bytes(&self) -> Vec<u8> {
        self.git(&["status", "--porcelain=v1", "-z"]).stdout
    }
}

pub fn assert_command_succeeded(action: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{action} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
