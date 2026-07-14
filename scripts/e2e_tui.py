#!/usr/bin/env python3
"""Run named Latte Lens production-binary E2E scenarios."""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

from e2e.runner import print_summary, run_suite
from e2e.scenarios import selected_cases, validate_clipboard_mode


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "binary",
        nargs="?",
        help="path to the latte-lens production binary (not needed with --self-test)",
    )
    parser.add_argument(
        "--scenario",
        choices=("files", "git-changes", "search-preview", "all"),
        default="all",
        help="named scenario group to execute (default: all)",
    )
    parser.add_argument(
        "--artifact-dir",
        type=Path,
        help="optional directory for bounded JSON evidence and cleanup receipts",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="test the parser, sandbox, timeout evidence, and cleanup without Latte Lens",
    )
    return parser.parse_args()


def run_self_test() -> int:
    import unittest

    suite = unittest.defaultTestLoader.discover(
        str(Path(__file__).resolve().parent), pattern="test_e2e*.py"
    )
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


def main() -> int:
    arguments = parse_arguments()
    if sys.platform == "win32":
        print("PTY E2E is supported on Linux and macOS", file=sys.stderr)
        return 2
    if arguments.self_test:
        if arguments.binary is not None:
            print("binary is not accepted with --self-test", file=sys.stderr)
            return 2
        return run_self_test()
    if arguments.binary is None:
        print("a production binary path is required", file=sys.stderr)
        return 2

    binary = Path(arguments.binary).resolve()
    if not binary.is_file():
        print(f"binary does not exist: {binary}", file=sys.stderr)
        return 2
    clipboard_mode = os.environ.get("LATTELENS_E2E_CLIPBOARD", "osc52")
    try:
        validate_clipboard_mode(clipboard_mode)
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 2

    artifact_directory = arguments.artifact_dir.resolve() if arguments.artifact_dir else None
    summary = run_suite(
        selected_cases(arguments.scenario),
        binary,
        clipboard_mode,
        artifact_directory,
    )
    print_summary(summary)
    return 0 if summary["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
