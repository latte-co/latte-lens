#!/usr/bin/env python3
"""End-to-end tests for the POSIX release installer."""

from __future__ import annotations

from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import threading
import unittest


ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "install.sh"


class FixtureServer(ThreadingHTTPServer):
    responses: dict[str, tuple[int, bytes]]
    requests: list[str]


class FixtureHandler(BaseHTTPRequestHandler):
    server: FixtureServer

    def do_GET(self) -> None:
        self.server.requests.append(self.path)
        status, body = self.server.responses.get(self.path, (404, b"not found"))
        self.send_response(status)
        if self.path.endswith(".zip"):
            content_type = "application/zip"
        elif self.path.endswith((".ps1", ".sha256")):
            content_type = "text/plain"
        else:
            content_type = "application/json"
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *args: object) -> None:
        del args


@contextmanager
def fixture_server():
    server = FixtureServer(("127.0.0.1", 0), FixtureHandler)
    server.responses = {}
    server.requests = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        thread.join()
        server.server_close()


def release_json(tag: str, *, prerelease: bool) -> bytes:
    return json.dumps(
        {"tag_name": tag, "prerelease": prerelease},
        indent=2,
    ).encode()


def archive_bytes(version: str, target: str) -> bytes:
    package = f"latte-lens-{version}-{target}"
    binary = (
        "#!/bin/sh\n"
        "if [ \"$1\" = \"--version\" ]; then\n"
        f"  printf 'latte-lens {version}\\n'\n"
        "  exit 0\n"
        "fi\n"
        "if [ \"$1\" = \"hooks\" ] && [ \"$2\" = \"setup\" ]; then\n"
        "  if [ -n \"${LATTE_LENS_TEST_HOOK_LOG:-}\" ]; then\n"
        "    printf 'hooks setup\\n' >> \"$LATTE_LENS_TEST_HOOK_LOG\"\n"
        "  fi\n"
        "  exit \"${LATTE_LENS_TEST_HOOK_EXIT:-0}\"\n"
        "fi\n"
        "exit 1\n"
    ).encode()
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        info = tarfile.TarInfo(f"{package}/latte-lens")
        info.mode = 0o755
        info.size = len(binary)
        archive.addfile(info, io.BytesIO(binary))
    return output.getvalue()


class InstallTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)
        self.fake_bin = self.root / "fake-bin"
        self.fake_bin.mkdir()
        fake_uname = self.fake_bin / "uname"
        fake_uname.write_text(
            "#!/bin/sh\n"
            "case \"$1\" in\n"
            "  -s) printf '%s\\n' \"$TEST_UNAME_S\" ;;\n"
            "  -m) printf '%s\\n' \"$TEST_UNAME_M\" ;;\n"
            "  *) exit 2 ;;\n"
            "esac\n",
            encoding="utf-8",
        )
        fake_uname.chmod(0o755)

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def env(
        self,
        server: FixtureServer,
        *,
        operating_system: str = "Linux",
        architecture: str = "x86_64",
    ) -> dict[str, str]:
        host, port = server.server_address
        environment = os.environ.copy()
        environment.update(
            {
                "HOME": str(self.root / "home"),
                "PATH": f"{self.fake_bin}:{environment['PATH']}",
                "TEST_UNAME_S": operating_system,
                "TEST_UNAME_M": architecture,
                "LATTE_LENS_API_URL": f"http://{host}:{port}/api",
                "LATTE_LENS_DOWNLOAD_URL": f"http://{host}:{port}/downloads",
                "LATTE_LENS_INSTALL_DIR": str(self.root / "install"),
            }
        )
        environment.pop("GITHUB_TOKEN", None)
        environment.pop("LATTE_LENS_VERSION", None)
        return environment

    def add_package(
        self,
        server: FixtureServer,
        tag: str,
        target: str,
        *,
        valid_checksum: bool = True,
    ) -> None:
        version = tag.removeprefix("v")
        archive_name = f"latte-lens-{version}-{target}.tar.gz"
        body = archive_bytes(version, target)
        digest = hashlib.sha256(body).hexdigest()
        if not valid_checksum:
            digest = "0" * 64
        server.responses[f"/downloads/{tag}/{archive_name}"] = (200, body)
        server.responses[f"/downloads/{tag}/{archive_name}.sha256"] = (
            200,
            f"{digest}  {archive_name}\n".encode(),
        )

    def run_installer(
        self,
        environment: dict[str, str],
        *arguments: str,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["sh", str(INSTALLER), *arguments],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
        )

    def test_installs_latest_stable_linux_x86_64_with_checksum(self) -> None:
        with fixture_server() as server:
            tag = "v1.2.3"
            server.responses["/api/releases/latest"] = (
                200,
                release_json(tag, prerelease=False),
            )
            self.add_package(server, tag, "x86_64-unknown-linux-musl")

            result = self.run_installer(self.env(server))

            self.assertEqual(result.returncode, 0, result.stderr)
            installed = self.root / "install" / "latte-lens"
            self.assertTrue(installed.is_file())
            self.assertTrue(os.access(installed, os.X_OK))
            version = subprocess.run(
                [str(installed), "--version"],
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            self.assertEqual(version, "latte-lens 1.2.3")
            self.assertIn("installed latte-lens 1.2.3", result.stdout)
            self.assertIn("is not in your PATH", result.stdout)
            self.assertIn("Code Agent hooks were not configured", result.stdout)

    def test_yes_flag_runs_hook_setup_after_binary_install(self) -> None:
        with fixture_server() as server:
            tag = "v1.2.3"
            server.responses["/api/releases/latest"] = (
                200,
                release_json(tag, prerelease=False),
            )
            self.add_package(server, tag, "x86_64-unknown-linux-musl")
            environment = self.env(server)
            hook_log = self.root / "hook-setup.log"
            environment["LATTE_LENS_TEST_HOOK_LOG"] = str(hook_log)

            result = self.run_installer(environment, "-y")

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(hook_log.read_text(encoding="utf-8"), "hooks setup\n")
            self.assertIn("configured Code Agent hooks", result.stdout)

    def test_hook_setup_failure_keeps_binary_and_reports_rollback(self) -> None:
        with fixture_server() as server:
            tag = "v1.2.3"
            server.responses["/api/releases/latest"] = (
                200,
                release_json(tag, prerelease=False),
            )
            self.add_package(server, tag, "x86_64-unknown-linux-musl")
            environment = self.env(server)
            environment["LATTE_LENS_TEST_HOOK_EXIT"] = "9"

            result = self.run_installer(environment, "--yes")

            self.assertNotEqual(result.returncode, 0)
            self.assertTrue((self.root / "install/latte-lens").is_file())
            self.assertIn("modified configs were rolled back", result.stdout)

    def test_falls_back_to_latest_preview_on_macos_arm64(self) -> None:
        with fixture_server() as server:
            tag = "v0.2.0-beta.1"
            server.responses["/api/releases?per_page=1"] = (
                200,
                json.dumps(
                    [{"tag_name": tag, "prerelease": True}],
                    indent=2,
                ).encode(),
            )
            self.add_package(server, tag, "aarch64-apple-darwin")

            result = self.run_installer(
                self.env(server, operating_system="Darwin", architecture="arm64")
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("falling back to the latest preview", result.stdout)
            self.assertIn("installing preview release", result.stdout)
            self.assertIn(
                "/downloads/v0.2.0-beta.1/"
                "latte-lens-0.2.0-beta.1-aarch64-apple-darwin.tar.gz",
                server.requests,
            )

    def test_requested_version_uses_release_tag_endpoint(self) -> None:
        with fixture_server() as server:
            tag = "v1.1.0"
            server.responses[f"/api/releases/tags/{tag}"] = (
                200,
                release_json(tag, prerelease=False),
            )
            self.add_package(server, tag, "x86_64-apple-darwin")
            environment = self.env(
                server,
                operating_system="Darwin",
                architecture="x86_64",
            )
            environment["LATTE_LENS_VERSION"] = "1.1.0"

            result = self.run_installer(environment)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f"/api/releases/tags/{tag}", server.requests)
            self.assertNotIn("/api/releases/latest", server.requests)

    def test_checksum_failure_preserves_existing_install(self) -> None:
        with fixture_server() as server:
            tag = "v1.2.3"
            server.responses["/api/releases/latest"] = (
                200,
                release_json(tag, prerelease=False),
            )
            self.add_package(
                server,
                tag,
                "x86_64-unknown-linux-musl",
                valid_checksum=False,
            )
            install_dir = self.root / "install"
            install_dir.mkdir()
            installed = install_dir / "latte-lens"
            installed.write_text("existing install", encoding="utf-8")

            result = self.run_installer(self.env(server))

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("checksum verification failed", result.stderr)
            self.assertEqual(installed.read_text(encoding="utf-8"), "existing install")

    def test_rejects_unsupported_architecture_before_downloading(self) -> None:
        with fixture_server() as server:
            result = self.run_installer(
                self.env(server, architecture="riscv64")
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("unsupported architecture: riscv64", result.stderr)
            self.assertEqual(server.requests, [])


if __name__ == "__main__":
    unittest.main()
