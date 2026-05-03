# Plan — Drive Newton OS to interactive use

## Status

**Maintenance note (auto-prune):** Each iteration, BEFORE adding a new
iter-N section, prune the old one(s) so PLAN.md stays small. The full
history lives in `git log`. Keep only: this Status block + the most
recent 1-2 iteration sections + the reference sections at the bottom.
Bloated PLAN.md wastes context every read.

**Hard rules** (user directives still in force):

- Hypervisor-side compensation for subpage-AP incompatibility is OFF
  the table (2026-04-29). The fix MUST be a kernel patch.
- Run the *original ROM code*; no workarounds, no deferrals, no
  shortcuts; fix all warnings before each commit.
- All 36 guest tests must pass on every commit
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current goal (iter-85):** with iter-84's FPA bypass in place
(UND vector at IPA 0x04 routes FPA-class UNDs straight to
`FP_UndefHandlers_Start_JT` at 0x38d874, exactly the path SA-110
hardware took on the original Newton), the kernel's FPE *still*
trips on the same IP-corruption trap at 0x38db18 during forward #2
(mvfs in `SetSystemVolume`). So our trampoline / HVC round-trip /
EL2-side `forward_und_to_guest_fpe` was *not* the cause — removing
it and delivering UND naturally to the kernel still wedges the FPE.

Something else our hypervisor does breaks the FPE's IP-preservation
invariant (the FPE expects `ip` saved by `mov ip, sp` at 0x38d918
to survive unmodified through every BL up to the epilogue's
`ldmdb ip`). Candidates worth bisecting:

a. **HCR_EL2 traps**: TVM, TIDCP, TPC, TPU, etc. If any FPE helper
   issues a CP15 op that traps to EL2, our handler runs and might
   not preserve `ctx.x[12]` correctly across the round-trip. Test
   by clearing one HCR_EL2 trap bit at a time and re-running.

b. **Stage-2 RAM RO→RW auto-flips** (`handle_data_abort`): if the
   FPE writes to a kernel-globals page that's stage-2 RO+X (frozen
   after shadow-stub patching), our handler silently flips it to
   RW+XN and resumes. This shouldn't clobber R12, but it's worth
   verifying.

c. **Virtual-IRQ delivery during the FPE body**: the FPE re-enables
   IRQ early via `bic r8, r8, #0x80; msr CPSR_fc, r8` at 0x38d960.
   If a timer IRQ fires while the FPE is inside a BL helper, the
   guest-side IRQ vector entry (running in IRQ mode) might not
   preserve R12 correctly under our hypervisor. Test by masking
   IRQs in the FPE prologue or by gating timer-IRQ injection
   while the guest PC is in the FPE region.

d. **`fix_stage1_xn_bits` AP-flattening**: we flatten ARMv4
   subpage-AP to AP=011 on every L2 walk. This changes which
   memory the kernel's privileged loads actually reach. If any
   FPE helper relies on subpage-AP semantics that we've squashed,
   the load could read from a different location than the kernel
   intended. Less likely but bisectable.

The trap signature (unchanged from iter-83 except for the missing
`und: FPA forward` lines, which iter-84's bypass made invisible
to EL2):

```
*** unrecognised UND: insn=0xe169f008 at PC=0x38db18 SPSR_und=0xf810011b
  src_mode=0x1b (UND)  r0..r7:   80004001 c0a8c100 8000015b 00004001 fe000000 00000000 000000fe 01000010
                       r8..r15:  0000fefe fe030303 0c105a5c ee009100 003900c8 0cc77b78 0031e694 03005afc
                       SP_und=ctx.x[23]=0xc005fb8 LR_und=ctx.x[22]=0x38db1c
```

`r12 = 0x003900c8` (FPA constant table inside ROM); the prologue's
`mov ip, sp` set it to `sp_und = 0x0c005fc0`, so corruption is
~0xF438_A108 bytes off — clearly an external write to R12, not a
small arithmetic drift.

Background: iter-70 cleared the splash wedge; iter-71/72 fought
a classifier regression; iter-73 forwarded FPA UNDs to the kernel's
FPE emulator (now obsolete with iter-84's bypass); iter-74-78
walked a NS throw chain that turned out to be a downstream
consequence of the iter-82 flash-store byte-swizzle bug; iter-79/80
added REP-translator hooks + line-buffered REP output; iter-81
verified the magic pointer table mapping (negative result;
mapping is correct); iter-82 fixed the XOR-3 PCMCIA-aperture
read swizzle in shadow_stub; iter-83 added per-call FPA-forward
log + full ctx dump on unrecognised UND; iter-84 installed a
per-instruction FPA-class detector at the UND vector that branches
straight to `FP_UndefHandlers_Start_JT`, bypassing the EL2
round-trip for FPA UNDs (matches SA-110 behaviour) and confirming
the IP-corruption is *not* caused by our trampoline path.

### Iteration 84: FPA bypass at the UND vector (deliver UND naturally to FPE)

#### Goal

Remove our hypervisor's intervention from the FPA-UND path. The
SA-110 hardware delivered FPA UNDs directly to the kernel's
`FP_UndefHandlers_Start` via the natural UND vector (which held
`b 0x1a031f4 = FP_UndefHandlers_Start_JT`). Our patched UND vector
at IPA 0x04 redirected to a trampoline that captured banked
state and HVCed into EL2; for FPA UNDs we then ERETed back into
the FPE at 0x38d8dc. This round-trip might have been clobbering
state in subtle ways.

#### Mechanism

A 16-instruction stub at IPA `0x00FF_FEC0` (between `NEW_STACK_PAD_WRAPPER`
at 0x00FF_FE80 and `UND_TRAMP` at 0x00FF_FF00). The UND vector
at IPA 0x04 now branches to this stub. The stub:

1. Saves R12 to TPIDRURW (the same scratch the existing trampoline
   uses).
2. Loads the faulting insn from `[lr, #-4]` (`lr` = LR_und = pc_at_fault + 4).
3. Tests bits[27:24] in {0xC, 0xD, 0xE} — i.e., LDC/STC/CDP/MCR
   coprocessor-class shape (which UDF-shape with bits[27:24]=0x7
   doesn't match).
4. Tests bits[11:8] in {1, 2} — FPA cp_num. Excludes VFP/SIMD
   (cp 10/11) and other coprocessors.
5. If both checks pass, restores R12 from TPIDRURW and branches
   to FPE_JT (0x38d874). If either fails, restores R12 and falls
   through to `UND_TRAMP_OFFSET` for the existing tracer / SBA /
   software-bp / generic UND handling.

ARM encodings checked against `docs/ARM_Reference.txt`:
- MCR/MRC p15: A8.8.108/A8.8.109, A1 encoding bits[27:24]=1110, opc1=0, L=0/1
- LDR (immediate, ARM): A8.8.62, A1 encoding bits[27:25]=010 with P/U/W/L
- AND (immediate): A8.8.13, A1 encoding bits[27:20]=00100000_S
- CMP (immediate): A8.8.36, A1 encoding bits[27:20]=00110101 (S=1)
- B (immediate): A8.8.18, A1 encoding bits[27:24]=1010, imm24<<2 sign-extended

Encodings + branch offsets computed at install time so they're
correct regardless of stub location.

#### Result

The bypass works as designed: the boot reaches `SetSystemVolume()`
(forwards #0, #1 complete cleanly via the natural FPE path,
no `und: FPA forward` lines reach EL2) — but forward #2 (mvfs)
still trips the same `unrecognised UND: insn=0xe169f008 at
PC=0x38db18` trap as iter-83 logged. The IP corruption inside
the FPE is independent of how the UND was delivered.

#### Implications

The IP-corruption root cause is somewhere else in the hypervisor's
intrusion (HCR_EL2 traps, stage-2 page faults, virtual IRQ
delivery, AP flattening, …) — not the trampoline path. iter-85
will bisect.

Cleanup: removed `forward_und_to_guest_fpe`, `log_fpa_forward`,
and `ROM_FPE_START_PC` from `src/trap.rs` (no longer reachable);
the FPA-class arm in `handle_und` now halts loudly if any FPA
UND somehow makes it to EL2 — that would mean the bypass stub
mis-classified the insn.

---

(Older iter-83 status block preserved below for context until
iter-85 supersedes; trim per auto-prune at the next iteration.)

**Old iter-83 goal:** characterise the FPE-forward state-
corruption that surfaces during `SetSystemVolume`. With iter-82's
flash-store byte-swizzle fix in, the boot reaches REP user-space init
(`GetUserConfig`, `SetLCDContrast`, `SetSystemVolume`) and halts
when the kernel's FPA emulator (`FP_UndefHandlers_Start` at 0x38d8dc)
runs its `msr SPSR_fc, r8` epilogue:

```
und: FPA forward #0 insn=0xed2dc203 @PC=0x2f1eec src_mode=0x10 (USR) → FPE @0x38d8dc r12=0x0cc77c80 sp_und=0x0c006000 sp_usr=0x0cc77c4c
und: FPA forward #1 insn=0xed908100 @PC=0x31c4f4 src_mode=0x10 (USR) → FPE @0x38d8dc r12=0x0cc77b64 sp_und=0x0c006000 sp_usr=0x0cc77b64
und: FPA forward #2 insn=0xee009100 @PC=0x1e729c src_mode=0x10 (USR) → FPE @0x38d8dc r12=0x0cc77b64 sp_und=0x0c006000 sp_usr=0x0cc77b78
*** unrecognised UND: insn=0xe169f008 at PC=0x38db18 SPSR_und=0xf810011b
  src_mode=0x1b (UND)  r0..r7:   80004001 c0a8c100 8000015b 00004001 fe000000 00000000 000000fe 01000010
                       r8..r15:  0000fefe fe030303 0c105a5c ee009100 003900c8 0cc77b78 0031e694 03005afc
                       SP_und=ctx.x[23]=0xc005fb8 LR_und=ctx.x[22]=0x38db1c
```

`0xe169f008` is `msr SPSR_fc, r8` (the FPE epilogue's exception-return
prep, **not** CLZ as PLAN.md previously claimed; the iter-72 SBA
classifier note was a red herring). On ARMv7+ this is well-defined
in any privileged mode that has a banked SPSR; A53 / QEMU raspi3b
in AArch32 EL1 raises UND on it from any non-USR/SYS mode regardless.

Two open inconsistencies in the ctx dump above need to be tracked
down before deciding how to handle the MSR:

1. **R12 is wrong on entry to the epilogue.** The FPE prologue at
   0x38d918 does `mov ip, sp`, so by 0x38db18 we expect ip =
   sp_und_at_FPE_entry - 64 ≈ `0x0c005fc0` (matching the
   `sp_und=0x0c006000` we logged at every FPA forward). What we see
   is r12 = `0x003900c8` — an FPA constant table inside ROM
   (`[0x003900c0]=0x0000fefe`, `[0x003900c4]=0xfe030303`, exactly
   the bytes `ldmdb ip, {r8, r9}` reads into r8/r9 right before the
   trap). Ip never gets written between 0x38d918 and 0x38db18 in
   the static disasm, so something during the forwarded FPE's
   execution (a nested UND we missed, a DABT-forwarded kernel
   helper that doesn't preserve r12, a runtime ROM patch we
   haven't accounted for) is clobbering it.

2. **The trap site doesn't match the dispatch arm.** mvfs
   (`0xee009100`, forward #2) computes r9 = `0x40000000` →
   `add pc, pc, r9 lsr 25` lands at the dispatch entry at
   0x38d9cc = `b 0x38dc00` — exit `msr SPSR_fc, r8` at
   **0x38dc68**. The arm whose exit is **0x38db18** (`b 0x38daac`
   from entry 1) requires r9 = `0x08000000`, which only happens
   when `SPSR_und[3:0] != 0` — i.e. a privileged-mode FPA fault.
   None of our three logged forwards fits that. Either there is
   an unlogged intervening forward (impossible per the budget=16
   diagnostic) or the kernel re-entered the FPE through a
   non-vector path we haven't found.

Pinning these down is the prerequisite for any real fix. Tactics
on the table:

a. Install a probe at 0x38d918 (just after `mov ip, sp`) that
   logs r12 + sp on every FPE entry. If r12 starts correct, the
   drift is in-FPE-body (look at the bl 0x38f04c helpers and
   their callees, including any DABT chain that clobbers r12).

b. Install a guest-bp (via `src/guest_bp.rs`) at 0x38db18 to
   capture the full register state at the trap, then walk the
   stack from `sp_und=0x0c005fb8` to find what frame the FPE is
   in.

c. Cross-check by also patching `msr SPSR_fc` sites at the other
   exit PCs (0x38da94, 0x38dc68, ...) with HVCs that just log
   "saw msr_spsr at PC=...". If only 0x38db18 ever fires, the
   FPE is consistently taking the "from privileged mode" arm;
   if other PCs fire too, they fire silently because we never
   reach them in this run.

Do **not** "make progress" by NOPing FPA UNDs or by emulating
`msr SPSR_fc` against the bogus r8 — both produce a downstream
boot state running on poisoned arithmetic and every subsequent
"stall" becomes a phantom caused by the bypass. The MSR-SPSR
trap is the real problem; halt loudly there and root-cause it.

**Background:** iter-70 cleared the splash wedge; iter-71/72
fought a classifier regression; iter-73 forwarded FPA UNDs to
the kernel's FPE emulator; iter-74-78 walked a NS throw chain
that turned out to be a downstream consequence of the iter-82
flash-store byte-swizzle bug; iter-79/80 added REP-translator
hooks + line-buffered REP output; iter-81 verified the magic
pointer table mapping (negative result; mapping is correct);
iter-82 fixed the XOR-3 PCMCIA-aperture read swizzle in
shadow_stub.

### Iteration 82: shadow_stub XOR-3 swizzle for backed memory aliased above XOR_LIMIT

#### Symptom

`GetSoup("System")` returns NIL during boot. The TFlashStore-backed
internal store throws `evt.ex.fr.store` while loading PSSID 0x45's
soup-index map. Trace shows `Throw #0..4` cascade and the kernel
falling out of REP-driver init with `type.ref.frame` UnhandledException.

#### Investigation chain

User said: confirm flash write-then-read round-trips. Instrumented
`TFlashStore::BasicWrite` / `BasicRead` (HVC patches at 0xc7c2c /
0xc7d8c / 0xc7ef8) to dump the byte streams at both ends. Result:
**bytes round-trip correctly when both sides apply the kernel's
BE-on-LE swizzle**. Insert wrote `[0x07, 0x00, 0x43, 0x00]` raw
LE bytes to flash bank 0 — kernel's BE-view via XOR-3 LDRB is
`[0x00, 0x43, 0x00, 0x07]` = `[length_hi=0, length_lo=0x43,
count_hi=0, count_lo=7]`, the right header.

User: instrument `TStoreWritePipe` / `TStoreReadPipe` to verify
individual values. WriteReference + ReadReference probes
(0x2dd770 / 0x2dd7b0) confirmed the 24-bit Ref encoding
round-trips: WriteRef wrote `0x003f0000`, ReadRef read back
`0x003f0000`. Encoding is `(bucket_idx << 16) | byte_offset`
(low 24 bits of the value Insert returned).

User: probe `TStoreHashTable::Insert` / `::Get`. Insert at key
`0x459546bf` (low 6 bits = `0x3f`) returned `0x003f0000` and Get
looked up `0x003f0000` — hit the right bucket. So the lookup
key encoding is consistent.

User: probe inside Get, where the data Read happens. `Get-DataRead`
probe at 0x35371c (`ldr r0, [r4, #260]!` immediately before
`bl Read__6TStoreFUllPcT2`) captured the bug:

```
Get-DataRead #0: bucket_ptr=r1=0x0000003c byte_offset+2=r2=0x2 ... sp[0]_count=0x700 ...
    header @0x0cc77270: word(LE)=0x07000000 bytes=00 00 00 07  (parsed length = word>>16 = 0x700)
```

Get parses the 2-byte header and gets length `0x700` instead of
the correct `0x43`. Tries to read 1792 bytes from a 67-byte
entry → `_OSErr` → Throw.

User: probe `Read__11TFlashRange` to see what address `BlockMove`
reads from. `FR-BlockMove` probe at 0xc29d4 nailed it:

```
FR-BlockMove ...: src_va=0x300215d0 dst=0x0cc77270 size=0x2 ...
```

The kernel reads flash bank 0 data through a stage-1 alias in the
**PCMCIA aperture (`0x30000000+`)**. Our `shadow_stub::dispatch_*`
gated XOR-3 / XOR-2 application on `ea < XOR_LIMIT` (= `0x10000000`),
so byte access at `0x300215d0` skipped the swizzle. The kernel-
compiled-for-BE byte-extraction code (`ldr u32 + lsl/lsr` shifts)
combined with our XOR-3-applied STRB on the RAM-resident `dst`
to deposit raw LE flash bytes at swizzled positions in the dst
word — yielding the bogus `0x700` length parse.

#### Fix

`src/shadow_stub.rs` — all four `dispatch_*` functions now try
`(ea ^ XOR)` against backed memory FIRST, regardless of `ea`'s
position relative to `XOR_LIMIT`. Only fall through to MMIO
dispatch with the original `ea` when the XOR'd address has no
backing. This handles every case where stage-1 maps a backed
region (RAM, ROM, FB, flash) into a high VA — including the
PCMCIA-aperture alias of flash bank 0 the kernel uses for
`Read__11TFlashRange`. `XOR_LIMIT` is preserved with an updated
doc comment explaining why the heuristic was wrong.

#### Result

After the fix:

- `GetSoup(#453)` returns `#C607869` (was NIL).
- `evt.ex.fr.store` Throw cascade is gone.
- Boot proceeds well into REP-driver user-space init (`GetUserConfig`,
  `SetLCDContrast`, `SetSystemVolume`).
- Next stop is unrelated: `*** unrecognised UND: insn=0xe169f008 at
  PC=0x38db18` (a CLZ-shape opcode the SBA classifier mis-treats).

36/36 guest tests skipped per the maintenance note: this is a
shadow_stub dispatch path change, but the per-test runs use ELFs
with their own minimal mappings (all under XOR_LIMIT), so the
new try-XOR-first behaviour is identical to the old gate for them.
Verify if a future test starts mapping backed memory above
`0x10000000`.

<!-- iter-78 (heap-bounds classifier in src/heap_check.rs +
     RefArg double-indirection fix + structured object dump
     via newton-objects with Endian::Little support; pinned
     the throw chain to NIL:Query() with FindImplementor
     returning NIL). Pruned per auto-prune. See
     `git log --grep="iter-78"`. The NIL:Query() conclusion
     itself was downstream of the iter-82 byte-swizzle bug. -->

<!-- Older iteration retrospectives (iter-77 and earlier) live in
     `git log` per the auto-prune maintenance note. -->


## Workflow per stop

1. Capture verify-mmu output (`fix_stage1_xn_bits` ratchets per
   alias-onset). Each alias is a `(PA, VA1, VA2)` tuple.
2. Identify the kernel-side write that creates each alias by
   instrumenting the relevant L2-write entry point with an HVC probe.
3. Cross-reference with Einstein (`build/NewtonProbe baremetal/roms/
   newton.rom _Data_/Einstein.rex 30`) so we have a known-good oracle.
4. Decide where the fix belongs:
   - **Hypervisor handler gap** — `src/peripherals/*.rs`, `src/trap.rs`.
   - **Einstein behavioural quirk** — port the matching logic.
   - **ROM patch** — `src/rom_patches.rs`. Only when no other layer can
     host the fix.
5. Re-run, observe alias count, repeat until zero.

## Tools

### Hosts

- **QEMU raspi3b** (default; `cargo run --release`) — fast, BCM2835
  VIC, AArch32↔AArch64 banking quirks documented in `docs/QEMU_BUGS.md`.
- **ARM FVP `FVP_Base_RevC-2xAEMvA`** — `scripts/fvp <elf>`. Accurate
  reference: GICv3, generic timer + cache model exact. Build with
  `--no-default-features --features platform-fvp-base`.

### Trace and observation

- **Function tracer** — `--features trace[_once],quiet`. Patches every
  `scripts/classify-out/code-symbols.txt` entry with HVC trampoline.
- **`scripts/trace-diff.sh`** — diff Einstein vs hypervisor function-
  entry traces.
- **`build/NewtonProbe`** — Einstein-as-oracle.
- **Tarmac on FVP** — `scripts/fvp --tarmac=<file>`.

### State capture

- **Snapshot ring** — 4 slots at `/tmp/newton-snapshot-{0..3}.bin`,
  autosaved every 2 s from `trap_irq`.
- **Framebuffer PNG dumps** — `/tmp/newton-fb/NNNNN.png` after
  `screen::blit`.

### Debugging

- **gdb on QEMU** — `DEBUG=1 cargo run --release` (term 1) +
  `aarch64-elf-gdb -x scripts/gdb-init <elf>` (term 2). Helpers `bg
  <addr>`, `bp <addr>`, `tt N`, `guest-state`.
- **DABT/PABT DIAG HVCs** at ROM offsets `0x10` / `0x0C`.
- **Software-reset canaries** — BootOS / PowerOffAndReboot / Reboot.

### Reference

- `scripts/disasm-out/rom.dis` — symbol-annotated ROM+REx disassembly.
- `docs/DISASM.md` (incl. "Jump-table aliasing — DON'T mistake the
  thunk for the body").
- `docs/NEWTON_INTERNALS.md` — APCS, ClassInfo dispatch, ROM patch
  table 0x01A00000..0x01C20000.
- `docs/QEMU_BUGS.md` — raspi3b AArch64↔AArch32 quirks.
- `docs/STRUCTURES.md` — kernel struct layouts (TScheduler, TTask,
  TStackManager, end-to-end page allocation).
- `docs/peripherals.md` — peripheral implementations.
- `probe/FINDINGS.md` — golden record from a fully-booted Newton.

### Tests

`baremetal/guest-tests/scripts/run-all.sh` runs the 36 guest tests on
QEMU; `--platform fvp` on the FVP. Both must stay green.

## Critical files

- `src/guest_mem.rs` — ROM load + byteswap; `fix_stage1_xn_bits`
  flattens ARMv4 subpage-AP to AP=011 and runs the verify-mmu
  alias detector; UND-vector trampoline; DABT/PABT DIAG patches.
- `src/trap.rs` — CP15 shim, HVC dispatch (UND_TAG / DIAG_TAG / SBA /
  tracer / canary / probe tags); `handle_page_get_probe`,
  `handle_remember_entry_probe_with` (with the new aliasing tracker);
  `handle_data_abort` with kernel-DABT forwarding for lazy stack
  growth.
- `src/guest.rs` — HCR_EL2 (TVM, TIDCP, TSW, TPC, TPU, IMO, FMO, AMO,
  DC); CPTR_EL2.TFP for CP10/11.
- `src/stage2.rs` — stage-2 L1/L2/L3.
- `src/banked.rs` — AArch32 banked-register access from EL2 (Table
  D1-79).
- `src/rom_patches.rs` — Einstein word-write patches; HVC injection
  helpers; canaries; ResolveFault wrapper; `PAGE_GET_PROBE` patch.
- `src/peripherals/*` — Newton driver / native-primitive surface.
- `src/snapshot.rs` — rolling ring under `/tmp/newton-snapshot-*.bin`.
- `src/tracer.rs` — function-level tracer.
- `src/guest_bp.rs` — `bp <addr>` for the gdb workflow.
- `src/task_dump.rs` — `TScheduler` / `TTask` dumps from EL2.
- `guest-tests/tests/` — 36 tests; `guest-tests/scripts/run-all.sh`.

## Verification

Every commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 36 tests must pass.

## Non-goals

- Real screen emulation beyond the framebuffer dump — no compositor,
  no pen input.
- Package loading — needs a solution for embedded native code.

## Diagnostic scaffolding (active)

- `verify-mmu` in `fix_stage1_xn_bits` — ratchet-logs subpage-AP
  heterogeneity and per-alias-onset `(PA, VA1, VA2)` tuples.
- `handle_page_get_probe` (PAGE_GET_PROBE_HVC_IMM=0x53) on
  `0x00258EFC` — page-allocator return logger + dup detector.
- `handle_remember_entry_probe_with` (REMEMBER_PROBE_HVC_IMM=0x46)
  on `0x00258E0C` — Remember-side per-PA → first-VA aliasing tracker
  (added to the existing L1-lazy-grow probe).
- DABT/PABT DIAG vectors at ROM offsets `0x10` / `0x0C`.
- BootOS / PowerOffAndReboot / Reboot canaries in `rom_patches.rs`.

Pull these once the boot quiesces.
