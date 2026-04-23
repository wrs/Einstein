@ tests/test_spsr_eret.S
@
@ Verifies (or refutes) the hypothesis that an AArch64 EL2 ERET back
@ into AArch32 SVC clobbers the guest's banked SPSR_svc. Architecturally
@ ERET loads CPSR from SPSR_EL2 and leaves banked SPSRs untouched; the
@ hypothesis is that QEMU raspi3b aliases the two and trashes SPSR_svc.
@
@ Two rounds, each prints the pre- and post-HVC SPSR value via hvc #2:
@   Round 1: HVC from SVC mode (ERET target = SVC)  — the suspected path.
@   Round 2: HVC from SYS mode (ERET target = SYS)  — SYS has no SPSR, so
@            SPSR_svc must not be touched; acts as a control.
@
@ PASS  → both sentinels survive (hypothesis refuted).
@ FAIL 0xB1 → SVC-origin clobber (hypothesis confirmed).
@ FAIL 0xB2 → SYS-origin clobber too (bug is broader than hypothesised).
@ FAIL 0xE1 → in-mode MSR SPSR did not take (sanity failure).

#include "../common/test_runtime.S"

    .equ SENT1, 0xDEADBEEF
    .equ SENT2, 0xCAFEBABE
    .equ SENT3, 0xB00BCAFE

    .global test_main
test_main:
    push    {r4-r7, lr}

    @ === Round 1 — HVC from SVC ================================
    ldr     r0, =SENT1
    msr     spsr, r0            @ write SPSR_svc (we are in SVC)
    mrs     r4, spsr            @ read back (some CPSR bits are RES0 etc.)

    mov     r0, r4
    hvc     #2                  @ log: SPSR_svc pre-HVC (effective written value)

    mov     r0, #0xA1
    hvc     #5                  @ benign round-trip through EL2

    mrs     r5, spsr            @ read SPSR_svc after ERET-to-SVC
    mov     r0, r5
    hvc     #2                  @ log: SPSR_svc post-HVC

    @ Compare post-HVC against the effective pre-HVC value (r4), not
    @ against the raw sentinel — RES0 bits are irrelevant; the question
    @ is whether the value changed across the trap.
    cmp     r5, r4
    bne     .Lclobber_svc

    @ === Round 2 — HVC from SYS (control) ======================
    ldr     r0, =SENT2
    msr     spsr, r0            @ still in SVC; SPSR_svc = SENT2
    mrs     r7, spsr            @ snapshot effective SPSR_svc before trap

    mov     r0, r7
    hvc     #2                  @ log: effective SPSR_svc pre-SYS-round-trip

    mrs     r0, cpsr
    bic     r1, r0, #0x1F
    orr     r1, r1, #0x1F       @ mode = SYS (0x1F)
    msr     cpsr_c, r1

    mov     r0, #0xA2
    hvc     #5                  @ round-trip with ERET target = SYS

    mrs     r0, cpsr
    bic     r1, r0, #0x1F
    orr     r1, r1, #0x13       @ mode = SVC (0x13)
    msr     cpsr_c, r1

    mrs     r6, spsr            @ SPSR_svc after SYS round-trip
    mov     r0, r6
    hvc     #2                  @ log: SPSR_svc post-SYS-round-trip

    cmp     r6, r7
    bne     .Lclobber_sys

    @ === Round 3 — stage-2 data abort from SVC =================
    @ Exercises the EC_DATA_ABORT_LOWER trap path in src/trap.rs rather
    @ than EC_HVC_A32. QEMU's helper_exception_return is the same for
    @ both, but the entry path (helper_cpsr_write etc.) differs.
    ldr     r0, =SENT3
    msr     spsr, r0
    mrs     r4, spsr
    mov     r0, r4
    hvc     #2                  @ log: effective SPSR_svc pre-DABT

    @ Read VIC INT_PRESENT — unmapped IPA → stage-2 abort; hypervisor's
    @ vic stub reads the shadow and ERETs back to the next insn.
    ldr     r1, =INT_PRESENT
    ldr     r1, [r1]

    mrs     r5, spsr
    mov     r0, r5
    hvc     #2                  @ log: SPSR_svc post-DABT

    cmp     r5, r4
    bne     .Lclobber_dabt

    @ All three survived: hypothesis refuted across HVC, HVC-from-SYS,
    @ and DABT paths on this QEMU build.
    pop     {r4-r7, lr}
    hvc     #3                  @ PASS
    bx      lr

.Lclobber_svc:
    mov     r0, #0xB1           @ ERET-to-SVC via HVC trashed SPSR_svc
    hvc     #4

.Lclobber_sys:
    mov     r0, #0xB2           @ ERET-to-SYS via HVC trashed SPSR_svc
    hvc     #4

.Lclobber_dabt:
    mov     r0, #0xB3           @ ERET-to-SVC via DABT trashed SPSR_svc
    hvc     #4
