#!/usr/bin/env sh
# Run axtail from the repository root, building it first if needed.
# Usage: ./run.sh [axtail arguments], e.g. ./run.sh /var/log --grep ERROR
set -eu
root="$(cd "$(dirname "$0")" && pwd)"
if [ ! -x "$root/axtail" ]; then
    "$root/build.sh"
fi
exec "$root/axtail" "$@"
