@ tests/test_cp15_fault_regs.S
@
@ Triggers a real guest-level data abort and verifies the abort
@ handler can read DFAR / DFSR through our CP15 trap shim.
@
@ Mechanism: enable SCTLR.A (alignment check) and do an unaligned
@ LDR. ARMv7 treats that as an alignment fault taken at EL1, so it
@ enters ABT mode at EL1 and vectors to ROM offset 0x10. We install
@ our own ABT vector that:
@   1. Reads DFAR via `MRC p15,0,Rt,c6,c0,0` — traps via TRVM, our
@      CP15 shim passes FAR_EL1.
@   2. Reads DFSR via `MRC p15,0,Rt,c5,c0,0` — same path, currently
@      stubbed to zero.
@   3. Skips the faulting LDR and returns.
@
@ After the abort returns, test_main checks:
@   * abort_count == 1 (exactly one abort taken).
@   * dfar snapshot == the unaligned address we LDR'd.
@   * dfsr snapshot reflects "alignment fault" when the shim passes
@     it through, otherwise zero (documents the current stub).
@
@ No shared runtime — this test owns its vector table so it can
@ install a real ABT handler.

    .syntax unified
    .arch armv7ve

#include "../common/hvc_abi.S"

    @ Test scratch in RAM.
    .equ ABORT_COUNT_ADDR, 0x04000200
    .equ DFAR_SAVE_ADDR,   0x04000204
    .equ DFSR_SAVE_ADDR,   0x04000208
    @ Unaligned RAM address the faulting LDR targets.
    .equ BAD_ADDR,         0x04010001

    @ === Vector table ===================================================
    .section .text.vectors, "ax"
    .global _start
_start:
    b       _reset              @ 0x00: reset
    b       .Lhang              @ 0x04: undef
    b       .Lhang              @ 0x08: SWI
    b       .Lhang              @ 0x0C: prefetch abort
    b       _dabt_handler       @ 0x10: data abort
    b       .                   @ 0x14: reserved
    b       .Lhang              @ 0x18: IRQ
    b       .Lhang              @ 0x1C: FIQ

    .section .text, "ax"

_reset:
    @ Stacks: ABT at 0x04003000, SVC at 0x04001000. Non-overlapping with
    @ the scratch locations above (which are at 0x04000200..0x04000210).
    msr     cpsr_c, #0xD7       @ ABT mode, I=1 F=1
    ldr     sp, =0x04003000
    msr     cpsr_c, #0xD3       @ SVC mode, I=1 F=1
    ldr     sp, =0x04001000

    @ Zero the scratch locations.
    mov     r0, #0
    ldr     r1, =ABORT_COUNT_ADDR
    str     r0, [r1]
    ldr     r1, =DFAR_SAVE_ADDR
    str     r0, [r1]
    ldr     r1, =DFSR_SAVE_ADDR
    str     r0, [r1]

    @ SCTLR.A = 1 enables strict alignment checking. MCR traps to EL2
    @ via HCR_EL2.TVM; our CP15 shim applies to SCTLR_EL1.
    mrc     p15, 0, r0, c1, c0, 0
    orr     r0, r0, #(1 << 1)   @ A = bit 1
    mcr     p15, 0, r0, c1, c0, 0

    bl      test_main
.Lhang:
    b       .Lhang

    @ === Data abort handler ==========================================
    @
    @ On entry: LR_abt = faulting_pc + 8 (ARM ARM B1.8).
    @ We read DFAR / DFSR into the scratch slots, bump abort_count,
    @ and skip the faulting instruction by returning to LR - 4
    @ (= faulting_pc + 4, i.e. the next instruction).
_dabt_handler:
    push    {r0-r3, lr}

    mrc     p15, 0, r0, c6, c0, 0  @ DFAR
    ldr     r1, =DFAR_SAVE_ADDR
    str     r0, [r1]

    mrc     p15, 0, r0, c5, c0, 0  @ DFSR
    ldr     r1, =DFSR_SAVE_ADDR
    str     r0, [r1]

    ldr     r1, =ABORT_COUNT_ADDR
    ldr     r2, [r1]
    add     r2, r2, #1
    str     r2, [r1]

    pop     {r0-r3, lr}
    subs    pc, lr, #4             @ skip faulting LDR, restore CPSR

    @ === Test body ===================================================
test_main:
    @ Trigger the alignment fault. Unaligned word load -> ABT.
    ldr     r4, =BAD_ADDR
    ldr     r5, [r4]               @ faulting instruction

    @ If we reach here, the handler returned successfully.

    @ Check 1: abort_count == 1.
    ldr     r0, =ABORT_COUNT_ADDR
    ldr     r1, [r0]
    cmp     r1, #1
    bne     .Lfail_count

    @ Check 2: DFAR snapshot matches the faulting address. The
    @ ARM ARM permits DFAR to hold any value that was involved in
    @ the fault; for alignment faults implementations generally
    @ report the unaligned VA. If this assertion ever fails on real
    @ hardware we'll loosen it.
    ldr     r0, =DFAR_SAVE_ADDR
    ldr     r1, [r0]
    ldr     r2, =BAD_ADDR
    cmp     r1, r2
    bne     .Lfail_dfar

    mov     r0, #1
    hvc     #HVC_MARK                     @ mark 1: abort taken, handler ran, DFAR matches

    @ Check 3: DFSR reports an alignment fault. In the short-descriptor
    @ format (TTBCR.EAE=0) the fault status is encoded in bits
    @ [10, 3:0] with the FS[4]=0 FS[3:0]=0b0001 combination meaning
    @ "Alignment fault". Verify the low nybble is 1 and bit 10 is 0.
    ldr     r0, =DFSR_SAVE_ADDR
    ldr     r3, [r0]
    mov     r0, r3
    hvc     #HVC_PRINT_HEX                     @ print the raw DFSR for the log
    and     r1, r3, #0xF
    cmp     r1, #1
    bne     .Lfail_dfsr_class
    and     r1, r3, #(1 << 10)
    cmp     r1, #0
    bne     .Lfail_dfsr_class

    mov     r0, #2
    hvc     #HVC_MARK                     @ mark 2: DFSR reports alignment fault

    hvc     #HVC_PASS                     @ PASS
    bx      lr

    @ === Failure paths ===============================================
.Lfail_count:
    mov     r0, r1
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x60
    hvc     #HVC_FAIL
.Lfail_dfar:
    mov     r0, r1
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x61
    hvc     #HVC_FAIL
.Lfail_dfsr_class:
    mov     r0, r3
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x62
    hvc     #HVC_FAIL
