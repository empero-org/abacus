#!/usr/bin/env bash
# Pass when the suite actually ran to completion. run_tests.sh writes the
# marker only on success, so the marker cannot be produced by reading the
# script and guessing.
set -uo pipefail

[ -f .test-passed ] || {
  echo "the suite never completed: .test-passed is absent"
  exit 1
}
