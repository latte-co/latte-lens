"""Hermetic filesystem/Git fixtures and read-only oracles for E2E."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
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
        "pub fn searchable() {\n// unique_workspace_phrase\nlet folded_value = 1;\n}\n",
        encoding="utf-8",
    )
    (root / "src" / "search-target-other.rs").write_text(
        "pub fn other_search_target() {}\n", encoding="utf-8"
    )
    (root / ".ignored" / "hidden.txt").write_text(
        "ignored_unique_phrase\n", encoding="utf-8"
    )
    run(
        "git",
        "add",
        ".gitignore",
        "docs/guide.txt",
        "src/search-target.rs",
        "src/search-target-other.rs",
        cwd=root,
        environment=environment,
    )
    run("git", "commit", "-q", "-m", "search fixture", cwd=root, environment=environment)


def _navigation_helper() -> Path:
    helper = (
        Path(__file__).resolve().parents[2]
        / "target"
        / "debug"
        / ("latte-lens-lsp-test-helper.exe" if os.name == "nt" else "latte-lens-lsp-test-helper")
    ).resolve()
    if not helper.is_file():
        raise RuntimeError(f"navigation helper is not built: {helper}")
    return helper


def _isolated_path_with_git(environment: dict[str, str], *directories: Path) -> str:
    git = shutil.which("git", path=environment.get("PATH"))
    if git is None:
        raise RuntimeError("git is required by the E2E read-only oracle")
    entries = [str(directory.resolve()) for directory in directories]
    entries.append(str(Path(git).resolve().parent))
    return os.pathsep.join(entries)


def _write_navigation_config(
    root: Path, environment: dict[str, str], helper: Path, role: str
) -> Path:
    home = Path(environment["HOME"]).resolve()
    environment["HOME"] = str(home)
    config = home / ".latte" / "latte-lens.jsonc"
    config.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if os.name != "nt":
        bin_dir = config.parent / "bin"
        bin_dir.mkdir(mode=0o700)
        helper_link = bin_dir / helper.name
        helper_link.symlink_to(helper)
        helper = helper_link
    helper_json = json.dumps(str(helper))
    role_json = json.dumps(role)
    config.write_text(
        f"""{{
  // Code navigation is the feature; the language server is its engine.
  "code_navigation": {{
    "enabled": true,
    "languages": {{
      "rust": {{
        "enabled": true,
        "engine": {{
          "type": "language_server",
          "command": [{helper_json}, {role_json},],
        }},
      }},
    }},
  }},
}}
""",
        encoding="utf-8",
    )
    return config


def create_code_navigation_fixture(root: Path, environment: dict[str, str]) -> None:
    init_repository(root, environment)
    caller = root / "a-caller.rs"
    target = root / "b-target.rs"
    caller.write_text("caller!(); // 😀中\n", encoding="utf-8")
    target.write_text("pub fn 目标😀() {}\n", encoding="utf-8")
    run("git", "add", "a-caller.rs", "b-target.rs", cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "navigation fixture", cwd=root, environment=environment)

    _write_navigation_config(root, environment, _navigation_helper(), "pty-lsp")
    environment["LATTELENS_TEST_CALLER_URI"] = caller.resolve().as_uri()
    environment["LATTELENS_TEST_TARGET_URI"] = target.resolve().as_uri()
    environment["LATTELENS_TEST_TRACE"] = str((root.parent / "lsp-trace.txt").resolve())
    environment["LATTELENS_TEST_RELEASE"] = str(
        (root.parent / "release-second-definition").resolve()
    )


def create_fold_mouse_navigation_fixture(
    root: Path, environment: dict[str, str]
) -> None:
    init_repository(root, environment)
    caller = root / "a-fold-caller.rs"
    target = root / "b-fold-target.rs"
    first_body = "\n".join(f"    let first_marker_{index:02} = {index};" for index in range(12))
    middle_body = "\n".join(
        ["    let middle_target_call = target_call();"]
        + [f"    let middle_marker_{index:02} = {index};" for index in range(14)]
        + ["    let middle_body_tail = 99;"]
    )
    caller.write_text(
        "pub fn first_region() {\n"
        f"{first_body}\n"
        "}\n\n"
        "pub fn middle_region_with_a_deliberately_long_anchor_that_forces_the_fold_summary_onto_a_synthetic_visual_row() {\n"
        f"{middle_body}\n"
        "}\n\n"
        "pub fn third_region() {\n"
        "    let third_body_marker = 3;\n"
        "}\n",
        encoding="utf-8",
    )
    target.write_text("pub fn target_destination() {}\n", encoding="utf-8")
    run("git", "add", caller.name, target.name, cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "fold mouse fixture", cwd=root, environment=environment)

    _write_navigation_config(root, environment, _navigation_helper(), "pty-lsp")
    environment["LATTELENS_TEST_CALLER_URI"] = caller.resolve().as_uri()
    environment["LATTELENS_TEST_TARGET_URI"] = target.resolve().as_uri()
    environment["LATTELENS_TEST_TRACE"] = str((root.parent / "fold-mouse-trace.txt").resolve())
    environment["LATTELENS_TEST_RELEASE"] = str((root.parent / "unused-fold-release").resolve())


def create_lsp_document_symbol_fixture(
    root: Path, environment: dict[str, str]
) -> None:
    """Force the documented LSP symbol fallback with a bounded local overflow."""

    init_repository(root, environment)
    caller = root / "large-symbols.rs"
    names = ["large_root", "nested_symbol", "flat_symbol"]
    names.extend(f"fixture_symbol_{index:04}" for index in range(4_094))
    caller.write_text(
        " ".join(f"pub fn {name}() {{}}" for name in names) + "\n",
        encoding="utf-8",
    )
    run("git", "add", caller.name, cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "large symbol fixture", cwd=root, environment=environment)

    _write_navigation_config(root, environment, _navigation_helper(), "pty-lsp")
    environment["LATTELENS_TEST_CALLER_URI"] = caller.resolve().as_uri()
    environment["LATTELENS_TEST_TARGET_URI"] = caller.resolve().as_uri()
    environment["LATTELENS_TEST_TRACE"] = str((root.parent / "symbol-trace.txt").resolve())
    environment["LATTELENS_TEST_RELEASE"] = str(
        (root.parent / "unused-definition-release").resolve()
    )


def create_crashing_lsp_fixture(root: Path, environment: dict[str, str]) -> None:
    init_repository(root, environment)
    caller = root / "crash-caller.rs"
    caller.write_text("caller!();\n", encoding="utf-8")
    run("git", "add", caller.name, cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "crash fixture", cwd=root, environment=environment)

    _write_navigation_config(root, environment, _navigation_helper(), "crash-initialize")
    environment["LATTELENS_TEST_TRACE"] = str((root.parent / "crash-trace.txt").resolve())


def _create_role_lsp_fixture(
    root: Path,
    environment: dict[str, str],
    *,
    role: str,
    trace_name: str,
) -> None:
    init_repository(root, environment)
    caller = root / "role-caller.rs"
    caller.write_text("caller!(); // 😀中\n", encoding="utf-8")
    run("git", "add", caller.name, cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", f"{role} fixture", cwd=root, environment=environment)
    _write_navigation_config(root, environment, _navigation_helper(), role)
    environment["LATTELENS_TEST_TRACE"] = str((root.parent / trace_name).resolve())


def create_incompatible_lsp_fixture(root: Path, environment: dict[str, str]) -> None:
    _create_role_lsp_fixture(
        root,
        environment,
        role="utf8-initialize",
        trace_name="utf8-trace.txt",
    )


def create_descendant_lsp_fixture(root: Path, environment: dict[str, str]) -> None:
    _create_role_lsp_fixture(
        root,
        environment,
        role="ready-descendant",
        trace_name="descendant-trace.txt",
    )


def create_timeout_lsp_fixture(root: Path, environment: dict[str, str]) -> None:
    _create_role_lsp_fixture(
        root,
        environment,
        role="timeout-navigation",
        trace_name="timeout-trace.txt",
    )


def create_batch_shutdown_lsp_fixture(
    root: Path, environment: dict[str, str]
) -> None:
    for repository_name in ("repo-a", "repo-b"):
        repository = root / repository_name
        repository.mkdir()
        init_repository(repository, environment)
        caller = repository / "caller.rs"
        caller.write_text(
            f"{repository_name.replace('-', '_')}!();\n", encoding="utf-8"
        )
        run("git", "add", caller.name, cwd=repository, environment=environment)
        run(
            "git",
            "commit",
            "-q",
            "-m",
            f"{repository_name} batch shutdown fixture",
            cwd=repository,
            environment=environment,
        )

    _write_navigation_config(root, environment, _navigation_helper(), "stalled-session-tree")
    environment["LATTELENS_TEST_TRACE"] = str(
        (root.parent / "batch-shutdown-trace.txt").resolve()
    )


def create_resilience_lsp_fixture(root: Path, environment: dict[str, str]) -> None:
    _create_role_lsp_fixture(
        root,
        environment,
        role="pty-resilience",
        trace_name="resilience-trace.txt",
    )
    environment["LATTELENS_TEST_LAUNCH_COUNT"] = str(
        (root.parent / "resilience-launch-count.txt").resolve()
    )


def create_default_lsp_fixture(root: Path, environment: dict[str, str]) -> None:
    init_repository(root, environment)
    caller = root / "basename-caller.rs"
    target = root / "basename-target.rs"
    caller.write_text("caller!();\n", encoding="utf-8")
    target.write_text("pub fn target() {}\n", encoding="utf-8")
    run("git", "add", caller.name, target.name, cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "basename fixture", cwd=root, environment=environment)

    helper = _navigation_helper()
    server_bin = root.parent / "default-server-bin"
    server_bin.mkdir(mode=0o700)
    server = server_bin / ("rust-analyzer.exe" if os.name == "nt" else "rust-analyzer")
    shutil.copy2(helper, server)
    if os.name != "nt":
        server.chmod(0o700)
    environment["PATH"] = _isolated_path_with_git(environment, server_bin)
    environment["LATTELENS_TEST_CALLER_URI"] = caller.resolve().as_uri()
    environment["LATTELENS_TEST_TARGET_URI"] = target.resolve().as_uri()
    environment["LATTELENS_TEST_TRACE"] = str((root.parent / "basename-trace.txt").resolve())
    environment["LATTELENS_TEST_RELEASE"] = str((root.parent / "basename-release").resolve())


def create_invalid_product_config_fixture(root: Path, environment: dict[str, str]) -> None:
    init_repository(root, environment)
    caller = root / "invalid-config.rs"
    caller.write_text("caller!();\n", encoding="utf-8")
    run("git", "add", caller.name, cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "invalid config fixture", cwd=root, environment=environment)
    config = (root.parent / "invalid-latte-lens.jsonc").resolve()
    config.write_text('{"code_navigation":', encoding="utf-8")
    environment["LATTELENS_CONFIG"] = str(config)


def create_code_navigation_without_lsp_fixture(
    root: Path, environment: dict[str, str]
) -> None:
    init_repository(root, environment)
    (root / "caller.rs").write_text("caller!();\n", encoding="utf-8")
    run("git", "add", "caller.rs", cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "no LSP fixture", cwd=root, environment=environment)
    empty_path = root.parent / "empty-path"
    empty_path.mkdir(mode=0o700)
    environment["PATH"] = _isolated_path_with_git(environment, empty_path)


def create_structure_fixture(root: Path, environment: dict[str, str]) -> None:
    """Create one compact file per supported folding/symbol language."""

    init_repository(root, environment)
    sources = {
        "a-structure.rs": """pub struct Alpha {
    value: i32,
}

impl Alpha {
    pub fn first(&self) {
        if self.value > 0 {
            println!(\"positive\");
        }
    }

    pub fn second(&self) {
        println!(\"second\");
    }
}

pub fn omega() {
    println!(\"omega\");
}
""",
        "b-structure.ts": """export class TypeScriptShape {
  method(): number {
    return 42;
  }
}

export function typescriptFunction(): string {
  return \"typescript\";
}
""",
        "c-structure.py": """class PythonShape:
    def method(self):
        return \"python\"


def python_function():
    return PythonShape()
""",
        "d-structure.go": """package fixture

type GoShape struct {
    Value int
}

func (shape GoShape) Method() int {
    return shape.Value
}

func GoFunction() GoShape {
    return GoShape{Value: 42}
}
""",
        "e-structure.md": """# Guide Root

root introduction

## Nested Topic

nested body marker

```rust
fn fenced_body() {}
```

# Final Topic

final body marker
""",
    }
    for name, content in sources.items():
        (root / name).write_text(content, encoding="utf-8")
    run("git", "add", ".", cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "structure fixture", cwd=root, environment=environment)


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


def create_repository_relation_fixture(root: Path, environment: dict[str, str]) -> None:
    """Create an offline submodule projection with placeholder and issue rows."""

    source = root.parent / "child-source"
    source.mkdir()
    init_repository(source, environment)
    (source / "tracked.txt").write_text("child initial\n", encoding="utf-8")
    run("git", "add", "tracked.txt", cwd=source, environment=environment)
    run("git", "commit", "-q", "-m", "child initial", cwd=source, environment=environment)

    init_repository(root, environment)
    run(
        "git",
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "--quiet",
        source.resolve().as_uri(),
        "modules/child",
        cwd=root,
        environment=environment,
    )
    modules = root / ".gitmodules"
    modules.write_text(
        modules.read_text(encoding="utf-8")
        + "\n[submodule \"missing\"]\n"
        + "\tpath = modules/missing\n"
        + "\turl = file:///latte-lens-e2e-missing\n"
        + "\n[submodule \"symlinked\"]\n"
        + "\tpath = modules/symlinked\n"
        + f"\turl = {source.resolve().as_uri()}\n",
        encoding="utf-8",
    )
    os.symlink(source.resolve(), root / "modules" / "symlinked")
    run("git", "add", ".", cwd=root, environment=environment)
    run("git", "commit", "-q", "-m", "declare local submodules", cwd=root, environment=environment)

    child = root / "modules" / "child"
    run("git", "config", "user.name", "Latte Lens E2E", cwd=child, environment=environment)
    run("git", "config", "user.email", "e2e@latte.invalid", cwd=child, environment=environment)
    (child / "tracked.txt").write_text("child advanced\n", encoding="utf-8")
    run("git", "add", "tracked.txt", cwd=child, environment=environment)
    run("git", "commit", "-q", "-m", "advance child", cwd=child, environment=environment)
    (child / "tracked.txt").write_text("child dirty\n", encoding="utf-8")
    (child / "untracked-child.txt").write_text("child untracked\n", encoding="utf-8")


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
        # The oracle must remain read-only itself. Without this, the first
        # `git status` may refresh the host checkout's index stat cache and the
        # next snapshot reports that self-induced write as an isolation leak.
        environment["GIT_OPTIONAL_LOCKS"] = "0"
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
            [
                "git",
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=no",
            ],
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
