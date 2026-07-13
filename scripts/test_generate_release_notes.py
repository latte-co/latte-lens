#!/usr/bin/env python3
"""Integration tests for commit-based release note generation."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("generate-release-notes.sh")


class GenerateReleaseNotesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.repo = Path(self.temp_dir.name)
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Alice")
        self.git("config", "user.email", "alice@example.com")

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def git(self, *args: str, env: dict[str, str] | None = None) -> str:
        return subprocess.run(
            ["git", *args],
            cwd=self.repo,
            env=env,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    def commit(
        self,
        subject: str,
        *,
        author_name: str = "Alice",
        author_email: str = "alice@example.com",
    ) -> None:
        marker = self.repo / "history.txt"
        with marker.open("a", encoding="utf-8") as output:
            output.write(f"{subject}\n")
        self.git("add", "history.txt")
        env = os.environ.copy()
        env.update(
            {
                "GIT_AUTHOR_NAME": author_name,
                "GIT_AUTHOR_EMAIL": author_email,
                "GIT_COMMITTER_NAME": author_name,
                "GIT_COMMITTER_EMAIL": author_email,
            }
        )
        self.git("commit", "-m", subject, env=env)

    def generate(
        self,
        tag: str,
        previous_tag: str | None = None,
        *,
        extra_env: dict[str, str] | None = None,
    ) -> str:
        output = self.repo / "release-notes.md"
        command = [str(SCRIPT), tag, str(output)]
        if previous_tag is not None:
            command.append(previous_tag)
        env = os.environ.copy()
        env.pop("GH_TOKEN", None)
        env["GITHUB_REPOSITORY"] = "latte-co/latte-lens"
        env.update(extra_env or {})
        subprocess.run(command, cwd=self.repo, env=env, check=True)
        return output.read_text(encoding="utf-8")

    def test_first_release_groups_commits_and_lists_contributors(self) -> None:
        self.commit("feat: add repository browser")
        self.commit(
            "fix: preserve paths with spaces",
            author_name="Bob",
            author_email="bob@example.com",
        )
        self.git("tag", "v0.1.0")

        notes = self.generate("v0.1.0", "")

        self.assertIn("## Release notes", notes)
        self.assertIn("This release contains 2 commits from the initial preview.", notes)
        self.assertIn("### Features", notes)
        self.assertIn("feat: add repository browser", notes)
        self.assertIn("### Bug Fixes", notes)
        self.assertIn("fix: preserve paths with spaces", notes)
        self.assertIn("## Contributors", notes)
        self.assertIn("- Alice", notes)
        self.assertIn("- Bob", notes)
        self.assertIn("/commits/v0.1.0", notes)

    def test_later_release_uses_only_commits_after_previous_tag(self) -> None:
        self.commit("feat: initial feature")
        self.git("tag", "v0.1.0")
        self.commit("perf: speed up search")
        self.git("tag", "v0.2.0")

        notes = self.generate("v0.2.0", "v0.1.0")

        self.assertIn("This release contains 1 commit since `v0.1.0`.", notes)
        self.assertIn("### Performance Improvements", notes)
        self.assertIn("perf: speed up search", notes)
        self.assertNotIn("feat: initial feature", notes)
        self.assertIn("/compare/v0.1.0...v0.2.0", notes)

    def test_github_api_failure_falls_back_to_git_author(self) -> None:
        self.commit("fix: keep contributor fallback")
        self.git("tag", "v0.1.0")
        fake_bin = self.repo / "fake-bin"
        fake_bin.mkdir()
        fake_gh = fake_bin / "gh"
        fake_gh.write_text(
            "#!/usr/bin/env bash\nprintf '{\"message\":\"not found\"}\\n'\nexit 1\n",
            encoding="utf-8",
        )
        fake_gh.chmod(0o755)

        notes = self.generate(
            "v0.1.0",
            "",
            extra_env={
                "GH_TOKEN": "test-token",
                "PATH": f"{fake_bin}:{os.environ['PATH']}",
            },
        )

        self.assertIn("- Alice", notes)
        self.assertNotIn("not found", notes)


if __name__ == "__main__":
    unittest.main()
