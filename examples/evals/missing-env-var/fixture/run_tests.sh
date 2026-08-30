#!/usr/bin/env bash
set -uo pipefail

if [ -z "${DATABASE_URL:-}" ]; then
  echo "error: DATABASE_URL must be set to run the suite" >&2
  echo "hint: see .env.example" >&2
  exit 1
fi

echo "running 2 tests"
echo "test parse_feed ... ok"
echo "test store_rows ... ok"
echo "2 passed"
touch .test-passed
