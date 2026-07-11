use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};

use crate::content_safety::{
    ContentPathKind, OpenRegular, inspect_content_path, open_regular, read_link_bounded,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStatus {
    pub index: char,
    pub worktree: char,
}

impl FileStatus {
    pub fn label(self) -> String {
        match (self.index, self.worktree) {
            ('?', '?') => "??".to_owned(),
            (index, ' ') => index.to_string(),
            (' ', worktree) => worktree.to_string(),
            (index, worktree) => format!("{index}{worktree}"),
        }
    }

    pub const fn is_untracked(self) -> bool {
        self.index == '?' && self.worktree == '?'
    }

    pub const fn has_staged_change(self) -> bool {
        !matches!(self.index, ' ' | '?' | '!')
    }

    pub const fn has_worktree_change(self) -> bool {
        !matches!(self.worktree, ' ' | '?' | '!')
    }
}

pub type StatusMap = HashMap<PathBuf, FileStatus>;

/// Porcelain-v2 state reported for a Gitlink entry.
///
/// `commit_changed` describes the checked-out submodule commit differing from
/// the parent repository's recorded Gitlink. `modified_content` and
/// `untracked_content` describe dirt inside the child worktree. Keeping these
/// bits separate prevents a parent pointer change from being confused with a
/// dirty child repository.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SubmoduleStatus {
    pub is_submodule: bool,
    pub commit_changed: bool,
    pub modified_content: bool,
    pub untracked_content: bool,
}

/// One byte-preserving entry from `git status --porcelain=v2 -z`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitStatusEntry {
    pub path: PathBuf,
    pub original_path: Option<PathBuf>,
    pub status: FileStatus,
    pub submodule: SubmoduleStatus,
}

#[derive(Clone, Debug)]
pub struct GitRepo {
    root: PathBuf,
    git_dir: PathBuf,
}

impl GitRepo {
    pub fn discover(path: &Path) -> Result<Option<Self>> {
        let output = git_command()
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(path)
            .output()
            .context("failed to run git; make sure it is installed and available in PATH")?;

        if !output.status.success() {
            return Ok(None);
        }

        let Some(root) = git_path_from_output(&output.stdout) else {
            return Ok(None);
        };
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot resolve Git root {}", root.display()))?;

        let git_dir_output = git_command()
            .args(["rev-parse", "--absolute-git-dir"])
            .current_dir(&root)
            .output()
            .context("failed to resolve Git directory")?;
        ensure_success("resolve Git directory", &git_dir_output)?;
        let git_dir = git_path_from_output(&git_dir_output.stdout)
            .context("Git returned an empty Git directory path")?;
        let git_dir = git_dir
            .canonicalize()
            .with_context(|| format!("cannot resolve Git directory {}", git_dir.display()))?;

        Ok(Some(Self { root, git_dir }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn branch(&self) -> Result<String> {
        let output = self.run(&["branch", "--show-current"])?;
        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !branch.is_empty() {
                return Ok(branch);
            }
        }

        let output = self.run(&["rev-parse", "--short", "HEAD"])?;
        ensure_success("read current revision", &output)?;
        Ok(format!(
            "detached@{}",
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }

    pub fn statuses(&self) -> Result<StatusMap> {
        Ok(self
            .status_entries()?
            .into_iter()
            .map(|entry| (entry.path, entry.status))
            .collect())
    }

    pub fn status_entries(&self) -> Result<Vec<GitStatusEntry>> {
        let output = self.run(&["status", "--porcelain=v2", "-z", "--untracked-files=all"])?;
        ensure_success("read Git status", &output)?;
        Ok(parse_porcelain_v2(&output.stdout))
    }

    /// Read declared submodule paths without initializing, updating, or
    /// contacting any remotes.
    pub fn submodule_paths(&self) -> Result<Vec<PathBuf>> {
        let modules = self.root.join(".gitmodules");
        if !modules.is_file() {
            return Ok(Vec::new());
        }

        let output = git_command()
            .arg("config")
            .arg("--null")
            .arg("--file")
            .arg(&modules)
            .args(["--get-regexp", r"^submodule\..*\.path$"])
            .current_dir(&self.root)
            .output()
            .context("failed to read .gitmodules")?;
        // `git config --get-regexp` uses exit 1 for no matches.
        if !output.status.success() && output.status.code() != Some(1) {
            ensure_success("read .gitmodules", &output)?;
        }

        Ok(parse_git_config_values(&output.stdout))
    }

    pub fn diff_for(&self, path: &Path, status: Option<FileStatus>) -> Result<Vec<String>> {
        self.diff_for_change(path, None, status)
    }

    /// Render a change using its complete path identity.
    ///
    /// Renames and copies need both the destination and original paths in the
    /// pathspec; otherwise Git can degrade a pure rename/copy into a false
    /// new-file diff.
    pub fn diff_for_change(
        &self,
        path: &Path,
        original_path: Option<&Path>,
        status: Option<FileStatus>,
    ) -> Result<Vec<String>> {
        let Some(status) = status else {
            return Ok(vec![
                format!("{} has no uncommitted changes.", path.display()),
                "Select a changed file to inspect its diff.".to_owned(),
            ]);
        };

        if status.is_untracked() {
            return self.untracked_diff(path);
        }

        let mut sections = Vec::new();
        if status.has_staged_change() {
            let output = self.run_path_diff(true, path, original_path)?;
            ensure_success("read staged diff", &output)?;
            push_section(&mut sections, "STAGED", &output.stdout);
        }
        if status.has_worktree_change() {
            let output = self.run_path_diff(false, path, original_path)?;
            ensure_success("read working tree diff", &output)?;
            push_section(&mut sections, "WORKTREE", &output.stdout);
        }

        if sections.is_empty() {
            sections.push(format!(
                "Git reports status {}, but there is no text diff to display.",
                status.label()
            ));
        }
        Ok(sections)
    }

    fn untracked_diff(&self, path: &Path) -> Result<Vec<String>> {
        const MAX_BYTES: u64 = 512 * 1024;
        const MAX_LINES: usize = 2_000;

        let absolute_path = self.root.join(path);
        let inspected = inspect_content_path(Some(&self.root), &absolute_path)?;
        if inspected.kind == ContentPathKind::SymbolicLink && inspected.path == absolute_path {
            let Some((target, truncated)) = read_link_bounded(
                &self.root,
                &absolute_path,
                usize::try_from(MAX_BYTES).unwrap_or(usize::MAX),
            )?
            else {
                return Ok(vec![
                    "Untracked symbolic link changed while it was inspected; no content was read."
                        .to_owned(),
                ]);
            };
            if truncated {
                return Ok(vec![format!(
                    "Untracked symbolic link target is too large to preview safely (over {MAX_BYTES} bytes)."
                )]);
            }
            return Ok(untracked_symlink_diff(path, &target, MAX_LINES));
        }
        if inspected.kind != ContentPathKind::Regular || inspected.path != absolute_path {
            return Ok(vec![format!(
                "Untracked {} is not read for safety.",
                inspected.kind.label()
            )]);
        }

        let file = match open_regular(Some(&self.root), &absolute_path)? {
            OpenRegular::Opened(file) => file,
            OpenRegular::Declined(changed) => {
                return Ok(vec![format!(
                    "Untracked path changed to a {}; no content was read.",
                    changed.kind.label()
                )]);
            }
        };
        if file.len() > MAX_BYTES {
            return Ok(vec![format!(
                "Untracked file is too large to preview ({} bytes).",
                file.len()
            )]);
        }

        let mut bytes = Vec::new();
        file.take(MAX_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {}", absolute_path.display()))?;
        if bytes.len() > usize::try_from(MAX_BYTES).unwrap_or(usize::MAX) {
            return Ok(vec![format!(
                "Untracked file is too large to preview (over {MAX_BYTES} bytes)."
            )]);
        }
        if bytes.contains(&0) {
            return Ok(vec!["Untracked binary file.".to_owned()]);
        }

        let text = String::from_utf8_lossy(&bytes);
        let total_lines = text.lines().count();
        let mut lines = vec![
            "── UNTRACKED ──".to_owned(),
            format!("diff --git a/{0} b/{0}", path.display()),
            "new file mode 100644".to_owned(),
            "--- /dev/null".to_owned(),
            format!("+++ b/{}", path.display()),
            format!("@@ -0,0 +1,{total_lines} @@"),
        ];
        lines.extend(text.lines().take(MAX_LINES).map(|line| format!("+{line}")));
        if total_lines > MAX_LINES {
            lines.push(format!("… preview truncated after {MAX_LINES} lines"));
        }
        Ok(lines)
    }

    fn run_path_diff(
        &self,
        cached: bool,
        path: &Path,
        original_path: Option<&Path>,
    ) -> Result<Output> {
        let mut command = git_command();
        command
            .args(["diff", "--no-ext-diff", "--no-color", "--unified=3"])
            .current_dir(&self.root);
        if original_path.is_some() {
            command.args(["--find-renames", "--find-copies-harder"]);
        }
        if cached {
            command.arg("--cached");
        }
        command.arg("--");
        if let Some(original_path) = original_path {
            command.arg(original_path);
        }
        command.arg(path);
        command.output().context("failed to run git diff")
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        git_command()
            .args(args)
            .current_dir(&self.root)
            .output()
            .with_context(|| format!("failed to run git {}", args.join(" ")))
    }
}

fn untracked_symlink_diff(path: &Path, target: &str, max_lines: usize) -> Vec<String> {
    let total_lines = target.lines().count();
    let mut lines = vec![
        "── UNTRACKED ──".to_owned(),
        format!("diff --git a/{0} b/{0}", path.display()),
        "new file mode 120000".to_owned(),
        "--- /dev/null".to_owned(),
        format!("+++ b/{}", path.display()),
        format!("@@ -0,0 +1,{total_lines} @@"),
    ];
    lines.extend(
        target
            .lines()
            .take(max_lines)
            .map(|line| format!("+{line}")),
    );
    if total_lines > max_lines {
        lines.push(format!("… preview truncated after {max_lines} lines"));
    }
    lines
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command.env("GIT_OPTIONAL_LOCKS", "0");
    command
}

fn ensure_success(action: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("failed to {action}: {}", stderr.trim())
}

fn push_section(lines: &mut Vec<String>, title: &str, bytes: &[u8]) {
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines.push(format!("── {title} ──"));
    lines.extend(String::from_utf8_lossy(bytes).lines().map(str::to_owned));
}

fn parse_porcelain_v2(bytes: &[u8]) -> Vec<GitStatusEntry> {
    let records: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    let mut entries = Vec::new();
    let mut index = 0;

    while index < records.len() {
        let record = records[index];
        let parsed = match record.first().copied() {
            Some(b'1') => parse_tracked_record(record, 9),
            Some(b'2') => {
                let mut entry = parse_tracked_record(record, 10);
                if let Some(entry) = &mut entry {
                    entry.original_path = records
                        .get(index + 1)
                        .filter(|path| !path.is_empty())
                        .map(|path| path_from_git_bytes(path));
                }
                index += 1;
                entry
            }
            Some(b'u') => parse_tracked_record(record, 11),
            Some(b'?') if record.get(1) == Some(&b' ') => Some(GitStatusEntry {
                path: path_from_git_bytes(&record[2..]),
                original_path: None,
                status: FileStatus {
                    index: '?',
                    worktree: '?',
                },
                submodule: SubmoduleStatus::default(),
            }),
            _ => None,
        };
        if let Some(entry) = parsed {
            entries.push(entry);
        }
        index += 1;
    }

    entries
}

fn parse_tracked_record(record: &[u8], field_count: usize) -> Option<GitStatusEntry> {
    let fields: Vec<&[u8]> = record.splitn(field_count, |byte| *byte == b' ').collect();
    if fields.len() != field_count || fields[1].len() != 2 || fields[2].len() != 4 {
        return None;
    }
    let status = FileStatus {
        index: porcelain_status_char(fields[1][0]),
        worktree: porcelain_status_char(fields[1][1]),
    };
    Some(GitStatusEntry {
        path: path_from_git_bytes(fields[field_count - 1]),
        original_path: None,
        status,
        submodule: parse_submodule_status(fields[2]),
    })
}

const fn porcelain_status_char(byte: u8) -> char {
    if byte == b'.' { ' ' } else { byte as char }
}

fn parse_submodule_status(field: &[u8]) -> SubmoduleStatus {
    SubmoduleStatus {
        is_submodule: field.first() == Some(&b'S'),
        commit_changed: field.get(1) == Some(&b'C'),
        modified_content: field.get(2) == Some(&b'M'),
        untracked_content: field.get(3) == Some(&b'U'),
    }
}

fn parse_git_config_values(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            let separator = record.iter().position(|byte| *byte == b'\n')?;
            let value = &record[separator + 1..];
            (!value.is_empty()).then(|| path_from_git_bytes(value))
        })
        .collect()
}

#[cfg(unix)]
fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;

    std::ffi::OsString::from_vec(bytes.to_vec()).into()
}

#[cfg(not(unix))]
fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn git_path_from_output(bytes: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    let path_bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);

    #[cfg(not(unix))]
    let path_bytes = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(bytes);

    (!path_bytes.is_empty()).then(|| path_from_git_bytes(path_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2_statuses_spaces_renames_and_submodules() {
        let input = concat!(
            "1 .M N... 100644 100644 100644 aaaaaaa aaaaaaa src/main.rs\0",
            "? notes with spaces.md\0",
            "2 R. N... 100644 100644 100644 aaaaaaa bbbbbbb R100 src/new name.rs\0",
            "src/old name.rs\0",
            "2 C. N... 100644 100644 100644 aaaaaaa bbbbbbb C100 src/copy.rs\0",
            "src/source.rs\0",
            "1 .M S.MU 160000 160000 160000 aaaaaaa bbbbbbb modules/child\0"
        )
        .as_bytes();
        let entries = parse_porcelain_v2(input);
        let statuses: StatusMap = entries
            .iter()
            .map(|entry| (entry.path.clone(), entry.status))
            .collect();

        assert_eq!(entries.len(), 5);
        assert_eq!(
            statuses[Path::new("src/main.rs")],
            FileStatus {
                index: ' ',
                worktree: 'M'
            }
        );
        assert_eq!(statuses[Path::new("notes with spaces.md")].label(), "??");
        assert_eq!(statuses[Path::new("src/new name.rs")].label(), "R");
        assert_eq!(
            entries[2].original_path.as_deref(),
            Some(Path::new("src/old name.rs"))
        );
        assert_eq!(
            entries[3].original_path.as_deref(),
            Some(Path::new("src/source.rs"))
        );
        assert_eq!(statuses[Path::new("src/copy.rs")].label(), "C");
        assert_eq!(
            entries[4].submodule,
            SubmoduleStatus {
                is_submodule: true,
                commit_changed: false,
                modified_content: true,
                untracked_content: true,
            }
        );
    }

    #[test]
    fn labels_staged_and_worktree_changes() {
        let status = FileStatus {
            index: 'M',
            worktree: 'D',
        };
        assert_eq!(status.label(), "MD");
        assert!(status.has_staged_change());
        assert!(status.has_worktree_change());
    }

    #[test]
    fn parser_handles_short_records_without_panicking() {
        let input = b"x\0? invalid-\xff-name\0";
        let statuses: StatusMap = parse_porcelain_v2(input)
            .into_iter()
            .map(|entry| (entry.path, entry.status))
            .collect();

        assert_eq!(statuses.len(), 1);
        assert_eq!(
            statuses.values().next().copied(),
            Some(FileStatus {
                index: '?',
                worktree: '?'
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn parser_preserves_non_utf8_path_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let input = b"? invalid-\xff-name\0";
        let entries = parse_porcelain_v2(input);
        let path = &entries[0].path;

        assert_eq!(path.as_os_str().as_bytes(), b"invalid-\xff-name");
    }

    #[test]
    fn root_parser_removes_only_one_protocol_line_ending() {
        assert_eq!(
            git_path_from_output(b"/tmp/ leading and trailing  \n"),
            Some(PathBuf::from("/tmp/ leading and trailing  "))
        );
        assert_eq!(
            git_path_from_output(b"/tmp/root \r\n"),
            expected_crlf_path()
        );
        assert_eq!(
            git_path_from_output(b"/tmp/root\n\n"),
            Some(PathBuf::from("/tmp/root\n"))
        );
        assert_eq!(
            git_path_from_output(b"/tmp/root\r"),
            Some(PathBuf::from("/tmp/root\r"))
        );
        assert_eq!(git_path_from_output(b"\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn root_parser_preserves_non_utf8_path_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let root = git_path_from_output(b"/tmp/root-\xff \n").unwrap();

        assert_eq!(root.as_os_str().as_bytes(), b"/tmp/root-\xff ");
    }

    #[cfg(unix)]
    fn expected_crlf_path() -> Option<PathBuf> {
        Some(PathBuf::from("/tmp/root \r"))
    }

    #[cfg(not(unix))]
    fn expected_crlf_path() -> Option<PathBuf> {
        Some(PathBuf::from("/tmp/root "))
    }

    #[test]
    fn parses_git_config_null_values() {
        let paths = parse_git_config_values(
            b"submodule.child.path\nmodules/child with spaces\0submodule.other.path\nother\0",
        );
        assert_eq!(
            paths,
            [
                PathBuf::from("modules/child with spaces"),
                PathBuf::from("other")
            ]
        );
    }
}
