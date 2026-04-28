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

**IMPORTANT:** The goal is to run the *original ROM code* successfully.
Don't introduce patches or workarounds just to get the run to progress
farther. Diagnose and fix the actual problem. *No workarounds, no deferrals,
no shortcuts.*

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

NOTE: Fix all compiler warnings before committing, to keep context clean.

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

- `baremetal/guest-tests/scripts/run-all.sh` runs the 36 guest tests
  on QEMU; `--platform fvp` runs the same suite on the FVP. Both
  must stay green. See "Verification" near the end of this file.

## Current stop — Stage-1 aliasing confirmed: kernel maps heap VA and user-stack VA to same PA

**Aliasing CONFIRMED at stage-1.** This iteration's stage-1
walk probe proves both VAs translate to PA 0x04032000:

```
VA 0x0ca6b010 → L1[0xca] coarse → L2[0x6b]=0x0403203e → PA 0x04032000
VA 0x0cc82018 → L1[0xcc] coarse → L2[0x82]=0x0403203e → PA 0x04032000
```

The L2 entries are byte-identical (0x0403203e). This is NOT a
stage-2 issue — both VAs reach the same IPA via stage-1. Two
distinct L2 tables (in different L1 slots, at different L2
PAs) both name the same backing physical page.

The `apply_resolve_fault_wrapper` ROM patch in
`src/rom_patches.rs:894` is the prime suspect. It makes the
kernel run `TStackManager::ResolveFault` **four times per
stack fault**, with the `GetMatchingPage→0` companion patch
forcing every subpage to allocate a fresh physical page. The
heavy allocator churn this produces is the most credible
cause for the kernel's page-pool re-issuing PA 0x04032000
(originally allocated to the RelocHeap) under stack-grow
pressure later in boot.

**Hypothesis #1 (banked-SP-aliases-heap)** was refuted in the
prior iteration. **Stage-2 bugs** (e.g., set_ram_page_ro_x
corrupting unrelated entries) are now also ruled out by this
iteration — both VAs reach the same IPA *before* stage-2 sees
them.

## Concrete next steps

1. **Diagnostic-only revert test.** Revert
   `apply_resolve_fault_wrapper` install + the
   `GetMatchingPage→0` patch (revert just those two,
   keep everything else), cold-boot, watch the heap-watch
   sanity-halt. Three possible outcomes:
   - **Heap stays intact** (boot wedges elsewhere on the
     original ARMv7 subpage-AP issue): confirms the wrapper
     or its companion patch is the cause. Next move: fix the
     wrapper (don't iterate 4×; or iterate 4× but keep
     bookkeeping consistent so the page-pool doesn't get
     confused).
   - **Heap still corrupts at the same point**: rules out
     the wrapper; the bug is somewhere else. Look at
     `fix_stage1_xn_bits` next — does it ever touch the PA
     field of an L2 entry?
   - **Boot fails earlier in some other way**: useful data;
     means the patches do gate against an earlier bug we
     haven't characterised since they were added.
2. **Audit the wrapper without reverting.** Read
   `apply_resolve_fault_wrapper` in detail; trace what
   `FindOrAllocPage` returns for each of the 4 iters; check
   whether the kernel's page-table-slot bookkeeping is left
   consistent across iters. The wrapper IS supposed to
   produce 1 page per fault (iter 0 allocates, iters 1-3
   reuse via slot-already-filled), so any double-allocation
   would be a real bug in the wrapper's logic. Pair this
   with PLAN.md option 3 (find every VA mapping to PA
   0x04032000 in the kernel's tables — easy walker since
   the kernel L1 sits at PA 0x04000000 and is 16 KiB).
3. **Audit `fix_stage1_xn_bits`** in `src/guest_mem.rs` for
   any path that could set the PA field of an L2 entry.
   It's documented as flattening AP bits; if it accidentally
   writes PA bits, that's another candidate.
4. **Cross-check Einstein's L2 tables.** Add a stage-1 walk
   to NewtonProbe at the equivalent boot offset, dumping
   the L2 entries for VA 0x0ca6b000 and a typical
   user-stack VA. If Einstein's two L2 entries name
   *different* PAs, that confirms the kernel wouldn't
   normally alias these VAs — the divergence has to come
   from us.

Diagnostic scaffolding (heap-watch sentinel, stage-2 RO
carve-out, sanity-halt with banked-SP + ring-SP capture,
stub-orig-PC decoder) stays armed.

The probe extension itself (`heap_header_dump` in `probe.cpp`)
stays in the tree; it's cheap and useful for any future
heap-state cross-check.

## Earlier stop — wild jump into SBA inline-stub pool downstream of the no-fit recovery

The previous SearchFreeList wedge is side-stepped (see resolved
stops). Boot progresses ~2400 trace lines further and halts on:

```
dabt-trip: PC=0x00f76368 mode=usr writing 0x00000082 -> IPA=0x3
*** unknown MMIO write halted ***
```

PC `0x00f76368` is **inside `shadow_stub`'s inline-stub pool**
(`SBA_STUB_POOL_IPA = 0x00E00000` .. `SBA_STUB_POOL_END = 0x00FFFFf00`,
in `src/shadow_stub.rs`). The bytes there are written by
`shadow_stub::patch_rom_from_bitmap` at boot — not by the ROM
image. Confirmed via paired `kmain` dumps: the slot reads zero
post-`load_rom` and `0xe5cc0000` post-`patch_rom_from_bitmap`.

Decoded stub body around PC:

```
+0x00 (0xf76368): e5cc0000  strb r0, [r12, #0]     ← faulting (r12=3)
+0x04           : e320f000  nop
-0x04           : e128f001  msr cpsr_c, r1
```

The stub assumes a specific calling convention (`r12` = effective
target, `r0` = byte to store, etc.) that's set up only by the
SBA-trap path that branches into it. Our wild jump arrives with
`r12 = 0x3` — a byte from the corrupted heap leaking through —
and the `strb r0, [r12]` writes to IPA 3 → unmapped → halt.

So this halt is **downstream blast radius** from the original
RelocHeap header corruption: the kernel dispatches through a
vtable / function pointer that's been clobbered to a value inside
the SBA stub pool (the corrupted heap header contains many
ROM-PC-like values; one of them lands a valid-looking branch
target inside our pool). Caller chain on the user stack still
carries the bad heap (`stack[sp]=0x0ca6b010`,
`lr_usr=0x00311e1c`).

Update — the gCurrentHeap-clear from this iteration's #3 attempt
DID write back (`prev=0x0ca6b010, write_ok=true`) but the boot
still wedges identically. Reason: the caller (`__nw__FUi`'s
recovery path) saved the bad heap pointer to its own stack
frame BEFORE calling SearchFreeList; clearing `task[-16]` after
the fact doesn't unwind those copies. So the cascade has to be
broken at a different layer.

The new stub-orig-PC decoder identified the wedge precisely:
`PC=0x00f76368` is `shadow_stub`'s emulation stub for ROM
`0x00312a18` = `strb r0, [r9]` inside `SetBlockSize`. The kernel
runs the strb naturally through the stub; `r9` (= NewBlock's
return value) is `0x3` — a corrupted block pointer sourced from
the bad heap's `heap[+0x48]`. So this isn't a wild branch into
the stub pool, just normal code path with corrupted register
contents.

Concrete next steps:

1. **Reject the bad heap at SetCurrentHeap entry.** The
   SetCurrentHeap probe at ROM `0x00142df0` already detects
   `r0=0x0ca6b010`. Substitute a known-good heap pointer
   (e.g. force `r0 = gFallbackHeap = *0x0c101080`) before
   letting the function run, OR ELR straight to the early-exit
   at `0x00142e08` so the no-op path fires. The bad heap
   never gets installed in `task[-16]`, the cascade never
   starts.
2. **Or validate NewBlock's return.** Hook the LDR/STR pair
   at SetBlockSize ROM `0x00312a08`/`0x00312a18` and short-
   circuit when `r0` (NewBlock return) is below a small
   threshold (e.g. `< 0x1000`). More surgical but only
   triggers when the bad heap actually produces a bad return.
3. **Cross-check Einstein** — what does Einstein do at the
   equivalent boot offset? Einstein's heap stays valid so it
   doesn't reach this state, but understanding the
   no-fit-recovery dispatch helps choose where to intercept.

## Earlier stop — RelocHeap header corruption in newt's MakeStoreObject path

The bus-error throw inside `CardFaultMonProc` (0x4e528) is triggered
upstream by a DABT at `SearchFreeList` PC=0x00313308 with
FAR=0xe52d006c. The "heap" pointer that `GetCurrentHeap` returns at
that point (0x0ca6b010) is **the legitimate RelocHeap** — created by
`NewHeap` call #3 with base=0x0ca6b000, size=2 MiB
(`/tmp/run-probe.log`). Multiple kernel call-sites switch into it
during normal operation (`NewHandle` at 0x141c40 / 0x1415d4,
`CompactHeap` at 0x31325c / 0x3132cc, `HUnlock` at 0x141ef0). So
SetCurrentHeap is **not** the source of the wedge.

The wedge is **content corruption**: the heap's 128-byte header has
been overwritten in `+0x00..+0x14` (saved ROM PCs and stack pointers
that look like `__ct__9TRefStackFv` ctor / `TStoreObjectWriter` ctor
frames) and at `+0x48` / `+0x60` (freelist position now points at
ROM PC `0x002dfa20`). SearchFreeList walks the bogus freelist and
dereferences `*0x002dfa24 = 0xe52d006c` (= `str r0, [sp, #-108]!`
instruction encoding), giving the FAR=0xe52d006c fault.

The RelocHeap region (0x0ca6b000..0x0cc6b000) does **not** overlap
newt's user stack (sp_usr=0x0cc81f04 at the wedge), so this isn't a
direct stack/heap aliasing bug. See `INVESTIGATION.md` for the
heap dump and probe trace.

Diagnostic scaffolding (still installed; stay armed via re-occupy
slot — no ROM-mutation churn per hit; capped at 32 lines per probe
but always log on the wedge-relevant arg matches):

- `kmain` installs `guest_bp` at `0x00313308` (SearchFreeList wild-r0
  halt), `0x001a4948` (TRefStack post-NewStack), `0x00142df0`
  (SetCurrentHeap entry), `0x00310e24` (NewHeap entry).
- `handle_user_bp_und` arms emulate the patched-out instruction
  (LDR-from-r0, `add sp, sp, #4`, `ldr r1, [pc, #40]`,
  `mov ip, sp`) and re-occupy the slot before ERET, so each
  invocation re-traps without disarming.

Concrete next steps:

1. **Hook `SearchFreeList` to fail gracefully.** Both QEMU and
   FVP reach the same wedge with identical corruption — this is
   a Newton-OS allocator divergence, not a hypervisor bug. The
   carve-out captures most writers but not the actual corrupting
   write (which lands during the RW window between fault-and-
   next-trap; see INVESTIGATION.md). Rather than chase the writer
   with an ARM-store decoder or stage-2 invalid-entry mode (both
   significant implementation cost), patch `SearchFreeList` (ROM
   `0x003132d8`) to short-circuit on a wild freelist node:
   detect that `*r0` would translate-fail (or matches the
   `0xe5xx_xxxx` instruction-encoding signature of a ROM PC
   misread as a freelist-next pointer) and return `r0 = 0`
   (no fit). The kernel's caller is `__nw__FUi`, which on
   "no fit" tries the next heap or throws `exMemFull` — both
   benign compared to the bus-error throw we currently get.
2. Cross-check Einstein at the equivalent boot offset (NewtonProbe
   60 s) — dump Einstein's RelocHeap header at the same point and
   diff. If Einstein's heap stays valid, the bug is in our
   reproduction of one of the upstream allocator side-effects;
   if Einstein corrupts it the same way, the ROM internally
   recovers from this, and so should we (option 1 mirrors that
   recovery).
3. Once the wedge no longer halts, observe what new state the
   boot reaches and pick the next stop.

The carve-out + dabt-on-carve scaffolding should stay armed — it
gives us a continuous log of write activity on the heap header
that's useful for any follow-up investigation.

## Earlier stop — newt self-deadlocks on the heap-store TULockingSemaphore

`newt` is permanently queued on a TSemaphore at `0x0c116eb8`'s
BlockOnInc list (queue at `+0x20 = 0x0c116ed8`). The owning
TSemaphoreGroup is at `0x0c116e94` (kernel id `0x13d7`). Its
TULockingSemaphore wrapper is at `0x0c116e7c`; the lock-state word
at `0x0c116e8c` holds `0x3063` — newt's own task id.

`task_dump`'s saved-PC walker shows newt at PC `0x3ae1fc` (the SVC
of `SemaphoreOpGlue`), `lr_usr=0x25a2e0` (after `bl SemOp` in
`TULockingSemaphore::Acquire`), and the user stack carries saved
LRs `0x143334` (`DisposPtr`'s `bl Acquire` site) and `0x354724`
(`MakeStoreObject`'s exception catch handler). The sequence is:

1. Newt enters `MakeStoreObject` (ROM 0x354178), acquires the heap
   store's TULockingSemaphore via `LockStore`. lock-word ← newt id.
2. Newt does `StorePermObject` work, then **throws `exBusError`**
   (Throw at trace 4149074 with r0 = literal pool entry pointing to
   `exBusError` at ROM 0x3712b8). Bus-error origin: unidentified —
   most likely a guest MMIO read or stage-2 fault we should
   silently default rather than turn into a guest exception.
3. The catch handler at ROM 0x3544f4 calls `TStoreWrapper::Abort`
   (0x354b50). Abort does **not** call `UnlockStore` — verified by
   reading the body. So the lock stays held by newt.
4. The destructor `~TStoreWrapper` (0x353ae4) runs through
   `DisposeRefHandle`, which lands in `DisposPtr` (0x14320c).
   DisposPtr calls `Acquire` on the heap semaphore at 0x143330.
5. That `Acquire`'s Swap finds `*lock_state == newt id`, so it
   calls SemOp → BlockOnInc. Self-deadlock — only newt could
   release the lock, and newt is now blocked on it.

Einstein cross-check (`scripts/fvp` not needed; `build/NewtonProbe`
60 s shows newt reaching `BLK→RDY` cycles with `Tmux RUN`, `scrn`
already created, etc.) — Einstein never lands on this deadlock.
The most plausible reading is that step 2 (the Bus Error) doesn't
fire on Einstein, so the lock-leak path is never taken there. So
the right fix is to **find the bus-error origin and make it not
throw**, not to retro-fit recursive-lock semantics into
TULockingSemaphore.

Concrete next steps:

1. Re-run with `--features trace,quiet` (every-call trace, not
   `trace_once`) and capture the data abort or MMIO read that
   triggers the throw. Cross-check against Einstein at the same
   trace offset — the divergence will name the handler we need to
   silently default. (See `docs/peripherals.md` for the existing
   silent-default arms; Einstein's `TMemory::ReadP` is the oracle
   for what value to return.)
2. Implement the silent default. Verify the deadlock disappears
   and newt makes forward progress. Past this point we expect
   `TScreenDriver::*` to instantiate (compare against the Einstein
   t=2 s probe dump).
3. Then return to "feed inputs": `peripherals/tablet.rs` is the
   lightest-touch path once `newt` is actually scheduling.

## Resolved stops (newest first)

| Date | Wedge | Resolution |
|------|-------|------------|
| 2026-04-27 | NULL-pointer SWP via `Swap(0,1)` at ROM `0x3ae204` (kernel `Acquire(NULL)` glue inside `VccOff__FiUl`) — stage-2 perm fault on write to ROM aperture, ISV=0 | trap.rs `try_absorb_rom_write`: mirror Einstein `TMemory::WriteP` (TMemory.cpp:1755-1766), drop the store; for SWP/SWPB also run the load piece into Rd. Boot reached steady-state idle. Test: `guest-tests/tests/test_swp_rom_aperture.S`. |
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
  for ISV=0 word LDR/STR; `try_absorb_rom_write` mirroring Einstein's
  silent-drop of writes to the ROM aperture (SWP/SWPB load piece runs).
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
- `guest-tests/tests/` — 36 tests; `guest-tests/scripts/run-test.sh`
  clears snapshots before each run.

## Verification

Each commit:

```
baremetal/guest-tests/scripts/run-all.sh
```

All 36 tests pass at the current commit.

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
- `guest_bp` installs from `kmain`: `0x00313308` (SearchFreeList
  ldr — emulates the load when r0 translates, ELRs to 0x00313360
  (no-fit exit) when r0 doesn't translate, allowing the kernel's
  out-of-memory recovery to handle it instead of bus-error-throwing),
  `0x001a4948` (TRefStack post-NewStack r0/r4 + sp/lr probe),
  `0x00142df0` (SetCurrentHeap entry r0/lr probe — always logs when
  r0=0x0ca6b010), `0x00310e24` (NewHeap entry r0(base)/r1(size)/lr —
  always logs when r0=0x0ca6b000). All four arms in
  `handle_user_bp_und` emulate the patched-out instruction and
  re-occupy the slot, so the marker UDF stays armed for the whole
  boot without per-hit ROM churn.
- `heap_watch::sample` (`src/heap_watch.rs`) called from
  `trap_sync_lower_aarch32` and `trap_irq` entry — samples
  `heap[0x0ca6b010]` every trap, maintains a 32-slot ring buffer
  of recent ELRs, and on every value transition logs the change
  plus the ring buffer to the kernel console. Used to bracket the
  RelocHeap-header corruption writer to a tight trap-stream window.
  Remove with the rest of this stop's scaffolding.
- `heap_watch::arm_carve_out_at_heap_va` + the carve-out branch
  in `handle_data_abort` — installs a stage-2 RO carve-out on
  the 4 KiB page backing `VA=0x0ca6b000`, follows the VA across
  stage-1 rebinds, and logs every guest-side perm fault on the
  page (writer ELR + IPA + decoded value when ISV=1).
  `stage2::ram_page_l3_entry` is the readback helper for
  verification. `heap_watch::log_stage1_walk` decodes the
  kernel's L1/L2 entries for VA 0x0ca6b010 to disambiguate
  stage-1-RO vs stage-1-RW writes. The "dabt-on-carve" trace
  in `handle_data_abort` (top of the function) is an
  unconditional all-class DABT log for the armed PA, used to
  distinguish "no fault fires" from "fault fires but our arm
  doesn't match". Remove together with the heap_watch sentinel.
- `heap_watch::check_heap_sanity` — multi-field heap-header
  invariant probe (heap[+0]=base, heap[+8]='skia' magic). Wired
  into `sample()` after the transition log; halts on the first
  trip-wire with a full header dump + ring-buffer trap stream.
  Gated on ELR being outside known heap-allocator PC ranges
  (`0x140000..0x148000`, `0x310000..0x320000`) so partial
  allocator updates don't false-positive.

Once the boot quiesces these can be pulled; the behavioural invariants
they enforce are codified in guest tests.
