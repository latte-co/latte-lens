"""Hermetic filesystem/Git fixtures and read-only oracles for E2E."""

from __future__ import annotations

import hashlib
import os
import subprocess
import tempfile
from pathlib import Path


def run(*args: str, cwd: Path, environment: dict[str, str] | None = None) -> None:
    subprocess.run(
        args,
        cwd=cwd,
        env=environment,
        check=True,
        capture_output=True,
    )


class Sandbox:
    """One disposable workspace with isolated user/config/runtime roots."""

    def __init__(self, scenario: str) -> None:
        self.scenario = scenario
        self._temporary = tempfile.TemporaryDirectory(prefix=f"latte-lens-e2e-{scenario}-")
        self.root = Path(self._temporary.name)
        self.repository = self.root / "workspace"
        self.home = self.root / "home"
        self.xdg_config = self.root / "xdg-config"
        self.xdg_data = self.root / "xdg-data"
        self.xdg_state = self.root / "xdg-state"
        self.xdg_cache = self.root / "xdg-cache"
        self.runtime = self.root / "runtime"
        self.tmp = self.root / "tmp"
        for directory in (
            self.repository,
            self.home,
            self.xdg_config,
            self.xdg_data,
            self.xdg_state,
            self.xdg_cache,
            self.runtime,
            self.tmp,
        ):
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.cleaned = False

    def environment(self, clipboard_mode: str = "osc52") -> dict[str, str]:
        environment = os.environ.copy()
        for key in list(environment):
            if key.startswith(("GIT_", "LATTELENS_")) or key == "OLDPWD":
                environment.pop(key)
        environment.update(
            {
                "HOME": str(self.home),
                "XDG_CONFIG_HOME": str(self.xdg_config),
                "XDG_DATA_HOME": str(self.xdg_data),
                "XDG_STATE_HOME": str(self.xdg_state),
                "XDG_CACHE_HOME": str(self.xdg_cache),
                "XDG_RUNTIME_DIR": str(self.runtime),
                "TMPDIR": str(self.tmp),
                "TMP": str(self.tmp),
                "TEMP": str(self.tmp),
                "PWD": str(self.repository),
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_OPTIONAL_LOCKS": "0",
                "LATTELENS_CLIPBOARD": clipboard_mode,
                "TERM": environment.get("TERM", "xterm-256color"),
            }
        )
        return environment

    def cleanup(self) -> dict[str, object]:
        root = str(self.root)
        self._temporary.cleanup()
        self.cleaned = not Path(root).exists()
        return {"sandbox_root": root, "sandbox_removed": self.cleaned}


def init_repository(root: Path, environment: dict[str, str]) -> None:
    run("git", "init", "-q", "-b", "main", cwd=root, environment=environment)
    run("git", "config", "user.name", "Latte Lens E2E", cwd=root, environment=environment)
    run("git", "config", "user.email", "e2e@latte.invalid", cwd=root, environment=environment)


def create_navigation_fixture(root: Path, environment: dict[str, str]) -> None:
    init_repository(root, environment)
    clean = root / "z-clean.rs"
    changed = root / "a-dir" / "nested" / "b-changed.rs"
    changed.parent.mkdir(parents=True, exist_ok=True)
    clean.write_text("pub fn clean() {}\n", encoding="utf-8")
    changed.write_text("pub fn before() {}\n", encoding="utf-8")
    run("git", "add", "z-clean.rs", "a-dir/nested/b-changed.rs", cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "fixture", cwd=root, environment=environment)
    changed.write_text("pub fn changed() {}\n", encoding="utf-8")

    nested = root / "vendor" / "nested"
    nested.mkdir(parents=True)
    init_repository(nested, environment)
    nested_file = nested / "nested-owned.txt"
    nested_file.write_text("nested before\n", encoding="utf-8")
    run("git", "add", "nested-owned.txt", cwd=nested, environment=environment)
    run("git", "commit", "-q", "-m", "nested fixture", cwd=nested, environment=environment)
    nested_file.write_text("nested changed\n", encoding="utf-8")


def create_search_fixture(root: Path, environment: dict[str, str]) -> None:
    init_repository(root, environment)
    (root / "docs").mkdir()
    (root / "src").mkdir()
    (root / ".ignored").mkdir()
    (root / ".gitignore").write_text(".ignored/\n", encoding="utf-8")
    (root / "docs" / "guide.txt").write_text(
        "alpha needle\nsecond needle\n", encoding="utf-8"
    )
    (root / "src" / "search-target.rs").write_text(
        "pub fn searchable() {}\n// unique_workspace_phrase\n", encoding="utf-8"
    )
    (root / ".ignored" / "hidden.txt").write_text(
        "ignored_unique_phrase\n", encoding="utf-8"
    )
    run("git", "add", ".gitignore", "docs/guide.txt", "src/search-target.rs", cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "search fixture", cwd=root, environment=environment)


def create_git_matrix_fixture(root: Path, environment: dict[str, str]) -> None:
    init_repository(root, environment)
    initial = {
        "staged.txt": "staged before\n",
        "worktree.txt": "worktree before\n",
        "both.txt": "both before\n",
        "deleted.txt": "delete me\n",
        "old-name.txt": "rename body\n",
        "clean.txt": "clean body\n",
    }
    for name, content in initial.items():
        (root / name).write_text(content, encoding="utf-8")
    run("git", "add", ".", cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "matrix fixture", cwd=root, environment=environment)

    (root / "staged.txt").write_text("staged after\n", encoding="utf-8")
    run("git", "add", "staged.txt", cwd=root, environment=environment)
    (root / "worktree.txt").write_text("worktree after\n", encoding="utf-8")
    (root / "both.txt").write_text("both staged\n", encoding="utf-8")
    run("git", "add", "both.txt", cwd=root, environment=environment)
    (root / "both.txt").write_text("both worktree\n", encoding="utf-8")
    (root / "deleted.txt").unlink()
    (root / "old-name.txt").rename(root / "renamed.txt")
    run("git", "add", "-A", "old-name.txt", "renamed.txt", cwd=root, environment=environment)
    (root / "untracked.txt").write_text("untracked content\n", encoding="utf-8")


def _file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    metadata = path.lstat() if path.exists() or path.is_symlink() else None
    if metadata is not None:
        digest.update(f"{metadata.st_mode:o}\0{metadata.st_size}\0{metadata.st_mtime_ns}\0".encode())
    if path.is_symlink():
        digest.update(b"symlink\0")
        digest.update(os.readlink(path).encode(errors="surrogateescape"))
    elif path.is_file():
        digest.update(b"file\0")
        digest.update(path.read_bytes())
    else:
        digest.update(b"missing\0")
    return digest.hexdigest()


def _snapshot_tree(root: Path, *, exclude_git: bool) -> dict[str, str]:
    snapshot: dict[str, str] = {}
    for current, directories, files in os.walk(root):
        current_path = Path(current)
        if exclude_git:
            directories[:] = [directory for directory in directories if directory != ".git"]
        for name in sorted(files):
            path = current_path / name
            relative = path.relative_to(root).as_posix()
            snapshot[relative] = _file_digest(path)
        for name in sorted(directories):
            path = current_path / name
            if path.is_symlink():
                snapshot[path.relative_to(root).as_posix()] = _file_digest(path)
    return snapshot


def _git_roots(workspace: Path) -> list[Path]:
    roots = []
    for current, directories, files in os.walk(workspace):
        path = Path(current)
        if ".git" in directories or ".git" in files:
            roots.append(path)
            directories[:] = [directory for directory in directories if directory != ".git"]
    return sorted(roots)


def _git_metadata_snapshot(workspace: Path) -> dict[str, str]:
    snapshot: dict[str, str] = {}
    for repository in _git_roots(workspace):
        marker = repository / ".git"
        git_dir = marker
        if marker.is_file():
            content = marker.read_text(encoding="utf-8", errors="surrogateescape")
            if content.startswith("gitdir: "):
                git_dir = (repository / content[8:].strip()).resolve()
        for name in ("HEAD", "index", "config", "packed-refs"):
            path = git_dir / name
            key = f"{repository.relative_to(workspace).as_posix() or '.'}/.git/{name}"
            snapshot[key] = _file_digest(path)
        refs = git_dir / "refs"
        if refs.exists():
            for path in sorted(refs.rglob("*")):
                if path.is_file() or path.is_symlink():
                    relative = path.relative_to(git_dir).as_posix()
                    key = f"{repository.relative_to(workspace).as_posix() or '.'}/.git/{relative}"
                    snapshot[key] = _file_digest(path)
    return snapshot


def _git_status_snapshot(
    workspace: Path, environment: dict[str, str]
) -> dict[str, bytes]:
    snapshot: dict[str, bytes] = {}
    for repository in _git_roots(workspace):
        result = subprocess.run(
            ["git", "status", "--porcelain=v2", "-z", "--untracked-files=all"],
            cwd=repository,
            env=environment,
            check=True,
            capture_output=True,
            timeout=10,
        )
        key = repository.relative_to(workspace).as_posix() or "."
        snapshot[key] = result.stdout
    return snapshot


def _guarded_path_snapshot(path: Path) -> dict[str, str]:
    """Snapshot the small host config surfaces the child must never reach."""

    if not path.exists() and not path.is_symlink():
        return {".": "missing"}
    if path.is_file() or path.is_symlink():
        return {".": _file_digest(path)}
    snapshot = {".": "directory"}
    snapshot.update(_snapshot_tree(path, exclude_git=False))
    return snapshot


class ExternalIsolationOracle:
    """Guard real user config and checkout metadata outside the E2E sandbox."""

    def __init__(self, host_cwd: Path | None = None, host_home: Path | None = None) -> None:
        self.host_cwd = (host_cwd or Path.cwd()).resolve()
        self.host_home = (host_home or Path.home()).resolve()
        self.guarded_paths = (
            self.host_home / ".gitconfig",
            self.host_home / ".config" / "git" / "config",
            self.host_home / ".config" / "latte-lens",
            self.host_home / ".local" / "state" / "latte-lens",
            self.host_home / ".cache" / "latte-lens",
        )
        self.config_before = {
            str(path): _guarded_path_snapshot(path) for path in self.guarded_paths
        }
        self.checkout_root = self._checkout_root()
        self.checkout_before = self._checkout_snapshot()

    def _host_git_environment(self) -> dict[str, str]:
        environment = os.environ.copy()
        for key in list(environment):
            if key.startswith("GIT_"):
                environment.pop(key)
        return environment

    def _checkout_root(self) -> Path | None:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=self.host_cwd,
            env=self._host_git_environment(),
            check=False,
            capture_output=True,
            timeout=10,
        )
        if result.returncode != 0:
            return None
        return Path(os.fsdecode(result.stdout.rstrip(b"\r\n"))).resolve()

    def _checkout_snapshot(self) -> dict[str, object]:
        if self.checkout_root is None:
            return {"git_root": None}
        git_dir = self.checkout_root / ".git"
        metadata = {
            name: _file_digest(git_dir / name)
            for name in ("HEAD", "index", "config", "packed-refs")
        }
        status = subprocess.run(
            ["git", "status", "--porcelain=v2", "-z", "--untracked-files=no"],
            cwd=self.checkout_root,
            env=self._host_git_environment(),
            check=True,
            capture_output=True,
            timeout=10,
        ).stdout
        return {"git_root": str(self.checkout_root), "metadata": metadata, "status": status}

    def verify(self) -> dict[str, object]:
        config_after = {
            str(path): _guarded_path_snapshot(path) for path in self.guarded_paths
        }
        checkout_after = self._checkout_snapshot()
        config_unchanged = self.config_before == config_after
        checkout_unchanged = self.checkout_before == checkout_after
        if not config_unchanged or not checkout_unchanged:
            raise AssertionError(
                "external isolation invariant failed: "
                f"host_config_unchanged={config_unchanged}, "
                f"host_checkout_unchanged={checkout_unchanged}"
            )
        return {
            "host_config_unchanged": config_unchanged,
            "host_checkout_unchanged": checkout_unchanged,
            "guarded_config_paths": len(self.guarded_paths),
        }


class ReadOnlyOracle:
    """Detect Lens writes while allowing explicit fixture-driver mutations."""

    def __init__(self, workspace: Path, environment: dict[str, str]) -> None:
        self.workspace = workspace
        self.environment = environment
        self.git_before = _git_metadata_snapshot(workspace)
        self.expected_git_status = _git_status_snapshot(workspace, environment)
        self.expected_worktree = _snapshot_tree(workspace, exclude_git=True)

    def record_driver_write(self, path: Path) -> None:
        """Allow one exact test-driver write without blessing other changes."""

        relative = path.relative_to(self.workspace).as_posix()
        self.expected_worktree[relative] = _file_digest(path)
        self.expected_git_status = _git_status_snapshot(self.workspace, self.environment)

    def record_driver_remove(self, path: Path) -> None:
        """Allow one exact test-driver removal without blessing other changes."""

        relative = path.relative_to(self.workspace).as_posix()
        self.expected_worktree.pop(relative, None)
        self.expected_git_status = _git_status_snapshot(self.workspace, self.environment)

    def verify(self) -> dict[str, object]:
        git_after = _git_metadata_snapshot(self.workspace)
        git_status_after = _git_status_snapshot(self.workspace, self.environment)
        worktree_after = _snapshot_tree(self.workspace, exclude_git=True)
        git_unchanged = self.git_before == git_after
        git_status_unchanged = self.expected_git_status == git_status_after
        worktree_unchanged = self.expected_worktree == worktree_after
        if not git_unchanged or not git_status_unchanged or not worktree_unchanged:
            raise AssertionError(
                "read-only invariant failed: "
                f"git_metadata_unchanged={git_unchanged}, "
                f"git_status_unchanged={git_status_unchanged}, "
                f"expected_worktree_unchanged={worktree_unchanged}"
            )
        return {
            "git_metadata_unchanged": git_unchanged,
            "git_status_unchanged": git_status_unchanged,
            "expected_worktree_unchanged": worktree_unchanged,
            "git_metadata_entries": len(git_after),
            "worktree_entries": len(worktree_after),
        }
