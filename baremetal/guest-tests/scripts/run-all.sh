#!/usr/bin/env bash
# Build every ARM-guest test and run it under QEMU. Exits 0 only if all
# tests report PASSED via HVC #3. Intended for CI and for a quick local
# green/red sanity pass.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"

"$here/build-tests.sh" >/dev/null

pass=0
fail=0
while read -r name; do
    [[ -z "$name" ]] && continue
    [[ "$name" =~ ^# ]] && continue
    if "$here/run-test.sh" "$name" </dev/null >/dev/null 2>&1; then
        printf "  \e[32mPASS\e[0m  %s\n" "$name"
        pass=$((pass+1))
    else
        printf "  \e[31mFAIL\e[0m  %s\n" "$name"
        fail=$((fail+1))
    fi
done < "$root/tests/MANIFEST"

echo
echo "  summary: $pass passed, $fail failed"
[[ $fail -eq 0 ]]
