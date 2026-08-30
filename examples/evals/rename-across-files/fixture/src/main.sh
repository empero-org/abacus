#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=./greet.sh
source "$(dirname "$0")/greet.sh"

say_hello "world"
