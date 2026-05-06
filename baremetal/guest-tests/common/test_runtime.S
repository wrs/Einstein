@ baremetal/guest-tests/common/test_runtime.S
@
@ Shared guest-side runtime for AArch32 peripheral tests.
@
@ Provides:
@   - the exception vector table at offset 0;
@   - stack setup for both SVC (0x1000) and IRQ (0x2000) modes;
@   - a shared IRQ handler that services Newton VIC interrupts by
@       1) reading int_present,
@       2) incrementing the word at RAM 0x04000100 (`irq_count`) and
@          saving the int_present snapshot into 0x04000104 (`irq_pending`),
@       3) clearing the observed bits via IntClear,
@       4) returning to the interrupted code;
@   - HVC helper macros;
@   - convenience macros `cpu_enable_irq` / `cpu_disable_irq`.
@
@ Each test provides `test_main`, called in SVC with interrupts masked.

    .syntax unified
    .arch armv7ve

    .equ IRQ_COUNT_ADDR,     0x04000100
    .equ IRQ_PENDING_ADDR,   0x04000104
    .equ INT_PRESENT,        0x0F183000
    .equ INT_CLEAR,          0x0F183800

#include "hvc_abi.S"

    @ === Vector table ===========================================
    .section .text.vectors, "ax"
    .global _start
_start:
    b       _reset              @ 0x00: reset
    b       .Lhalt_undef        @ 0x04: undefined instruction
    b       .Lhalt_swi          @ 0x08: SWI
    b       .Lhalt_pabt         @ 0x0C: prefetch abort
    b       .Lhalt_dabt         @ 0x10: data abort
    b       .                   @ 0x14: reserved
    b       _irq_entry          @ 0x18: IRQ
    b       _fiq_entry          @ 0x1C: FIQ

    .section .text, "ax"
_reset:
    @ Stacks must live in RAM (stage-2 RW), not in the test image
    @ region (stage-2 RO). Newton RAM base is 0x0400_0000 and we use
    @ 0x04001000 for SVC, 0x04002000 for IRQ, 0x04003000 for FIQ.
    @ Test scratch below 0x0400_1000 (irq_count at 0x0400_0100, etc.)
    @ is untouched by stack pushes because the SP grows downward
    @ from 0x04001000 and never reaches 0x04000100.
    msr     cpsr_c, #0xD2       @ IRQ mode, I=1 F=1
    ldr     sp, =0x04002000
    msr     cpsr_c, #0xD1       @ FIQ mode, I=1 F=1
    ldr     sp, =0x04003000
    msr     cpsr_c, #0xD3       @ SVC mode, I=1 F=1
    ldr     sp, =0x04001000

    @ Zero irq_count / irq_pending so tests start from a known state.
    ldr     r0, =IRQ_COUNT_ADDR
    mov     r1, #0
    str     r1, [r0]
    ldr     r0, =IRQ_PENDING_ADDR
    str     r1, [r0]

    bl      test_main
    @ Fall through: halt if test_main returns without signalling.
.Lhang:
    b       .Lhang

_irq_entry:
    @ IRQ entry: LR_irq = PC+4 of the interrupted instruction, CPSR is
    @ saved into SPSR_irq. We'll adjust LR by 4 on return.
    sub     lr, lr, #4
    push    {r0-r5, lr}
    @ Read int_present to see what fired.
    ldr     r0, =INT_PRESENT
    ldr     r1, [r0]
    @ Save it into a RAM slot tests can inspect.
    ldr     r2, =IRQ_PENDING_ADDR
    str     r1, [r2]
    @ Bump the per-test counter.
    ldr     r2, =IRQ_COUNT_ADDR
    ldr     r3, [r2]
    add     r3, r3, #1
    str     r3, [r2]
    @ Clear the same bits via IntClear.
    ldr     r0, =INT_CLEAR
    str     r1, [r0]
    pop     {r0-r5, lr}
    @ Return: restore CPSR from SPSR, PC from LR.
    movs    pc, lr

_fiq_entry:
    @ Same shape but for FIQ. Newton FIQs are used for time-critical
    @ stuff; for tests the behaviour is identical.
    sub     lr, lr, #4
    push    {r0-r7, lr}
    ldr     r0, =INT_PRESENT
    ldr     r1, [r0]
    ldr     r2, =IRQ_PENDING_ADDR
    str     r1, [r2]
    ldr     r2, =IRQ_COUNT_ADDR
    ldr     r3, [r2]
    add     r3, r3, #1
    str     r3, [r2]
    ldr     r0, =INT_CLEAR
    str     r1, [r0]
    pop     {r0-r7, lr}
    movs    pc, lr

.Lhalt_undef:
    mov     r0, #1
    b       .Lreport_unexpected_exception
.Lhalt_swi:
    mov     r0, #2
    b       .Lreport_unexpected_exception
.Lhalt_pabt:
    mov     r0, #3
    b       .Lreport_unexpected_exception
.Lhalt_dabt:
    mov     r0, #4
    b       .Lreport_unexpected_exception

.Lreport_unexpected_exception:
    hvc     #HVC_FAIL
    b       .Lhang

    @ === HVC / CPSR helper macros ======================
    .macro hvc_putc ch
        mov     r0, #\ch
        hvc     #HVC_PRINT_BYTE
    .endm

    .macro hvc_hex32 reg
        mov     r0, \reg
        hvc     #HVC_PRINT_HEX
    .endm

    .macro hvc_pass
        hvc     #HVC_PASS
    .endm

    .macro hvc_fail code
        mov     r0, #\code
        hvc     #HVC_FAIL
    .endm

    .macro hvc_mark reg
        mov     r0, \reg
        hvc     #HVC_MARK
    .endm

    @ CPSR bit manipulation
    .macro cpu_enable_irq
        cpsie   i
    .endm

    .macro cpu_disable_irq
        cpsid   i
    .endm

    .macro cpu_enable_fiq
        cpsie   f
    .endm
