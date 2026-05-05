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

**Current goal (iter-100):** iter-99 landed the fault-handler LDR
byteswap stubs (B-to-stub pattern, `r12` scratch, no REV — ARMv4),
but the boot still wedges at the same `unrecognised UND: insn=
0xe3a02a13 at PC=0x7a56e4` — meaning the kernel's UND handler is
NOT being reached, so the iter-99 stubs are inert. Diagnosis: this
PC is a real CPU-fetch UND on the un-byteswapped encoding, not a
kernel-decoded UND.

Look at 0x007a56cc..0x007a56f0 in `rom.dis`:

```
7a56cc: e1a0c00d   mov ip, sp
7a56d0: e92dd800   push {fp, ip, lr, pc}
7a56d4: e24cb004   sub fp, ip, #4
...
7a56e4: e3a02a13   mov r2, #0x13000   ; gROMPublicJumpTable
...
7a56f0: e91ba800   ldmdb fp, {fp, sp, pc}
```

A clean APCS-prologue function. But the classifier marks 0x7a56cc..
0x7a56f0 as data — no symbol, no static B/BL anywhere in the ROM
disasm targets it. It's reached only via runtime function-pointer
dispatch (likely from a ROM-driver class info struct).

Without classification as code, the bytes are NOT byteswapped at
load. CPU LE-fetch reads the original BE bytes interpreted as LE:
`mov r2, #0x13000` (BE 0xe3a02a13) becomes 0x132aa0e3 — `MOVWNE
r2, #0xa0e3` in ARMv7+ or undefined in ARMv4. The CPU UNDs.

Fix candidates:

1. **MANUAL_CODE_ROOTS in classify-rom** — add 0x007a56cc and any
   peer functions found by inspection in the 0x7a5xxx ROM-driver
   region. Smallest change; depends on hand-finding each missing
   entry.
2. **Discover the runtime caller** — instrument the kernel's
   dispatch path (likely a ClassInfo method invocation) to log the
   target address; trace back to the static structure that stores
   the function pointer; teach the classinfo collector to follow
   that field.
3. **REx-aware function discovery pass** — walk the REx package
   data structures for any field whose word value points at
   prologue-shaped code, seed each as a root. Catches all ROM-
   driver entry points at once.

Order: try (1) for the immediate unblock, then (3) as the durable
fix once the kernel-side dispatch is understood.

Iter-99 stubs are still in place and still correct — they will
matter as soon as boot reaches a real fault-handler-decoded UND
(e.g., FP-emulation entry, byte-access shadow_stub UDF). Don't
revert.

Remaining classifier diagnostic noise: 35 ROM-soup walk entries,
all legitimate ROM-driver TClassInfo trampolines at 0x7a5xxx
(TMainDisplayDriver, TScreenDriver, "four"-named driver) plus a
handful of B-AL run dispatch tables. The user-defined ROM-soup
range (0x3afda8..0x800000) intentionally over-reaches; the logging
is left enabled as a tripwire.

### Iteration 99: fault-handler LDR byteswap stubs

Goal: clear the suspected PC=0x7a56e4 stall by making the kernel's
instruction-as-data `LDR`s in the fault handlers return the
encoding the kernel was compiled to recognise, despite our load-
time byteswap of code-marked words.

Patches (B-to-stub pattern, `r12` as scratch, no REV — ARMv4):

| PC          | Insn                  | Stub action |
|-------------|-----------------------|-------------|
| `0x003931e4`| `ldr r0, [lr]`        | DABT decode: load + byteswap r0 + B 0x003931e8 |
| `0x0038ce9c`| `ldr r1, [lr, #-4]`   | UND marker: load + byteswap r1 + B 0x0038cea0 |

Each stub is 6 words allocated in the patch-stub arena
(`alloc_patch_stub`):

```text
LDR Rt, [lr...]              ; load (byteswapped numerical)
EOR r12, Rt, Rt, ROR #16     ; classic ARMv4 byteswap
BIC r12, r12, #0xFF0000      ;
MOV Rt, Rt, ROR #8           ;
EOR Rt, Rt, r12, LSR #8      ;
B   resume_pc                ;
```

`r12` (`ip`) verified unused across both replaced LDRs (read the
disasm in trap.rs::handle_und context); APCS caller-saved.

Result: 36/36 guest tests pass. Boot still wedges at the same
PC=0x7a56e4 — the stubs are inert because the wedge fires before
any kernel fault handler runs. Diagnosis (queued for iter-100):
classifier miss at 0x7a56cc.

### Iteration 98: classifier refinement — data-stop ranges, alt-entry, ROM-soup log

Goal: drive the classifier's false-positive rate down by patching
three observed misclassifications (3861e4–e8 missing as code,
3948e8–39965c spurious as code, 7a0dbc–7a11fc spurious as code)
and add diagnostic logging for any walk that crosses into the
post-code ROM data region.

Major changes:
- `DATA_STOP_RANGES` — half-open `[start, end)` ranges the walker
  refuses to enter and `load_symbol_roots` refuses to seed.
  Mirrors `classify-symbols.py`'s `DATA_RANGES`. Stops the cascade
  where data symbols (e.g. `PublicFiller` at 0x003948e4 with first
  word `0xE6000410`) seed the walker, who then walks linearly
  through bp-weight data and pushes misdecoded `bne` targets into
  NSRuntime / package data at 0x7a0dbc, 0x7ed138, 0x7ed2ec.
- `collect_alt_entry_roots` — new indirect-pass collector for the
  `mov r0, pc; mov pc, lr` micro-trampoline that follows
  `add/sub pc, ip, #N` in Newton's class-info dispatch. The pair
  is a "get class-name string pointer" alt entry; without this
  collector the alt entry at 0x3861e4 (TClassInfoRegistryImpl::
  ClassInfo dispatch helper) was unmarked because no static caller
  exists.
- `ROM_SOUP_RANGE = 0x3afda8..0x800000` walk-entry log: per popped
  walk, the first word inside the range produces a stderr line
  with the full origin trace stack. Diagnostic only — does not
  drop bits.
- `SeedSource::Symbol` now carries the symbol name as
  `&'static str` (leaked at parse time). `Seed(Symbol "PublicFiller_1")`
  vs the prior useless `Seed(Symbol)`.

Result:
- `byte-access-static` 28291 → 27769 (-522 false positives).
- 1879 symbol roots dropped via data-stop-range; 2 alt-entry
  roots added.
- Boot reaches PC=0x7a56e4 (TMain… driver init); 35 remaining
  ROM-soup walk-entries, all legitimate ROM-driver class info.
- Invariant (oracle ⊆ static) still holds.

<!-- Older iteration retrospectives (iter-97 and earlier) live in
     `git log` per the auto-prune maintenance note. -->
<!-- iter-90 deferred shadow_stub deletion: still gated off
     (`patch_rom_from_bitmap` no longer called from `main.rs`); full
     removal + SBA dispatch arms + `unxor_sub_word` guest-test path
     is a follow-up commit. -->



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
