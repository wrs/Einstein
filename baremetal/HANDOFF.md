# Handoff — Newton hypervisor, baremetal branch

You're taking over a bare-metal Type-1 hypervisor that runs the Apple
Newton OS 2.x ROM natively on a Cortex-A53 (QEMU `raspi3b` today; Pi 3B
and Pi Zero 2 W eventually). The Newton is the *guest*, running as
AArch32 at EL1 under your EL2 Rust code. The whole project lives under
`/home/user/Einstein/baremetal/`.

## Read these first (15 minutes)

Start at the repo root:

1. `HIGHLEVEL.md` — architecture, phasing, answered vs open design
   questions. §16 is the open-questions block; most are answered.
2. `IMPLEMENTATION.md` — pure-Rust plan (post-pivot from an
   aborted C++ link-in). §3 tells you where each peripheral lives.
3. `baremetal/docs/peripherals.md` — the **spec** for each Newton
   peripheral, with pointers into Einstein's C++ as ground truth.
   This is your reference when porting.
4. `baremetal/probe/FINDINGS.md` — the §16 answers (descriptor
   formats, CP15 op surface, SWP sites, domain usage, etc.) from a
   probe run against the real 717006 ROM.
5. `baremetal/README.md` — build / run / debug loop.
6. `baremetal/guest-tests/README.md` — the ARM-guest test tier
   (HVC protocol, how to add tests).

## Environment assumptions

This sandbox already has:

- `rustc` 1.94.1 via rustup, target `aarch64-unknown-none-softfloat`
- `arm-none-eabi-gcc` for AArch32 test binaries
- `qemu-system-aarch64` 8.2 with `raspi3b` + `virt` machine support
- `gdb-multiarch`
- The 717006 Newton ROM at `baremetal/roms/newton.rom` (8 MiB,
  gitignored; **do not commit**)
- Einstein itself buildable with the existing `build/` subtree

If any of those are missing (fresh sandbox, etc.), `apt install
qemu-system-arm gcc-arm-none-eabi gdb-multiarch` and `rustup target add
aarch64-unknown-none-softfloat` will get you back.

## What works today

- Hypervisor boots at EL2 under QEMU raspi3b; stage-1 identity map and
  stage-2 for guest RAM / ROM / flash / framebuffer all functional.
- Newton ROM loads, byteswaps, and runs natively. Guest progresses
  through CP15 init, SCTLR toggles, TTBR/DACR install, PCMCIA probe,
  SVC↔FIQ mode switches. Then stalls in a pre-scheduler wait loop
  because the kernel never arms a timer-match — likely needs real
  peripheral emulation (see "ahead of you").
- ARM-guest test framework at `baremetal/guest-tests/`:
  `test_hello` passes; `test_flash` and `test_dma` pass against the
  ad-hoc `mmio.rs`; `test_vic` hangs — this is your first task.
- Rust VIC (`baremetal/src/vic.rs`) has real timer-match edge detection
  (`match_fired` bitmap, cleared on guest write).

## Immediate task: debug `test_vic`

Symptom: after the guest unmasks IRQs and the timer-2 match fires, the
progress beacons show ELR stuck in the test's tick-read loop
(`ldr r9, [r4]; cmp r9, r7; blo .-4`) but SPSR.mode reporting IRQ.
That's contradictory unless the guest is taking an IRQ, our handler
runs, `movs pc, lr` returns to IRQ mode somehow, and traps there.

Likely suspects:

- `common/test_runtime.S`'s `_irq_entry` — does `sub lr, lr, #4` and
  `movs pc, lr` restore CPSR properly? Check SPSR_irq at entry.
- `update_virq` in `baremetal/src/trap.rs` — is HCR_EL2.VI being
  cleared when int_present goes to 0? (It checks `vic::irq_pending()`,
  which gates on `int_present & int_ctrl & ~fiq_mask`.)
- Whether handler's MMIO reads (IntPresent, IntClear) re-enter
  cleanly — they trap back to EL2 while the guest is in IRQ mode.

Reproduce:
```bash
cd /home/user/Einstein
baremetal/guest-tests/scripts/build-tests.sh
baremetal/guest-tests/scripts/run-test.sh test_vic
```

If timeout, inspect `/tmp/guest-test_vic.out` and
`/tmp/claude-*/run-test.sh.*` for beacon lines.

Don't regress `test_hello`, `test_flash`, `test_dma` while fixing this.

## Ahead of you (after test_vic passes)

Port each Newton peripheral from Einstein's C++ to Rust, one module per
peripheral under `baremetal/src/peripherals/`:

1. `flash.rs` — port `TFlash`. Backing + Newton "DLDS"/"OSCD" header
   seed + masked writes + erase. See `docs/peripherals.md` §Flash.
2. `vic.rs` — move current `src/vic.rs` into `peripherals/`, keep
   edge detection. See §Interrupt controller.
3. `dma.rs` — trivial: assignment register + log-only stubs.
   See §DMA manager.
4. `pcmcia.rs` — "no card" probes for slots 0/1.
5. `native_primitives.rs` — coproc 10/11 gateway. Big; needed for
   the screen.
6. `screen.rs` — Blit intercept + framebuffer dump.

Each module gets `#[cfg(test)]` Rust unit tests and an ARM-guest test
under `guest-tests/tests/`. Pattern: see `test_vic` / `test_flash`
for guest tests.

Then Phase 3: wire peripherals into `baremetal/src/mmio.rs`, delete the
ad-hoc stubs and the bring-up shims in `guest_mem.rs` (vector patches,
CP15 encoding rewrite). Phase 4: polish.

## Hard rules

- **Pure Rust.** The previous attempt linked Einstein's C++ via a
  cxx-core directory; it was deleted in `3df141c` after we decided the
  complexity wasn't worth the 30-60 lines of real logic per peripheral.
  Do not resurrect it. Einstein stays a reading reference.
- **Never commit the ROM.** `baremetal/.gitignore` handles `*.rom /
  *.rex / roms/*`. Verify `git status` before any `git add -A`.
- **Upstream Einstein is read-only.** We don't modify `Emulator/`,
  `Monitor/`, `app/`, etc. The only exceptions already in the tree are
  some probe-specific instrumentation under `baremetal/probe/` that
  links against Einstein sources at build time via its own CMake.
- **Branch is `baremetal`.** Push there; don't create new branches.
- **Commit in reviewable chunks.** Walter reviews frequently; don't
  batch 20 peripherals into one megacommit.
- **Don't invent ROM-behaviour.** When unsure what a peripheral should
  return or what a register does, grep Einstein and cite the file:line
  in the commit message. `docs/peripherals.md` is the authoritative
  spec — update it first, then port.

## Useful quick commands

```bash
# Build + run hypervisor on Newton ROM
cd baremetal && cargo run --release

# Attach gdb
cd baremetal && DEBUG=1 cargo run --release &
gdb-multiarch target/aarch64-unknown-none-softfloat/release/newton-hypervisor \
    -ex 'target remote :1234' -ex 'break kmain' -ex continue

# Build + run one ARM-guest test
baremetal/guest-tests/scripts/run-test.sh test_hello

# Build + run all ARM-guest tests
baremetal/guest-tests/scripts/run-all.sh

# Rebuild only the guest test binaries (not the hypervisor)
baremetal/guest-tests/scripts/build-tests.sh

# Probe real Einstein against the 717006 ROM for oracle behaviour
cmake --build build --target NewtonProbe
build/NewtonProbe baremetal/roms/newton.rom - 90
```

## Known gotchas

- **QEMU raspi3b's serial routing**: first `-serial` is PL011 (what
  `baremetal/scripts/run-qemu.sh` relies on). Swapping to `-serial
  null -serial stdio` routes to the mini-UART instead, which our
  driver doesn't init.
- **Descriptor-bit ambiguity**: ARMv4 SBZ bits become ARMv7 XN / AP[2]
  / TEX in the same slots. `guest_mem.rs::fix_stage1_xn_bits()` walks
  the guest's L1 on first TTBR-write and normalises each section /
  coarse / small / large-page descriptor into minimal-valid ARMv7
  form. Don't let the Newton's raw tables walk on A53 directly.
- **CP15 encoding rewrite**: 717006 emits `MCR p15, 0, Rn, cN, cN, 0`
  (StrongARM lax encoding); ARMv7 wants `CRm=0`. We rewrite in-place
  at ROM load (`patch_cp15_encodings`). Don't remove this shim until
  you have a runtime CP15 translator trapping the StrongARM variants.
- **Vector patches**: `guest_mem.rs::load_newton_rom()` overwrites ROM
  words 1..=6 with `movs pc, lr` so early exceptions don't chain into
  the unmapped ROM jump-table VAs. This is a known cheat that will
  come off once the full MMIO/peripheral stack is in place. Mention it
  in any commit that touches the CP15 or interrupt-delivery paths.
- **Pre-scheduler stall**: even with everything above working, the
  Newton kernel never reaches the scheduler-init stage where it arms
  a timer match. Honest reason is unclear without deeper probe work;
  likely it's waiting for a peripheral response we don't emulate
  (PCMCIA card detection or a serial chip register), not anything
  architectural. Don't spend effort on scheduler issues until the
  peripheral ports land.

## Notes on the todo list

The `TodoWrite` list at session end was:

1. Pivot (completed in `3df141c`).
2. Debug test_vic ← **start here**.
3–8. Port peripherals in the order listed above.
9. Wire peripherals into mmio.rs.
10. Remove bring-up shims.

Commit messages on this branch tag milestones (M1, M2, M3, M4, M5) that
match `HIGHLEVEL.md` §11 phasing, so `git log --oneline` is a
reasonable progress narrative.

## Context, honestly

I spent a lot of tokens pattern-matching against the Newton boot log
and doing speculative stubs before landing on the probe + oracle
approach. The probe is a much better use of time than poking MMIO at
random — when you find yourself guessing what a register should
return, build a probe run and check Einstein's behaviour first. That
instinct is the single most valuable thing this session produced.

Good luck.
