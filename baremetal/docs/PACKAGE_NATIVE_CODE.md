# Package native code above the ROM aperture — design note

The known remaining functional gap is add-on app packages (the `.pkg`
install flow). Most of that work is guest-side (soups → flash store →
package loader) and rides on machinery that already works. The one place
where package code stresses a hypervisor invariant is **native code
inside a package**: it arrives at runtime in RAM (or flash), not at ROM
load time, so it is invisible to the build-time classifier that the rest
of the BE-8 code/data discipline leans on. This note states which
`inline_patch` "real code" invariants extend to that code, what the
dynamic stage-2 rescan path guarantees, and how to triage a wedge whose
PC is above the ROM aperture — because the bitmap-first doctrine in
`CLAUDE.md` does **not** apply there.

Grounded in: `src/newton/inline_patch.rs`, `src/hv/stage2.rs`
(`set_ram_page_ro_x` / `set_ram_page_rw_xn`, `ram_l3_entry_ptr`),
`src/hv/trap/mod.rs::handle_instruction_abort`, `src/hv/trap/dabt.rs`,
`src/hv/guest_endian.rs`, `tools/classify-rom`.

## What "real code" means today, and where the definition stops

The build-time classifier (`tools/classify-rom`) walks the ROM+REx image
from a vetted symbol root set and emits `reach.bitmap`: one bit per
32-bit word across the **16 MiB ROM aperture** (`0x0000_0000 ..
0x0100_0000`), set = code, clear = data. That bitmap drives two things:

1. **BE-8 code/data discrimination** (`guest_endian::pa_is_rom_code`).
   The guest runs BE-8, so EL2's own *data* reads of guest memory
   byte-swap to recover the Newton numerical value — except for ROM
   *code* words, which are stored native-LE (instruction fetch is always
   LE on the A53) and must come back un-swapped when EL2 decodes a
   faulting instruction in `handle_und`. The bitmap is what tells those
   two cases apart.

2. **The `inline_patch` liveness walker** (`live_regs_at`), which walks an
   APCS-conformant CFG forward from a PC to find dead caller-saved
   registers a stub can borrow. Its root set is the same vetted symbol
   list.

Both are **ROM-aperture-only**. `pa_is_rom_code` returns true only for
`pa + 4 <= ROM_SIZE` (plus the hypervisor's own runtime-written stub
windows, which it short-circuits explicitly). There is no bit, and no
classifier coverage, for any address ≥ `0x0100_0000` or anywhere in the
RAM aperture (`0x0400_0000 .. 0x0440_0000`). Package native code lands
there, so **none of the build-time "real code" knowledge applies to
it.**

### Which invariants *do* extend

- **Instruction fetch correctness does not depend on the bitmap.** The
  A53 fetches instructions itself, LE, straight out of stage-2-mapped
  host memory. The kernel's package loader is responsible for laying the
  native code down in the byte order the hardware fetches (the same
  contract demand-paged kernel code already satisfies). EL2's
  `guest_endian` byteswap is only ever applied to EL2's *data* reads, not
  to the hardware fetch path — so package code executes correctly
  regardless of classifier coverage.

- **The stage-2 RO+X ↔ RW+XN rescan invariant extends to any RAM page.**
  See below. This is the mechanism package code rides on, and it is
  address-driven (RAM aperture), not bitmap-driven.

### Which invariants *break*

- **EL2 emulation of a faulting instruction in RAM.** If package code
  takes a trap EL2 must decode (an unaligned access, a `SWP`, an
  FPA-class UND, a CP15 op) and the faulting PC is in RAM, EL2 reads the
  instruction word through `guest_endian`, which — having no bitmap bit
  for that address — **byteswaps it as data**. The decode is then
  garbage. The ROM path dodges this because the bitmap marks code words;
  RAM code has no such marking. This is the first thing that will bite
  package native code that isn't pure straight-line integer math.

- **The `inline_patch` inline-stub facility cannot be aimed at RAM.** Its
  stub pool lives in the ROM aperture and its liveness walker is seeded
  from ROM symbols; it has no notion of a RAM-resident function. Any
  `SWP`/FPA rewrite strategy for package code would need a different
  mechanism.

## What the dynamic rescan path guarantees

RAM is mapped `RW + XN` at stage-2. The first time the guest *fetches*
from a RAM page, the instruction abort lands in
`handle_instruction_abort` (`trap/mod.rs`): for a permission fault inside
the RAM aperture it flips that 4 KiB page to `RO + executable`
(`stage2::set_ram_page_ro_x`) and retries — subsequent fetches succeed.
The first time the guest *writes* that now-frozen page, the data abort
lands in `trap/dabt.rs`, which flips it back to `RW + XN`
(`stage2::set_ram_page_rw_xn`); the next fetch takes another XN trap and
the page is re-frozen. This is how Newton's demand-pager (which writes
code into RAM and then runs it) is modelled, and package native code
loaded into RAM rides the identical path.

What it guarantees:

- **Self-coherence of W→X transitions per page.** A page cannot be
  simultaneously writable and executable at stage-2; every code page the
  guest writes is re-frozen `RO+X` before it can be fetched again, so a
  fetch never sees a stale (pre-write) mapping. The `invalidate_ipa_s2`
  in each flip does the stage-2 TLB maintenance.

What it explicitly does **not** guarantee:

- It does **not** classify the bytes. The flip trusts the kernel's
  demand-pager / package loader: whatever the guest fetches is treated as
  code, full stop. There is no reach-bitmap consultation on the RAM path
  (`ram_l3_entry_ptr` is purely an address range check), so EL2 has no
  opinion about which RAM words are code vs data.
- It is **RAM-aperture-only.** `set_ram_page_ro_x` / `set_ram_page_rw_xn`
  no-op outside `0x0400_0000 .. 0x0440_0000`. Package code mapped at any
  other IPA would need its region added to the manifest and the rescan
  predicate first.
- It does **not** I-cache-maintain on the guest's behalf. The guest is
  responsible for its own `DCCMVAC`/`ICIMVAU` after writing code (the
  Newton kernel does this); EL2 only manages the stage-2 permission flip.

## Triage recipe when a wedge PC is above the aperture

The **bitmap-first triage** in `CLAUDE.md` ("is PC X marked as code in
the reach bitmap? if not, the loader byteswapped it wrong") is a
ROM-aperture procedure. It is silently meaningless for a PC ≥
`0x0100_0000` or in the RAM aperture — there is no bit to check. For a
wedge whose PC is above the aperture:

1. **Confirm the PC is RAM/flash-resident, not ROM.** `PC >= 0x0100_0000`
   or inside `0x0400_0000 .. 0x0440_0000` (RAM) means classifier triage
   does not apply — do not grep `code-regions.txt`.

2. **Check the stage-2 permission state of the page.** Is the page
   `RO+X` (was fetched, frozen) or `RW+XN` (was written, awaiting
   refetch)? A wedge fetching from a page still `RW+XN` means the
   instruction-abort flip didn't happen — look at
   `handle_instruction_abort`'s `in_ram && is_permission` gate and the
   IPA resolution, not at the bitmap.

3. **If EL2 is decoding a RAM instruction and getting garbage, suspect
   the byteswap.** `guest_endian` has no code bit for RAM, so any EL2
   emulation read of a RAM instruction word is byteswapped as data. A
   decoded instruction that looks endian-flipped (fields in the wrong
   nibbles) at a RAM PC is this, not a classifier miss. The fix lives in
   the RAM-code read path / a RAM analogue of `rom_word_is_code`, not in
   `tools/classify-rom`.

4. **Cross-check against the kernel's package-load + cache-maintenance
   sequence in the disasm**, and against Einstein's package loader as the
   oracle, before assuming a hypervisor bug. The loud-halt convention
   still holds: an unknown trap from RAM code halts with a context dump
   pointing at the faulting PC.

## Snapshot interaction (already safe)

The snapshot ROM fingerprint covers only the ROM aperture; installed
packages live in flash and RAM. Flash is covered by the header's flash
fingerprint, and RAM is one of the three saved regions, so a resume
correctly carries installed-package state and rejects a slot whose flash
has diverged. The snapshot debug loop survives package work unchanged.
See `docs/SNAPSHOT_RESUME_CONTRACT.md`.
