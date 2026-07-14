"""Scenario orchestration, bounded JSON evidence, and cleanup receipts."""

from __future__ import annotations

import contextlib
import json
import signal
import subprocess
import time
from pathlib import Path
from typing import Iterator

from .fixtures import ExternalIsolationOracle, ReadOnlyOracle, Sandbox
from .scenarios import ScenarioCase, ScenarioContext
from .terminal import E2EAssertionError, MAX_EVIDENCE_BYTES, PtySession


SCHEMA_VERSION = 1
SCENARIO_BUDGET_SECONDS = 60.0
SUITE_BUDGET_SECONDS = 240.0


@contextlib.contextmanager
def hard_deadline(seconds: float, label: str) -> Iterator[None]:
    """Interrupt a scenario even if it is blocked outside a screen wait."""

    if seconds <= 0:
        raise E2EAssertionError("scenario_timeout", f"{label} exhausted its hard deadline")
    previous_handler = signal.getsignal(signal.SIGALRM)
    previous_timer = signal.getitimer(signal.ITIMER_REAL)
    started = time.monotonic()

    def expire(_signum: int, _frame: object) -> None:
        raise E2EAssertionError(
            "scenario_timeout", f"{label} exceeded its {seconds:.3f}s hard deadline"
        )

    signal.signal(signal.SIGALRM, expire)
    inherited = previous_timer[0]
    effective = min(seconds, inherited) if inherited > 0 else seconds
    signal.setitimer(signal.ITIMER_REAL, effective)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_handler)
        if inherited > 0:
            elapsed = time.monotonic() - started
            signal.setitimer(
                signal.ITIMER_REAL,
                max(0.001, inherited - elapsed),
                previous_timer[1],
            )


def _bounded_message(error: BaseException) -> str:
    message = str(error)
    if len(message) <= 8_000:
        return message
    return f"{message[:3_000]}\n...[bounded evidence omitted]...\n{message[-4_500:]}"


def _write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def run_case(
    case: ScenarioCase,
    binary: Path,
    clipboard_mode: str,
    artifact_directory: Path | None,
    budget_seconds: float = SCENARIO_BUDGET_SECONDS,
) -> tuple[dict[str, object], dict[str, object]]:
    started = time.monotonic()
    sandbox = Sandbox(case.id)
    environment = sandbox.environment(clipboard_mode)
    session: PtySession | None = None
    evidence: dict[str, object] = {}
    oracle_evidence: dict[str, object] = {}
    external_evidence: dict[str, object] = {}
    status = "passed"
    failure_kind: str | None = None
    failure: str | None = None
    process_receipt: dict[str, object] = {
        "process_exited": True,
        "pty_closed": True,
        "forced_termination": False,
    }
    try:
        with hard_deadline(budget_seconds, case.id):
            external_oracle = ExternalIsolationOracle()
            case.fixture(sandbox.repository, environment)
            oracle = ReadOnlyOracle(sandbox.repository, environment)
            session = PtySession.launch(binary, sandbox.repository, environment)
            context = ScenarioContext(sandbox, session, environment, clipboard_mode, oracle)
            case.journey(context)
            session.quit_cleanly()
            oracle_evidence = oracle.verify()
            external_evidence = external_oracle.verify()
    except Exception as error:  # evidence must survive assertion and process failures
        status = "failed"
        failure_kind = getattr(error, "kind", type(error).__name__)
        failure = _bounded_message(error)
    finally:
        if session is not None:
            session.close()
            evidence = session.evidence()
            process_receipt = session.cleanup_receipt()
        sandbox_receipt = sandbox.cleanup()

    cleanup = {
        "schema_version": SCHEMA_VERSION,
        "scenario": case.id,
        **process_receipt,
        **sandbox_receipt,
    }
    if not cleanup["process_exited"] or not cleanup["pty_closed"] or not cleanup["sandbox_removed"]:
        status = "failed"
        failure_kind = failure_kind or "cleanup"
        failure = failure or "process, PTY, or sandbox cleanup receipt is incomplete"

    duration = round(time.monotonic() - started, 3)
    result: dict[str, object] = {
        "id": case.id,
        "group": case.group,
        "status": status,
        "duration_seconds": duration,
        "assertion_count": len(evidence.get("assertions", [])),
        "terminal_bytes": evidence.get("terminal_bytes", 0),
        "read_only": oracle_evidence,
        "external_isolation": external_evidence,
        "cleanup": {
            "process_exited": cleanup["process_exited"],
            "pty_closed": cleanup["pty_closed"],
            "sandbox_removed": cleanup["sandbox_removed"],
            "forced_termination": cleanup["forced_termination"],
        },
    }
    if failure is not None:
        result["failure_kind"] = failure_kind
        result["failure"] = failure
        result["screen"] = str(evidence.get("screen", ""))[-8_000:]

    if artifact_directory is not None:
        _write_json(artifact_directory / f"{case.id}.json", evidence)
        _write_json(artifact_directory / f"{case.id}-cleanup.json", cleanup)
    return result, cleanup


def run_suite(
    cases: list[ScenarioCase],
    binary: Path,
    clipboard_mode: str,
    artifact_directory: Path | None,
    *,
    scenario_budget_seconds: float = SCENARIO_BUDGET_SECONDS,
    suite_budget_seconds: float = SUITE_BUDGET_SECONDS,
) -> dict[str, object]:
    started = time.monotonic()
    suite_deadline = started + suite_budget_seconds
    original_clipboard: bytes | None = None
    clipboard_restored = True
    if clipboard_mode == "native":
        original_clipboard = subprocess.run(
            ["pbpaste"], check=False, capture_output=True, timeout=5
        ).stdout

    results: list[dict[str, object]] = []
    cleanup_receipts: list[dict[str, object]] = []
    try:
        for case in cases:
            remaining = suite_deadline - time.monotonic()
            result, cleanup = run_case(
                case,
                binary,
                clipboard_mode,
                artifact_directory,
                budget_seconds=min(scenario_budget_seconds, remaining),
            )
            results.append(result)
            cleanup_receipts.append(cleanup)
    finally:
        if original_clipboard is not None:
            restore = subprocess.run(
                ["pbcopy"],
                input=original_clipboard,
                check=False,
                capture_output=True,
                timeout=5,
            )
            clipboard_restored = restore.returncode == 0

    passed = sum(result["status"] == "passed" for result in results)
    summary: dict[str, object] = {
        "schema_version": SCHEMA_VERSION,
        "status": "passed" if passed == len(results) and clipboard_restored else "failed",
        "selected_cases": [case.id for case in cases],
        "passed": passed,
        "failed": len(results) - passed,
        "duration_seconds": round(time.monotonic() - started, 3),
        "scenario_budget_seconds": scenario_budget_seconds,
        "suite_budget_seconds": suite_budget_seconds,
        "clipboard_mode": clipboard_mode,
        "clipboard_restored": clipboard_restored,
        "scenarios": results,
    }
    if artifact_directory is not None:
        _write_json(artifact_directory / "summary.json", summary)
        _write_json(
            artifact_directory / "cleanup.json",
            {
                "schema_version": SCHEMA_VERSION,
                "clipboard_restored": clipboard_restored,
                "receipts": cleanup_receipts,
            },
        )
    return summary


def print_summary(summary: dict[str, object]) -> None:
    for result in summary["scenarios"]:
        suffix = ""
        if result["status"] != "passed":
            suffix = f" ({result.get('failure_kind')}: {result.get('failure')})"
        print(
            f"E2E {result['status']}: {result['id']} "
            f"({result['duration_seconds']}s, {result['assertion_count']} assertions){suffix}"
        )
    # One bounded line is easy for CI collectors and downstream tooling.
    compact = dict(summary)
    compact["scenarios"] = [
        {key: value for key, value in result.items() if key != "screen"}
        for result in summary["scenarios"]
    ]
    encoded = json.dumps(compact, ensure_ascii=False, separators=(",", ":"))
    if len(encoded.encode()) > MAX_EVIDENCE_BYTES:
        raise RuntimeError("bounded E2E summary unexpectedly exceeded its evidence budget")
    print(f"E2E_SUMMARY={encoded}")
