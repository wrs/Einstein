@ tests/test_midr.S
@
@ Verifies MIDR virtualisation via VPIDR_EL2. A guest
@ `MRC p15, 0, Rt, c0, c0, 0` from EL1 must return the StrongARM
@ SA-1100 Main ID value Newton OS expects (0x4401A100), not the
@ Cortex-A53 MIDR the silicon exposes.
@
@ Einstein hard-codes this at `Emulator/TARMProcessor.cpp:99`.
@ Hypervisor implements it by writing VPIDR_EL2 in `guest::configure_el2_traps`.

#include "../common/test_runtime.S"

    .global test_main
test_main:
    @ MRC p15, 0, r0, c0, c0, 0 — encoding 0xEE100F10.
    ldr     r0, =0xDEADBEEF                 @ sentinel so a no-op read trips
    .inst   0xEE100F10

    ldr     r1, =0x4401A100
    cmp     r0, r1
    bne     .Lfail_midr

    hvc_pass
    bx      lr

.Lfail_midr:
    hvc     #2                              @ dump the observed MIDR
    hvc_fail 0x60
