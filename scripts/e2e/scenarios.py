"""Named production-binary journeys for completed Latte Lens features."""

from __future__ import annotations

import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from .fixtures import (
    ReadOnlyOracle,
    Sandbox,
    create_git_matrix_fixture,
    create_navigation_fixture,
    create_search_fixture,
)
from .terminal import E2EAssertionError, PtySession, TerminalScreen


FixtureBuilder = Callable[[Path, dict[str, str]], None]
Journey = Callable[["ScenarioContext"], None]


@dataclass(frozen=True)
class ScenarioCase:
    id: str
    group: str
    fixture: FixtureBuilder
    journey: Journey


@dataclass
class ScenarioContext:
    sandbox: Sandbox
    session: PtySession
    environment: dict[str, str]
    clipboard_mode: str
    oracle: ReadOnlyOracle

    @property
    def repository(self) -> Path:
        return self.sandbox.repository

    def write_text(self, relative: str, content: str) -> Path:
        path = self.repository / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        self.oracle.record_driver_write(path)
        return path

    def remove(self, relative: str) -> None:
        path = self.repository / relative
        path.unlink()
        self.oracle.record_driver_remove(path)


def wait_for_initial_files(session: PtySession) -> None:
    session.wait_raw((b"?1000h",), "initial mouse-enabled terminal")
    session.wait_screen(
        ("LATTE LENS", "1 Files", "2 Git changes", "Files", "Tree"),
        "initial all-files tree with tree focus",
        absent=(".git/",),
    )
    session.wait_until(
        lambda screen: (
            "Scanning files…" not in screen.text()
            and "Loading workspace…" not in screen.text()
            and (" loaded" in screen.text() or " entries" in screen.text())
        ),
        "initial filesystem and repository snapshot",
    )


def _click_disclosure(session: PtySession, marker: str) -> None:
    divider = _interior_divider(session.screen) or session.screen.columns
    for row, cells in enumerate(session.screen.cells):
        line = "".join(cells[:divider])
        marker_column = line.find(marker)
        if marker_column < 0:
            continue
        disclosure = next(
            (
                column
                for column in range(marker_column - 1, -1, -1)
                if cells[column] in "▾▸"
            ),
            None,
        )
        if disclosure is not None:
            session.click(disclosure, row)
            return
    raise E2EAssertionError("input_target", f"cannot find tree disclosure for: {marker}")


def _click_tree_row(session: PtySession, marker: str) -> None:
    divider = _interior_divider(session.screen) or session.screen.columns
    for row, cells in enumerate(session.screen.cells):
        line = "".join(cells[:divider])
        column = line.find(marker)
        if column >= 0:
            session.click(column, row)
            return
    raise E2EAssertionError("input_target", f"cannot find tree row for: {marker}")


def _interior_divider(screen: TerminalScreen) -> int | None:
    candidates = [
        column
        for column in screen.vertical_rule_columns()
        if 4 < column < screen.columns - 5
    ]
    return candidates[0] if candidates else None


def _divider_near(screen: TerminalScreen, expected: int, tolerance: int = 2) -> bool:
    return any(abs(column - expected) <= tolerance for column in screen.vertical_rule_columns())


def files_navigation(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    session.wait_screen(
        ("a-dir",),
        "initial collapsed all-files directory",
        absent=("b-changed.rs",),
    )

    # Discover the current semantic divider instead of encoding a layout/style
    # coordinate. This keeps resize behavior covered while the TUI evolves.
    session.wait_until(lambda screen: _interior_divider(screen) is not None, "tree divider visible")
    original = _interior_divider(session.screen)
    assert original is not None
    row = min(10, session.screen.rows - 3)
    expanded = min(original + 9, session.screen.columns - 10)
    session.drag(original, row, expanded, row)
    session.wait_until(
        lambda screen: _divider_near(screen, expanded), "expanded tree divider"
    )
    current = _interior_divider(session.screen)
    assert current is not None
    session.drag(current, row, 0, row)
    session.wait_until(
        lambda screen: (_interior_divider(screen) or original) < original,
        "minimum tree divider constraint",
    )
    current = _interior_divider(session.screen)
    assert current is not None
    session.drag(current, row, session.screen.columns - 1, row)
    session.wait_until(
        lambda screen: (_interior_divider(screen) or 0) > original,
        "minimum content divider constraint",
    )
    current = _interior_divider(session.screen)
    assert current is not None
    session.drag(current, row, original, row)
    session.wait_until(
        lambda screen: _divider_near(screen, original), "restored tree divider"
    )

    session.key(b"h")
    session.key(b"\r")
    session.wait_screen(
        ("▾ a-dir", "nested"), "keyboard-opened collapsed All Files directory"
    )
    _click_disclosure(session, "a-dir")
    session.wait_screen(
        ("a-dir", "Tree"),
        "mouse-closed directory with one click",
        absent=("nested", "b-changed.rs"),
    )
    _click_disclosure(session, "a-dir")
    session.wait_screen(("a-dir", "nested"), "mouse-opened directory with one click")
    session.key(b"\r")
    session.wait_screen(
        ("a-dir",),
        "keyboard-closed directory",
        absent=("nested", "b-changed.rs"),
    )

    # Preserve the original cross-scope state assertion in the Files group.
    context.write_text("y-untracked.txt", "new file\n")
    session.key(b"2")
    session.wait_screen(
        ("Git changes", "b-changed.rs", "nested-owned.txt", "Diff", "diff --git"),
        "Git Changes available for Files scope-state check",
    )
    session.key(b"l")
    session.wait_screen(("Diff", "Content"), "Files journey content focus cue")
    session.key(b"h")
    session.key(b"\x1b[H")
    session.key(b"\x1b[A")
    session.wait_screen(("Git changes", "Tabs"), "Files journey scope-tabs focus cue")
    files_tab = session.screen.find("1 Files")
    if files_tab is None:
        raise E2EAssertionError("input_target", "Files scope tab is not visible")
    session.click(*files_tab)
    session.wait_screen(
        ("Files", "a-dir", "Tree"),
        "mouse-selected collapsed All Files scope",
        absent=("nested", "b-changed.rs"),
    )
    _click_tree_row(session, "z-clean.rs")
    session.wait_screen(
        ("Preview", "clean()", "Tree"), "mouse-selected All Files preview"
    )

    copy_position = session.screen.find("clean()")
    if copy_position is None:
        raise E2EAssertionError("input_target", "clean() preview text is not visible")
    copy_column, copy_row = copy_position
    copy_start = len(session.output)
    session.drag(copy_column, copy_row, copy_column + 4, copy_row)
    expected_status = (
        "Copied 5 characters"
        if context.clipboard_mode == "native"
        else "Sent 5 characters to terminal clipboard"
    )
    session.wait_screen((expected_status,), "mouse-release Preview clipboard copy")
    if context.clipboard_mode == "native":
        clipboard = subprocess.run(["pbpaste"], check=True, capture_output=True).stdout
        if clipboard != b"clean":
            raise E2EAssertionError(
                "clipboard", f"native clipboard mismatch: expected b'clean', got {clipboard!r}"
            )
    else:
        session.wait_raw(
            (b"\x1b]52;c;Y2xlYW4=\x07",),
            "exact Preview OSC 52 clipboard payload",
            start=copy_start,
        )
    session.key(b"\x03")
    session.wait_screen((expected_status,), "Ctrl+C Preview clipboard recopy")
    session.scroll_down(min(session.screen.columns - 5, copy_column + 20), copy_row)


def files_refresh(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    _click_tree_row(session, "z-clean.rs")
    session.wait_screen(("Preview", "clean()"), "refresh baseline selection preview")
    context.write_text("refresh-added.txt", "refresh content\n")
    session.key(b"r")
    session.wait_screen(
        ("refresh-added.txt", "clean()"),
        "Files refresh adds a file and preserves selected preview",
    )
    context.remove("refresh-added.txt")
    session.key(b"r")
    session.wait_screen(
        ("clean()",),
        "Files refresh removes a file and preserves selected preview",
        absent=("refresh-added.txt",),
    )


def git_navigation(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    context.write_text("y-untracked.txt", "new file\n")
    session.key(b"2")
    session.wait_screen(
        (
            "Git changes",
            "a-dir",
            "nested",
            "b-changed.rs",
            "vendor/nested",
            "nested-owned.txt",
            "Diff",
            "diff --git",
            "changed()",
        ),
        "refreshed Git Changes tree with expanded ancestors and diff",
    )

    session.key(b"\x1b[H")
    session.key(b"\r")
    session.wait_screen(
        ("." ,),
        "keyboard-collapsed repository group",
        absent=("b-changed.rs", "nested-owned.txt"),
    )
    session.key(b"\r")
    session.wait_screen(
        ("b-changed.rs", "nested-owned.txt"), "keyboard-reopened repository group"
    )
    _click_disclosure(session, ".")
    session.wait_screen(
        ("Git changes",),
        "mouse-collapsed repository group",
        absent=("b-changed.rs", "nested-owned.txt"),
    )
    _click_disclosure(session, ".")
    session.wait_screen(
        ("vendor/nested", "nested-owned.txt"), "mouse-reopened repository group"
    )

    session.key(b"\x1b[F")
    session.wait_screen(
        ("nested-owned.txt", "+nested changed"),
        "nested repository diff routing",
        absent=("+pub fn changed()",),
    )
    context.write_text("vendor/nested/second.txt", "second nested file\n")
    session.key(b"r")
    session.wait_screen(
        ("second.txt", "+nested changed"),
        "repository refresh with stable owning selection",
    )
    session.key(b"l")
    session.wait_screen(("Diff", "Content"), "visible content focus cue")
    session.key(b"h")
    session.key(b"\x1b[H")
    session.key(b"\x1b[A")
    session.wait_screen(("Git changes", "Tabs"), "visible scope-tabs focus cue")


def _line_with(screen: TerminalScreen, marker: str) -> str:
    divider = _interior_divider(screen) or screen.columns
    for cells in screen.cells:
        line = "".join(cells[:divider]).rstrip()
        if marker in line:
            return line
    return ""


def git_status_matrix(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    session.key(b"2")
    session.wait_screen(
        (
            "staged.txt",
            "worktree.txt",
            "both.txt",
            "deleted.txt",
            "renamed.txt",
            "untracked.txt",
            "Diff",
        ),
        "representative staged/worktree/both/deleted/rename/untracked status matrix",
    )
    expected_statuses = {
        "staged.txt": "ᴍ",
        "worktree.txt": "ᴍ",
        "both.txt": "ᴍᴍ",
        "deleted.txt": "D",
        "renamed.txt": "R",
        "untracked.txt": "??",
    }
    for marker, status in expected_statuses.items():
        line = _line_with(session.screen, marker)
        if status not in line:
            raise E2EAssertionError(
                "status_matrix", f"expected {status!r} on {marker!r} row, got {line!r}"
            )
        session.assertions.append(f"{marker} exposes {status} status")

    _click_tree_row(session, "worktree.txt")
    session.wait_screen(
        ("worktree.txt", "+worktree after"), "status matrix selects a worktree-only diff"
    )
    _click_tree_row(session, "both.txt")
    session.wait_screen(
        ("both.txt", "+both staged", "+both worktree"),
        "status matrix selects owning diff",
        absent=("LOADING",),
    )
    session.key(b"\x06")
    session.wait_screen(("Find", "0/0"), "in-diff find opens")
    session.key(b"both")
    session.wait_screen(("Find", "/"), "in-diff find accepts a query", absent=("0/0",))
    session.key(b"\x1b")
    session.wait_screen(("Diff", "both.txt"), "in-diff find closes back to diff", absent=(" Find ",))
    session.key(b"p")
    session.wait_screen(("Preview", "both worktree"), "changed file switches from Diff to Preview")
    session.key(b"d")
    session.wait_screen(("Diff", "diff --git"), "changed file switches back to Diff")


def search_preview(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    session.key(b"/")
    session.wait_screen(("Open File", "File", "Text"), "file search opens")
    session.key(b"search-target")
    session.wait_screen(
        ("search-target.rs", "results"), "file search returns the matching path"
    )
    session.key(b"\r")
    session.wait_screen(
        ("Preview", "searchable()"), "file search opens selected preview", absent=("Open File",)
    )

    session.key(b"\x06")
    session.wait_screen(("Find", "0/0"), "Preview find opens")
    session.key(b"searchable")
    session.wait_screen(("Find", "1/1"), "Preview find locates current content")
    session.key(b"\r")
    session.wait_screen(("1/1",), "Preview find next wraps predictably")
    session.key(b"\x1b")
    session.wait_screen(("Preview", "searchable()"), "Preview find closes", absent=(" Find ",))

    session.key(b"\x14")
    session.wait_screen(("Search Workspace", "Aa", "Word", ".*", "Ign"), "text search opens")
    session.key(b"unique_workspace_phrase")
    session.wait_screen(
        ("search-target.rs:2", "unique_workspace_phrase"),
        "text search streams a workspace match",
    )
    session.key(b"\r")
    session.wait_screen(
        ("Preview", "unique_workspace_phrase"),
        "text search opens the selected result",
        absent=("Search Workspace",),
    )

    session.key(b"\x14")
    session.wait_screen(("Search Workspace",), "text search session reopens")
    session.key(b"\x15")
    session.key(b"ignored_unique_phrase")
    session.wait_screen(("No matches",), "ignored text is excluded by default")
    session.key(b"\x1b[15~")
    session.wait_screen(
        ("hidden.txt:1", "ignored_unique_phrase"), "F5 includes ignored search results"
    )
    session.key(b"\x10")
    session.wait_screen(("Open File", "File", "Text"), "Ctrl+P switches to file search")
    session.key(b"\x14")
    session.wait_screen(
        ("Search Workspace", "ignored_unique_phrase"),
        "Ctrl+T restores the text-search session",
    )
    session.key(b"\x1b")
    session.wait_screen(
        ("Preview", "unique_workspace_phrase"),
        "Esc restores the pre-search Preview",
        absent=("Search Workspace",),
    )


CASES = (
    ScenarioCase("files-navigation", "files", create_navigation_fixture, files_navigation),
    ScenarioCase("files-refresh", "files", create_navigation_fixture, files_refresh),
    ScenarioCase("git-navigation", "git-changes", create_navigation_fixture, git_navigation),
    ScenarioCase("git-status-matrix", "git-changes", create_git_matrix_fixture, git_status_matrix),
    ScenarioCase("search-preview", "search-preview", create_search_fixture, search_preview),
)


def selected_cases(group: str) -> list[ScenarioCase]:
    if group == "all":
        return list(CASES)
    return [case for case in CASES if case.group == group]


def validate_clipboard_mode(mode: str) -> None:
    if mode not in ("native", "osc52"):
        raise ValueError("LATTELENS_E2E_CLIPBOARD must be native or osc52")
    if mode == "native" and sys.platform != "darwin":
        raise ValueError("native clipboard E2E is currently supported only on macOS")
