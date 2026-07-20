"""Named production-binary journeys for completed Latte Lens features."""

from __future__ import annotations

import os
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from .fixtures import (
    ReadOnlyOracle,
    Sandbox,
    create_default_lsp_fixture,
    create_batch_shutdown_lsp_fixture,
    create_code_navigation_fixture,
    create_code_navigation_without_lsp_fixture,
    create_crashing_lsp_fixture,
    create_descendant_lsp_fixture,
    create_directory_product_config_fixture,
    create_disabled_product_config_fixture,
    create_fold_mouse_navigation_fixture,
    create_git_matrix_fixture,
    create_incompatible_lsp_fixture,
    create_invalid_product_config_fixture,
    create_lsp_document_symbol_fixture,
    create_missing_product_config_fixture,
    create_navigation_fixture,
    create_repository_relation_fixture,
    create_resilience_lsp_fixture,
    create_search_fixture,
    create_symlink_preview_fixture,
    create_structure_fixture,
    create_timeout_lsp_fixture,
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
    session.drain()
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
    session.drain()
    divider = _interior_divider(session.screen) or session.screen.columns
    for row, cells in enumerate(session.screen.cells):
        line = "".join(cells[:divider])
        column = line.find(marker)
        if column >= 0:
            session.click(column, row)
            return
    raise E2EAssertionError("input_target", f"cannot find tree row for: {marker}")


def _click_marker_on_line(
    session: PtySession, marker: str, *, alongside: tuple[str, ...] = ()
) -> None:
    session.click(*_marker_position_on_line(session, marker, alongside=alongside))


def _marker_position_on_line(
    session: PtySession, marker: str, *, alongside: tuple[str, ...] = ()
) -> tuple[int, int]:
    session.drain()
    for row, cells in enumerate(session.screen.cells):
        line = "".join(cells)
        if marker in line and all(companion in line for companion in alongside):
            return line.index(marker), row
    raise E2EAssertionError(
        "input_target",
        f"cannot find {marker!r} alongside {alongside!r}",
    )


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


def symlink_preview_smoke(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    session.wait_screen(
        (
            "a-directory-link",
            "Preview",
            "../linked-repositories/sample-framework",
        ),
        "production binary previews a sandboxed directory symlink target",
        absent=("Preview unavailable", "target-content-must-not-be-read"),
    )
    _click_tree_row(session, "b-file-link.txt")
    session.wait_screen(
        ("b-file-link.txt", "Preview", "../linked-files/sample.txt"),
        "production binary previews a sandboxed file symlink target",
        absent=("Preview unavailable", "file-target-content-must-not-be-read"),
    )


def keyboard_controls(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)

    # Move through the explicit Tabs -> Tree -> Content focus model and both
    # scope-tab arrow branches before exercising the global tab shortcuts.
    session.key(b"\x1b[A")
    session.wait_screen(("Tabs",), "Up from the first tree row focuses scope tabs")
    session.key(b"\x1b[C")
    session.wait_screen(("Git changes", "b-changed.rs", "Diff"), "Right tab selects Git Changes")
    session.key(b"\x1b[D")
    session.wait_screen(("Files", "a-dir", "Tabs"), "Left tab returns to Files")
    session.key(b"\x1b[B")
    session.wait_screen(("Tree",), "Down from tabs returns focus to the tree")
    session.key(b"\t")
    session.wait_screen(("Git changes", "Diff"), "Tab advances to Git Changes")
    session.key(b"\x1b[Z")
    session.wait_screen(("Files", "a-dir"), "BackTab returns to Files")

    # Tree navigation covers bounded first/last selection, activation, and
    # the transition into the content pane.
    session.key(b"G")
    session.wait_screen(("z-clean.rs", "Preview", "clean()"), "tree End selects the last file")
    session.key(b"\x1b[C")
    session.wait_screen(("Content",), "Right moves focus into content")
    for key in (
        b"j",
        b"k",
        b"\x1b[6~",
        b"\x1b[5~",
        b"\x04",
        b"\x15",
        b"\x1b[1;2C",
        b"\x1b[1;2D",
        b"G",
        b"g",
        b"\x1b[C",
    ):
        session.key(key)
    session.key(b"\x1b[D")
    session.key(b"g")
    session.key(b"\r")
    session.wait_screen(("▾ a-dir", "nested"), "tree Home and Enter expand the first directory")
    _click_disclosure(session, "nested")
    session.wait_screen(
        ("b-changed.rs",),
        "nested directory loading makes the changed file searchable",
    )

    # Wheel-up is separate from wheel-down in Crossterm and must route to the
    # pointed pane, independent of the current keyboard focus.
    tree_marker = session.screen.find("a-dir")
    if tree_marker is None:
        raise E2EAssertionError("input_target", "tree row is not visible for wheel routing")
    session.scroll_up(*tree_marker)
    content_column = min(session.screen.columns - 2, (_interior_divider(session.screen) or 40) + 8)
    session.scroll_up(content_column, min(10, session.screen.rows - 3))

    # Loading a diff from All Files must resolve the owning repository instead
    # of assuming the workspace root. Cycling here covers the All Files branch
    # of changed-file navigation before the Git Changes projection takes over.
    session.key(b"/")
    session.key(b"b-changed")
    session.wait_screen(("b-changed.rs", "Open File"), "file search finds the changed file")
    session.key(b"\r")
    session.wait_screen(("Preview", "changed()"), "file search opens the changed preview")
    session.key(b"d")
    session.wait_screen(("Diff", "diff --git"), "All Files resolves the owning repository diff")
    session.key(b"n")
    session.wait_screen(("Diff", "diff --git"), "All Files cycles to its next changed entry")
    session.key(b"N")
    session.wait_screen(("Diff", "diff --git"), "All Files cycles to its previous changed entry")

    # Changed-file navigation runs in both directions only while Diff owns the
    # content pane. Preview/Diff toggles then return to the same owning change.
    session.key(b"2")
    session.wait_screen(("Git changes", "Diff", "diff --git"), "Git Changes is ready for change cycling")
    session.wait_until(
        lambda screen: all(
            marker not in screen.text()
            for marker in ("Refreshing workspace", "Loading directory", "Loading content", "LOADING")
        )
        and "a-dir" in screen.text(),
        "Git Changes refresh settles before directory interaction",
    )
    session.key(b"\x1b[D")
    session.key(b"\x1b[H")
    session.key(b"\x1b[B")
    session.wait_screen(
        ("changed file", "directory"),
        "keyboard selects the Git directory and renders its aggregate information",
    )
    session.key(b"\r")
    session.wait_screen(("b-changed.rs",), "Git directory reopens after its information view")
    _click_tree_row(session, "b-changed.rs")
    session.wait_screen(("Diff", "diff --git"), "Git changed-file selection restores the diff")
    session.key(b"n")
    session.wait_screen(("Diff", "diff --git"), "n selects the next changed file")
    session.key(b"N")
    session.wait_screen(("Diff", "diff --git"), "N selects the previous changed file")
    session.key(b"p")
    session.wait_screen(("Preview",), "p loads the changed file preview")
    session.key(b"d")
    session.wait_screen(("Diff", "diff --git"), "d restores the owning diff")
    session.click_marker("r  Refresh")
    session.wait_screen(("Git changes", "Diff"), "mouse refresh preserves Git Changes")
    session.key(b"1")
    session.wait_screen(("Files", "a-dir"), "1 switches back to the complete file tree")
    session.key(b"\x1b")
    session.wait_screen(("Press Esc again to quit",), "Esc requests quit confirmation")


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


def git_review_state(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    session.key(b"2")
    session.wait_until(
        lambda screen: all(
            marker in _line_with(screen, "worktree.txt")
            for marker in ("○", "+1", "-1")
        ),
        "worktree diff row exposes unreviewed state and line counts",
    )

    _click_tree_row(session, "worktree.txt")
    session.wait_screen(
        ("worktree.txt", "+worktree after", "Space review"),
        "reviewable worktree diff is fully loaded",
    )
    session.key(b" ")
    session.wait_until(
        lambda screen: "✓" in _line_with(screen, "worktree.txt")
        and "1/6 reviewed" in screen.text(),
        "Space marks the current diff version reviewed",
    )

    context.write_text("worktree.txt", "worktree after changed\nsecond line\n")
    session.key(b"r")
    session.wait_until(
        lambda screen: all(
            marker in _line_with(screen, "worktree.txt")
            for marker in ("↻", "+2", "-1")
        )
        and "1 changed" in screen.text()
        and "+worktree after changed" in screen.text(),
        "refresh marks a reviewed file stale and loads its updated diff and line counts",
    )
    session.key(b" ")
    session.wait_until(
        lambda screen: "✓" in _line_with(screen, "worktree.txt")
        and "1/6 reviewed" in screen.text()
        and "1 changed" not in screen.text(),
        "Space reviews the refreshed diff version",
    )


def repository_relation_matrix(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    session.key(b"2")
    session.wait_screen(
        (
            "Git changes",
            "(submodule pointer)",
            "modules/child",
            "tracked.txt",
            "untracked-child.txt",
            "modules/missing",
            "modules/symlinked",
            "diff --git a/modules/child b/modules/child",
        ),
        "projected submodule, placeholder, issue, and pointer diff are visible",
        absent=("Refreshing workspace",),
    )

    child_line = _line_with(session.screen, "modules/child")
    if "submodule" not in child_line or not child_line.rstrip().endswith("3"):
        raise E2EAssertionError(
            "repository_relation", f"child relation/count row is malformed: {child_line!r}"
        )
    session.assertions.append("child repository projects pointer, tracked, and untracked changes")

    issue_line = _line_with(session.screen, "modules/symlinked")
    if "[error]" not in issue_line:
        raise E2EAssertionError(
            "repository_relation", f"declared symlink issue row is malformed: {issue_line!r}"
        )
    session.assertions.append("declared submodule symlink remains an explicit issue row")

    session.key(b"p")
    session.wait_screen(
        ("A submodule pointer has no file preview.", "Press d to inspect"),
        "pointer Preview is intentionally unavailable",
    )
    session.key(b"d")
    session.wait_screen(
        ("Submodule pointer", "diff --git a/modules/child b/modules/child"),
        "pointer Diff returns to the parent Gitlink",
    )

    _click_tree_row(session, "modules/missing")
    session.wait_screen(
        ("submodule placeholder repository", "uninitialized", "clean"),
        "undeployed submodule keeps its explicit placeholder details",
    )

    _click_tree_row(session, "modules/symlinked")
    session.wait_screen(
        ("Repository error", "refusing to follow a submodule symlink"),
        "repository issue selection explains the symlink boundary",
    )

    _click_tree_row(session, "modules/child")
    session.wait_screen(
        ("submodule repository", "pointer changed", "internal modified", "internal untracked"),
        "child relation details survive deliberate collapse",
        absent=("tracked.txt", "untracked-child.txt"),
    )
    _click_tree_row(session, "modules/child")
    session.wait_screen(
        ("tracked.txt", "untracked-child.txt"),
        "child repository reopens with both internal changes",
    )
    session.key(b"r")
    session.wait_until(
        lambda screen: (
            "Refreshing workspace" not in screen.text()
            and "tracked.txt" in screen.text()
            and "untracked-child.txt" in screen.text()
            and "▌" in _line_with(screen, "modules/child")
            and "pointer changed" in screen.text()
            and "internal untracked" in screen.text()
        ),
        "refresh preserves expanded child selection and relation state",
    )


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
    session.key(b"l")
    session.key(b"{")
    session.wait_screen(
        ("Preview", "▸", "3 lines"),
        "Preview semantic fold collapses from the content pane",
        absent=("folded_value",),
    )

    session.key(b"\x06")
    session.wait_screen(("Find", "0/0"), "Preview find opens")
    session.key(b"searchable")
    session.wait_screen(("Find", "1/1"), "Preview find locates current content")
    session.key(b"\r")
    session.wait_screen(("1/1",), "Preview find next wraps predictably")
    session.key(b"\x1b")
    session.wait_screen(("Preview", "searchable()"), "Preview find closes", absent=(" Find ",))

    session.key(b"\x06")
    session.wait_screen(("Find", "0/0"), "Preview body find opens")
    session.key(b"folded_value")
    session.wait_screen(
        ("Find", "folded_value", "let folded_value = 1"),
        "Preview find expands every folded ancestor of a body match",
    )
    session.key(b"\x1b")
    session.wait_screen(
        ("Preview", "folded_value"),
        "Preview body find closes after revealing the match",
        absent=(" Find ",),
    )

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


def search_controls(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)

    # Ctrl+F on directory info must explain why find is unavailable instead of
    # opening a control that cannot search the current content mode.
    session.key(b"\x06")
    session.wait_screen(
        ("Open a file preview or diff before using Ctrl+F",),
        "Preview find explains its content precondition",
    )

    # Open the file picker through its real mouse hit box, exercise every query
    # editing/navigation branch, then open a production preview by double click.
    session.click_marker("/ Open")
    session.wait_screen(("Open File", "File", "Text"), "mouse opens file search")
    session.key(b"alpha beta")
    session.key(b"\x1b[H")
    session.key(b"\x1b[C")
    session.key(b"\x1b[3~")
    session.key(b"\x1b[F")
    session.key(b"\x1b[D")
    session.key(b"\x7f")
    session.key(b"\x17")
    session.key(b"\x15")
    session.wait_screen(("Type a file name or path",), "file query editing clears cleanly")
    session.key(b"search-target")
    session.wait_screen(
        ("search-target-other.rs", "search-target.rs", "results"),
        "edited file query converges",
    )
    for key in (b"\x1b[B", b"\x1b[A", b"\x1b[6~", b"\x1b[5~"):
        session.key(key)
    session.wait_screen(
        ("· src/search-target.rs",),
        "file-search navigation keeps the result selected",
    )
    # Move away and back through application state before locating the target.
    # Converge on both the selected row and its asynchronous Preview: coverage
    # instrumentation can otherwise leave a stale row coordinate pointing at
    # the other result while the content request catches up. Sending the two
    # target clicks in one terminal batch then exercises the real double-click
    # path without a wall-clock delay between driver actions.
    session.key(b"\x1b[B")
    session.wait_screen(
        ("▌ · src/search-target-other.rs",),
        "file-search selection moves away from the double-click target",
    )
    session.key(b"\x1b[A")
    session.wait_screen(
        ("Open File", "▌ · src/search-target.rs", "searchable()"),
        "target row and asynchronous Preview converge before double click",
    )
    if context.environment.get("CARGO_LLVM_COV") == "1":
        # Coverage instrumentation can make the redraw between two separately
        # observed clicks exceed the product's 400 ms double-click window. The
        # regular Linux/macOS E2E jobs exercise that wall-clock interaction;
        # the coverage runner uses the equivalent acceptance path after proving
        # the production mouse hit box selected this result.
        session.key(b"\r")
    else:
        file_result = _marker_position_on_line(session, "· src/search-target.rs")
        session.double_click(*file_result)
    session.wait_screen(
        ("Preview", "searchable()"),
        "the selected file-search result is accepted",
        absent=("Open File",),
    )

    # Preview find keyboard and mouse controls share the production hit boxes.
    session.key(b"\x06")
    session.key(b"searchable")
    session.wait_screen(("Find", "1/1"), "Preview find has one result")
    session.key(b"\x1b[12~")
    session.key(b"\x1b[A")
    session.key(b"\x1b[B")
    session.key(b"\x1b[H")
    session.key(b"\x1b[C")
    session.key(b"\x1b[3~")
    session.key(b"\x1b[F")
    session.key(b"\x1b[D")
    session.key(b"\x7f")
    session.key(b"\x17")
    session.key(b"\x15")
    session.wait_screen(("Find", "0/0"), "Preview find editing reaches an empty query")
    session.key(b"searchable")
    session.wait_screen(("Find", "1/1"), "Preview find query is restored")
    _click_marker_on_line(session, "Aa", alongside=("Find", "1/1"))
    _click_marker_on_line(session, "↑", alongside=("Find", "1/1"))
    _click_marker_on_line(session, "↓", alongside=("Find", "1/1"))
    _click_marker_on_line(session, "Find", alongside=("1/1",))
    _click_marker_on_line(session, "[x]", alongside=("Find", "1/1"))
    session.wait_screen(
        ("Preview", "searchable()"),
        "Preview find closes through its mouse control",
        absent=(" Find ",),
    )

    # Handoff from Preview find to workspace search, then cover every text
    # option through both keyboard and mouse controls.
    session.key(b"\x06")
    session.key(b"\x14")
    session.wait_screen(("Search Workspace", "File", "Text"), "find hands off to text search")
    search_modes = _marker_position_on_line(session, "File Text")
    session.click(*search_modes)
    session.wait_screen(("Open File",), "mouse switches search to files")
    search_modes = _marker_position_on_line(session, "File Text")
    session.click(search_modes[0] + len("File "), search_modes[1])
    session.wait_screen(("Search Workspace",), "mouse switches search back to text")
    session.key(b"unique_workspace_phrase")
    session.wait_screen(
        ("search-target.rs:2", "unique_workspace_phrase"),
        "workspace query is ready for option controls",
    )
    for key in (b"\x1b[12~", b"\x1b[13~", b"\x1b[14~"):
        session.key(key)
    session.wait_screen(
        ("search-target.rs:2", "unique_workspace_phrase"),
        "case, word, and regex keyboard options preserve the exact match",
    )
    for marker in ("Aa", "Word", ".*", "Ign"):
        _click_marker_on_line(session, marker, alongside=("File", "Text", "Aa", "Word", ".*", "Ign"))
    session.wait_screen(
        ("search-target.rs:2", "unique_workspace_phrase"),
        "mouse search options preserve the result",
    )
    _click_marker_on_line(
        session,
        "unique_workspace_phrase",
        alongside=("Clear",),
    )
    result = session.screen.find("search-target.rs:2")
    if result is None:
        raise E2EAssertionError("input_target", "text-search result is not visible")
    session.scroll_down(*result)
    _click_marker_on_line(session, "Clear ×")
    session.wait_screen(("Type text to search the workspace",), "mouse clears text search")
    session.key(b"ignored_unique_phrase")
    session.wait_screen(
        ("hidden.txt:1", "ignored_unique_phrase"),
        "mouse-enabled ignored search finds the hidden fixture",
    )
    session.key(b"\r")
    session.wait_screen(
        ("Preview", "ignored_unique_phrase"),
        "Enter accepts a text-search result",
        absent=("Search Workspace",),
    )

    # Inactive header buttons must remain mouse-addressable after a search
    # session has restored its preview.
    session.click_marker("^T Text")
    session.wait_screen(("Search Workspace",), "mouse reopens text search from the header")
    _click_marker_on_line(session, "Esc ×")
    session.wait_screen(("Preview", "ignored_unique_phrase"), "mouse closes text search")


def _content_token_position(session: PtySession, marker: str) -> tuple[int, int]:
    divider = _interior_divider(session.screen)
    if divider is None:
        raise E2EAssertionError("input_target", "content divider is not visible")
    for row, cells in enumerate(session.screen.cells):
        line = "".join(cells[divider + 1 :])
        column = line.find(marker)
        if column >= 0:
            return divider + 1 + column, row
    raise E2EAssertionError("input_target", f"cannot find content token: {marker}")


def _content_fold_marker_position(session: PtySession) -> tuple[int, int]:
    divider = _interior_divider(session.screen)
    if divider is None:
        raise E2EAssertionError("input_target", "content divider is not visible")
    for row, cells in enumerate(session.screen.cells):
        for column in range(divider + 1, len(cells)):
            if cells[column] in "▾▸":
                return column, row
    raise E2EAssertionError("input_target", "no content fold marker is visible")


def _wait_trace(
    context: ScenarioContext,
    markers: tuple[str, ...],
    label: str,
    *,
    timeout: float = 10.0,
) -> str:
    trace = Path(context.environment["LATTELENS_TEST_TRACE"])
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        text = trace.read_text(encoding="utf-8") if trace.exists() else ""
        if all(marker in text for marker in markers):
            context.session.assertions.append(label)
            return text
        if context.session.process.poll() is not None:
            break
        time.sleep(0.02)
    text = trace.read_text(encoding="utf-8") if trace.exists() else ""
    raise E2EAssertionError(
        "helper_receipt",
        f"{label} is missing {markers!r}; helper trace={text!r}",
    )


def _drain_for(session: PtySession, duration: float, label: str) -> None:
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        session.drain()
        if session.process.poll() is not None:
            raise E2EAssertionError("child_exit", f"{label}: TUI exited unexpectedly")
        time.sleep(0.02)
    session.drain()
    session.assertions.append(label)


def _assert_process_gone(session: PtySession, pid: int, label: str) -> None:
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        session.drain()
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            session.assertions.append(label)
            return
        except PermissionError as error:
            raise E2EAssertionError("helper_receipt", f"cannot inspect helper pid {pid}: {error}")
        time.sleep(0.02)
    raise E2EAssertionError("helper_receipt", f"helper pid {pid} was not killed and reaped")


def code_navigation(context: ScenarioContext) -> None:
    session = context.session
    session.wait_raw((b"?1000h",), "navigation terminal enables mouse capture")
    session.wait_screen(
        ("LATTE LENS", "2 entries", "Preview", "caller!"),
        "navigation fixture and caller Preview load",
        absent=("LOADING", "Loading content"),
    )

    session.key(b"l")
    session.key(b"\x04")
    session.wait_screen(
        ("b-target.rs", "pub fn"),
        "Ctrl-D definition commits the cross-file target",
        absent=("Navigation target is invalid",),
    )
    server_receipts = tuple(
        f"server-call-ok={call}"
        for call in (
            "config-valid",
            "config-invalid",
            "folders-valid",
            "folders-absent",
            "folders-invalid",
            "apply-edit",
            "register",
            "unregister",
            "progress",
            "progress-negative",
            "progress-positive",
            "message",
            "unknown",
        )
    )
    _wait_trace(
        context,
        ("helper-started=", "initialized-received", "did-open", *server_receipts),
        "configured helper validates bounded server-call replies",
    )

    session.key(b"\x1b[1;3D")
    session.wait_screen(("a-caller.rs", "caller!"), "definition history returns to caller")
    session.key(b"\x0f")
    session.wait_screen(
        ("implementation fixture error", "caller!"),
        "Ctrl-O surfaces a bounded language-server error without moving",
    )
    session.key(b"\x0f")
    session.wait_screen(
        ("No implementations found.", "caller!"),
        "Ctrl-O handles a valid null implementation result",
    )
    session.key(b"\x0f")
    session.wait_screen(
        ("Implementations", "b-target.rs"),
        "Ctrl-O opens a picker even for one implementation",
    )
    session.key(b"\r")
    session.wait_screen(
        ("b-target.rs", "pub fn"),
        "Enter commits the implementation target on the reused session",
        absent=("Implementations",),
    )

    session.key(b"\x12")
    session.wait_screen(
        ("References", "a-caller.rs", "b-target.rs"),
        "Ctrl-R opens the multi-result picker",
    )
    session.key(b"\x1b[A")
    session.key(b"\r")
    session.key(b"\r")
    session.key(b"\x1b[B")
    _drain_for(
        session,
        0.2,
        "reference file group collapses, expands, and returns selection to its first result",
    )
    session.key(b"\r")
    session.wait_screen(
        ("a-caller.rs", "caller!"),
        "picker commits the first reference",
        absent=("References",),
    )
    session.key(b"\x1b[1;3D")
    session.wait_screen(("b-target.rs", "pub fn"), "Alt-Left returns through navigation history")
    session.key(b"\x1b[1;3C")
    session.wait_screen(("a-caller.rs", "caller!"), "Alt-Right moves forward through history")

    session.key(b"\x04")
    _wait_trace(context, ("definition-held",), "second definition is held for cancellation")
    session.wait_screen(("Finding definition…", "caller!"), "held definition stays responsive")
    session.key(b"\x1b")
    Path(context.environment["LATTELENS_TEST_RELEASE"]).touch()
    trace = _wait_trace(
        context,
        ("cancel-received", "definition-2"),
        "Esc cancellation reaches the reused helper session",
    )
    if trace.count("helper-started=") != 1:
        raise E2EAssertionError(
            "helper_receipt",
            f"expected one reused helper process, got trace {trace!r}",
        )
    session.assertions.append("one configured helper process serves every navigation request")
    session.wait_screen(
        ("a-caller.rs", "caller!"),
        "late cancelled definition response cannot move the current Preview",
        absent=("Finding definition…",),
    )
    session.key(b"\x04")
    session.wait_screen(
        ("b-target.rs", "pub fn"),
        "a request after cancellation succeeds on the same session",
    )

    session.key(b"\x04")
    session.wait_screen(
        ("No definition found.", "b-target.rs"),
        "a null definition result keeps the current Preview",
    )
    session.key(b"\x04")
    session.wait_screen(
        ("Navigation target opened.", "b-target.rs"),
        "a LocationLink definition resolves inside the current Preview",
    )
    session.key(b"\x04")
    session.wait_screen(
        ("Dependency Source", "dependency_target", "Navigation target opened."),
        "a recognized external dependency definition opens a temporary read-only source view",
    )
    session.key(b"\x1b[1;3D")
    session.wait_screen(
        ("Preview", "b-target.rs"),
        "dependency source history returns to the workspace Preview",
        absent=("Dependency Source",),
    )
    session.key(b"\x04")
    session.wait_screen(
        ("Navigation target range is invalid.", "b-target.rs"),
        "an out-of-range same-document definition cannot move the Preview",
    )
    session.key(b"\x04")
    session.wait_screen(
        ("Definitions", "a-caller.rs", "b-target.rs"),
        "multiple definitions are sorted and deduplicated in a picker",
    )
    session.key(b"\r")
    session.wait_screen(
        ("a-caller.rs", "caller!", "Navigation target opened."),
        "definition picker commits its first safe target",
        absent=("Definitions",),
    )

    session.key(b"\x12")
    session.wait_screen(
        ("No references found.", "caller!"),
        "an empty reference array produces a bounded empty-result status",
    )
    session.key(b"\x12")
    session.wait_screen(
        ("References", "b-target.rs"),
        "a single reference still uses the explicit reference picker",
    )
    session.key(b"\x1b")
    session.wait_screen(
        ("a-caller.rs", "caller!"),
        "single-reference picker closes before the next semantic request",
        absent=("References",),
    )

    session.key(b"\x0f")
    session.wait_screen(
        ("Implementations", "a-caller.rs", "b-target.rs"),
        "multiple implementations use the implementation picker",
    )
    session.key(b"\r")
    session.wait_screen(
        ("a-caller.rs", "caller!", "Navigation target opened."),
        "implementation picker commits a safe current-document target",
        absent=("Implementations",),
    )
    session.key(b"\x12")
    session.wait_screen(
        ("invalid or mixed LSP Location/LocationLink item", "caller!"),
        "mixed reference variants fail and retire the configured session",
    )
    _wait_trace(
        context,
        ("definition-8", "references-4", "implementation-4"),
        "result-shape matrix reaches its terminal protocol rejection",
    )


def structure_navigation(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    _click_tree_row(session, "a-structure.rs")
    session.wait_screen(
        ("Preview", "pub struct Alpha", "pub fn omega"),
        "Rust structure Preview loads semantic declarations",
    )
    session.key(b"l")
    session.key(b"\x13")
    session.wait_screen(
        ("Document Symbols", "Alpha", "first", "second", "omega"),
        "Rust local document-symbol picker exposes nested declarations",
    )
    for key in (b"\x1b[B", b"\x1b[A", b"\x1b[6~", b"\x1b[5~"):
        session.key(key)
    omega = _marker_position_on_line(session, "omega")
    session.click(*omega)
    session.wait_screen(
        ("Preview", "Navigation target opened.", "pub fn omega"),
        "symbol picker click reveals the local declaration",
        absent=("Document Symbols",),
    )
    session.key(b"g")
    session.wait_screen(
        ("pub struct Alpha", "println!(\"positive\")", "println!(\"second\")"),
        "content Home returns the Rust structure viewport to the first declaration",
    )

    session.key(b"{")
    session.wait_screen(
        ("Preview", "▸", "lines"),
        "Rust collapse-all projects semantic fold summaries",
        absent=("println!(\"positive\")", "println!(\"second\")"),
    )
    session.key(b"}")
    session.wait_screen(
        ("println!(\"positive\")", "println!(\"second\")"),
        "Rust expand-all restores original source lines",
    )
    marker = _content_fold_marker_position(session)
    session.click(*marker)
    session.wait_screen(("Preview", "▸"), "fold gutter click collapses a semantic region")
    session.click(*marker)
    session.wait_screen(("Preview", "▾"), "fold gutter click reopens a semantic region")

    language_cases = (
        (
            "b-structure.ts",
            ("TypeScriptShape", "method", "typescriptFunction"),
            'return "typescript";',
        ),
        (
            "c-structure.py",
            ("PythonShape", "method", "python_function"),
            'return "python"',
        ),
        ("d-structure.go", ("GoShape", "Method", "GoFunction"), "return shape.Value"),
        ("e-structure.md", ("Guide Root", "Nested Topic", "Final Topic"), "nested body marker"),
    )
    for filename, symbols, body_marker in language_cases:
        session.key(b"/")
        session.key(b"\x15")
        session.key(filename.encode())
        session.wait_screen((filename,), f"file search finds {filename}")
        session.key(b"\r")
        session.wait_screen(
            ("Preview", body_marker),
            f"{filename} Preview loads before structure navigation",
            absent=("Open File",),
        )
        session.key(b"l")
        session.key(b"\x13")
        session.wait_screen(
            ("Document Symbols", *symbols),
            f"{filename} local document symbols are visible",
        )
        session.key(b"\x1b")
        session.wait_screen(
            ("Preview", body_marker),
            f"{filename} symbol picker closes",
            absent=("Document Symbols",),
        )
        session.key(b"{")
        session.wait_screen(
            ("Preview", "▸"),
            f"{filename} collapse-all exposes fold markers",
            absent=(body_marker,),
        )
        session.key(b"}")
        session.wait_screen(
            ("Preview", body_marker),
            f"{filename} expand-all restores source content",
        )


def fold_keyboard_and_mouse_definition(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(
        (
            "a-fold-caller.rs",
            "first_region",
            "first_marker_00",
            "Preview",
        ),
        "multiregion Rust Preview loads at the first fold",
        absent=("LOADING", "Loading content"),
    )
    session.key(b"l")
    session.key(b"]")
    session.wait_screen(
        ("middle_region_with_a_deliberately_long_anchor", "middle_target_call"),
        "first ] jumps to the second visible fold",
        absent=("first_region",),
    )
    session.key(b"]")
    session.wait_screen(
        ("third_region", "third_body_marker"),
        "second ] jumps to the third visible fold",
        absent=("middle_region_with_a_deliberately_long_anchor",),
    )
    session.key(b"[")
    session.wait_screen(
        ("middle_region_with_a_deliberately_long_anchor", "middle_body_tail"),
        "[ returns to the preceding visible fold",
        absent=("first_region",),
    )

    session.key(b"\r")
    session.wait_screen(
        ("middle_region_with_a_deliberately_long_anchor", "…", "lines"),
        "Enter collapses the current fold onto its synthetic summary row",
        absent=("middle_target_call", "middle_body_tail"),
    )
    trace_path = Path(context.environment["LATTELENS_TEST_TRACE"])
    summary_position = _content_token_position(session, "lines")
    session.alt_move(*summary_position)
    session.alt_click(*summary_position)
    _drain_for(session, 0.35, "Alt-click on a synthetic fold row remains locally inert")
    trace = trace_path.read_text(encoding="utf-8") if trace_path.exists() else ""
    if "definition-" in trace or "helper-started=" in trace:
        raise E2EAssertionError(
            "helper_receipt",
            f"synthetic fold row unexpectedly launched semantic navigation: {trace!r}",
        )
    session.assertions.append("synthetic collapsed row emits no definition request")
    session.wait_screen(
        ("a-fold-caller.rs", "middle_region_with_a_deliberately_long_anchor"),
        "synthetic-row Alt-click cannot navigate away",
    )

    session.key(b" ")
    session.wait_screen(
        ("middle_target_call", "middle_body_tail"),
        "Space expands the current fold and restores its real tokens",
        absent=("…",),
    )
    token_position = _content_token_position(session, "target_call")
    session.alt_move(*token_position)
    session.alt_click(*token_position)
    session.wait_screen(
        ("b-fold-target.rs", "target_destination", "Navigation target opened."),
        "Alt-hover and Alt-click on a real token perform definition navigation",
    )
    trace = _wait_trace(
        context,
        ("helper-started=", "definition-1"),
        "expanded token issues one configured definition request",
    )
    if trace.count("definition-") != 1:
        raise E2EAssertionError(
            "helper_receipt", f"expected exactly one definition request, got trace {trace!r}"
        )
    session.assertions.append("exactly one definition request follows the real-token Alt-click")


def lsp_document_symbols(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(
        ("LATTE LENS", "large-symbols.rs", "pub fn large_root"),
        "large Rust Preview loads before the LSP symbol fallback",
        absent=("LOADING", "Loading content"),
    )
    session.key(b"l")

    session.key(b"\x13")
    session.wait_screen(
        ("Document Symbols", "LargeRoot", "NestedSymbol"),
        "nested DocumentSymbol results populate the production picker",
    )
    _wait_trace(
        context,
        ("document-symbol-1", "did-open"),
        "incomplete local symbols fall back to the configured LSP session",
    )
    session.key(b"\x1b")
    session.wait_screen(
        ("Preview", "pub fn large_root"),
        "nested LSP symbol picker closes without changing the Preview",
        absent=("Document Symbols",),
    )

    session.key(b"\x13")
    session.wait_screen(
        ("Document Symbols", "FlatSymbol", "fixture"),
        "flat SymbolInformation results populate the production picker",
    )
    _wait_trace(context, ("document-symbol-2",), "flat LSP symbols are requested")
    session.key(b"\x1b")
    session.wait_screen(
        ("Preview", "pub fn large_root"),
        "flat LSP symbol picker closes before the next request",
        absent=("Document Symbols",),
    )

    session.key(b"\x13")
    session.wait_screen(
        ("No document symbols found.", "pub fn large_root"),
        "null LSP document symbols produce the bounded empty-result status",
        absent=("Document Symbols",),
    )
    trace = _wait_trace(context, ("document-symbol-3",), "null LSP symbols are requested")

    session.key(b"\x13")
    session.wait_screen(
        ("Document Symbols", "ChildrenNull"),
        "nested symbols accept an explicit null children field",
    )
    session.key(b"\x1b")
    session.wait_screen(
        ("Preview", "pub fn large_root"),
        "null-children symbol picker closes before the empty-result request",
        absent=("Document Symbols",),
    )
    session.key(b"\x13")
    session.wait_screen(
        ("No document symbols found.", "pub fn large_root"),
        "an empty symbol array follows the same bounded empty-result path",
        absent=("Document Symbols",),
    )
    session.key(b"\x13")
    session.wait_screen(
        ("mixes nested and flat variants", "pub fn large_root"),
        "mixed document-symbol variants fail and retire the configured session",
    )
    trace = _wait_trace(
        context,
        ("document-symbol-6",),
        "document-symbol boundary matrix reaches its terminal protocol rejection",
    )
    if trace.count("helper-started=") != 1:
        raise E2EAssertionError(
            "helper_receipt",
            f"expected one reused document-symbol helper, got trace {trace!r}",
        )
    session.assertions.append("one helper session serves every document-symbol request")


def crashing_lsp(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(
        ("LATTE LENS", "crash-caller.rs", "caller!"),
        "crashing-LSP fixture Preview loads",
        absent=("LOADING", "Loading content"),
    )
    session.key(b"l")
    session.key(b"\x04")
    _wait_trace(
        context,
        ("crash-started", "crash-after-initialize"),
        "configured helper exits during initialization",
    )
    session.wait_screen(
        ("language server stopped unexpectedly", "caller!"),
        "unexpected server exit is surfaced without moving the Preview",
        absent=("Finding definition…",),
    )

    session.key(b"\x04")
    session.wait_screen(
        ("Language server is restarting after failure:", "caller!"),
        "immediate retry observes the bounded restart backoff",
    )
    trace = Path(context.environment["LATTELENS_TEST_TRACE"]).read_text(encoding="utf-8")
    if trace.count("crash-started") != 1:
        raise E2EAssertionError(
            "helper_receipt",
            f"restart backoff unexpectedly launched another helper: {trace!r}",
        )
    session.assertions.append("restart backoff prevents a crash loop")


def incompatible_lsp(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(("role-caller.rs", "caller!"), "UTF-8 LSP fixture Preview loads")
    session.key(b"l")
    session.key(b"\x04")
    _wait_trace(context, ("utf8-initialize-sent",), "server selects an incompatible encoding")
    session.wait_screen(
        ("unsupported positionEncoding", "requires UTF-16", "caller!"),
        "incompatible position encoding fails the deferred navigation request",
        absent=("Finding definition…",),
    )
    session.key(b"\x04")
    session.wait_screen(
        ("Language server is restarting after failure:", "caller!"),
        "incompatible server cleanup records restart backoff",
    )


def descendant_lsp(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(("role-caller.rs", "caller!"), "descendant LSP fixture Preview loads")
    session.key(b"l")
    session.key(b"\x04")
    trace = _wait_trace(
        context,
        ("ready-before-direct-exit", "descendant=", "direct="),
        "ready server exits while a descendant still owns its pipes",
    )
    session.wait_screen(
        ("Language server request timed out.", "caller!"),
        "pipe-holding descendant reaches the bounded request deadline",
        absent=("Finding definition…",),
    )
    if trace.count("ready-before-direct-exit") != 1:
        raise E2EAssertionError("helper_receipt", f"unexpected descendant trace: {trace!r}")
    session.assertions.append("exit-first server launches exactly one descendant tree")


def timeout_lsp(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(("role-caller.rs", "caller!"), "timeout LSP fixture Preview loads")
    session.key(b"l")
    session.key(b"\x04")
    _wait_trace(context, ("timeout-definition-held",), "definition request is held by the server")
    session.wait_screen(
        ("Language server request timed out.", "caller!"),
        "three-second request deadline cancels a non-responsive server request",
        absent=("Finding definition…",),
    )


def batch_shutdown_lsp(context: ScenarioContext) -> None:
    session = context.session
    session.wait_raw((b"?1000h",), "batch-shutdown terminal enables mouse capture")
    session.wait_screen(
        ("LATTE LENS", "Files", "repo-a", "repo-b"),
        "two-repository batch-shutdown workspace loads",
    )
    session.wait_until(
        lambda screen: (
            "Scanning files…" not in screen.text()
            and "Loading workspace…" not in screen.text()
            and (" loaded" in screen.text() or " entries" in screen.text())
        ),
        "two-repository workspace snapshot converges",
    )

    for repository_name in ("repo-a", "repo-b"):
        session.key(b"\x10")
        session.wait_screen(("Open File",), f"file search opens for {repository_name}")
        session.key(b"\x15")
        session.key(f"{repository_name}/caller.rs".encode())
        session.wait_screen(
            (f"{repository_name}/caller.rs", "1 result"),
            f"file search resolves {repository_name} caller",
        )
        session.key(b"\r")
        session.wait_screen(
            (
                "Preview",
                f"{repository_name}/caller.rs",
                f"{repository_name.replace('-', '_')}!();",
            ),
            f"{repository_name} caller Preview opens",
            absent=("Open File", "LOADING", "Loading content"),
        )
        session.key(b"\x04")
        session.wait_screen(
            ("No definition found.", f"{repository_name}/caller.rs"),
            f"{repository_name} keeps one ready stalled-session tree",
            absent=("Finding definition…",),
        )

    trace = _wait_trace(
        context,
        ("stalled-root=", "stalled-direct=", "stalled-descendant=", "definition="),
        "both ready process trees are active before process-wide shutdown",
    )
    roots = {
        line.split("=", 1)[1]
        for line in trace.splitlines()
        if line.startswith("stalled-root=")
    }
    expected_counts = {
        "stalled-root=": 2,
        "stalled-direct=": 2,
        "stalled-descendant=": 2,
        "definition=": 2,
    }
    if len(roots) != 2 or any(trace.count(marker) != count for marker, count in expected_counts.items()):
        raise E2EAssertionError(
            "helper_receipt", f"expected two distinct ready process trees, got trace {trace!r}"
        )
    session.assertions.append("two distinct ready process trees await one shutdown batch")


def lsp_resilience_backoff_cleanup(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(
        ("role-caller.rs", "caller!"),
        "resilience fixture Preview loads",
        absent=("LOADING", "Loading content"),
    )
    session.key(b"l")
    trace_path = Path(context.environment["LATTELENS_TEST_TRACE"])

    session.key(b"\x12")
    session.wait_screen(
        ("Configured language server does not provide References.", "caller!"),
        "launch one exposes unsupported Ctrl-R without sending References",
    )
    trace = _wait_trace(
        context,
        ("resilience-launch=1", "resilience-initialize-ok=1"),
        "launch one initializes without References capability",
    )
    if "resilience-definition=1" in trace:
        raise E2EAssertionError("helper_receipt", f"unsupported References changed trace: {trace!r}")
    session.assertions.append("unsupported References sends no semantic request")

    session.key(b"\x04")
    session.wait_screen(
        ("bounded JSON string token is too large", "caller!"),
        "launch one invalid error shape retires the ready session",
    )
    _wait_trace(context, ("resilience-response=1",), "launch one arms its invalid error shape")
    session.key(b"\x04")
    session.wait_screen(
        ("Language server is restarting after failure:", "caller!"),
        "first immediate retry observes one-second backoff",
    )
    _drain_for(session, 1.1, "PTY remains drained through one-second backoff")

    session.key(b"\x04")
    session.wait_screen(
        ("fixture initialize denied", "caller!"),
        "launch two surfaces the bounded initialize error",
    )
    _wait_trace(
        context,
        ("resilience-launch=2", "resilience-initialize-denied"),
        "launch two records initialize denial",
    )
    session.key(b"\x04")
    session.wait_screen(
        ("Language server is restarting after failure:", "caller!"),
        "second immediate retry observes two-second backoff",
    )
    _drain_for(session, 2.1, "PTY remains drained through two-second backoff")

    session.key(b"\x04")
    session.wait_screen(
        ("malformed language server JSON", "caller!"),
        "launch three rejects framed malformed JSON",
    )
    _wait_trace(
        context,
        ("resilience-launch=3", "resilience-response=3"),
        "launch three arms framed malformed JSON before sending",
    )
    session.key(b"\x04")
    session.wait_screen(
        ("Language server is restarting after failure:", "caller!"),
        "third immediate retry observes four-second backoff",
    )
    _drain_for(session, 4.1, "PTY remains drained through four-second backoff")

    session.key(b"\x04")
    session.wait_screen(
        ("JSON-RPC response is missing its id", "caller!"),
        "launch four rejects a response without id",
    )
    trace = _wait_trace(
        context,
        ("resilience-launch=4", "resilience-ignore-term-pid=", "resilience-response=4"),
        "launch four records its SIGTERM-resistant pid",
    )
    pid_line = next(
        line for line in trace.splitlines() if line.startswith("resilience-ignore-term-pid=")
    )
    helper_pid = int(pid_line.split("=", 1)[1])
    _assert_process_gone(session, helper_pid, "SIGTERM-resistant launch four is killed and reaped")
    session.key(b"\x04")
    session.wait_screen(
        ("Language server is restarting after failure:", "caller!"),
        "fourth immediate retry observes eight-second backoff",
    )
    _drain_for(session, 8.1, "PTY remains drained through eight-second backoff")

    session.key(b"\x04")
    session.wait_screen(
        ("JSON-RPC response must contain exactly one of result or error", "caller!"),
        "launch five rejects a response with result and error",
    )
    trace = _wait_trace(
        context,
        ("resilience-launch=5", "resilience-response=5"),
        "launch five reaches the permanent-failure boundary",
    )
    session.key(b"\x04")
    session.wait_screen(
        ("Language server is disabled after repeated failures:", "caller!"),
        "immediate request after fifth failure is permanently disabled",
    )
    _drain_for(session, 0.2, "disabled session remains responsive without respawn")
    trace = trace_path.read_text(encoding="utf-8")
    if trace.count("resilience-launch=") != 5:
        raise E2EAssertionError("helper_receipt", f"expected exactly five launches: {trace!r}")
    if Path(context.environment["LATTELENS_TEST_LAUNCH_COUNT"]).read_text().strip() != "5":
        raise E2EAssertionError("helper_receipt", "persisted helper launch count is not five")
    session.assertions.append("exactly five persisted helper launches reach permanent disablement")


def default_lsp(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(
        ("basename-caller.rs", "caller!"), "default language-server fixture Preview loads"
    )
    session.key(b"l")
    session.key(b"\x04")
    session.wait_screen(
        ("basename-target.rs", "pub fn target", "Navigation target opened."),
        "PATH-resolved trusted server completes production definition navigation",
    )
    _wait_trace(
        context,
        ("helper-started=", "definition-1"),
        "built-in server discovery launches rust-analyzer without configuration",
    )


def invalid_product_config(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(
        ("invalid-config.rs", "Configuration:", "invalid Latte Lens config"),
        "malformed explicit config is sanitized and surfaced in the footer",
    )
    session.key(b"l")
    session.key(b"\x04")
    session.wait_screen(
        ("Code navigation is unavailable for Rust: no language server was found.", "caller!"),
        "invalid config disables navigation without affecting the Preview",
    )


def missing_product_config(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(
        ("missing-config.rs", "Configuration:", "explicit Latte Lens config does not exist"),
        "missing explicit config is surfaced without blocking startup",
    )


def directory_product_config(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(
        ("directory-config.rs", "Configuration:", "not a regular file"),
        "directory config is rejected and surfaced without blocking startup",
    )


def disabled_product_config(context: ScenarioContext) -> None:
    session = context.session
    session.wait_screen(
        ("disabled-config.rs", "caller!"),
        "explicitly disabled navigation starts without a configuration warning",
        absent=("Configuration:",),
    )
    session.key(b"l")
    session.key(b"\x04")
    session.wait_screen(
        ("Code navigation is unavailable for Rust: no language server was found.", "caller!"),
        "explicitly disabled navigation remains unavailable without affecting the Preview",
    )


def code_navigation_without_lsp(context: ScenarioContext) -> None:
    session = context.session
    session.wait_raw((b"?1000h",), "no-LSP terminal enables mouse capture")
    session.wait_screen(
        ("LATTE LENS", "Preview", "caller!"),
        "no-LSP fixture Preview loads",
        absent=("LOADING", "Loading caller.rs"),
    )
    session.key(b"\x04")
    session.wait_screen(
        ("Focus Preview to navigate.", "caller!"),
        "semantic navigation is ignored while the file tree owns focus",
    )
    session.key(b"l")
    session.key(b"\x1b[1;3D")
    session.wait_screen(("No previous navigation location.",), "empty back history is bounded")
    session.key(b"\x1b[1;3C")
    session.wait_screen(("No forward navigation location.",), "empty forward history is bounded")
    session.key(b"\x13")
    session.wait_screen(
        ("No document symbols found.", "caller!"),
        "complete empty local symbols avoid unnecessary LSP startup",
    )
    session.key(b"\x04")
    session.wait_screen(
        ("Code navigation is unavailable for Rust: no language server was found.", "caller!"),
        "Ctrl-D without LSP reports unavailable and stays in place",
    )


CASES = (
    ScenarioCase("files-navigation", "files", create_navigation_fixture, files_navigation),
    ScenarioCase("files-refresh", "files", create_navigation_fixture, files_refresh),
    ScenarioCase(
        "symlink-preview-smoke",
        "files",
        create_symlink_preview_fixture,
        symlink_preview_smoke,
    ),
    ScenarioCase("keyboard-controls", "files", create_navigation_fixture, keyboard_controls),
    ScenarioCase("git-navigation", "git-changes", create_navigation_fixture, git_navigation),
    ScenarioCase("git-status-matrix", "git-changes", create_git_matrix_fixture, git_status_matrix),
    ScenarioCase("git-review-state", "git-changes", create_git_matrix_fixture, git_review_state),
    ScenarioCase(
        "repository-relation-matrix",
        "git-changes",
        create_repository_relation_fixture,
        repository_relation_matrix,
    ),
    ScenarioCase("search-preview", "search-preview", create_search_fixture, search_preview),
    ScenarioCase("search-controls", "search-preview", create_search_fixture, search_controls),
    ScenarioCase(
        "code-navigation", "code-navigation", create_code_navigation_fixture, code_navigation
    ),
    ScenarioCase(
        "code-navigation-no-lsp",
        "code-navigation",
        create_code_navigation_without_lsp_fixture,
        code_navigation_without_lsp,
    ),
    ScenarioCase(
        "structure-navigation",
        "code-navigation",
        create_structure_fixture,
        structure_navigation,
    ),
    ScenarioCase(
        "fold-keyboard-mouse-definition",
        "code-navigation",
        create_fold_mouse_navigation_fixture,
        fold_keyboard_and_mouse_definition,
    ),
    ScenarioCase(
        "lsp-document-symbols",
        "code-navigation",
        create_lsp_document_symbol_fixture,
        lsp_document_symbols,
    ),
    ScenarioCase(
        "crashing-lsp",
        "code-navigation",
        create_crashing_lsp_fixture,
        crashing_lsp,
    ),
    ScenarioCase(
        "incompatible-lsp",
        "code-navigation",
        create_incompatible_lsp_fixture,
        incompatible_lsp,
    ),
    ScenarioCase(
        "descendant-lsp",
        "code-navigation",
        create_descendant_lsp_fixture,
        descendant_lsp,
    ),
    ScenarioCase(
        "timeout-lsp",
        "code-navigation",
        create_timeout_lsp_fixture,
        timeout_lsp,
    ),
    ScenarioCase(
        "batch-shutdown-lsp",
        "code-navigation",
        create_batch_shutdown_lsp_fixture,
        batch_shutdown_lsp,
    ),
    ScenarioCase(
        "lsp-resilience-backoff-cleanup",
        "code-navigation",
        create_resilience_lsp_fixture,
        lsp_resilience_backoff_cleanup,
    ),
    ScenarioCase(
        "default-language-server",
        "code-navigation",
        create_default_lsp_fixture,
        default_lsp,
    ),
    ScenarioCase(
        "invalid-product-config",
        "code-navigation",
        create_invalid_product_config_fixture,
        invalid_product_config,
    ),
    ScenarioCase(
        "missing-product-config",
        "code-navigation",
        create_missing_product_config_fixture,
        missing_product_config,
    ),
    ScenarioCase(
        "directory-product-config",
        "code-navigation",
        create_directory_product_config_fixture,
        directory_product_config,
    ),
    ScenarioCase(
        "disabled-product-config",
        "code-navigation",
        create_disabled_product_config_fixture,
        disabled_product_config,
    ),
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
