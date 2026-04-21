@ tests/test_cp15_strongarm_clock.S
@
@ Verifies the hypervisor's UND handler treats
@   MCR p15, 0, Rt, c15, c1, 2
@ (StrongARM-specific clock-control write) as a silent no-op.
@
@ On ARMv8, this CP15 encoding doesn't exist, so the MCR raises an
@ UND exception locally at EL1 — HCR_EL2.TIDCP never sees it. The
@ hypervisor therefore recognises the pattern inside handle_und
@ rather than handle_cp15_trap.
@
@ Test reuses the UND trampoline convention from test_und_handler
@ (save LR_und / SPSR_und to fixed RAM, then HVC #0x10 into EL2).

    .syntax unified
    .arch armv7ve

    @ Match src/trap.rs UND_SAVE_LR_IPA / _SPSR_IPA — see test_und_handler.S
    @ for why we moved off of 0x04000400 (it overlaps the kernel's L1 table).
    .equ UND_SAVE_LR_ADDR,   0x04005F00
    .equ UND_SAVE_SPSR_ADDR, 0x04005F04

    .section .text.vectors, "ax"
    .global _start
_start:
    b       _reset              @ 0x00: reset
    b       _und_trampoline     @ 0x04: UND -> trampoline -> EL2
    b       .Lhang              @ 0x08: SWI
    b       .Lhang              @ 0x0C: prefetch abort
    b       .Lhang              @ 0x10: data abort
    b       .                   @ 0x14: reserved
    b       .Lhang              @ 0x18: IRQ
    b       .Lhang              @ 0x1C: FIQ

    .section .text, "ax"

_reset:
    msr     cpsr_c, #0xDB       @ UND, I=1 F=1
    ldr     sp, =0x04006000
    msr     cpsr_c, #0xD3       @ SVC, I=1 F=1
    ldr     sp, =0x04008000

    bl      test_main
.Lhang:
    b       .Lhang

_und_trampoline:
    push    {r0, r1}
    ldr     r0, =UND_SAVE_LR_ADDR
    str     lr, [r0]
    mrs     r1, spsr
    ldr     r0, =UND_SAVE_SPSR_ADDR
    str     r1, [r0]
    pop     {r0, r1}
    hvc     #0x10               @ UND_TAG
    b       .Lhang

test_main:
    @ First MCR: the trap log should emit one "StrongARM clock" line.
    ldr     r0, =0xDEADBEEF
    mcr     p15, 0, r0, c15, c1, 2
    mov     r0, #1
    hvc     #5                  @ mark 1: first MCR returned

    @ Second MCR: budget exhausted, silent.
    ldr     r0, =0x00000042
    mcr     p15, 0, r0, c15, c1, 2
    mov     r0, #2
    hvc     #5                  @ mark 2

    @ Third MCR: still silent, still returns.
    mov     r0, #0
    mcr     p15, 0, r0, c15, c1, 2
    mov     r0, #3
    hvc     #5                  @ mark 3

    hvc     #3                  @ PASS
    bx      lr
