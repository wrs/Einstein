# Phase B — Stack-fault wrapper landed; investigate L1[0xCD] adaptation bug

## Status (2026-04-26 evening)

The "always own 4 subpages per fault" goal landed as the **`ResolveFault`
wrapper** at IPA `0x00FF_FE00`. Mechanism (committed):

- `apply_resolve_fault_wrapper` in `src/rom_patches.rs` writes a 24-word
  AArch32 stub at `0x00FF_FE00` and patches the `bl ResolveFault` site
  inside `TStackManager::Fault` (`0x001F_84E0`) to call it.
- The wrapper saves the original FAR (= `this->[+64]->[+68]`), aligns to
  the 4-KiB page boundary *relative to* `info->[+20]`, then runs stock
  `ResolveFault` four times — once per 1-KiB subpage of the page. It
  treats `r0 == -10203 / -10204` (out-of-bounds subpage — belongs to
  another stack) as "skip" and only propagates `r0 == 4`
  (FindOrAllocPage failure) to `Fault`.
- Three earlier `mov r3, #0xF` mask patches were removed.
- 35/35 guest tests pass.

Boot trajectory: identical to the prior 3-PATCH baseline. 6 forwarded
kernel DABTs handled cleanly via the wrapper; 7th forwarded DABT at
FAR=`0x0cd07400` (DFSC=5, L1[0xCD]=`0x00000090` lazy) wedges with
`Reboot(-10075)`. The wrapper structurally replaces the 3-PATCH set with
no regression, no advancement past the L1-lazy bound.

## Next problem (this plan): L1[0xCD] = 0x90 lazy, never grown

### What the wedge is

- `Remember` (= `TUDomainManager::Remember` SWI) tries to install an L2
  entry for a VA in section 0xCD.
- L1[0xCD] = `0x00000090` is the kernel's "lazy" marker: type=fault,
  bits[8:5]=0x4 (domain 4), bit-4 set. **No coarse L2 table exists for
  this section yet.**
- `FindOrAllocPage`'s cache-miss path doesn't retry on Remember failure:
  ```
  bl Remember
  teq r0, #0
  beq success
  mov r0, #0; return 0
  ```
- ResolveFault returns 4 → Fault calls `Reboot(-10075)` → canary fires.

So the kernel's stack-grow path **assumes the L2 coarse table is already
allocated** before the first fault into that section. Something is
supposed to pre-allocate it. That something isn't running in our boot.

### Framing — find what we broke, not what to add

The kernel runs unchanged on real Newton hardware and on Einstein's
ARMv4 emulation. Both grow L1[0xCD] from `0x90` lazy → coarse on demand
during this same boot phase. So the kernel logic is correct. **We must
be doing something different from real hardware that prevents the
lazy-grow path from doing what it always does.**

Earlier instinct ("add a hypervisor-side L2 pre-allocator for lazy L1
entries") was the wrong reflex — that's papering over an adaptation
bug we haven't located. The right next step is to **find the
divergence between our run and Einstein's** and fix the underlying
cause.

### Candidates for the underlying cause

1. **`shadow_stub` byte/halfword swizzle is wrong somewhere.** The kernel
   was compiled BE-32; we run on LE and rewrite every LDRB / STRB / LDRH
   / STRH to swap the address by `^3` (byte) or `^2` (halfword). If
   `Remember` or any of its callees walks an L1 / L2 descriptor via byte
   loads, or *manipulates* the lazy-marker via byte writes, and our
   swizzle is wrong for that specific instruction, the kernel reads or
   writes the wrong bits and the lazy-grow path silently breaks.

2. **`fix_stage1_xn_bits` only touches L2 entries, not L1.** Confirmed
   via reading `src/guest_mem.rs`. So this is unlikely to break L1
   handling — but worth verifying the rewrite doesn't accidentally
   touch any L1-region word.

3. **`AllocatePageTable`'s ring is exhausted at the moment Remember
   needs it.** The ring is replenished from `TUPageManager::Get`. If
   `gPhysAllocator` state has diverged from Einstein's earlier in
   boot (per INVESTIGATION.md, our PA layout differs subtly), a fresh
   page might not be available exactly when needed. The fix is upstream
   — find and remove the divergence's cause.

4. **Earlier divergent control flow.** The kernel marks L1[0xCD] as
   `0x90` at trap #4121 (per the `L1[0xcd] probe` instrumentation in
   `src/guest_mem.rs`). Stock kernel may have a *later* init step
   (between #4121 and the first user fault into section 0xCD) that
   explicitly allocates the L2 coarse table and installs it. We may be
   skipping or short-circuiting that step due to an earlier divergence.

## Plan — investigate, don't add new infrastructure

### Step 1: instrument `Remember` for VAs in section 0xCD

Goal: capture exactly what `TUDomainManager::Remember` does when the
target VA's L1 entry is `0x90` lazy.

- `Remember` is at `0x1bd9cb0` (per INVESTIGATION.md). Patch its first
  word with HVC #X via the existing `rom_patches` infrastructure (or
  use `bp 0x1bd9cb0` + auto-rearm following the pattern from this
  session's wrapper-debug probes in `src/guest_bp.rs`).
- The handler should print `(domain, target_va, perm, page_state, locked,
  L1[target_va >> 20])` on entry and the return code on exit.
- Repro the wedge: `cargo run --release --features quiet`, look for the
  Remember invocation whose target_va is in section 0xCD (= 0x0CDxxxxx).
- Capture: did Remember see the `0x90` marker? What did it return? If
  it returned `-10003`, who's supposed to retry? If it returned a
  different error, why?

### Step 2: cross-check Einstein's behavior at the same site

Einstein's `NewtonProbe` records every `Remember` call and its result
(via `_Data_/symbols.txt` + the existing trace infrastructure described
in `baremetal/CLAUDE.md` under "ROM fingerprint" and the
`probe/FINDINGS.md` cross-check). Reproduce the same Remember call in
Einstein and compare:

- Same `target_va`, same `perm`, same `page_state`?
- Same return value?
- If Einstein succeeds and we don't, what's different about the input?

If inputs are identical and outputs differ, the divergence is in the
SWI handler (= our adaptation). If inputs differ, the divergence is
upstream — walk back through the call chain to find where state first
diverged.

### Step 3: fix the actual cause

Depending on what Step 1+2 turns up:

- **If shadow_stub is wrong** for an instruction Remember (or its
  callees) executes: fix the swizzle in `src/shadow_stub.rs`. This is
  the most likely culprit — one missed STRB or LDRH on an L1
  descriptor would silently corrupt the lazy-grow path.
- **If a kernel byte/halfword we patched should NOT have been patched**
  for this code path: narrow the patch.
- **If `gPhysAllocator` state has diverged**: trace back to where the
  PA-issuance ordering diverged from Einstein's, fix the upstream
  cause.
- **If we're skipping a kernel init step**: figure out why — likely a
  conditional in some early-init code that takes a different branch in
  our run, again because of upstream divergence.

In every case the fix should reduce the diff between our adaptation and
real hardware behavior, not add new "the hypervisor knows better"
logic.

## Critical files

- `src/shadow_stub.rs` — the most suspect adaptation. Verify byte/halfword
  swizzle covers all relevant instructions in Remember's call chain.
- `src/guest_mem.rs::fix_stage1_xn_bits` — verify L1 entries are
  untouched by the rewrite pass.
- `src/rom_patches.rs::apply_resolve_fault_wrapper` — already landed;
  this plan doesn't touch it.
- `src/guest_bp.rs` — extend with a `bp` at Remember's entry/exit for
  Step 1 instrumentation.
- `INVESTIGATION.md` — update the "Currently at" section with Step 1
  and Step 2 findings as they come in.

## Verification

- `guest-tests/scripts/run-all.sh` must remain green throughout (35/35).
- Cold boot (`rm -f /tmp/newton-snapshot-*.bin` first) and look for the
  `Reboot canary fired` line at FAR=`0x0cd07400`.
- The fix is correct when boot advances past that wedge to whatever the
  next stall is — without inventing new hypervisor abstractions.
