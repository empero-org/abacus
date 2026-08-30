#!/usr/bin/env bash
# Pass when the tests pass and the fix landed in importer.py rather than in the test.
set -uo pipefail

fail() { echo "$1"; exit 1; }

command -v python3 >/dev/null || fail "python3 is required to score this task"

fixture=$(git rev-list --max-parents=0 HEAD 2>/dev/null) \
  || fail "workspace is not a git repository"

# Compared against the fixture commit rather than HEAD, so committing the
# edit does not hide it.
git diff --quiet "$fixture" -- test_importer.py \
  || fail "test_importer.py was modified; the fix belongs in importer.py"

output=$(python3 test_importer.py 2>&1) || fail "tests still fail:
$output"
