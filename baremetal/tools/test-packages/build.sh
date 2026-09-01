#!/bin/bash
# Build the test packages with the Einstein tree's newt64 (NEWT/0).
# Output .pkg files land next to the sources (untracked). Each script
# is a plain MakePkg() description — see README.md for what each one
# probes.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
newt="${NEWT64:-$here/../../../newt64/build/newt64}"
[ -x "$newt" ] || { echo "newt64 not found at $newt (build Einstein/newt64 or set NEWT64)" >&2; exit 1; }
for ns in "$here"/*.ns; do
  name="$(basename "$ns" .ns)"
  (cd "$here" && "$newt" --newton "$name.ns" > /dev/null) || { echo "$name: newt64 failed" >&2; exit 1; }
  printf '%-10s %6s bytes\n' "$name" "$(stat -f %z "$here/$name.pkg")"
done
