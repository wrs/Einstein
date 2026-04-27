# Current-stop handoff

Live notes for the next iteration. Replace this file's body when the
current stop is fixed and a new one takes over — git history is the
archive of past investigations.

## Stop: NULL-pointer write at REx 0x95c444 (2026-04-27)

```
*** data abort ISV=0 at ELR=0x95c444 SPSR=0x20000110
    IPA=0 FAR=0 iss=0x4e
    SCTLR_EL1 (guest) M-bit = 1 (stage-1 ON)
```

`iss=0x4e` ⇒ `WnR=1`, `DFSC=0x0e` (stage-2 permission fault, level 2).
`FnV` clear → `FAR=0` is valid; the guest dereferenced VA=0. Stage-1
identity-maps VA 0 → IPA 0 via `L1[0]` coarse @ PA 0x400; stage-2 has
IPA 0..0x1000000 RO (the ROM aperture) — hence the permission fault on
the write.

`ELR=0x95c444` is in **Einstein.rex** (REx base 0x00800000,
REx-offset 0x15c444). Trace tail (`trace_once,quiet`):

```
trace 4147559 0x00050d18 VccOff(int)              (usr) ...
trace 4147560 0x00050d28 VccOff(int, unsigned long) (usr) ...
*** data abort ISV=0 at ELR=0x95c444 ...
```

`VccOff` is a PCMCIA `TCardSocket` method, so the failing write
originates somewhere in the REx-resident PCMCIA driver path — most
likely Einstein REx code that populates / probes a `TCardSocket`
field assumed non-NULL but left blank in our setup.

## Why we halt

`try_emulate_isv0_dabt` (`src/trap.rs:542`) only emulates word
`LDR/STR` immediate (B=0, P/U/W/L decoded; Rn≠15, Rt≠15). The faulting
instruction at `0x95c444` doesn't match — it's likely a byte/halfword
store, an LDM/STM, or a pre/post-indexed-with-writeback variant — so
the emulator returns `false` and we drop into the loud-halt path at
`src/trap.rs:462`.

## Things we don't know yet

- **The instruction at `0x95c444`.** `scripts/disasm-out/rom.dis` only
  covers base ROM (≤ `0x71fc4c`); REx is missing. Cheapest path:
  `objdump -b binary -m armv5t -D --adjust-vma=0x800000` over the
  embedded `_Data_/Einstein.rex`.
- **Which register was 0.** The current halt path doesn't dump the
  AArch32 register file for ISV=0 aborts that fall through to halt.
  Either widen the abort-time dump or set a `bp 0x95c444` breakpoint
  via `scripts/gdb-init` and inspect `ctx.x[]` from gdb.
- **Whether Einstein's TCardSocket model populates state we leave
  blank.** The Newton's PCMCIA driver code in REx is shared with
  Einstein, so whatever non-NULL state the code expects has to come
  from somewhere. Check `Emulator/Newt/Driver/*` and
  `Emulator/Network/*` for TCardSocket initialisation paths.

## Suggested order of attack

1. **Find the instruction.** Disassemble REx at offset `0x15c444` and
   the surrounding ~32 bytes. Identify what's being written and via
   which base register.
2. **Identify the NULL.** Either widen the ISV=0 halt-path dump to
   print `r0..r12 + sp_<mode> + lr_<mode>` (mirror the
   `is_obviously_unreachable_ipa` dump in `trap.rs:472`) or use `bp`.
   Trace the NULL back to the field it came from.
3. **Cross-check Einstein.** Run the same offset under
   `build/NewtonProbe` (or step Einstein) and confirm whether the
   same call writes to a non-NULL pointer. If yes — Einstein populates
   something we don't; replicate. If no — the failure is structural
   in this code path and we deliver the abort to the guest's DABT
   vector instead of halting (the kernel's `UnhandledException` then
   runs and we see what the guest does about it).
4. **Apply the fix in the right layer.** Hypervisor handler /
   peripheral state / abort delivery — see PLAN.md "Workflow per
   stop" for the decision tree.

## Related context that's still live

- `src/peripherals/pcmcia.rs` halts loudly on every PCMCIA-class MMIO
  surface it doesn't recognise, by design. That's a deliberate
  trip-wire from Phase A — extend rather than silence.
- The `unknown bank #5` mmio arm (`src/mmio.rs`,
  `0x20000000..0x30000000` silent-zero) covers reads / writes
  *inside* PCMCIA bank 5; the current fault is a write to IPA=0 (ROM
  aperture), not bank 5, so that arm doesn't apply.
