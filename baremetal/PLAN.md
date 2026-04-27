# Plan — Drive Newton OS to interactive use

## Status

**Phase A done.** Every CPU instruction and MMIO region in the early-boot
path has a real handler; "unknown sub-case" responses are loud trip-wires.

**Phase B done.** Boot reaches `TInterpreter::TInterpreter` and the full
driver suite. The `newt` task is alive and the system enters its idle
pause loop. The per-stall chronology that got us here is in
`INVESTIGATION.md` and the git log; the table at the bottom of this
file is the condensed view.

**Now: keep fixing stops until the system works.** No more phases — each
remaining wedge is its own commit and (where the surface is testable in
isolation) its own `guest-tests/tests/test_<name>.S`. There is no fixed
end-state milestone; we drive forward until the boot quiesces in a
steady-state idle that responds to whatever tablet / serial / network
inputs we choose to feed it.

## Workflow per stop

1. Capture the trace tail (`--features trace_once,quiet` for one-shot
   first-touch, `trace,quiet` when a tight loop is the symptom).
   `INVESTIGATION.md` is the running log; update it as facts accrue.
2. Identify PC, mode, and faulting access. Cross-reference against
   `scripts/disasm-out/rom.dis`, `_Data_/symbols.txt`,
   `_Data_/demangled_symbols.txt`, and Einstein's source under
   `Emulator/`. PCs ≥ 0x00800000 land in `Einstein.rex`; symbols there
   are not in our tables — read the rex bytes via the ROM disasm
   pipeline or step through Einstein.
3. Run the same offset under Einstein
   (`build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex
   30`) so we have a known-good oracle.
4. Decide where the fix belongs:
   - **Hypervisor handler gap** — implement / extend the relevant
     handler in `src/peripherals/*.rs`, `src/trap.rs`, etc.
   - **Einstein behavioural quirk we need to mirror** — port the
     specific arm of Einstein logic into our matching path (the
     `unknown bank #5` silent-zero in `src/mmio.rs` is the canonical
     example).
   - **ROM patch** — add to `src/rom_patches.rs` only when there is no
     other layer that can host the fix. We're past the era where ROM
     patches are routine; prefer hypervisor- or peripheral-side
     interventions.
   - **Deliver to the guest** — some aborts (NULL derefs, alignment,
     external aborts) are intended to be observed by the guest's own
     DABT vector. If the kernel has a recovery path, route the abort
     to it instead of halting.
5. Add a `guest-tests/tests/test_<name>.S` if the surface is testable
   without booting the ROM. Otherwise, the cross-Einstein comparison
   plus the live trace is the regression evidence.
6. Re-run, go to next stall.

## Tools available

### Hosts to run under

- **QEMU raspi3b** (default; `cargo run --release`) — fast, BCM2835
  VIC, AArch32↔AArch64 banking quirks documented in
  `docs/QEMU_BUGS.md`. The day-to-day driver. Wrapper:
  `scripts/run-qemu.sh`.
- **ARM FVP `FVP_Base_RevC-2xAEMvA`** —
  `scripts/fvp <elf>`. Accurate reference: GICv3, generic timer +
  cache model exact. Slow wall-clock, but required when QEMU's
  banking weirdness is suspect or when only Tarmac will do. Add
  `--gdb` for an Iris debug server on host port 7100. Build with
  `--no-default-features --features platform-fvp-base`.

### Trace and observation

- **Function-level tracer** — `--features trace` patches every entry
  in `scripts/classify-out/code-symbols.txt` with an HVC trampoline
  and logs `seq PC name (mode) r0..r3 lr` on each call. Use
  `--features trace_once` for first-touch (each function logs once
  per session, ~2800× quieter on a long boot). `--features quiet`
  silences the recurring diagnostic chatter (`fix_stage1_xn_bits`,
  XN re-walks, etc.) and is almost always desirable alongside trace.
  Trace mutates ROM, so traced runs cold-boot (snapshots saved with
  trace off are rejected on load and vice versa).
  Post-hoc first-call filter on a full `trace` log:

  ```sh
  awk '/^trace / && !seen[$4]++' run.log
  ```

  Same effect as `trace_once` but lets you keep the every-call log
  around and re-derive the first-call view (or any other dedup key
  — `$3` for PC-uniqueness, which separates overloaded methods that
  share a `$4` token).
- **Tarmac windowing on FVP** — `scripts/fvp --tarmac-window=<file>
  <elf>`. The plugin starts with tracing OFF; `src/tarmac.rs` emits
  `<<TRM_START>>` / `<<TRM_STOP>>` on the UART and the FVP's
  `bp.pl011_uart0.toggle_mti` flips the TarmacTrace on/off. Use to
  capture an instruction-accurate slice around a stall instead of a
  10+ GiB full-boot trace. `--tarmac=<file>` (no window) traces the
  whole run.
- **`scripts/trace-diff.sh`** — runs Einstein (`NewtonTrace`) and the
  hypervisor with function-entry tracing on, diffs the two logs.
  First diverging trace line is usually the right place to start.
- **`build/NewtonProbe`** — Einstein-as-oracle. `build/NewtonProbe
  baremetal/roms/newton.rom _Data_/Einstein.rex 30` runs the same ROM
  under Einstein, captures every CP15 access, SWP, mode transition,
  data abort `{PC, FAR, FSR, mode}`, and prefetch abort
  `{PC, IFSR, mode}`. Diff vs. our trap log to localise divergence.
  Findings cached in `probe/FINDINGS.md`.
- **Function tracer trampoline pool** is at IPA `0x00900000..
  0x00E00000`; tracer-side debug probes (putc buffering,
  newt-tripwire poll, mode-13 SP_svc tracking) live in
  `tracer::log_trace_at` and fire per-call even in `trace_once`
  mode.

### State capture

- **Snapshot ring** — 4 slots at `/tmp/newton-snapshot-{0..3}.bin`,
  autosaved every 2 s of wall-clock from `trap_irq`. `cargo run
  --release` resumes from the newest valid slot if the ROM
  fingerprint matches; cold-boot by `rm /tmp/newton-snapshot-*.bin`.
  Guest-triggered save: `HVC #0x20`. Captures GUEST_RAM + GUEST_FB +
  flash + EL1 sysregs + AArch64 GPRs (which alias every AArch32
  banked SP/LR per ARM ARM Table D1-79).
- **Framebuffer PNG dumps** — `/tmp/newton-fb/NNNNN.png`, written 1 s
  after the most recent `screen::blit`. 320×480 1-bpp grayscale,
  inverted so PNG viewers reproduce the panel. See `src/fb_dump.rs`.

### Debugging in flight

- **gdb on QEMU** — `DEBUG=1 cargo run --release` (term 1) +
  `aarch64-elf-gdb -x scripts/gdb-init <elf>` (term 2). EL2
  hypervisor BPs / source-line / `stepi` / `bt` work. Guest AArch32
  BPs go through helpers in `scripts/gdb-init`:
  - `bg <addr>` — conditional stop at `trap_sync_lower_aarch32` when
    `$ELR_EL2 == <addr>`. Catches naturally-trapping guest insns
    only (data/insn abort, SVC/HVC, CP15) — not UND, because the UND
    trampoline HVCs into EL2.
  - `bp <addr>` — patches the ROM word with `UDF #0xFFFE` so any
    ROM-range PC stops in `handle_user_bp_und` with `faulting_pc`
    set. Snapshot autosaves are gated while a `bp` is live.
  - Convenience: `tt N`, `guest-state`, `bp-clear`, `bp-list`.
- **DABT-vector DIAG HVC** at ROM offset `0x10` — every stage-1 DABT
  passes through `handle_diag` with full banked-register context
  before being forwarded to the kernel's DAH. Same for PABT at
  `0x0C`. These are diagnostic scaffolding (see the section near the
  end of this file), not load-bearing for guest correctness.
- **Software-reset canaries** — BootOS / PowerOffAndReboot / Reboot
  canaries in `rom_patches.rs` fire `HVC #0x42`/`0x43`/`0x44` on the
  first call so the path is loud rather than silently re-entered.

### Reference and disassembly

- **`scripts/disasm-out/rom.dis`** — full symbol-annotated ROM
  disassembly. Currently covers base ROM (≤ `0x71fc4c`) only; REx
  is not yet pipelined through. See `docs/DISASM.md`.
- **`docs/NEWTON_INTERNALS.md`** — APCS calling convention,
  two-level object dispatch, ROM jump-table at `0x01A00000..
  0x01C20000`, DDK header locations.
- **`docs/QEMU_BUGS.md`** — raspi3b AArch64↔AArch32 quirks,
  especially around banked registers at exception entry. Read
  before suspecting hypervisor code at that boundary.
- **`docs/STRUCTURES.md`** — Newton kernel data-structure layouts
  decoded from the disasm.
- **`docs/WORKFLOW.md`** — process notes (Einstein-driver review by
  sub-agent; test-per-feature; finish-the-phase semantics).
- **`docs/peripherals.md`** — peripheral implementations.
- **`probe/FINDINGS.md`** — golden record of what a fully-booted
  Newton actually does. Regenerate with `cmake --build build
  --target NewtonProbe` and `build/NewtonProbe baremetal/roms/
  newton.rom - 90`.

### Test suites

- `baremetal/guest-tests/scripts/run-all.sh` runs the 35 guest tests
  on QEMU; `--platform fvp` runs the same suite on the FVP. Both
  must stay green. See "Verification" near the end of this file.

## Current stop — NULL-pointer write at REx 0x95c444 (2026-04-27)

```
*** data abort ISV=0 at ELR=0x95c444 SPSR=0x20000110
    IPA=0 FAR=0 iss=0x4e
    SCTLR_EL1 (guest) M-bit = 1 (stage-1 ON)
```

Decoded: `iss=0x4e` ⇒ `WnR=1` (write), `DFSC=0x0e` (stage-2 permission
fault, level 2). `FnV` clear so `FAR=0` is valid → guest accessed
VA = 0. Stage-1 maps VA 0 → IPA 0 (kernel `L1[0]` is the small-page
coarse table at PA 0x400, identity-mapping the first 1 MiB); stage-2
has IPA 0..0x1000000 mapped read-only as the ROM aperture, hence the
permission fault on the write.

`ELR=0x95c444` lands in **Einstein.rex** (REx base 0x00800000, REx
offset 0x15c444). The trace tail just before the abort:

```
trace 4147559 0x00050d18 VccOff(int)              (usr) ...
trace 4147560 0x00050d28 VccOff(int, unsigned long) (usr) ...
*** data abort ISV=0 at ELR=0x95c444 ...
```

`VccOff` is a PCMCIA `TCardSocket` method, so the failing write
originates somewhere in the REx-resident PCMCIA driver path. The
faulting instruction wasn't a plain word `LDR`/`STR` immediate, so
`try_emulate_isv0_dabt` (`src/trap.rs:542`) declined to handle it and
dropped to the loud halt at `src/trap.rs:462`.

Next steps:

- Disassemble REx around `0x95c444` to identify the instruction shape
  (likely byte/halfword store, LDM/STM, or pre/post-indexed with
  writeback) and whatever pointer chain produced VA=0. The disasm
  toolchain currently only covers base ROM (up to `0x71fc4c`); extend
  it to cover REx, or use `objdump` directly on the REx region we
  embed.
- Cross-check against Einstein: does `TCardSocket::VccOff` legitimately
  hit a NULL state field on this code path in Einstein, or does
  Einstein's PCMCIA model populate something we leave blank? Most
  likely the latter — we currently halt loudly on every PCMCIA-class
  surface in `src/peripherals/pcmcia.rs`.
- Decide whether to (a) extend `try_emulate_isv0_dabt` to cover the
  faulting instruction shape and let `mmio::write` drop it like a
  legitimate MMIO write, (b) populate the PCMCIA state the driver
  expects to be non-NULL, or (c) deliver the abort to the guest's
  DABT vector and let the kernel's `UnhandledException` path run.

## Resolved stops (newest first)

| Date | Wedge | Resolution |
|------|-------|------------|
| 2026-04-27 | TEncodingMap.+16 = 0x20000110 (out-of-stage-2 IPA) at `ConvertToUnicodeFunc_Contiguous8` | mmio.rs: `0x20000000..0x30000000` "unknown bank #5" silent-zero matching Einstein's `TMemory::ReadP` (TMemory.cpp:1026-1034). Boot advanced 10× → reaches TInterpreter. |
| 2026-04-27 | `Reboot` canary inside `TInterpreter::TInterpreter` — DFSC=5 at FAR=0x0cd07400 on lazy-L1 section grow during `TRefStructStack::Fill` (L1[0xCD]=0x90 lazy marker) | γ-fix in `handle_diag`: read L1.domain from the faulting VA's L1 entry and write it into DFSR_EL1.bits[7:4] before forwarding to DAH (ARMv7 leaves Domain UNK on DFSC=5; kernel was reading 0). |
| 2026-04-26 | BootOS canary entry #2 (R0=0x0cc80c80) — `name`-task stack-overrun corrupts neighbour task on shared PA | 3-instruction ROM patch in `TStackManager::ResolveFault` (mask=0xF) forces per-page stack allocation. |
| 2026-04-25 | `newt`-DABT alias narrows to scheduling order | IRQ-rate + tick-page divergence fixed. |
| 2026-04-25 | Recursive DABT in `TStackInfo::Init` | Flash recovery path eliminated. |

See `INVESTIGATION.md` for the full chain of analysis on each.

## Critical files

- `src/guest_mem.rs` — ROM load + byteswap; `fix_stage1_xn_bits` (L1 +
  coarse-L2 normalise; flattens ARMv4 subpage-AP to AP=011; skips the
  shadow-stub scratch L1 slot so it doesn't fight the installer; now
  returns `bool` indicating whether ROM bytes mutated this call so
  flash-checksum reseeds skip when nothing changed); UND-vector
  trampoline at ROM offset `0x00FFFF00`; DABT-vector DIAG HVC patch at
  ROM offset `0x10`; `dump_stage1_walk`; scratch-VA L1 section
  installer at `L1[0x60]`.
- `src/trap.rs` — CP15 shim (TVM trap on writes to VM regs); HVC
  dispatch (UND_TAG / DIAG_TAG / DIAG_LR_TAG / SBA / tracer / canary
  tags); `handle_und` (SWP, SystemBoot/Debugger/TapFileCntl UND, MCR
  c15,1,2 StrongARM clock, MCR c7,c7,0 deprecated cache-invalidate);
  `handle_fp_simd` → CP10/11; two-stage `handle_diag` /
  `handle_diag_lr` DABT-intercept stub; `handle_data_abort` with
  kernel-DABT forwarding for lazy stack growth; `try_emulate_isv0_dabt`
  for ISV=0 word LDR/STR.
- `src/guest.rs` — HCR_EL2 (TVM, TIDCP, TSW, IMO, FMO, AMO);
  CPTR_EL2.TFP for CP10/11; DC bit toggling across stage-1 on/off.
- `src/stage2.rs` — stage-2 L1/L2/L3. 2 MiB blocks for ROM/RAM/flash/FB;
  4 KiB L3 pages for the MMIO window `0x0F000000..0x0F200000` and the
  64 KiB shadow-stub scratch carve-out at IPA `0x0600_0000`.
- `src/timer.rs` — CNTHP driver; instruction-anchored synthetic ticks.
- `src/banked.rs` — AArch32 banked-register access from EL2 per
  ARM ARM Table D1-79.
- `src/peripherals/{serial,serial_driver,native_primitives,screen,
  platform,battery,tablet,sound,network,printer,host_call,
  in_translator,out_translator,flash,flash_driver,vic,dma,pcmcia}.rs`
  — Newton driver / native-primitive surface.
- `src/mmio.rs` — routes the MMIO window plus the `0x20000000..
  0x30000000` "unknown bank #5" silent-zero arm and PCMCIA banks.
- `src/rom_patches.rs` — Einstein word-write patches; debugger HVC
  injections; GetClock / SetAlarm wrap-detect ls→cc fixes;
  PowerOffAndReboot / Reboot / BootOS canaries; `TStackManager::
  ResolveFault` per-page-stack-allocation patches.
- `src/shadow_stub.rs` — BE-32 byte/halfword-access patcher (DeadReg /
  Stack / ScratchVA stub variants; 16-word stub layout).
- `src/snapshot.rs` — rolling ring under `/tmp/newton-snapshot-{0..3}.bin`.
- `src/tracer.rs` — function-level tracer (HVC trampolines on every
  `code-symbols.txt` entry); `trace_once` feature gates the per-call
  trace line behind a fired-bitmap so each function logs at most once.
- `src/fb_dump.rs` — 1 s after each `screen::blit`, dumps GUEST_FB to
  `/tmp/newton-fb/NNNNN.png` via Arm semihosting.
- `src/guest_bp.rs` — `bp <addr>` infrastructure for the gdb workflow.
- `src/task_dump.rs` — `TScheduler` / `TTask` dumps from EL2.
- `src/tarmac.rs` — Tarmac-like instruction-trace markers.
- `src/unaligned.rs` — `handle_align_fault` emulator for SCTLR.A=1
  unaligned LDR/STR aborts.
- `guest-tests/tests/` — 35 tests; `guest-tests/scripts/run-test.sh`
  clears snapshots before each run.

## Verification

Each commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 35 tests pass at the current commit.

## Non-goals

- Real screen emulation beyond the framebuffer dump — no compositor,
  no pen input.
- Package loading — needs a solution for embedded native code.

## Diagnostic scaffolding

These are load-bearing for the current stop-fixing loop and stay until
the boot is steady-state-quiet:

- DABT-vector HVC patch at ROM offset `0x10` →
  `handle_diag` / `handle_diag_lr` in `trap.rs`. Catches every stage-1
  DABT with full banked-register context.
- PABT-vector HVC patch at ROM offset `0x0C` — same DIAG path.
- `handle_diag_from_bp` hook in `guest_bp.rs::handle_user_bp_und`.
- 500-entry trap log budget at the top of `trap_sync_lower_aarch32`;
  HVC `#0x50` (tracer TAG) suppressed to avoid doubling trace output.
- Bring-up VA walks in `handle_diag`.
- BootOS / PowerOffAndReboot / Reboot canaries in `rom_patches.rs`.

Once the boot quiesces these can be pulled; the behavioural invariants
they enforce are codified in guest tests.
