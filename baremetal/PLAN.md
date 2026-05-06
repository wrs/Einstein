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
- All 36 guest tests must pass on every commit that touches hypervisor
  functionality (not merely diagnostics):
  (`baremetal/guest-tests/scripts/run-all.sh`).

**Current goal (iter-106):** iter-105 wedge is **resolved**. Root
cause was hypothesis #1 above (bytes at 0x800968 weren't byteswapped
at load time). The 2-instruction stub `ldr r0, [r0]; b 0x800904`
at REx PA 0x00800968 is reached only by the kernel dereferencing a
package-internal function-pointer slot at 0x00800df4, which the
shape-based REx classifier never marked as code. Without
byteswapping, the kernel's ERET fetched garbage at the BE-format
bytes of 0xe5900000 and the guest wild-branched to PC=0 with T=1,
producing the "thumb-und" wedge.

Fix in iter-105:
- `tools/classify-rom/src/main.rs` now parses each NewtonOS package
  in the REx pkgl block, walks its relocation table when
  `kDirRelocationFlag` is set (per DCL TDCLPackage.cpp), and seeds
  every code-shaped pointer-slot value as a walker root via
  `va_to_pa`. This is the kernel's own loader-format authority on
  what's a pointer; no shape-heuristic guessing needed.
- The 'FDRV' tag heuristic (scan every word for fnptr-shaped
  values) was redundant with `collect_classinfo_roots` (precise
  trampoline-shape match) and produced 8 spurious main-ROM-vector
  seeds in Einstein.rex; removed.
- Earlier iter-105 commit added `collect_pcrel_ldr_thunk_run_roots`
  for non-relocatable packages whose `LDR pc, [pc, #-4] + literal`
  thunk tables (e.g. 0x826xxx) had unmarked siblings.
- CLAUDE.md got a "bitmap-first triage" note: when a wedge points
  at a specific guest PC, grep `code-regions.txt` first; an
  unmarked PC means the loader didn't byteswap the word and the
  fix lives in `tools/classify-rom`, not in `src/trap.rs`.

Boot now advances past PC=0 into real REx code at PC=0x800194
(NATIVE_PRIM dispatch) and onward. The current visible activity
is a tight loop between two tasks (`'user'` and `'OBJM'`) doing
heavy MMIO traffic at 0x0F241000 (Voyager serial-chip control
register) interleaved with NATIVE_PRIM 0x106 / 0x107
(`PowerOnSubsystem(6)` / `PowerOffSubsystem(7)`). EinsteinProbe
confirms this is normal early-boot flash-init activity — on the
reference oracle the same code reaches 27 live kernel tasks (newt,
OBJM, pckm, idle, PMGR, PTBL, alrt, sndm, scrn, …) in 2 s wall
clock. Our hypervisor under QEMU TCG with all the iter-105
diagnostic probes still active runs much slower per kernel-second,
so reaching the same point will take significantly longer wall
clock.

**Next (iter-108):**

iter-107 closed (commit `f66bd0a9`). Investigation overturned the
SCTLR.V hypothesis — empirically `SCTLR.V` stays 0 throughout
boot, so the kernel uses **low** vectors and our bypass stub at
IPA 0x00FF_FEC0 IS the right install site. Real cause of the
bypass-stub miss: cache coherence. `write_rom_code_word` stores
into EL2's D-cache via Normal-WB; on Cortex-A53 / AEMv8-A the
I-cache is non-coherent, so the AArch32 instruction fetch cold-
loads stale memory bytes for the stub region. The classifier
marks 0x00FF_FExx as data (no walker reach), so loader-time
byteswap leaves bytes BE-natural; AArch32 LE instruction fetch
decodes them as garbage and falls through to UND_TRAMP, which
HVCs into EL2 → "FPA UND reached EL2" wedge.

iter-107 fix shipped (option (b) plus cache hygiene):
- `handle_und` FPA-arm now ERETs into FPE_JT (= 0x0038_D874) in
  UND mode, replicating the in-ROM bypass semantic from EL2.
  Per-miss counter; first 4 misses log.
- `patch_und_vector` calls `cpu::icache_publish_range` after
  installing the UND vector / bypass stub / UND_TRAMP / SBA stubs
  / UND_RETURN_STUB so the writes are visible to the AArch32
  I-cache fetch path (DC CVAU + DSB ISH + IC IVAU + DSB ISH +
  ISB per cache line). `icache_publish_range` was un-gated from
  the previous `nh_guest_test`-only build.

New wedge — `WriteDebugByte` NULL ring-buffer:

```
dabt-trip: PC=0x00199ce8 mode=und writing 0x00000020 -> IPA=0x0
           r0=0x0c1017b4 r1=0 r2=0x20 r3=0
*** unknown MMIO read halted ***
  IPA = 0x00000000  W  value=0x00000000  @ELR=0x199ce8
```

PC=0x00199ce8 sits inside `WriteDebugByte__Fc` (starting at
0x00199ccc). The instruction is `strb r2, [r3, r1]` — store byte
into a ring buffer at `obj[28]`. `r0 = 0x0c1017b4` (= the
`rdpInfo` debug context); `obj[28]` is the buffer pointer, which
loads as 0 → effective address 0 + 0 = 0 → the kernel writes
0x20 to IPA 0, which our hypervisor halts on as "unknown MMIO".

The call comes from UND mode (`mode=und`), reached via the FPE
handler that fired through iter-107's EL2 reroute. The FPE emits
debug bytes via `WriteDebugByte` for emulation tracing because
some prior init path set `gWantSerialDebugging=1` (per
INVESTIGATION.md / iter-79 force-enable). On a real Newton the
debug-card path (`rdpInfo` at 0x00199c10 → `ReadDebugLong` etc.)
initialises the ring-buffer pointer first; we're reaching
WriteDebugByte before that init runs.

Investigation needed:
1. **Confirm the call path.** Walk back from PC=0x199ce8 through
   `lr(und)=0x001993d8` to find which FPE arm calls
   WriteDebugByte. Likely a "log emulated FPA insn" trace.
2. **Decide:**
   a. Suppress the force-enable of `gWantSerialDebugging` so
      WriteDebugByte is never called from FPE. Cleanest if iter-79
      doesn't depend on debug output for forward progress.
   b. Patch `WriteDebugByte` to no-op (or to validate `obj[28]`
      before writing) so the unset-buffer case is harmless.
   c. Initialise the debug ring buffer in `rom_patches.rs` so
      `obj[28]` is non-NULL before the FPE runs.
3. Cross-check against EinsteinProbe to see how the reference
   oracle handles the same path — if EinsteinProbe never enters
   WriteDebugByte from the FPE, the right fix is (a).

### Iteration 105: REx pkgl relocation-table seeder

Goal: kill the iter-104 wedge at PC=0 with `SPSR.T=1`, root-caused
in the iter-105 task-switch + pre-ERET probes (see git log for the
diagnostic stack). The drvr task's saved_pc=0x00800968 was a valid
REx address, the kernel's ERET intent was correct, yet the user
ran zero instructions — proving the bytes at 0x00800968 weren't
the assembled `ldr r0, [r0]; b 0x800904` stub the disassembly
showed.

Cause: the classifier never reached 0x00800968. The 2-instruction
stub is referenced only by an absolute function-pointer slot at
0x00800df4 inside the EinsteinPlatformDriver package's part data;
nothing in ROM/REx code BLs it directly. With the slot's first-
word value (0x00800900-shape) failing every shape heuristic, the
walker had no way in. Without `reach=true` the loader didn't
byteswap the word at load time, so the kernel's BE-32 fetch of
0xe5900000 decoded as garbage and the ERET wild-branched.

Fix is in two parts (two separate commits):

1. **`collect_pcrel_ldr_thunk_run_roots`** (earlier iter-105
   commit): scan for runs of ≥3 consecutive `LDR pc, [pc, #-4]
   + <literal>` thunk pairs and seed every LDR-PA. Catches
   sibling thunks in a vtable-shaped table (e.g.
   0x008264dc, 0x008264fc, … 0x00826544 in the non-relocatable
   FGSoft package) whose only references are through structural
   pointers.
2. **pkgl relocation-table seeder** (this commit): parse each
   NewtonOS package in the REx pkgl block, walk its relocation
   table when `kDirRelocationFlag` is set (DCL TDCLPackage.cpp
   format), read each pointer-slot value, and seed it as a
   walker root via `va_to_pa`. The relocation table is the
   loader's authoritative list of pointers; no shape heuristic
   needed. In Einstein.rex this seeded 6 roots across 4
   relocatable packages — including 0x00800968 itself.

Heuristic removal: the 'FDRV' tag scanner in `rex_header_roots`
(scan every word of the FDRV class-info block for fnptr-shaped
values) was redundant with `collect_classinfo_roots` (precise
trampoline-shape match) and added 8 spurious main-ROM-vector
seeds. Removed; popcount unchanged. SeedSource::RexHeader is
unused now and gone.

Operational follow-on: CLAUDE.md gained a "bitmap-first triage"
note — when a wedge points at a guest PC, grep the
`code-regions.txt` first; an unmarked PC means the loader didn't
byteswap and the fix lives in `tools/classify-rom`, not
`src/trap.rs`.

Result:
- 36/36 guest tests pass.
- `reachable-code popcount` 880060 → 880087.
- `byte-access-static popcount` unchanged at 27790 (these aren't
  byte accesses).
- Cold boot advances from PC=0 wedge to PC=0x800194 (NATIVE_PRIM
  dispatch in EinsteinPlatformDriver), enters the normal early-
  init flash/serial-chip MMIO loop confirmed against EinsteinProbe.

<!-- Older iteration retrospectives (iter-98 through iter-104) live
     in `git log` per the auto-prune maintenance note. -->

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
