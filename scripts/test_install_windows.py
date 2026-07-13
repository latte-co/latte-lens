#!/usr/bin/env python3
"""Windows end-to-end tests for the PowerShell release installer."""

from __future__ import annotations

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

from scripts.test_install import fixture_server, release_json


ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "install.ps1"


@unittest.skipUnless(os.name == "nt", "PowerShell installer requires Windows")
class WindowsInstallTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cargo_toml = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        match = re.search(r'^version = "([^"]+)"$', cargo_toml, re.MULTILINE)
        if match is None:
            raise AssertionError("Cargo.toml did not include a package version")
        cls.version = match.group(1)
        cls.tag = f"v{cls.version}"
        cls.archive_name = (
            f"latte-lens-{cls.version}-x86_64-pc-windows-msvc.zip"
        )
        cls.archive = ROOT / "dist" / cls.archive_name
        cls.checksum = Path(f"{cls.archive}.sha256")
        if not cls.archive.is_file() or not cls.checksum.is_file():
            raise unittest.SkipTest("build the Windows release package before this test")
        cls.powershell = shutil.which("powershell.exe") or shutil.which("pwsh.exe")
        if cls.powershell is None:
            raise unittest.SkipTest("PowerShell was not found")

    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary_directory.name)

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def environment(self, server) -> dict[str, str]:
        host, port = server.server_address
        environment = os.environ.copy()
        environment.update(
            {
                "LATTE_LENS_API_URL": f"http://{host}:{port}/api",
                "LATTE_LENS_DOWNLOAD_URL": f"http://{host}:{port}/downloads",
                "LATTE_LENS_INSTALL_DIR": str(self.root / "install"),
                "LATTE_LENS_NO_MODIFY_PATH": "1",
            }
        )
        environment.pop("GITHUB_TOKEN", None)
        environment.pop("LATTE_LENS_VERSION", None)
        return environment

    def add_package(self, server, *, valid_checksum: bool = True) -> None:
        archive_path = f"/downloads/{self.tag}/{self.archive_name}"
        server.responses[archive_path] = (200, self.archive.read_bytes())
        checksum = self.checksum.read_bytes() if valid_checksum else b"0" * 64
        server.responses[f"{archive_path}.sha256"] = (200, checksum)

    def run_installer(self, environment: dict[str, str], server) -> subprocess.CompletedProcess[str]:
        host, port = server.server_address
        server.responses["/install.ps1"] = (200, INSTALLER.read_bytes())
        command = (
            f"Invoke-RestMethod -Uri 'http://{host}:{port}/install.ps1' "
            "-UseBasicParsing | Invoke-Expression"
        )
        return subprocess.run(
            [
                self.powershell,
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
        )

    def test_installs_latest_preview_and_runs_real_binary(self) -> None:
        with fixture_server() as server:
            server.responses["/api/releases?per_page=1"] = (
                200,
                b'[{"tag_name":"' + self.tag.encode() + b'","prerelease":true}]',
            )
            self.add_package(server)

            result = self.run_installer(self.environment(server), server)

            self.assertEqual(result.returncode, 0, result.stderr)
            installed = self.root / "install" / "latte-lens.exe"
            self.assertTrue(installed.is_file())
            version = subprocess.run(
                [str(installed), "--version"],
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            self.assertEqual(version, f"latte-lens {self.version}")
            self.assertIn("falling back to the latest preview", result.stdout)
            self.assertIn(f"installed latte-lens {self.version}", result.stdout)

    def test_checksum_failure_preserves_existing_install(self) -> None:
        with fixture_server() as server:
            server.responses["/api/releases/latest"] = (
                200,
                release_json(self.tag, prerelease=False),
            )
            self.add_package(server, valid_checksum=False)
            install_dir = self.root / "install"
            install_dir.mkdir()
            installed = install_dir / "latte-lens.exe"
            installed.write_bytes(b"existing install")

            result = self.run_installer(self.environment(server), server)

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("checksum verification failed", result.stderr)
            self.assertEqual(installed.read_bytes(), b"existing install")

    def test_requested_version_uses_release_tag_endpoint(self) -> None:
        with fixture_server() as server:
            server.responses[f"/api/releases/tags/{self.tag}"] = (
                200,
                release_json(self.tag, prerelease=True),
            )
            self.add_package(server)
            environment = self.environment(server)
            environment["LATTE_LENS_VERSION"] = self.version

            result = self.run_installer(environment, server)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f"/api/releases/tags/{self.tag}", server.requests)
            self.assertNotIn("/api/releases/latest", server.requests)


if __name__ == "__main__":
    unittest.main()
