#!/usr/bin/env python3
"""Validate the Conventional Commit title used for a squash-merged PR."""

import re
import sys

ALLOWED_TYPES = (
    "build",
    "chore",
    "ci",
    "docs",
    "feat",
    "fix",
    "perf",
    "refactor",
    "revert",
    "style",
    "test",
)
TYPE_PATTERN = "|".join(ALLOWED_TYPES)
TITLE_PATTERN = re.compile(
    rf"^(?P<type>{TYPE_PATTERN})(?:\((?P<scope>[a-z0-9][a-z0-9._/-]*)\))?"
    r"(?P<breaking>!)?: (?P<subject>\S(?:.*\S)?)$"
)
MAX_TITLE_LENGTH = 72


def validate_title(title: str) -> str | None:
    if len(title) > MAX_TITLE_LENGTH:
        return f"PR title exceeds {MAX_TITLE_LENGTH} characters"
    if TITLE_PATTERN.fullmatch(title) is None:
        allowed = ", ".join(ALLOWED_TYPES)
        return (
            "PR title must be <type>[optional scope][!]: <subject>; "
            f"allowed types: {allowed}"
        )
    return None


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: validate_pr_title.py <title>", file=sys.stderr)
        return 2
    error = validate_title(sys.argv[1])
    if error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
