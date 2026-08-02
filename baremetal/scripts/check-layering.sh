#!/usr/bin/env bash
# Import-discipline lint for the layered source tree (see the layering
# refactor plan / docs/review-2026-06). One crate, six layer directories
# mirroring the eventual crates; this script enforces the dependency
# direction between them:
#
#   arch        — pure AArch64/AArch32 mechanism; zero upward deps.
#   hv          — generic hypervisor core; may use arch. Sanctioned
#                 hv→peripherals edge: src/hv/mmio.rs (the MMIO
#                 router) is THE single file allowed to import
#                 peripherals — its closed-enum PeriphId match is the
#                 dispatch point, so a forgotten model stays a compile
#                 error (no fn-pointer registration). No other hv file
#                 may import peripherals. Sanctioned hv→newton edge:
#                 src/hv/hooks.rs is THE single file allowed to name
#                 newton — its `type ActiveGuest = newton::NewtonOs`
#                 alias is the GuestOs hook seam; every other hv file
#                 reaches Newton logic via `hooks::ActiveGuest::…`.
#   newton      — Newton-OS-specific logic; may use arch, hv,
#                 peripherals.
#   peripherals — guest device models; usable by newton and (via the
#                 router) hv. Sanctioned peripherals→hv edge: models
#                 may import the hv service modules hv::{layout,
#                 guest_endian, guest_mem} and hv::mmio (the
#                 MmioPeripheral trait). Everything else in hv —
#                 trap internals, stage2, timer, snapshot — stays
#                 forbidden.
#   host        — host drivers/backends; below main, not imported by
#                 the guest-facing layers — with one global exception:
#                 host::platform is the board API (conceptually the
#                 lowest crate despite its location) and is importable
#                 from ANY layer. Sanctioned upward edges from host:
#                 backends may call the peripherals::{vic::raise_*,
#                 vic::is_powered_off, queue} event-injection APIs
#                 (host feeds events into guest models; the powered-off
#                 read backs Einstein's first-tap-is-power-button
#                 policy in the input layer), and may consume the
#                 hv::{guest_mem, guest_endian} read/translate
#                 accessors (host renders/streams guest memory: pi_fb
#                 scaling, pi_hdmi PCM reads). Nothing else crosses.
#   diag        — diagnostics; reachable from anywhere via its stable
#                 surface (real impls behind cfg(nh_diag), no-op stubs
#                 otherwise — see src/diag/mod.rs). Any layer may
#                 import diag; diag's own imports are unconstrained —
#                 it sits atop every layer and renders their state.
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
# no longer match anything — the allowlist may only shrink as the
# remaining legacy edges are removed.
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
# constants, the is_powered_off read (input-layer power-button
# policy), or a plain module import of peripherals::vic (whose only
# legitimate use from host is raising interrupts into the guest model).
host_sanctioned='peripherals::vic::(raise|INT_|is_powered_off)|use crate::([a-z_:, {]*)peripherals::vic[};]'

# Sanctioned peripherals→hv service modules: layout, guest_endian,
# guest_mem, and mmio (the MmioPeripheral trait). A line is sanctioned
# only if ALL its hv:: references target these modules — checked by
# stripping the sanctioned references and testing for leftovers, so a
# grouped import mixing hv::mmio with e.g. hv::stage2 still flags.
periph_sanctioned_strip='hv::(layout|guest_endian|guest_mem|mmio)'

# Sanctioned host→hv service modules: the guest_mem / guest_endian
# read/translate accessors (host backends consume guest memory — fb
# scaling, PCM sample reads). Same strip-and-test mechanics.
host_hv_sanctioned_strip='hv::(guest_mem|guest_endian)'

# host::platform is globally importable (see the layer table above).
# Applied to every layer's forbidden-host scan via strip-and-test.
host_platform_strip='host::platform'

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
            # host::platform is the board API — importable from any
            # layer; skip when no other host:: reference remains.
            if [[ "$forb" == host ]]; then
                residue="$(sed -E "s/${host_platform_strip}//g" <<<"$text")"
                if ! grep -qE '(^|[^A-Za-z0-9_])host::' <<<"$residue"; then
                    continue
                fi
            fi
            # Host backends may use the hv guest-memory accessors; skip
            # only when no unsanctioned hv:: reference remains.
            if [[ "$layer" == host && "$forb" == hv ]]; then
                residue="$(sed -E "s/${host_hv_sanctioned_strip}//g" <<<"$text")"
                if ! grep -qE '(^|[^A-Za-z0-9_])hv::' <<<"$residue"; then
                    continue
                fi
            fi
            # The MMIO router is the single sanctioned hv→peripherals
            # edge (closed-enum dispatch to the device models).
            if [[ "$layer" == hv && "$forb" == peripherals \
                && "$file" == src/hv/mmio.rs ]]; then
                continue
            fi
            # hooks.rs is the single sanctioned hv→newton edge (the
            # `ActiveGuest = newton::NewtonOs` GuestOs seam).
            if [[ "$layer" == hv && "$forb" == newton \
                && "$file" == src/hv/hooks.rs ]]; then
                continue
            fi
            # Models may use the hv service modules; skip only when no
            # unsanctioned hv:: reference remains on the line.
            if [[ "$layer" == peripherals && "$forb" == hv ]]; then
                residue="$(sed -E "s/${periph_sanctioned_strip}//g" <<<"$text")"
                if ! grep -qE '(^|[^A-Za-z0-9_])hv::' <<<"$residue"; then
                    continue
                fi
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
echo "check-layering: OK — 0 unlisted violations ($allowed allowlisted legacy edges)"
