@ tests/test_finetable_rewrite.S
@
@ Verifies that the hypervisor's L1 normalisation pass rewrites ARMv4
@ fine-table descriptors (type = 0b11) to fault entries (0b00) when the
@ guest installs its TTBR0.
@
@ Mechanism:
@   1. The guest's L1 is expected at RAM base (0x0400_0000) by
@      guest_mem::fix_stage1_xn_bits. We plant a single fine-table L1
@      entry at the slot covering VA 0x78000000 (slot 0x780, byte
@      offset 0x1E00) and leave every other slot as zero.
@   2. Writing TTBR0 via MCR p15,0,Rt,c2,c0,0 traps to EL2 via TVM.
@      The hypervisor applies the write to real TTBR0_EL1 and — on the
@      first TTBR write only — runs fix_stage1_xn_bits over the L1.
@   3. Read the slot back. If the pass ran, it should read 0.
@
@ The MMU is never turned on in this test; we're only asserting that
@ the fix-up pass observes and rewrites the fine-table descriptor.
@
@ No shared runtime — this test installs its own vectors so nothing
@ else writes the L1 region between our plant and our read.

    .syntax unified
    .arch armv7ve

#include "../common/hvc_abi.S"

    .equ L1_BASE,        0x04000000
    .equ FINE_SLOT_VA,   0x78000000
    .equ FINE_SLOT_IDX,  0x780              @ = FINE_SLOT_VA >> 20
    .equ FINE_SLOT_OFF,  0x1E00             @ = FINE_SLOT_IDX << 2
    .equ FINE_SLOT_ADDR, L1_BASE + FINE_SLOT_OFF

    @ Fine-table descriptor: L2 base in bits [31:12], type=0b11 in [1:0].
    @ The L2 base is arbitrary (the hypervisor rewrites to 0 regardless).
    .equ FINE_DESC,      0xAABBC003

    @ === Vector table ===============================================
    .section .text.vectors, "ax"
    .global _start
_start:
    b       _reset
    b       .Lhang              @ 0x04: undef
    b       .Lhang              @ 0x08: SWI
    b       .Lhang              @ 0x0C: prefetch abort
    b       .Lhang              @ 0x10: data abort
    b       .                   @ 0x14: reserved
    b       .Lhang              @ 0x18: IRQ
    b       .Lhang              @ 0x1C: FIQ

    .section .text, "ax"

_reset:
    @ SVC stack above the L1 region so stack pushes don't perturb it.
    @ SP grows down from 0x04008000; L1 occupies 0x04000000..0x04004000.
    msr     cpsr_c, #0xD3       @ SVC, I=1 F=1
    ldr     sp, =0x04008000
    bl      test_main
.Lhang:
    b       .Lhang

test_main:
    @ Plant the fine-table descriptor at the L1 slot for VA 0x78000000.
    ldr     r0, =FINE_SLOT_ADDR
    ldr     r1, =FINE_DESC
    str     r1, [r0]

    @ Sanity: read it back — should be FINE_DESC before the rewrite.
    ldr     r2, [r0]
    ldr     r1, =FINE_DESC
    cmp     r2, r1
    bne     .Lfail_plant

    mov     r0, #1
    hvc     #HVC_MARK                  @ mark 1: descriptor planted

    @ Write TTBR0. Value points at our L1 (16 KiB aligned at 0x04000000).
    @ The existing fix_stage1_xn_bits walks GUEST_RAM from offset 0 and
    @ doesn't actually read TTBR0, but we issue the real write so the
    @ first-TTBR-write hook fires.
    ldr     r0, =L1_BASE
    mcr     p15, 0, r0, c2, c0, 0

    @ Read the slot back. The hypervisor should have rewritten it to 0.
    ldr     r0, =FINE_SLOT_ADDR
    ldr     r2, [r0]
    cmp     r2, #0
    bne     .Lfail_not_rewritten

    mov     r0, #2
    hvc     #HVC_MARK                  @ mark 2: slot rewritten to fault
    hvc     #HVC_PASS                  @ PASS
    bx      lr

    @ === Failure paths ===============================================
.Lfail_plant:
    mov     r0, r2
    hvc     #HVC_PRINT_HEX                  @ print the observed value
    mov     r0, #0x70
    hvc     #HVC_FAIL

.Lfail_not_rewritten:
    mov     r0, r2              @ leftover value — should have been 0
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x71
    hvc     #HVC_FAIL
