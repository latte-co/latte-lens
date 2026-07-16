"""Self-tests for the E2E harness; never launches the Latte Lens binary."""

from __future__ import annotations

import os
import pty
import subprocess
import sys
import time
import unittest
from pathlib import Path
from unittest import mock


SCRIPTS = Path(__file__).resolve().parent
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

from e2e.fixtures import (  # noqa: E402
    ExternalIsolationOracle,
    ReadOnlyOracle,
    Sandbox,
    init_repository,
)
from e2e.runner import hard_deadline  # noqa: E402
from e2e.terminal import (  # noqa: E402
    E2EAssertionError,
    MAX_EVIDENCE_BYTES,
    PtySession,
    TerminalScreen,
)


class TerminalScreenTests(unittest.TestCase):
    def test_streaming_utf8_cursor_erase_and_rules(self) -> None:
        screen = TerminalScreen(columns=12, rows=5)
        encoded = "A界".encode()
        screen.feed(encoded[:2])
        screen.feed(encoded[2:])
        screen.feed(b"\x1b[2;2Hrow\x1b[K")
        screen.feed(b"\x1b[1;6H\xe2\x94\x82")
        screen.feed(b"\x1b[2;6H\xe2\x94\x82")
        screen.feed(b"\x1b[3;6H\xe2\x94\x82")

        self.assertIn("A界", screen.text())
        self.assertEqual(screen.find("row"), (1, 1))
        self.assertIn(5, screen.vertical_rule_columns())

    def test_alternate_screen_resets_prior_cells(self) -> None:
        screen = TerminalScreen(columns=10, rows=3)
        screen.feed(b"stale\x1b[?1049hclean")
        self.assertNotIn("stale", screen.text())
        self.assertIn("clean", screen.text())


class SandboxTests(unittest.TestCase):
    def test_environment_is_isolated_and_cleanup_is_receipted(self) -> None:
        with mock.patch.dict(
            os.environ,
            {"GIT_DIR": "/host/git", "LATTELENS_UNSAFE_TEST": "/host/lens"},
        ):
            sandbox = Sandbox("self-test")
            root = sandbox.root
            environment = sandbox.environment()
            self.assertEqual(environment["HOME"], str(sandbox.home))
            self.assertEqual(environment["PWD"], str(sandbox.repository))
            self.assertEqual(environment["XDG_DATA_HOME"], str(sandbox.xdg_data))
            self.assertEqual(environment["XDG_RUNTIME_DIR"], str(sandbox.runtime))
            self.assertEqual(environment["GIT_CONFIG_GLOBAL"], os.devnull)
            self.assertEqual(environment["GIT_OPTIONAL_LOCKS"], "0")
            self.assertNotIn("GIT_DIR", environment)
            self.assertNotIn("LATTELENS_UNSAFE_TEST", environment)
            receipt = sandbox.cleanup()
            self.assertTrue(receipt["sandbox_removed"])
            self.assertFalse(root.exists())

    def test_read_only_oracle_allows_only_named_driver_mutation(self) -> None:
        sandbox = Sandbox("oracle-self-test")
        try:
            environment = sandbox.environment()
            init_repository(sandbox.repository, environment)
            tracked = sandbox.repository / "tracked.txt"
            tracked.write_text("before\n", encoding="utf-8")
            subprocess.run(
                ["git", "add", "tracked.txt"],
                cwd=sandbox.repository,
                env=environment,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "commit", "-q", "-m", "fixture"],
                cwd=sandbox.repository,
                env=environment,
                check=True,
                capture_output=True,
            )
            oracle = ReadOnlyOracle(sandbox.repository, environment)
            allowed = sandbox.repository / "allowed.txt"
            allowed.write_text("driver\n", encoding="utf-8")
            oracle.record_driver_write(allowed)
            self.assertTrue(oracle.verify()["expected_worktree_unchanged"])
            self.assertTrue(oracle.verify()["git_status_unchanged"])
            tracked.write_text("unexpected\n", encoding="utf-8")
            with self.assertRaisesRegex(AssertionError, "read-only invariant"):
                oracle.verify()
        finally:
            sandbox.cleanup()

    def test_external_oracle_detects_host_config_escape_in_an_isolated_host(self) -> None:
        sandbox = Sandbox("external-oracle-self-test")
        try:
            environment = sandbox.environment()
            init_repository(sandbox.repository, environment)
            oracle = ExternalIsolationOracle(
                host_cwd=sandbox.repository, host_home=sandbox.home
            )
            self.assertTrue(oracle.verify()["host_config_unchanged"])
            (sandbox.home / ".gitconfig").write_text("[user]\nname = escaped\n")
            with self.assertRaisesRegex(AssertionError, "external isolation"):
                oracle.verify()
        finally:
            sandbox.cleanup()

    def test_external_oracle_does_not_refresh_the_host_git_index(self) -> None:
        sandbox = Sandbox("external-oracle-index-self-test")
        try:
            environment = sandbox.environment()
            init_repository(sandbox.repository, environment)
            tracked = sandbox.repository / "tracked.txt"
            tracked.write_text("stable\n", encoding="utf-8")
            subprocess.run(
                ["git", "add", "tracked.txt"],
                cwd=sandbox.repository,
                env=environment,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "commit", "-q", "-m", "fixture"],
                cwd=sandbox.repository,
                env=environment,
                check=True,
                capture_output=True,
            )

            # Make the worktree stat data differ from the index while keeping
            # the content clean, so an ordinary `git status` would refresh it.
            tracked_stat = tracked.stat()
            os.utime(
                tracked,
                ns=(tracked_stat.st_atime_ns, tracked_stat.st_mtime_ns + 5_000_000_000),
            )
            index = sandbox.repository / ".git" / "index"
            index_before = (index.read_bytes(), index.stat().st_mtime_ns)

            oracle = ExternalIsolationOracle(
                host_cwd=sandbox.repository, host_home=sandbox.home
            )

            self.assertEqual(oracle._host_git_environment()["GIT_OPTIONAL_LOCKS"], "0")
            self.assertTrue(oracle.verify()["host_checkout_unchanged"])
            self.assertEqual((index.read_bytes(), index.stat().st_mtime_ns), index_before)
        finally:
            sandbox.cleanup()


class ProcessEvidenceTests(unittest.TestCase):
    def _sleeping_session(self) -> PtySession:
        master_fd, slave_fd = pty.openpty()
        process = subprocess.Popen(
            [sys.executable, "-c", "import time; print('ready', flush=True); time.sleep(30)"],
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            close_fds=True,
            start_new_session=True,
        )
        os.close(slave_fd)
        return PtySession(process, master_fd, TerminalScreen(80, 24))

    def test_timeout_has_failure_kind_bounded_evidence_and_cleanup(self) -> None:
        session = self._sleeping_session()
        try:
            session.wait_raw((b"ready",), "fake helper readiness")
            with self.assertRaises(E2EAssertionError) as captured:
                session.wait_screen(("never-visible",), "intentional timeout", timeout=0.03)
            self.assertEqual(captured.exception.kind, "screen_convergence")
            session.output.extend(b"x" * (MAX_EVIDENCE_BYTES + 10))
            self.assertEqual(len(session.raw_tail()), MAX_EVIDENCE_BYTES)
        finally:
            session.close()
        receipt = session.cleanup_receipt()
        self.assertTrue(receipt["process_exited"])
        self.assertTrue(receipt["pty_closed"])
        self.assertTrue(receipt["forced_termination"])

    def test_hard_deadline_interrupts_code_outside_terminal_waits(self) -> None:
        started = time.monotonic()
        with self.assertRaises(E2EAssertionError) as captured:
            with hard_deadline(0.03, "deadline-self-test"):
                time.sleep(10)
        self.assertEqual(captured.exception.kind, "scenario_timeout")
        self.assertLess(time.monotonic() - started, 0.5)

    def test_launched_child_uses_the_sandbox_repository_as_cwd(self) -> None:
        sandbox = Sandbox("child-cwd-self-test")
        helper = sandbox.root / "cwd-helper.py"
        helper.write_text(
            "#!/usr/bin/env python3\n"
            "import os, time\n"
            "print(os.getcwd(), flush=True)\n"
            "time.sleep(30)\n",
            encoding="utf-8",
        )
        helper.chmod(0o755)
        session = PtySession.launch(
            helper, sandbox.repository, sandbox.environment(), columns=80, rows=24
        )
        try:
            session.wait_raw(
                (os.fsencode(sandbox.repository),), "child reports sandbox cwd"
            )
        finally:
            session.close()
            sandbox.cleanup()


class CoverageScriptTests(unittest.TestCase):
    def test_instrumented_target_cannot_canonicalize_to_helper_target(self) -> None:
        sandbox = Sandbox("coverage-target-collision-self-test")
        try:
            cargo_marker = sandbox.root / "cargo-invoked"
            fake_cargo = sandbox.root / "fake-cargo"
            fake_cargo.write_text(
                "#!/bin/sh\n"
                'printf invoked > "$CARGO_MARKER"\n'
                "exit 99\n",
                encoding="utf-8",
            )
            fake_cargo.chmod(0o755)
            environment = os.environ.copy()
            environment.update(
                {
                    "CARGO": str(fake_cargo),
                    "CARGO_MARKER": str(cargo_marker),
                    "PYTHON": sys.executable,
                    "E2E_COVERAGE_TARGET_DIR": "target/../target",
                }
            )
            result = subprocess.run(
                [str(SCRIPTS / "coverage-e2e.sh")],
                cwd=SCRIPTS.parent,
                env=environment,
                check=False,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn(
                "must not resolve to the non-instrumented helper target",
                result.stderr,
            )
            self.assertFalse(cargo_marker.exists(), "collision must fail before Cargo runs")
        finally:
            sandbox.cleanup()


if __name__ == "__main__":
    unittest.main()
