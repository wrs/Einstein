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
#                 surface (real impls or no-op stubs).
#
# Allowed direction, low to high: arch ← hv ← newton. main.rs wires
# everything together and may import all layers.
#
# Skeleton phase: the layer directories don't exist yet (they arrive in
# the directory-move phase). Until then this script only verifies that
# fact and exits 0, so it can already be wired into check-matrix.sh;
# the per-edge grep rules tighten here as the layers land.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"

# Full layer list: arch hv newton peripherals host diag.
# src/peripherals already exists (it keeps its place in the layered
# tree), so only the directories introduced by the refactor gate the
# skeleton→lint transition.
new_layers=(arch hv newton host diag)

present=()
for layer in "${new_layers[@]}"; do
    [[ -d "$root/src/$layer" ]] && present+=("$layer")
done

if [[ ${#present[@]} -eq 0 ]]; then
    echo "check-layering: no layer directories yet — skeleton, nothing to lint"
    exit 0
fi

# Layer directories exist but no import rules are implemented yet.
# Fail loudly rather than green-lighting unchecked imports: whoever
# creates the first layer directory must land the edge rules here in
# the same change.
echo "check-layering: layer dirs present (${present[*]}) but lint rules"
echo "check-layering: are not implemented yet — add the per-edge import"
echo "check-layering: checks to scripts/check-layering.sh"
exit 1
