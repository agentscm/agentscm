"""Contract tests for the PR title check."""

import subprocess
import sys
import unittest
from pathlib import Path

VALIDATOR = Path(__file__).with_name("validate_pr_title.py")


class ValidatePrTitleTests(unittest.TestCase):
    def run_validator(self, title: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(VALIDATOR), title],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_accepts_supported_title_forms(self) -> None:
        for title in (
            "feat: Add candidate fingerprints",
            "fix(discovery): Preserve changed paths",
            "ci!: Change the release check",
            "docs(api/v2): Explain the contract",
        ):
            with self.subTest(title=title):
                self.assertEqual(self.run_validator(title).returncode, 0)

    def test_rejects_invalid_titles(self) -> None:
        for title in (
            "Add candidate fingerprints",
            "feature: Add candidate fingerprints",
            "feat: ",
            "feat(Discovery): Add candidate fingerprints",
            "feat: Add candidate fingerprints ",
            "feat: " + "a" * 67,
        ):
            with self.subTest(title=title):
                result = self.run_validator(title)
                self.assertEqual(result.returncode, 1)
                self.assertTrue(result.stderr)


if __name__ == "__main__":
    unittest.main()
