# QEMU raspi3b bugs at the AArch64 / AArch32 boundary

This file catalogs QEMU `raspi3b` (TCG, `qemu-system-aarch64 -M raspi3b`)
bugs we've hit at the AArch64 EL2 ↔ AArch32 lower-EL boundary, with
minimal repro tests and workaround pointers. The boundary is a known
flaky cluster upstream — Maydell's 2015 SPSR_EL1/banked_spsr[1]
indexing fixes, the 2016 cpsr_write mode-bits patch, and the 2022/2024
SPSR_hyp access patches all touched related code paths. Expect more
findings here over time.

Before suspecting our own code at this boundary, check this list. When
adding a new entry, include a minimal repro in `guest-tests/tests/` and
update `MANIFEST` so the bug stays observable across QEMU upgrades.

---

## Bug #1 — `msr spsr_el2, x` from AArch64 EL2 clobbers SPSR_svc with `x`

### Symptom

When EL2 Rust does `msr spsr_el2, <val>` (e.g. via `return_to_guest`
in `src/trap.rs`) and then ERETs to AArch32, QEMU additionally writes
`<val>` into the storage backing the AArch32 banked SPSR_svc (almost
certainly `env->banked_spsr[1]`). The CPSR-on-ERET is correct; the
side effect is that the guest's banked SPSR_svc is silently
overwritten with whatever value the hypervisor wrote to SPSR_EL2.

### Why

Architecturally `SPSR_EL1` aliases `SPSR_svc` — both live in
`env->banked_spsr[1]` per `aarch64_banked_spsr_index()` in QEMU's
`target/arm/cpu.h`. Maydell's 2015 patch series fixed several
"wrong banked_spsr[] index" bugs in the SPSR_EL1 path; this looks like
the same family but on the SPSR_EL2 side, leaking writes into
`banked_spsr[1]`.

ERET itself is innocent — the damage is done by the MSR. We confirmed
this by isolating the variable: HVC and DABT round-trips, which use
the CPU's auto-saved SPSR_EL2 unchanged, do not exhibit the clobber.

### Repro

`guest-tests/tests/test_spsr_eret_und.S`:

1. SVC mode: write sentinel into SPSR_svc (`msr spsr, sentinel`).
2. Execute `SystemBootUND` (`0xE6000010`). UND trampoline → HVC → EL2.
3. `handle_und` writes `msr spsr_el2, spsr_und` and ERETs.
4. Back in SVC: read SPSR_svc, compare with sentinel.
5. Test fails (`hvc #4` with code `0xB4`) when SPSR_svc has been
   overwritten with the SPSR_EL2 value.

Companion `test_spsr_eret.S` exercises the HVC and DABT paths as
controls — both pass.

Observed values from one run:

```
guest-hex: 0xfe00000d   ← SPSR_svc pre-UND (sentinel, RES0-masked)
und:   handle_und ... SPSR_EL2 written = 0x1d3
guest-hex: 0x000001d3   ← SPSR_svc post-UND — overwritten with SPSR_EL2 value
```

### Affected paths in our hypervisor

- `handle_und` → `return_to_guest(ctx, elr, spsr_und)` — confirmed.
- Any future path that does `msr spsr_el2, x` from EL2 before ERET
  to a lower EL.

Not affected: HVC and DABT round-trips. They rely on the CPU's
auto-saved SPSR_EL2 and never explicitly rewrite it.

### Workaround options (not yet implemented as of 2026-04-23)

1. **Stop using `msr spsr_el2, x` in the UND return path.** Let the
   CPU's auto-saved SPSR_EL2 ERET back to UND mode, then have an
   in-guest UND-mode stub do `movs pc, lr` to architecturally
   transition to SVC via SPSR_und. The mode switch happens AArch32-
   side and never goes through QEMU's buggy MSR helper. Trampoline
   region at `0x00FFFF00` already exists.

2. **Re-write SPSR_svc via banked AArch64 MSR after the SPSR_EL2
   write.** Untested whether `msr spsr_svc, x` from AArch64 EL2
   actually takes on QEMU raspi3b — banked *reads* are documented as
   returning 0; banked *writes* may or may not work.

Option 1 is structurally cleaner because it avoids the buggy code
path entirely. Option 2 is a smaller local change if option 1 turns
out to require invasive rework.

---

## Pre-existing characterized issues

These are documented in `CLAUDE.md` and `src/trap.rs` comments and
listed here for completeness; they pre-date this file.

- **gdbstub is aarch64-only.** Guest AArch32 software breakpoints
  don't work — the mode switch is dropped. Workaround: the `bp`
  helper in `scripts/gdb-init` patches `UDF #0xFFFE` into the ROM
  word and traps to EL2.

- **`MRS <x>, ELR_EL1` / `MRS <x>, SP_EL1` from AArch64 EL2 do NOT
  return AArch32 R14_svc / R13_svc.** ELR_EL1 and SP_EL1 are
  AArch64-only special-purpose registers (only present when
  FEAT_AA64 is implemented); the ARM ARM defines no architectural
  mapping between them and any AArch32 banked R13/R14. Read R13_svc
  / R14_svc from `ctx.x[19]` / `ctx.x[18]` per Table D1-79 instead.

  `MRS <x>, SPSR_<mode>` for `<mode>` ∈ {abt, und, irq, fiq} is a
  defined AArch64 named sysreg and works on FVP and QEMU.
  `SPSR_svc` is `SPSR_EL1` (architecturally mapped — DDI 0487 D13.2).

  There is **no** AArch64 named-sysreg path to `LR_<mode>` for
  any mode — `MRS (Banked register)` is A32/T32-only per F7.1.115.
  Use the X-register slots (Table D1-79). Reading `LR_<mode>` on
  the assumption that it's AArch64-accessible was a misdiagnosis;
  the values come from `ctx.x[16..30]` directly.

- **NOT A BUG — `ctx.x[13]` / `ctx.x[14]` at EL2 AArch64 entry from
  AArch32 are `SP_usr` / `LR_usr`, NOT the active mode's banked
  R13/R14.** This is architected behavior per ARM ARM DDI 0487
  D1.21.1 Table D1-79 "Base instruction set register mapping between
  AArch32 state and AArch64 state": the AArch64 GPR file always maps
  AArch32 banked regs **by bank name**, not by the mode active at
  exception entry. Specifically:
  ```
    R0..R7            x0..x7      (shared / always)
    R8..R12           x8..x12     (R8_usr..R12_usr — for non-FIQ modes
                                  these are the same physical regs)
    SP_usr            x13         ← NOT SP_<current-mode>
    LR_usr            x14         ← NOT LR_<current-mode>
    SP_hyp            x15
    LR_irq            x16
    SP_irq            x17
    LR_svc            x18
    SP_svc            x19
    LR_abt            x20         ← read this for LR_abt, not x14
    SP_abt            x21
    LR_und            x22
    SP_und            x23
    R8_fiq..R12_fiq   x24..x28    (FIQ-mode R8-R12 live here)
    SP_fiq            x29
    LR_fiq            x30
  ```
  So when HVC fires from AArch32 ABT mode, `ctx.x[14]` is whatever
  `LR_usr` happens to contain at that moment (a ROM-parked user-mode
  return address, possibly Thumb-bit-set, possibly zero) — not
  `LR_abt`. **This was misdiagnosed as "QEMU banked-reg plumbing is
  flaky" for months.** FVP and QEMU both faithfully implement the
  architected mapping; the wrong expectation was ours.

  **Fix pattern**: in the EL2 HVC handler, read the banked reg from
  the architected slot: `ctx.x[20]` for `LR_abt`, `ctx.x[21]` for
  `SP_abt`, `ctx.x[18..19]` for `LR_svc`/`SP_svc`, `ctx.x[22..23]`
  for UND, `ctx.x[16..17]` for IRQ, `ctx.x[24..30]` for FIQ-banked
  R8-R12 + SP + LR. For SPSR_<mode>, the AArch64 named sysregs
  `spsr_abt` / `spsr_und` / `spsr_irq` / `spsr_fiq` work (LLVM's
  assembler accepts them); SPSR_svc is `spsr_el1` (architecturally
  mapped per DDI 0487 D13.2). There is **no** AArch64-named sysreg
  for SP/LR in any AArch32 banked mode — Table D1-79 is the only
  access path from AArch64 EL2 (MRS Banked Register is AArch32-only
  per F7.1.115).

- **Upper 32 bits of x16..x30 on AArch32→AArch64 exception entry are
  CONSTRAINED UNPREDICTABLE** per ARM ARM D1.21.2 Table D1-85. Always
  w-view (`ctx.x[N] as u32`) when reading banked values. Don't rely
  on the upper halves being zero-extended.

## Tooling pitfall — `timeout` doesn't kill QEMU under semihosting

Plain `timeout N cargo run --release` does **not** stop QEMU at the
deadline. `timeout`'s default signal is SIGTERM, and
`qemu-system-aarch64` running with our semihosting + `-no-reboot`
configuration ignores it (the busy semihosting loop never reaches
the signal-poll site). The `cargo run` wrapper exits, the QEMU
child is reparented, and the run keeps emitting traps for as long
as you let it — which can produce wildly misleading trap-rate
measurements if you assume the wall-clock window equals your
intended timeout.

Use `timeout -k 2 N` (or `timeout -s KILL N`) for any QEMU run
that needs to die on a deadline:

```bash
# Cold-boot capture, definitely-stops-after-30 s:
rm -f /tmp/newton-snapshot-{0..3}.bin
timeout -k 2 30 cargo run --release > /tmp/cold.log 2>&1
```

For boot verification, prefer `scripts/boot-check.sh`: it launches
the run with the log redirected, polls for success markers (default:
the Welcome-UI milestone), and SIGKILLs QEMU the moment they all
appear — the check costs one boot instead of a fixed worst-case
window, and every exit path (match, timeout, Ctrl-C) kills QEMU.

Discovered iter-60 (2026-04-30): a `timeout 30` measurement of
"249 M DIAG_TAGs over 30 s" was actually a 5+ minute run because
QEMU never received SIGKILL. The same fix applied with `-k 2`
gave the correct ≈0 DIAG_TAGs over 30 s.

The Ctrl-C symptom in interactive runs has the same root cause —
`scripts/run-qemu.sh` was updated in iter-58 from `-serial stdio`
to `-serial mon:stdio` so Ctrl-A x quits cleanly; Ctrl-C still
won't reliably kill semihosting QEMU.
