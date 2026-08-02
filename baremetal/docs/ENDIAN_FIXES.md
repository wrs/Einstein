# Endianness mitigations: BE-32 ROM on a little-endian A53

The Newton 717006 ROM was assembled for the SA-1100 in ARMv4 **BE-32**
(word-invariant big-endian) mode. Cortex-A53 AArch32 supports only LE
and BE-8 — there is no BE-32. This note is the reference for how the
hypervisor makes a BE-32 guest work on an LE host, grounded in the
ARMv4 spec, and an audit confirming every B-bit-visible behavior is
mitigated.

## ARMv4 B-bit (CP15 c1, bit 7): exact behavioral differences

Reference: *ARM Architecture Reference Manual* (ARM DDI 0100I — the
ARMv6 ARM, which retains the full legacy spec for ARMv4 through
BE-32). Key sections: **A2.7 Endian support** (pp. A2-30..A2-36) and
**B3.4.3 Register 1 (Control Register)** (p. B3-13). The critical
normative text:

> **B (bit[7]).** ARM processors which support both little-endian and
> big-endian word-invariant memory systems use this bit to **configure
> the ARM processor to rename the four byte addresses within a 32-bit
> word**.
> 0 = configured little-endian memory system (LE)
> 1 = configured big-endian word-invariant memory system (BE-32)

And the mapping rule (p. A2-39):

> `Byte[A]` in the LE endianness model, `Byte[A]` in the BE-8
> endianness model, and **`Byte[A EOR 3]` in the BE-32 endianness
> model are the same actual byte of memory**. If `X` is word-aligned,
> `Word[X]` consists of the same four bytes of actual memory in the
> same order in the LE and BE-32 endianness models.

From that one rule every behavioral difference falls out. The ROM
sets B=1 (BE-32). Compared to B=0 (LE), *everything below* is the
complete list.

### Things that DIFFER between B=1 and B=0

For the same physical memory, observed by the same executing
instruction:

| Class | Effect |
|---|---|
| `LDRB`, `LDRBT` | Byte fetched from `phys[A ^ 3]` instead of `phys[A]`. |
| `LDRSB`, `LDRSBT` | Same `^3`; sign-extension unaffected. |
| `STRB`, `STRBT` | Byte stored to `phys[A ^ 3]`. |
| `LDRH`, `LDRHT`, `LDRSH`, `LDRSHT` | 16-bit halfword fetched from `phys[A ^ 2 .. (A ^ 2)+1]`. Signed variants sign-extend; the XOR is unchanged. |
| `STRH`, `STRHT` | Halfword stored at `phys[A ^ 2]`. |
| `SWPB` | Same `^3` as LDRB/STRB. |

Those seven instruction classes — the entire set of **sub-word memory
accesses** — are the *only* place where B=1 and B=0 diverge
architecturally.

### Things that DO NOT differ (proof-level identical)

Because `Word[X]` at word-aligned `X` is the same 32 bits of physical
memory in both models, every word-sized access is bit-identical:

- `LDR` / `STR` to a word-aligned address — identical value,
  identical byte pattern.
- `LDR` to an unaligned address (legacy rotated-LDR):
  `Word[Align(Addr)]` is the same, and the rotate amount `8 *
  Addr[1:0]` uses the same `Addr`, so the rotated result is identical.
- `STR` to an unaligned address (forced-aligned-store): identical.
- `LDM` / `STM` / `PUSH` / `POP` — sequences of word accesses.
- `LDRD` / `STRD` — pair of word accesses.
- `SWP` (word) — word-aligned load+store pair.
- `LDC` / `STC` — coprocessor multi-word.
- `LDREX` / `STREX` — word-aligned.
- **Instruction fetch** — all ARM instructions are word-aligned words.
- **Exception vector fetch** — word-aligned fetches at `0x00000000`
  or `0xFFFF0000`.
- **MMU page-table walks** — the MMU reads 4-byte descriptors at
  word-aligned addresses.
- `MRC` / `MCR` / `MRRC` / `MCRR` — register transfers, no memory
  operand.
- Data-processing, multiply, multiply-halfword (`SMLAxy`, …) —
  register-to-register; operate on register bit-fields regardless of
  endianness.
- `REV` / `REV16` / `REVSH` (ARMv6) — register-only.
- CPSR / SPSR transfer, mode change, interrupt masking, alignment
  check, debug — none are endian-sensitive on ARMv4.

### What is NOT in ARMv4 and therefore N/A for this ROM

- No `SETEND`, no CPSR.E bit, no SCTLR.EE bit, no BE-8. Those all
  arrive in ARMv6.
- No separate instruction/data endianness. Pre-ARMv6, the B bit
  governs every memory access; after ARMv6, instruction fetch is
  fixed little-endian and the E/EE bits govern only data and PTW
  separately.

### One caveat outside endianness proper

Pre-ARMv6 legacy mode pairs `B=1` with `SCTLR.U=0`. In that mode,
**unaligned `LDR` rotates** (Addr[1:0] ≠ 0 → word-aligned load +
rotate-right by `8·Addr[1:0]`). ARMv7+ implementations (and Cortex-A53
AArch32) are forced into `SCTLR.U=1` semantics where unaligned `LDR`
does a true unaligned load — **no rotate**. Strictly speaking this is
a difference between ARMv4 and ARMv7/v8, not between B=1 and B=0
(the rotate also happens under B=0 on real ARMv4). But it's the same
kind of mismatch a BE-32 ROM expects against modern hardware, so it's
flagged below.

## Audit: how the hypervisor mitigates every difference

Strategy:

1. **Guest runs in LE the whole time.** `guest::eret_to_guest` sets
   `SPSR_EL2 = 0x1D3` (SVC, I=F=A=1, E=0). The guest's `CPSR.E` stays
   0 and `SCTLR_EL1.EE` is never set — LE data endian throughout.
   See `src/hv/guest.rs`.

2. **ROM + REx are byte-swapped per word at load.**
   `guest_mem::load_newton_rom` reverses every 32-bit word so the LE
   guest sees the same word value a BE-32 guest would. See
   `src/newton/loader.rs`.

3. **Every sub-word access in the ROM is rewritten.**
   `inline_patch::patch_rom_from_bitmap` walks a build-time classifier
   bitmap and replaces every `LDRB` / `STRB` / `LDRH` / `STRH` /
   `LDRSB` / `LDRSH` / `SWPB` with `UDF #(0x8000|idx)`. The UND
   trampoline drops into `handle_sba_udf`, which recomputes the
   effective address and does `phys[A ^ 3]` (byte) or `phys[A ^ 2]`
   (halfword), then ERETs past the UDF with CPSR flags intact. See
   `src/newton/inline_patch.rs`.

Verification against the difference table above:

| Difference class | Mitigation | Status |
|---|---|---|
| `LDRB` / `LDRBT` / `LDRSB` / `LDRSBT` | `inline_patch::decode` Form 1 (cond 010…B=1) catches `LDRB`/`LDRBT`. Form 2 (op=10, L=1) catches `LDRSB`/`LDRSBT`. XOR 3 applied in Rust. `src/newton/inline_patch.rs`. | ✅ |
| `STRB` / `STRBT` | Form 1, L=0 path. XOR 3. | ✅ |
| `LDRH` / `LDRHT` / `LDRSH` / `LDRSHT` | Form 2, op=01 and op=11. XOR 2. | ✅ |
| `STRH` / `STRHT` | Form 2, op=01, L=0. XOR 2. | ✅ |
| `SWPB` | Form 3 (`0x0140_0090` pattern). XOR 3 on the atomic byte swap. `src/newton/inline_patch.rs`. | ✅ |
| Word-aligned `LDR`/`STR`/`LDM`/`STM`/`LDRD`/`STRD`/`SWP` | No action needed — word access is architecturally identical. Byteswap-at-load makes LE `Word[X]` = BE-32 `Word[X]`. | ✅ |
| Unaligned `LDR` rotate | Not a B-bit difference, but an ARMv4↔ARMv7 mismatch that any BE-32 ROM running on A53 hits. Handled **systematically via SCTLR.A=1 alignment-fault emulation**. The CP15 shim at `src/hv/trap/cp15.rs` ORs `A=1` into every guest SCTLR write, so any unaligned LDR/STR raises an alignment fault at EL1; the DABT-vector trampoline (`src/newton/guest_trampolines.rs::patch_dabt_vector`) fast-paths `DFSR.FS[3:0]==1` to `HVC #ALIGN_TAG`, and `src/newton/unaligned.rs::handle_align_fault` decodes the faulting instruction and performs the aligned word load + ROR in EL2 Rust. Covers every static unaligned LDR imm + every `[Rn, Rm, LSL #1]` site without needing a ROM-patch whitelist. | ✅ |
| Instruction fetch / exception vector fetch / PTW | Word-aligned — identical. No mitigation needed. | ✅ |
| Thumb instruction fetch / Thumb halfword fetch | Newton 2.x ROM is pure ARMv4 (SA-1100 target), no Thumb. Unused. | ✅ (by absence) |
| MMIO sub-word accesses | Two regimes: IPAs `< 0x1000_0000` (includes the tick-page at `0x0F18_1000`) are stage-2-mapped RAM and the inline-patch XOR applies uniformly. Trapped MMIO at `≥ 0x1000_0000` is not XOR'd by inline_patch; peripheral byte accesses are documented as not used in this band (see `src/newton/inline_patch.rs`). `src/hv/mmio.rs` handles the word/halfword/byte SAS from ESR syndrome. | ✅ for the ROM as observed; fragile assumption for new peripheral code |
| Guest write of B bit (`MCR p15, 0, Rd, c1, c0`) | Trapped, forwarded to `SCTLR_EL1`. Bit 7 of `SCTLR_EL1` is ITD (not B), so the write is architecturally a no-op for endianness on A53 — exactly the behavior we want, since inline_patch + byteswap make the CPU *already* appear BE-32 to the ROM regardless of what the ROM programs. `src/hv/trap/cp15.rs`. | ✅ |
| Guest write of `CPSR.E` / `SCTLR.EE` | ARMv4 code never issues `SETEND` and doesn't touch bits 25/9 of SCTLR. Not observed in the ROM. | ✅ (by absence) |

## Gaps and caveats worth knowing

1. **`inline_patch` skips FIQ mode.** `src/newton/inline_patch.rs`
   documents it: the UND trampoline and the AArch64-view-of-AArch32-
   R8..R12 path don't cover FIQ. The Newton FIQ handler doesn't do
   sub-word access in observed runs, but it's an open edge.

2. **Unaligned-LDR emulator covers R0-R14 Rn/Rt/Rm but halts on
   R15 (PC) as an operand.** Rare in rotate-LDR idioms and
   unreachable in the 717006 ROM we've run; extend
   `src/newton/unaligned.rs::handle_align_fault` if we ever see one.
   Banked-register access is correct across all AArch32 modes per
   ARM ARM Table D1-79 (see `src/newton/unaligned.rs::ctx_slot_for_reg`).

3. **Unaligned `STR` is emulated as "store aligned word".**
   ARMv4 `STR` to unaligned address is UNPREDICTABLE; SA-1100
   stores to the aligned word with no rotation, which matches what
   the emulator does. The ROM shouldn't actually hit this in
   practice — rotate-STR is not an idiom.

4. **Unaligned `LDM` / `STM` / `LDRD` / `STRD` are not emulated.**
   The alignment-fault handler only recognises the LDR/STR A1
   immediate-offset and register-offset forms. ARMv4 requires
   LDM/STM to be word-aligned and LDRD/STRD to be 8-byte-aligned,
   so SCTLR.A=1 faults on unaligned here are latent bugs in guest
   code; we halt loudly in the align handler rather than emulate.

5. **`Rt==Rm` `SWPB` is refused at patch time**
   (`src/newton/inline_patch.rs`) — architecturally `UNPREDICTABLE`,
   so refusing is correct, but worth knowing.

6. **Trapped MMIO above `XOR_LIMIT` (0x1000_0000) is not XOR'd.** The
   design assumes no sub-word MMIO accesses land there, which is
   currently true for the Newton peripheral map — adding a new
   byte-level MMIO register above that boundary would silently
   mishandle endianness. Documented at `src/newton/inline_patch.rs`.

## Bottom line

Every architectural divergence between B=1 and B=0 on ARMv4 — i.e.
the seven sub-word memory-access forms — is mitigated by the
combination of load-time per-word byteswap and inline_patch's
UDF-trap emulation with `A^3` / `A^2`. Word-sized accesses need no
mitigation because they are bit-identical under word-invariant BE.

The non-endian ARMv4↔ARMv7 unaligned-LDR rotate difference is also
handled, systematically: `SCTLR.A=1` forces every unaligned LDR/STR
to alignment-fault, the DABT trampoline fast-paths these to
`HVC #ALIGN_TAG`, and `src/newton/unaligned.rs` emulates with SA-1100
semantics in EL2 Rust. No per-site ROM-patch whitelist needed.

## Sources

- ARM Architecture Reference Manual, ARM DDI 0100I (Rev I, 2005) —
  covers ARMv4 through ARMv6, including the full BE-32 legacy spec.
- `src/hv/guest.rs`, `src/hv/guest_mem.rs` + `src/newton/loader.rs`,
  `src/newton/inline_patch.rs`, `src/newton/rom_patches.rs`,
  `src/hv/trap/`, `src/hv/mmio.rs` in this tree.
