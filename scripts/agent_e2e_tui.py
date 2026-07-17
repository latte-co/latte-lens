#!/usr/bin/env python3
"""Run the synthetic Agent observability journey through a real POSIX PTY."""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

from e2e.fixtures import create_navigation_fixture
from e2e.runner import print_summary, run_suite
from e2e.scenarios import ScenarioCase, ScenarioContext, validate_clipboard_mode


def click_screen_marker(context: ScenarioContext, marker: str) -> None:
    session = context.session
    session.drain()
    for row, cells in enumerate(session.screen.cells):
        line = "".join(cells)
        column = line.find(marker)
        if column >= 0:
            session.click(column, row)
            return
    raise AssertionError(f"cannot find screen marker: {marker}")


def click_agent_row(context: ScenarioContext, index: int) -> None:
    session = context.session
    session.drain()
    dividers = [
        column
        for column in session.screen.vertical_rule_columns()
        if 4 < column < session.screen.columns - 5
    ]
    divider = dividers[0] if dividers else session.screen.columns
    rows = []
    for row, cells in enumerate(session.screen.cells):
        line = "".join(cells[:divider])
        column = line.find("synthetic/harness")
        if column >= 0:
            rows.append((column, row))
    if index >= len(rows):
        raise AssertionError(f"cannot find Agent row {index}; found {len(rows)}")
    session.click(*rows[index])


def agent_metadata_to_live(context: ScenarioContext) -> None:
    session = context.session
    session.wait_raw((b"?1000h",), "harness mouse-enabled terminal")
    session.wait_screen(
        ("LATTE LENS", "1 Files", "2 Git changes", "3 Agents"),
        "production UI with Agent scope",
    )
    session.key(b"3")
    session.wait_screen(
        ("Agents", "0/0 live", "No observed Agent sessions in this workspace."),
        "empty exact workspace does not invent Agent sessions",
    )
    session.key(b"r")
    session.wait_screen(
        (
            "Agents",
            "0/2 live",
            "synthetic/harness",
            "metadata",
            "Metadata only",
            "Activity      Unknown",
            "Explain",
        ),
        "metadata-only session without invented live state",
        absent=("prompt-canary", "token-canary"),
    )
    session.key(b"G")
    session.wait_screen(
        ("Start confirmed", "Observers     none", "Agents        0/0 live+"),
        "second concurrently observed session has independent metadata",
        absent=("prompt-canary", "token-canary"),
    )
    session.key(b"g")
    session.wait_screen(
        ("Mid-session", "Observers     synthetic/harness"),
        "first session remains independently selectable",
        absent=("prompt-canary", "token-canary"),
    )
    session.key(b"\r")
    session.key(b"l")
    session.key(b"G")
    session.key(b"g")
    session.key(b"h")
    session.key(b"p")
    session.wait_screen(("Agent session", "Metadata only"), "preview keeps Agent detail")
    session.key(b"d")
    session.wait_screen(("Agent session", "Metadata only"), "diff keeps Agent detail")
    session.key(b"r")
    session.wait_screen(
        (
            "1/2 live",
            "live Working",
            "Live observed",
            "Activity      Working",
            "Authoritative",
            "synthetic/harness",
        ),
        "refresh-triggered live evidence through the production reducer and UI",
        absent=("prompt-canary", "token-canary"),
    )
    session.key(b"r")
    session.wait_screen(
        ("Live observed", "Activity      Waiting permission"),
        "live activity advances to waiting permission",
    )
    session.key(b"r")
    session.wait_screen(
        ("live Idle", "Activity      Idle"),
        "live activity advances to idle",
    )
    session.key(b"r")
    session.wait_screen(
        ("live Unknown", "Activity      Unknown", "Freshness     Stale"),
        "evidence expiry is reduced into stale unknown activity",
    )
    session.key(b"r")
    session.wait_screen(
        ("! synthetic/harness", "dropped 3"),
        "session-attributed and unattributed live drops are visible",
    )
    session.key(b"r")
    session.wait_screen(
        ("~ synthetic/harness", "Reconciling", "gaps 1"),
        "stream gap is visible as reconciling partial coverage",
    )
    session.key(b"r")
    session.wait_screen(
        ("snapshot Some(Partial)", "Reconciling"),
        "contract downgrade re-arbitrates the visible session",
    )

    session.resize(72, 24)
    session.wait_screen(
        ("LATTE LENS", "Agents", "^⇧F", "Reconciling"),
        "Agent diagnostics remain usable in a narrow terminal",
    )
    session.resize(120, 30)
    session.wait_screen(("Agents", "Reconciling"), "Agent view redraws after terminal restore")

    session.key(b"\x1b[Z")
    session.wait_screen(("2 Git changes", "Diff"), "BackTab moves from Agents to Git changes")
    session.key(b"\x1b[Z")
    session.wait_screen(("1 Files", "a-dir"), "BackTab moves from Git changes to Files")
    click_screen_marker(context, "3 Agents")
    session.wait_screen(
        ("Agents", "1/2 live", "synthetic/harness"),
        "mouse click returns to the Agent scope",
    )
    click_agent_row(context, 1)
    session.wait_screen(
        ("Start confirmed", "Metadata only", "Agents        0/0 live+"),
        "mouse selects the independent metadata session",
    )
    click_agent_row(context, 0)
    session.wait_screen(
        ("Mid-session", "Live observed", "Reconciling", "dropped 3"),
        "mouse restores the live session and its diagnostics",
    )
    session.key(b"r")
    session.wait_screen(
        ("Activity      Working", "Reconciling"),
        "backpressure phase is armed by one final live update",
    )
    for _ in range(12):
        session.key(b"r")
    session.wait_screen(
        ("Agent expiry queue is full; coverage is partial",),
        "metadata and expiry backpressure stay visible without blocking the TUI",
    )


CASE = ScenarioCase(
    id="agent-metadata-live",
    group="agent-observability",
    fixture=create_navigation_fixture,
    journey=agent_metadata_to_live,
)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--artifact-dir", type=Path)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    if sys.platform == "win32":
        print("Agent PTY E2E is supported on Linux and macOS", file=sys.stderr)
        return 2
    binary = arguments.binary.resolve()
    if not binary.is_file():
        print(f"binary does not exist: {binary}", file=sys.stderr)
        return 2
    clipboard_mode = os.environ.get("LATTELENS_E2E_CLIPBOARD", "osc52")
    try:
        validate_clipboard_mode(clipboard_mode)
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 2
    summary = run_suite(
        [CASE],
        binary,
        clipboard_mode,
        arguments.artifact_dir.resolve() if arguments.artifact_dir else None,
    )
    print_summary(summary)
    return 0 if summary["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
