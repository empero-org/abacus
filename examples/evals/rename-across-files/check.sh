#!/usr/bin/env bash
# Pass when the rename is complete across code and docs and the script still runs.
set -uo pipefail

fail() { echo "$1"; exit 1; }

if grep -rq --exclude-dir=.git "say_hello" .; then
  fail "the old name say_hello is still present:
$(grep -rn --exclude-dir=.git 'say_hello' .)"
fi

for file in src/greet.sh src/main.sh README.md; do
  [ -f "$file" ] || fail "$file is missing"
  grep -q "greet_user" "$file" || fail "greet_user missing from $file"
done

output=$(bash src/main.sh 2>&1) || fail "src/main.sh failed to run: $output"
[ "$output" = "Hello, world!" ] || fail "unexpected output: $output"
