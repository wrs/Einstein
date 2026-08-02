#!/usr/bin/env bash
# Import-discipline lint for the layered source tree (see the layering
# refactor plan / docs/review-2026-06). One crate, six layer directories
# mirroring the eventual crates; this script enforces the dependency
# direction between them:
#
#   arch        — pure AArch64/AArch32 mechanism; zero upward deps.
#   hv          — generic hypervisor core; may use arch.
#   newton      — Newton-OS-specific logic; may use arch, hv,
#                 peripherals.
#   peripherals — guest device models; usable by newton and hv.
#   host        — host drivers/backends; below main, not imported by
#                 the guest-facing layers. Sanctioned upward edge:
#                 host backends may call the peripherals::{vic::raise_*,
#                 queue} event-injection APIs (host feeds events into
#                 guest models). Nothing else crosses upward.
#   diag        — diagnostics; reachable from anywhere via its stable
#                 surface (real impls or no-op stubs). Any layer may
#                 import diag; diag's own imports are unconstrained
#                 until the phase-6 diag-layer split.
#
# Allowed direction, low to high: arch ← hv ← newton. main.rs and
# panic.rs wire everything together and may import all layers.
#
# Mechanics: for each layer directory, grep its files (line comments
# stripped) for references into forbidden layers — `crate::<layer>::`
# paths and `<layer>::` segments inside grouped `use crate::{…}`
# imports both match. Every hit must be covered by an entry in
# scripts/layering-allowlist.txt (`<file-glob> <line-regex> # phase N`);
# uncovered hits fail the lint, and so do stale allowlist entries that
# no longer match anything — the allowlist may only shrink as phases
# 3-7 remove the legacy edges.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
cd "$root"

allowfile="$here/layering-allowlist.txt"
[[ -f "$allowfile" ]] || { echo "check-layering: missing $allowfile"; exit 1; }

# Forbidden target layers per source layer.
layers=(arch hv newton peripherals host)
forbidden_arch="hv newton peripherals host"
forbidden_hv="newton peripherals host"
forbidden_newton="host"
forbidden_peripherals="hv newton host"
forbidden_host="hv newton peripherals"

# Sanctioned host→peripherals event-injection edge: vic raise_*/INT_*
# constants, or a plain module import of peripherals::vic (whose only
# legitimate use from host is raising interrupts into the guest model).
host_sanctioned='peripherals::vic::(raise|INT_)|use crate::([a-z_:, {]*)peripherals::vic[};]'

# Load allowlist entries (glob, regex), skipping comments/blank lines.
allow_globs=()
allow_regexes=()
allow_used=()
while read -r glob rx _rest; do
    [[ -z "${glob:-}" || "$glob" == \#* ]] && continue
    allow_globs+=("$glob")
    allow_regexes+=("$rx")
    allow_used+=(0)
done < "$allowfile"

violations=0
allowed=0

for layer in "${layers[@]}"; do
    forb_var="forbidden_$layer"
    for forb in ${!forb_var}; do
        pat="(^|[^A-Za-z0-9_])${forb}::"
        while IFS= read -r hit; do
            [[ -z "$hit" ]] && continue
            file="${hit%%:*}"
            rest="${hit#*:}"
            lineno="${rest%%:*}"
            text="${rest#*:}"
            if [[ "$layer" == host && "$forb" == peripherals ]] \
                && grep -qE "$host_sanctioned" <<<"$text"; then
                continue
            fi
            hit_allowed=0
            for i in "${!allow_globs[@]}"; do
                # Only consider entries aimed at this forbidden layer,
                # so one multi-import line can't be green-lit by an
                # entry covering a different edge on the same line.
                [[ "${allow_regexes[$i]}" == *"$forb"* ]] || continue
                # shellcheck disable=SC2053
                [[ "$file" == ${allow_globs[$i]} ]] || continue
                if grep -qE "${allow_regexes[$i]}" <<<"$text"; then
                    hit_allowed=1
                    allow_used[$i]=1
                    break
                fi
            done
            if [[ $hit_allowed == 1 ]]; then
                allowed=$((allowed + 1))
            else
                echo "check-layering: $file:$lineno: $layer must not import $forb:"
                echo "    ${text}"
                violations=$((violations + 1))
            fi
        done < <(find "src/$layer" -name '*.rs' | sort | while read -r f; do
            sed 's|//.*||' "$f" | grep -nE "$pat" | sed "s|^|$f:|"
        done)
    done
done

stale=0
for i in "${!allow_globs[@]}"; do
    if [[ ${allow_used[$i]} == 0 ]]; then
        echo "check-layering: stale allowlist entry (edge no longer exists — delete it):"
        echo "    ${allow_globs[$i]} ${allow_regexes[$i]}"
        stale=$((stale + 1))
    fi
done

if [[ $violations -gt 0 || $stale -gt 0 ]]; then
    echo "check-layering: FAIL — $violations unlisted violation(s), $stale stale allowlist entr(y/ies)"
    exit 1
fi
echo "check-layering: OK — 0 unlisted violations ($allowed allowlisted legacy edges pending phases 3-7)"
