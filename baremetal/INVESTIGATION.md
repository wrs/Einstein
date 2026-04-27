# Phase B boot-stall investigation

Live notes. Update as we learn more. REMOVE old updates once resolved.

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
