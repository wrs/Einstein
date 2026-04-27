# Phase B boot-stall investigation

Live notes. Update as we learn more. REMOVE old updates once resolved.

## Resolved — IPA 0x200001a0 trip-wire = "unknown bank #5" silent-zero (QEMU, 2026-04-27)

**Wedge** (qemu-notrace.log, `phaseB-2026-04-27-mmio200001a0/`): after
the γ-fix landed, boot advances ~10× further (~2.1 M traps) and hits a
new trip-wire — unknown MMIO read at IPA `0x200001a0` from PC
`0x002584A4` (the `ldr lr, [r2, lr, lsl #1]` inside
`ConvertToUnicodeFunc_Contiguous8__FPCvPUsPvl` at ROM `0x00258480`).

Decoded fault: a `MakeString__FPCc(c_string)` call at `0x0031c230`
invokes the post-ship-patch dispatcher `ConvertToUnicode__FPCvPUslT3`
(VA `0x01be7384` → real fn `0x002572ec`). The dispatcher loads the
encoding-1 descriptor from the kernel's char-encoding table at
`0x0c107790` and tail-calls Contiguous8 with `r2 = descriptor`. The
convert function then reads `*(r2 + 16)` — supposed to be the per-
encoding 256×u16 lookup table base — but on this run that field is
`0x20000110`, which stage-1 1:1-maps (kernel `L1[0x200] = 0x20000c0e`
section) to IPA `0x20000110`. Stage-2 has nothing in
`0x20000000..0x30000000`, so the load takes a stage-2 fault and our
Phase A "loud halt on unknown MMIO" trip-wire fires.

**Root cause vs. Einstein**: Einstein's `TMemory::ReadP`
(`Emulator/TMemory.cpp:1026-1034`) silently returns 0 for any
read in `kFlashBank2End (0x10400000) .. kPCMCIA0Base (0x30000000)`
("unknown bank #5"). The 717006 kernel evidently has at least one
TInterpreter-side code path (this MakeString conversion via a
partially-initialised TEncodingMap) where the lookup table base is
bogus; Einstein masks it by returning U+0000 for every byte and the
boot keeps going with a garbled string. The deeper "why is the
TEncodingMap.+16 wrong?" question is decoupled from this wedge —
it's a NewtonScript-level bug Einstein masks the same way. The
descriptor at `0x0c645928` (encoding-1) we dump *post-fault* has the
correct `+16 = 0x0062b848` (REx region), so the convert was passed
either a different descriptor or one that hadn't been initialised
yet by `InstallBuiltInEncodings` at the moment of call.

**Fix** (in tree, this jj change): in `src/mmio.rs`, add an
`UNKNOWN_BANK5_BASE..END = 0x20000000..0x30000000` arm to both
`read` and `write` that returns 0 / drops the write. This mirrors
Einstein's TMemory default. Also tightened `is_obviously_unreachable_ipa`
in `src/trap.rs` to flag `0x10000000..0x30000000` so the dabt-trip
register-context dump fires for any future read or write into that
gap (previously only writes-into-ROM triggered it).

**Result** (qemu7.log): boot advances another 10× further (~24 M
traps in 240 s). Task list now includes `newt` (TInterpreter,
id=0x3063, prio=10, in [RDY] state) along with `pssm`, `main`, and
the full driver suite — `cdsv`, `cdpr`, `pg&e`, `Tmux`, `OBJM`, `PMGR`,
`PTBL`, `STKF/STKP/STKU`, `drvr`, `ROMF`, `ROMP`, `alrt`, `sndm`,
`name`, `pckm`, `cmgr`. 27 tasks total, matching Einstein's t=2 s
snapshot from `build/NewtonProbe`. The system is now in the kernel's
normal idle loop — `idle` task is [RUN], `PauseSystemKernelGlue` →
`PauseSystem (TPlatformDriver)` cycling at PC=`0x3ad6f4` /
`0x3adb0c` / `0x800a0c` (REx) — *not* a wedge.

**Phase B reaches `TInterpreter::TInterpreter`.** 35/35 guest tests
still green.

---

## Resolved — γ root cause + fix: ARMv7 leaves DFSR.Domain UNK on DFSC=5; kernel reads 0 → no monitor (QEMU, 2026-04-27)

**Root cause** (qemu13.log): ARMv7 ARM B4.1.51 specifies DFSR.Domain
is UNK for DFSC=0b00101 (translation, section). Real Newton's
StrongARM-era kernel was written assuming the domain field is always
valid — its `mrc p15,0,r1,c5,c5,{0}` at DAH PC `0x393480` reads what
StrongARM exposes as a status register that always carries the L1
entry's domain regardless of fault status. Our hypervisor rewrites
all `c5,c5,0` encodings to `c5,c0,0` (= DFSR_EL1) at ROM-load time
(`guest_mem::patch_cp15_encodings`), so on DFSC=5 the kernel reads
DFSR_EL1 with `bits[7:4]` left at whatever ARMv7 hardware put there
— zero on QEMU.

Direct evidence in qemu13.log fault #2:

```
FME-entry[6]: r0(mask)=0x00000000 task[+0x70]=0x55555 task[+0x64]=0x00 task[+0x58]=0x05
                                  (OR-mask)         (scratch[4])     (DFSR.low)
```

vs. every prior recovered abort (DFSC=7):

```
FME-entry[0..5]: r0(mask)=0x121a task[+0x70]=0x55555 task[+0x64]=0x13a5 task[+0x58]=0x47
```

Kernel computes `domain = (DFSR.low_byte >> 4) & 0xF`. For DFSC=7:
domain=4, `GetDomainAndFaultMonitorFromDomainNumber(4)` returns the
domain-4 monitor (scratch[0]=0x121a, scratch[4]=0x13a5),
`FaultMonitorEntry(0x121a)` succeeds. For DFSC=5: domain=0,
`GetDomainAndFaultMonitor(0)` finds nothing, scratch[0]=0,
`FaultMonitorEntry(0)` returns -10015, kernel reboots.

**Fix** (in tree, jj change `9a8aff1a` then committed): in
`handle_diag` before forwarding a forwardable DABT to DAH, read the
faulting VA's L1 entry, extract `bits[8:5]` (domain), and overlay
into DFSR_EL1 (= ESR_EL1) `bits[7:4]`. Idempotent for valid-domain
DFSCs (DFSC=7 already has the correct domain there). For DFSC=5 it
provides what ARMv7 leaves UNK.

**Result** (qemu14.log): boot advances past the L1[0xCD] wedge.
Fault #2 at FAR=0x0CD07400 now sees DFSR=0x45 (domain=4),
GetDomainAndFaultMonitorFromDomainNumber(4) returns the monitor,
FaultMonitorEntry returns 0 (success), kernel grows L1[0xCD] from
0x90 lazy → 0x04023081 coarse via Remember(va, perm=0)→SWI #12, the
4-iter ResolveFault wrapper allocates 4 subpages, Fill resumes.

The boot then progresses several thousand traps further and hits a
new trip-wire: unknown MMIO read at IPA `0x200001a0` from PC
`0x002584A4`. That's Phase B's standard "loud halt on unknown MMIO" —
a new investigation (separate from the L1[0xCD] wedge).

35/35 guest tests still green.

---
