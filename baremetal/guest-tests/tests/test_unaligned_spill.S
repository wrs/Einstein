@ tests/test_unaligned_spill.S
@
@ Exercises the spill-based inline stub for unaligned LDRs at sites
@ with NO dead scratch registers (src/newton/unaligned_inline.rs).
@ Each site keeps all five scratch candidates {R0..R3, R12} live
@ right after the access, so the liveness picker fails and the
@ installer must fall back to the spill stub (STMDB sp!/LDMIA sp!
@ around the rotate-LDR body).
@
@ Each site executes twice. The DABT trampoline bumps a fault
@ counter in RAM, so the test distinguishes the two executions
@ mechanically:
@   pass 1: alignment fault → EL2 emulation + stub install
@           (counter +1)
@   pass 2: the installed stub runs natively — NO fault
@           (counter unchanged; a re-trap fails the test)
@ Both passes must produce the SA-1100 rotate-LDR result AND leave
@ every live register (R0..R3, R12) and the base/offset registers
@ intact.
@
@ Sites:
@   A — ldr r6, [r4, r5, lsl #1]   the hot-ROM shape (halfword-table
@       read); register offset, EA ≡ 2 (mod 4).       marks 1, 2
@   B — ldrne r6, [r4, r5, lsl #1] conditional: pass 1 runs with NE
@       true (faults, installs), pass 2 with NE false — the stub's
@       conditional LDR/ROR must skip, leaving r6 untouched, and the
@       stub must preserve NZCV (checked with beq after).
@                                                     marks 3, 4
@   C — ldr r6, [sp, #2]           Rn == SP: the stub's push moves
@       SP, so the builder inserts the `ADD sea, sp, #8` base fixup.
@                                                     marks 5, 6
@
@ Register protocol per pass: r0-r3/ip seeded with constants, r4/r5
@ base+index, r6 = result (seeded with a sentinel), r7 = liveness
@ sum, r8 = fault-counter snapshot, r9/r10 = expected values,
@ r11 = pass number.

    .syntax unified
    .arch armv7ve

#include "../common/hvc_abi.S"

    .equ DABT_SAVE_PA,    0x060000A0

    .equ SRC_ADDR,        0x04010000
    .equ SRC_WORD,        0x11223344
    .equ FAULT_COUNT,     0x04010020

    .equ SEED_R0,         0x01010101
    .equ SEED_R1,         0x02020202
    .equ SEED_R2,         0x03030303
    .equ SEED_R3,         0x04040404
    .equ SEED_IP,         0x0C0C0C0C
    .equ SEED_SUM,        0x16161616   @ r0+r1+r2+r3+ip
    .equ SENTINEL,        0xDEADBEEF
    .equ ROR16_WORD,      0x33441122   @ SRC_WORD ROR 16

    @ === Vector table ===================================================
    .section .text.vectors, "ax"
    .global _start
_start:
    b       _reset              @ 0x00: reset
    b       .Lhang              @ 0x04: undef
    b       .Lhang              @ 0x08: SWI
    b       .Lhang              @ 0x0C: prefetch abort
    b       _dabt_trampoline    @ 0x10: data abort
    b       .                   @ 0x14: reserved
    b       .Lhang              @ 0x18: IRQ
    b       .Lhang              @ 0x1C: FIQ

    .section .text, "ax"

_reset:
    @ SVC and ABT stacks.
    msr     cpsr_c, #0xD7       @ ABT mode, I=1 F=1
    ldr     sp, =0x04003000
    msr     cpsr_c, #0xD3       @ SVC mode, I=1 F=1
    ldr     sp, =0x04008000

    @ SCTLR.A = 1 so unaligned LDR/STR raises an alignment fault.
    mrc     p15, 0, r0, c1, c0, 0
    orr     r0, r0, #(1 << 1)
    mcr     p15, 0, r0, c1, c0, 0

    @ Seed the source word and zero the fault counter (aligned stores).
    ldr     r0, =SRC_ADDR
    ldr     r1, =SRC_WORD
    str     r1, [r0]
    ldr     r0, =FAULT_COUNT
    mov     r1, #0
    str     r1, [r0]

    bl      test_main
.Lhang:
    b       .Lhang

    @ === DABT trampoline (production shape + fault counter) =============
    @ Save R0/R1 → TPIDR scratch regs first (they are then free), bump
    @ the RAM fault counter, save LR/SP/SPSR to DABT_SAVE, HVC #Align.
_dabt_trampoline:
    mcr     p15, 0, r0,  c13, c0, 2  @ save R0  → TPIDRURW
    mcr     p15, 0, r1,  c13, c0, 3  @ save R1  → TPIDRRO
    ldr     r0, =FAULT_COUNT
    ldr     r1, [r0]
    add     r1, r1, #1
    str     r1, [r0]
    ldr     r0, =DABT_SAVE_PA
    str     lr,  [r0]                @ LR_abt    +0x00
    str     sp,  [r0, #4]            @ SP_abt    +0x04
    mrs     r1,  spsr
    str     r1,  [r0, #8]            @ SPSR_abt  +0x08
    hvc     #HVC_ALIGN
    b       .                        @ guard (handler ERETs past the fault)

    @ --- helpers -----------------------------------------------------
    @ Seed r0-r3/ip, r4 = SRC_ADDR, r5 = 1, r6 = sentinel, and stash
    @ the current fault count in r8.
seed_regs:
    ldr     r0, =FAULT_COUNT
    ldr     r8, [r0]
    ldr     r0, =SEED_R0
    ldr     r1, =SEED_R1
    ldr     r2, =SEED_R2
    ldr     r3, =SEED_R3
    ldr     ip, =SEED_IP
    ldr     r4, =SRC_ADDR
    mov     r5, #1
    ldr     r6, =SENTINEL
    bx      lr

    @ Verify the liveness sum in r7, the seeds in r0-r3/ip, and the
    @ base/index in r4/r5. Clobbers r9/r10. Falls into .Lfail_regs on
    @ mismatch.
check_live_regs:
    ldr     r9, =SEED_SUM
    cmp     r7, r9
    bne     .Lfail_regs
    ldr     r9, =SEED_R0
    cmp     r0, r9
    bne     .Lfail_regs
    ldr     r9, =SEED_R1
    cmp     r1, r9
    bne     .Lfail_regs
    ldr     r9, =SEED_R2
    cmp     r2, r9
    bne     .Lfail_regs
    ldr     r9, =SEED_R3
    cmp     r3, r9
    bne     .Lfail_regs
    ldr     r9, =SEED_IP
    cmp     ip, r9
    bne     .Lfail_regs
    ldr     r9, =SRC_ADDR
    cmp     r4, r9
    bne     .Lfail_regs
    cmp     r5, #1
    bne     .Lfail_regs
    bx      lr

    @ Verify the fault counter moved by r10 (0 or 1) relative to the
    @ r8 snapshot. Clobbers r9.
check_fault_delta:
    ldr     r9, =FAULT_COUNT
    ldr     r9, [r9]
    sub     r9, r9, r8
    cmp     r9, r10
    bne     .Lfail_fault_count
    bx      lr

test_main:
    push    {lr}
    mov     r11, #1              @ pass number

.Lpass_loop:
    @ ==== Site A: register-offset, all candidates live ==============
    bl      seed_regs
site_a:
    ldr     r6, [r4, r5, lsl #1] @ EA = SRC_ADDR + 2 → alignment fault
    add     r7, r0, r1           @ r0, r1 live
    add     r7, r7, r2           @ r2 live
    add     r7, r7, r3           @ r3 live
    add     r7, r7, ip           @ ip live

    ldr     r9, =ROR16_WORD
    cmp     r6, r9
    bne     .Lfail_result
    bl      check_live_regs
    cmp     r11, #1
    moveq   r10, #1              @ pass 1: exactly one fault
    movne   r10, #0              @ pass 2: stub — no fault
    bl      check_fault_delta

    mov     r0, r11              @ marks 1 / 2
    hvc     #HVC_MARK

    @ ==== Site B: conditional (NE) ==================================
    @ Pass 1 (r11=1): Z=0 → NE true → executes, faults, installs.
    @ Pass 2 (r11=2): Z=1 → NE false → the stub's conditional
    @ LDR/ROR skip; r6 keeps the sentinel. The adds below are S=0,
    @ so the `bne` after them doubles as the check that the stub's
    @ S=0 body preserved NZCV across the push/EA/pop.
    bl      seed_regs
    cmp     r11, #2
site_b:
    ldrne   r6, [r4, r5, lsl #1]
    add     r7, r0, r1
    add     r7, r7, r2
    add     r7, r7, r3
    add     r7, r7, ip
    bne     .Lsite_b_executed

    @ pass 2: cond failed — sentinel intact, no fault, regs intact.
    ldr     r9, =SENTINEL
    cmp     r6, r9
    bne     .Lfail_result
    bl      check_live_regs
    mov     r10, #0
    bl      check_fault_delta
    mov     r0, #4               @ mark 4
    hvc     #HVC_MARK
    b       .Lsite_c

.Lsite_b_executed:
    @ pass 1: cond passed — rotate result, one fault.
    ldr     r9, =ROR16_WORD
    cmp     r6, r9
    bne     .Lfail_result
    bl      check_live_regs
    mov     r10, #1
    bl      check_fault_delta
    mov     r0, #3               @ mark 3
    hvc     #HVC_MARK

.Lsite_c:
    @ ==== Site C: Rn == SP (base fixup) =============================
    bl      seed_regs
    ldr     r9, =SRC_WORD
    str     r9, [sp]             @ aligned seed at [sp]
site_c:
    ldr     r6, [sp, #2]         @ EA = sp + 2 → alignment fault
    add     r7, r0, r1
    add     r7, r7, r2
    add     r7, r7, r3
    add     r7, r7, ip

    ldr     r9, =ROR16_WORD
    cmp     r6, r9
    bne     .Lfail_result
    bl      check_live_regs
    cmp     r11, #1
    moveq   r10, #1
    movne   r10, #0
    bl      check_fault_delta

    add     r0, r11, #4          @ marks 5 / 6
    hvc     #HVC_MARK

    @ ==== next pass / done ==========================================
    add     r11, r11, #1
    cmp     r11, #2
    ble     .Lpass_loop

    hvc     #HVC_PASS
    pop     {lr}
    bx      lr

    @ === Failure paths ==============================================
.Lfail_result:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x80
    hvc     #HVC_FAIL
.Lfail_regs:
    mov     r0, r9
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x81
    hvc     #HVC_FAIL
.Lfail_fault_count:
    mov     r0, r9
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x82
    hvc     #HVC_FAIL
