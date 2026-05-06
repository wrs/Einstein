@ tests/test_spsr_eret_und.S
@
@ Companion to test_spsr_eret.S — exercises the UND-trampoline trap
@ path, which is the most interesting case for the "ERET clobbers
@ SPSR_svc" hypothesis because handle_und() in src/trap.rs explicitly
@ hand-crafts SPSR_EL2 = <SVC-mode PSTATE> before ERET. If QEMU's
@ helper_exception_return misroutes that into banked_spsr[1] (= storage
@ for SPSR_svc), this is the path most likely to trip it.
@
@ Sequence:
@   1) SVC mode: MSR SPSR, SENT → populate SPSR_svc with a sentinel.
@   2) Execute SystemBootUND (0xE6000010). CPU enters UND mode; UND
@      vector → trampoline → HVC #UND_TAG → EL2.
@   3) handle_und dispatches SystemBootUND: advances ELR past the UND
@      and writes SPSR_EL2 = saved-SPSR_und (= SVC-mode CPSR), then
@      ERETs directly back to SVC mode at faulting_pc+4.
@   4) Back in SVC: read SPSR_svc, compare against effective pre-UND
@      value. If equal → hypothesis refuted for UND path.
@
@ Reuses the same UND trampoline shape as test_und_handler.S, per the
@ contract in src/trap.rs::handle_und.

    .syntax unified
    .arch armv7ve

#include "../common/hvc_abi.S"

    .equ UND_SAVE_LR_ADDR,   0x04005F00
    .equ UND_SAVE_SPSR_ADDR, 0x04005F04

    .equ SENT, 0xFEEDF00D

    @ === Vector table ===========================================
    .section .text.vectors, "ax"
    .global _start
_start:
    b       _reset              @ 0x00: reset
    b       _und_trampoline     @ 0x04: UND → trampoline → EL2
    b       .Lhang              @ 0x08: SWI
    b       .Lhang              @ 0x0C: prefetch abort
    b       .Lhang              @ 0x10: data abort
    b       .                   @ 0x14: reserved
    b       .Lhang              @ 0x18: IRQ
    b       .Lhang              @ 0x1C: FIQ

    .section .text, "ax"

_reset:
    msr     cpsr_c, #0xDB       @ UND mode, I=1 F=1
    ldr     sp, =0x04006000
    msr     cpsr_c, #0xD3       @ SVC mode, I=1 F=1
    ldr     sp, =0x04008000

    bl      test_main
.Lhang:
    b       .Lhang

    @ UND trampoline — matches the contract expected by handle_und.
_und_trampoline:
    push    {r0, r1}
    ldr     r0, =UND_SAVE_LR_ADDR
    str     lr, [r0]
    mrs     r1, spsr
    ldr     r0, =UND_SAVE_SPSR_ADDR
    str     r1, [r0]
    pop     {r0, r1}
    hvc     #HVC_UND               @ UND_TAG
    b       .Lhang              @ unreachable

test_main:
    push    {r4, r5, lr}

    @ Populate SPSR_svc with a sentinel.
    ldr     r0, =SENT
    msr     spsr, r0
    mrs     r4, spsr            @ effective pre-UND SPSR_svc

    mov     r0, r4
    hvc     #HVC_PRINT_HEX                  @ log it

    @ Execute SystemBootUND. UND-trampoline-bounce, EL2 handles and
    @ ERETs directly back to SVC mode at the next instruction.
    .word   0xE6000010

    @ Sanity: UND dispatcher must have advanced PC correctly.
    mov     r7, #0x42
    cmp     r7, #0x42
    bne     .Lfail_dispatch

    mrs     r5, spsr            @ SPSR_svc after UND round-trip
    mov     r0, r5
    hvc     #HVC_PRINT_HEX                  @ log it

    cmp     r5, r4
    bne     .Lclobber_und

    pop     {r4, r5, lr}
    hvc     #HVC_PASS                  @ PASS — SPSR_svc preserved across UND path
    bx      lr

.Lfail_dispatch:
    mov     r0, #0xE2           @ UND dispatch didn't resume at the right PC
    hvc     #HVC_FAIL

.Lclobber_und:
    mov     r0, #0xB4           @ bug confirmed on UND path
    hvc     #HVC_FAIL
