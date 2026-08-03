# Endianness: a BE-32 ROM on an A53 that has no BE-32

The Newton 717006 ROM was assembled for the SA-1100 in ARMv4 **BE-32**
(word-invariant big-endian) mode. Cortex-A53 AArch32 supports only LE
and BE-8 — there is no BE-32. This note is the reference for how the
hypervisor closes that gap: it runs the guest in **BE-8** and picks
each ROM word's storage layout to suit how that word is used. Grounded
in the ARMv4 spec, with an audit confirming every B-bit-visible
behaviour is accounted for.

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

## How the hypervisor handles this

The strategy is to run the guest in **BE-8** and choose each ROM word's
storage layout so that both instruction fetch and BE-8 data access read
it correctly. No guest instruction is rewritten for endianness, and
there is no per-access trap.

### 1. The guest runs BE-8

`src/hv/guest.rs:177-180` sets `SPSR_EL2 = 0x3D3` before the first
ERET — SVC mode, `I=F=A=1`, and **bit 9 (E) set**. `src/hv/guest.rs:145-150`
sets `SCTLR_EL1.EE | E0E` (bits 25 and 24) as part of cold-boot EL1
state. So `CPSR.E=1` and `SCTLR_EL1.EE=1` from the first guest
instruction.

The kernel's own `SCTLR` writes never set `EE`, so they would drop the
guest back to LE. They can't: `HCR_EL2.TVM` traps them to
`src/hv/trap/cp15.rs:121-140`, which routes through
`src/newton/os.rs:815-832` (`massage_sctlr`) and re-ORs `A | EE | E0E`
into every value the guest writes.

A consequence worth knowing: with `SCTLR_EL1.EE=1` the **hardware
page-table walker reads big-endian**, so EL2's page-table accessors
byte-swap (`src/hv/guest_mem.rs:77-104`).

> **Guest-test builds are the exception.** Under the `nh_guest_test`
> cfg the SPSR is `0x1D3` and `SCTLR_EL1.EE` stays 0 — the guest runs
> **LE**, and the XOR-lane helpers in `src/hv/be8.rs:82-93` and the
> `^3`/`^2` arms of `src/hv/guest_endian.rs` compile in. None of that
> is in the ROM build, so read the `not(nh_guest_test)` arm when you
> want to know what the hypervisor does with the Newton ROM.

### 2. ROM and REx are stored per word, by classification

`src/newton/loader.rs:326-356` (ROM) and `:371-394` (REx) branch on
`guest_mem::rom_word_is_code(index)`:

- **Code word** → `u32::from_be_bytes(on_disk)` written natively, i.e.
  the on-disk bytes **reversed** in host memory. AArch32 instruction
  fetch on Cortex-A53 is always little-endian regardless of
  `SCTLR.EE`, so the fetcher must see the LE encoding of the numerical
  instruction value the BE-32 ROM held.
- **Data word** → the four on-disk bytes stored **verbatim**. Under
  `CPSR.E=1` the CPU byte-reverses on every `LDR`/`STR`, so a
  BE-encoded host word yields the original numerical value.

The discriminator is the classifier's `reach.bitmap`
(`src/hv/guest_mem.rs:42-58`), one bit per 32-bit word across the
16 MiB aperture, `include_bytes!`d into the image. It is the sole input
to this decision, and out-of-range indices are treated as data.

### 3. Sub-word access needs no mitigation on data words

This is the whole trick, and it is why no instruction rewriting is
needed. Under BE-8 the CPU performs the byte-lane transform on every
multi-byte transfer, while a single-byte access simply addresses
`mem[A]`. For a data word holding on-disk bytes `b0 b1 b2 b3`:

- `LDRB` at offset 0 reads `host[W+0] = b0` — the Newton-logical MSB,
  which is exactly what BE-32 would return. Correct with zero
  transform.
- `LDR` on the same word returns `from_be_bytes(b0..b3)` because the
  CPU reverses on load — the original BE numerical value.
- `LDRH` at offset 0 returns `(b0<<8)|b1`, the BE halfword.

One storage layout satisfies all three. `src/hv/guest_endian.rs:110-113`
reflects this: the EL2-side `guest_read_u8_pa` is a plain byte read in
the ROM build.

Verification against the difference table above:

| Difference class | How it is handled | Status |
|---|---|---|
| `LDRB` / `LDRBT` / `LDRSB` / `LDRSBT` | Nothing to do on data words — BE-8 byte addressing already matches BE-32. `src/newton/loader.rs:344-355`. | ✅ |
| `STRB` / `STRBT` | Same. | ✅ |
| `LDRH` / `LDRHT` / `LDRSH` / `LDRSHT` | Same — the CPU's BE-8 halfword reversal reproduces the BE-32 halfword. | ✅ |
| `STRH` / `STRHT` | Same. | ✅ |
| `SWPB` | `SCTLR_EL1.SW` is never set, so `SWP`/`SWPB` trap as UND and are emulated at EL2: `src/hv/trap/und.rs:794-877`. The byte path uses raw `read_byte_pa`/`write_byte_pa` (no lane transform, BE-8-natural); the word path goes through the swapping `guest_endian` accessors. ROM-aperture targets are absorbed separately in `src/hv/trap/dabt.rs:419-471`. | ✅ |
| Word-aligned `LDR`/`STR`/`LDM`/`STM`/`LDRD`/`STRD`/`SWP` | Architecturally identical under word-invariant BE; the verbatim data-word layout plus BE-8 reversal reproduces the value. | ✅ |
| Unaligned `LDR` rotate | Not a B-bit difference but an ARMv4↔ARMv7 mismatch. `SCTLR.A=1` is forced by `massage_sctlr` (`src/newton/os.rs:815-832`) so unaligned LDR/STR fault at EL1; the DABT-vector trampoline (`src/newton/guest_trampolines.rs:418-460`) fast-paths `DFSR.FS[3:0]==1` to `HVC #Align`; `src/newton/unaligned.rs:250-267` does the aligned load + `rotate_right(8·(addr&3))`. A lazy inline stub (`src/newton/unaligned_inline.rs:311-352`) then removes the trap for that PC. The stub does no endianness work — BE-8 makes address+rotate sufficient. | ✅ |
| Instruction fetch / exception vector fetch | Word-aligned, and code words are stored LE-reversed precisely for this. | ✅ |
| MMU page-table walks | `SCTLR_EL1.EE=1` makes the walker read BE; EL2 accessors swap to match (`src/hv/guest_mem.rs:77-104`). | ✅ |
| Thumb instruction fetch | Newton 2.x ROM is pure ARMv4 (SA-1100 target), no Thumb. | ✅ (by absence) |
| MMIO sub-word accesses | Handled with BE-8 lane math, not address XOR. `src/hv/mmio.rs:111-152` reads the aligned word side-effect-free and extracts the lane via `be8::extract_sub_word`; writes splice via `be8::splice_byte`/`splice_halfword` (`:184-249`). Lane 0 is bits[31:24] (`src/hv/be8.rs:27-76`), the BE-8 convention, with const-eval round-trip assertions at `be8.rs:110-135`. Serial windows bypass at natural offset, mirroring Einstein's `TMemory::ReadBP`. | ✅ |
| Guest write of B bit (`MCR p15, 0, Rd, c1, c0`) | Trapped and forwarded to `SCTLR_EL1`, where bit 7 is ITD, not B — architecturally a no-op for endianness on A53. The guest is already BE-8 regardless of what the ROM programs. `src/hv/trap/cp15.rs`. | ✅ |
| Guest write of `CPSR.E` / `SCTLR.EE` | ARMv4 code has no `SETEND` and doesn't touch SCTLR bits 25/24. `massage_sctlr` re-ORs `EE|E0E` anyway, so a stray write can't take effect. | ✅ |

## Gaps and caveats worth knowing

1. **Byte reads of *code* words are wrong, deliberately.** A code word
   is stored byte-reversed, so a guest `LDRB` against one returns
   `on_disk[A ^ 3]`. Nothing compensates at runtime. The design
   assumes the ROM never reads its own instruction stream as sub-word
   data.

   Where the ROM reads code as a **word**, it is patched per site:
   `src/newton/rom_patches.rs:507-570` rewrites each entry in
   `rom_ver::INSN_AS_DATA_LDRS` (`src/newton/rom_ver/r717006/mod.rs:144-172`
   — the DataAbort and UndefinedInstruction handlers reading their own
   faulting instruction, two SWIBoot sites, plus an FPE pair) into
   `B stub`, where the stub is `<orig LDR>; REV Rd,Rd; B resume`. The
   hypervisor's own AArch32 trampolines use the same idiom
   (`src/newton/guest_trampolines.rs:488-500`). There is no equivalent
   for byte-granular reads of code words; if one is ever needed it
   does not exist yet.

2. **Unaligned-LDR emulator halts on R15 (PC) as an operand.** Rare in
   rotate-LDR idioms and unreachable in the 717006 ROM we've run;
   extend `src/newton/unaligned.rs` if one appears. Banked-register
   access is correct across all AArch32 modes per ARM ARM Table D1-79
   (`src/arch/banked.rs`).

3. **Unaligned `STR` is emulated as "store aligned word".** ARMv4
   `STR` to an unaligned address is UNPREDICTABLE; SA-1100 stores to
   the aligned word with no rotation, which is what
   `src/newton/unaligned.rs:268-281` does.

4. **Unaligned `LDM` / `STM` / `LDRD` / `STRD` are not emulated.** The
   alignment-fault handler recognises only the LDR/STR A1
   immediate- and register-offset forms. ARMv4 requires those to be
   aligned, so a fault there is a latent guest-code bug; we halt
   loudly rather than emulate.

5. **`SWP`/`SWPB` with an operand ≥ r13 halts loudly**
   (`src/hv/trap/und.rs:803-809`).

6. **`SBA_*` does not mean what it says.** The stub pool at
   `src/newton/inline_patch.rs:44-49` is named `SBA_STUB_POOL_*` for
   "shadow byte access", but it has nothing to do with byte access: it
   holds `unaligned_inline`'s rotate-LDR stubs and the DABT
   trampoline. Read the name as "the stub pool".

## Bottom line

Every architectural divergence between B=1 and B=0 on ARMv4 — the
seven sub-word memory-access forms — is handled by running the guest
BE-8 and storing each ROM word in the layout its use requires: code
words byte-reversed for the always-LE instruction fetcher, data words
verbatim so BE-8 access reproduces BE-32 semantics. Word-sized
accesses need nothing because they are bit-identical under
word-invariant BE. No guest instruction is rewritten for endianness,
and there is no per-access trap.

The one asymmetry is that byte access to a *code* word is not
correct; word-sized reads of code are fixed by a small per-site patch
list, and byte-sized ones are assumed not to occur.

The non-endian ARMv4↔ARMv7 unaligned-LDR rotate difference is handled
separately and systematically: `SCTLR.A=1` forces the fault, the DABT
trampoline fast-paths it to `HVC #Align`, EL2 emulates SA-1100
semantics, and a lazy inline stub retires the trap per PC.

## Sources

- ARM Architecture Reference Manual, ARM DDI 0100I (Rev I, 2005) —
  covers ARMv4 through ARMv6, including the full BE-32 legacy spec.
- `src/hv/guest.rs` (SPSR/SCTLR at ERET), `src/newton/loader.rs` +
  `src/hv/guest_mem.rs` (per-word load layout), `src/newton/os.rs`
  (`massage_sctlr`), `src/hv/be8.rs` + `src/hv/mmio.rs` (lane math),
  `src/hv/trap/und.rs` (SWP/SWPB), `src/newton/unaligned.rs` +
  `src/newton/unaligned_inline.rs` (rotate-LDR),
  `src/newton/rom_patches.rs` (code-read-as-data sites).
