@ tests/test_shadow_stub.S
@
@ Exercises the EL2 shadow-stub mechanism (src/shadow_stub.rs) by
@ asking the hypervisor to patch this test image's own byte/halfword
@ accesses, then running a battery of subtests that verify the
@ patched code does the BE-32-correct thing:
@
@   - byte access:      phys[A] viewed as phys[A^3]
@   - halfword access:  phys[A] viewed as phys[A^2]
@   - addresses >= XOR_LIMIT pass through unmodified (MMIO bypass)
@
@ Test protocol:
@   HVC #0x30 with r0 = start_ipa, r1 = end_ipa (exclusive) asks the
@   hypervisor to scan that range for byte/halfword accesses and
@   install stubs. Returns the patched-insn count in r0.
@
@ Everything inside [patch_start, patch_end) is a candidate for
@ scanning. 32-bit constants inside that range are loaded with
@ `movw`+`movt` rather than `ldr =…` to keep the assembler from
@ tucking literal-pool words into the scan region (some literals
@ accidentally decode as byte/halfword access encodings).

    .syntax unified
    .arch armv7ve

    .equ SCRATCH_BASE,       0x04003000   @ 16-byte scratch, word aligned
    .equ SCRATCH_HI,         0x10003000   @ flash-bank-1 IPA (≥ XOR_LIMIT)

    @ === Vector table ===========================================
    .section .text.vectors, "ax"
    .global _start
_start:
    b       _reset              @ 0x00: reset
    b       .Lhalt_undef        @ 0x04: UND
    b       .Lhalt_swi          @ 0x08: SWI
    b       .Lhalt_pabt         @ 0x0C: PABT
    b       .Lhalt_dabt         @ 0x10: DABT
    b       .                   @ 0x14: reserved
    b       .Lhalt_irq          @ 0x18: IRQ
    b       .Lhalt_fiq          @ 0x1C: FIQ

    .section .text, "ax"

    @ === load_u32 macro: load a 32-bit constant without literals ===
    @ Uses MOVW + MOVT to avoid putting a .word into the scanned region.
    @ `val` must be a numeric constant expression.
    .macro  load_u32 reg, val
        movw    \reg, #((\val) & 0xFFFF)
        movt    \reg, #(((\val) >> 16) & 0xFFFF)
    .endm

    @ Load a link-time symbol via the MOVW/MOVT relocations.
    .macro  load_sym reg, sym
        movw    \reg, #:lower16:\sym
        movt    \reg, #:upper16:\sym
    .endm

_reset:
    msr     cpsr_c, #0xD3               @ SVC, I=1 F=1
    load_u32 sp, 0x04001000

    @ Ask the hypervisor to scan/patch our subtest code. We pass the
    @ range [PATCH_START, PATCH_END).
    load_sym r0, patch_start
    load_sym r1, patch_end
    hvc     #0x30                       @ returns patched-count in r0
    hvc     #2                          @ print it for the log

    bl      test_main

    @ If test_main returns without signalling, treat as failure.
    mov     r0, #0xFF
    hvc     #4                          @ FAIL
.Lhang:
    b       .Lhang

.Lhalt_undef:
    mov     r0, #0x01
    b       .Lhalt_fail
.Lhalt_swi:
    mov     r0, #0x02
    b       .Lhalt_fail
.Lhalt_pabt:
    mov     r0, #0x03
    b       .Lhalt_fail
.Lhalt_dabt:
    mov     r0, #0x04
    b       .Lhalt_fail
.Lhalt_irq:
    mov     r0, #0x05
    b       .Lhalt_fail
.Lhalt_fiq:
    mov     r0, #0x06
    b       .Lhalt_fail
.Lhalt_fail:
    hvc     #4
    b       .Lhang

    @ Jump target: r0 = subtest number, prints and halts FAIL.
.Lfail:
    hvc     #2                          @ print subtest number
    mov     r0, #0xEE
    hvc     #4
    b       .Lhang

    @ Macro: fail with a subtest number.
    .macro  fail_st n
        mov     r0, #\n
        b       .Lfail
    .endm

test_main:
    push    {r4-r11, lr}

    bl      subtest_01_word_then_byte
    bl      subtest_02_byte_then_word
    bl      subtest_03_byte_then_byte
    bl      subtest_04_halfword
    bl      subtest_05_signed
    bl      subtest_06_reg_offset
    bl      subtest_07_reg_offset_shift
    bl      subtest_08_pre_writeback
    bl      subtest_09_post_index
    bl      subtest_10_conditional
    bl      subtest_11_mmio_skip
    bl      subtest_12_sp_imm
    bl      subtest_13_sp_neg_imm
    bl      subtest_14_sp_reg
    bl      subtest_15_sp_writeback
    bl      subtest_16_sp_post_index
    bl      subtest_17_swpb
    bl      subtest_18_ldrd_verify
    bl      subtest_19_abort_transparency

    pop     {r4-r11, lr}
    @ Print PASS banner via unpatched HVC_PUTC.
    mov     r0, #'P'
    hvc     #1
    mov     r0, #'A'
    hvc     #1
    mov     r0, #'S'
    hvc     #1
    mov     r0, #'S'
    hvc     #1
    mov     r0, #'\n'
    hvc     #1
    hvc     #3                          @ guest test PASSED
    bx      lr

    @ ==============================================================
    @ Patched region begins here. Every byte/halfword access below
    @ is a candidate for shadow-stub replacement.
    @ ==============================================================
    .global patch_start
patch_start:

    @ ---- subtest 1: word-write then byte-reads --------------------
subtest_01_word_then_byte:
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0x11223344
    str     r5, [r4]
    ldrb    r6, [r4]
    cmp     r6, #0x11
    bne     .Lst1_fail
    ldrb    r6, [r4, #1]
    cmp     r6, #0x22
    bne     .Lst1_fail
    ldrb    r6, [r4, #2]
    cmp     r6, #0x33
    bne     .Lst1_fail
    ldrb    r6, [r4, #3]
    cmp     r6, #0x44
    bne     .Lst1_fail
    bx      lr
.Lst1_fail:
    mov     r0, r6
    hvc     #2
    fail_st 1

    @ ---- subtest 2: byte-writes then word-read --------------------
subtest_02_byte_then_word:
    load_u32 r4, SCRATCH_BASE
    mov     r5, #0
    str     r5, [r4]
    mov     r5, #0x11
    strb    r5, [r4]
    mov     r5, #0x22
    strb    r5, [r4, #1]
    mov     r5, #0x33
    strb    r5, [r4, #2]
    mov     r5, #0x44
    strb    r5, [r4, #3]
    ldr     r6, [r4]
    load_u32 r7, 0x11223344
    cmp     r6, r7
    bne     .Lst2_fail
    bx      lr
.Lst2_fail:
    mov     r0, r6
    hvc     #2
    fail_st 2

    @ ---- subtest 3: idempotent STRB then LDRB ---------------------
subtest_03_byte_then_byte:
    load_u32 r4, SCRATCH_BASE
    mov     r5, #0
    str     r5, [r4]
    mov     r8, #0
.Lst3_loop:
    add     r5, r8, #0x80
    strb    r5, [r4, r8]
    ldrb    r6, [r4, r8]
    cmp     r6, r5
    bne     .Lst3_fail
    add     r8, r8, #1
    cmp     r8, #4
    bne     .Lst3_loop
    bx      lr
.Lst3_fail:
    mov     r0, r6
    hvc     #2
    fail_st 3

    @ ---- subtest 4: halfword variants -----------------------------
subtest_04_halfword:
    load_u32 r4, SCRATCH_BASE
    mov     r5, #0
    str     r5, [r4]
    movw    r5, #0xAABB
    strh    r5, [r4]
    ldrh    r6, [r4]
    movw    r7, #0xAABB
    cmp     r6, r7
    bne     .Lst4_fail
    ldrb    r6, [r4]
    cmp     r6, #0xAA
    bne     .Lst4_fail
    ldrb    r6, [r4, #1]
    cmp     r6, #0xBB
    bne     .Lst4_fail
    bx      lr
.Lst4_fail:
    mov     r0, r6
    hvc     #2
    fail_st 4

    @ ---- subtest 5: signed byte & halfword ------------------------
subtest_05_signed:
    load_u32 r4, SCRATCH_BASE
    mov     r5, #0
    str     r5, [r4]
    mov     r5, #0xFF
    strb    r5, [r4]
    ldrsb   r6, [r4]
    mvn     r7, #0                      @ 0xFFFFFFFF
    cmp     r6, r7
    bne     .Lst5_fail
    mov     r5, #0
    str     r5, [r4]
    movw    r5, #0x8001
    strh    r5, [r4]
    ldrsh   r6, [r4]
    load_u32 r7, 0xFFFF8001
    cmp     r6, r7
    bne     .Lst5_fail
    bx      lr
.Lst5_fail:
    mov     r0, r6
    hvc     #2
    fail_st 5

    @ ---- subtest 6: register-offset LDRB --------------------------
subtest_06_reg_offset:
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0xDEADBEEF
    str     r5, [r4]
    mov     r9, #0
    ldrb    r6, [r4, r9]
    cmp     r6, #0xDE
    bne     .Lst6_fail
    mov     r9, #1
    ldrb    r6, [r4, r9]
    cmp     r6, #0xAD
    bne     .Lst6_fail
    mov     r9, #3
    ldrb    r6, [r4, r9]
    cmp     r6, #0xEF
    bne     .Lst6_fail
    bx      lr
.Lst6_fail:
    mov     r0, r6
    hvc     #2
    fail_st 6

    @ ---- subtest 7: register-offset with LSL #2 -------------------
subtest_07_reg_offset_shift:
    load_u32 r4, SCRATCH_BASE
    mov     r5, #0xAA
    strb    r5, [r4]
    mov     r5, #0xBB
    mov     r9, #1
    strb    r5, [r4, r9, lsl #2]        @ byte at SCRATCH_BASE + 4
    ldrb    r6, [r4]
    cmp     r6, #0xAA
    bne     .Lst7_fail
    mov     r9, #1
    ldrb    r6, [r4, r9, lsl #2]
    cmp     r6, #0xBB
    bne     .Lst7_fail
    bx      lr
.Lst7_fail:
    mov     r0, r6
    hvc     #2
    fail_st 7

    @ ---- subtest 8: pre-indexed writeback -------------------------
subtest_08_pre_writeback:
    load_u32 r4, SCRATCH_BASE
    add     r5, r4, #4
    load_u32 r6, 0x11223344
    str     r6, [r5]
    mov     r10, r4
    ldrb    r6, [r4, #4]!
    cmp     r6, #0x11
    bne     .Lst8_fail
    sub     r7, r4, r10
    cmp     r7, #4
    bne     .Lst8_fail
    bx      lr
.Lst8_fail:
    mov     r0, r6
    hvc     #2
    fail_st 8

    @ ---- subtest 9: post-indexed ----------------------------------
subtest_09_post_index:
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0xAABBCCDD
    str     r5, [r4]
    mov     r10, r4
    ldrb    r6, [r4], #1
    cmp     r6, #0xAA
    bne     .Lst9_fail
    sub     r7, r4, r10
    cmp     r7, #1
    bne     .Lst9_fail
    bx      lr
.Lst9_fail:
    mov     r0, r6
    hvc     #2
    fail_st 9

    @ ---- subtest 10: conditional LDRBEQ ---------------------------
subtest_10_conditional:
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0x11223344
    str     r5, [r4]
    @ Taken branch: Z=1.
    mov     r0, #0
    cmp     r0, r0
    mov     r6, #0xEE
    ldrbeq  r6, [r4]
    cmp     r6, #0x11
    bne     .Lst10_fail
    @ Not taken: Z=0.
    mov     r0, #1
    cmp     r0, #0
    mov     r6, #0xEE
    ldrbeq  r6, [r4]
    cmp     r6, #0xEE
    bne     .Lst10_fail
    bx      lr
.Lst10_fail:
    mov     r0, r6
    hvc     #2
    fail_st 10

    @ ---- subtest 11: MMIO skip (addr >= XOR_LIMIT) ----------------
subtest_11_mmio_skip:
    load_u32 r4, SCRATCH_HI
    mov     r5, #0
    str     r5, [r4]                    @ clear word
    mov     r5, #0x5A
    strb    r5, [r4]                    @ no-XOR: phys[0] = 0x5A
    ldr     r6, [r4]                    @ word read (not patched): LE word
    cmp     r6, #0x5A
    bne     .Lst11_fail
    load_u32 r5, 0x04030201
    str     r5, [r4]
    ldrb    r6, [r4]                    @ no-XOR: phys[0] = 0x01
    cmp     r6, #0x01
    bne     .Lst11_fail
    ldrb    r6, [r4, #1]                @ no-XOR: phys[1] = 0x02
    cmp     r6, #0x02
    bne     .Lst11_fail
    ldrb    r6, [r4, #3]                @ no-XOR: phys[3] = 0x04
    cmp     r6, #0x04
    bne     .Lst11_fail
    bx      lr
.Lst11_fail:
    mov     r0, r6
    hvc     #2
    fail_st 11

    @ ---- subtest 12: SP-relative LDRB [SP, #imm] ------------------
    @ Regression check for the original bug where the stub spilled
    @ scratch to [SP, #-4]! before computing EA, skewing [SP, #imm]
    @ by -4. Puts a known word on the stack and reads each byte lane.
subtest_12_sp_imm:
    push    {r4, r5, r6, r7, lr}
    load_u32 r5, 0xAABBCCDD
    sub     sp, sp, #16
    str     r5, [sp]                    @ word at [sp]
    ldrb    r6, [sp]                    @ phys[0] = 0xAA
    cmp     r6, #0xAA
    bne     .Lst12_fail
    ldrb    r6, [sp, #1]                @ 0xBB
    cmp     r6, #0xBB
    bne     .Lst12_fail
    ldrb    r6, [sp, #2]                @ 0xCC
    cmp     r6, #0xCC
    bne     .Lst12_fail
    ldrb    r6, [sp, #3]                @ 0xDD
    cmp     r6, #0xDD
    bne     .Lst12_fail
    add     sp, sp, #16
    pop     {r4, r5, r6, r7, lr}
    bx      lr
.Lst12_fail:
    mov     r0, r6
    add     sp, sp, #16
    pop     {r4, r5, r6, r7, lr}
    hvc     #2
    fail_st 12

    @ ---- subtest 13: SP-relative LDRB [SP, #-imm] -----------------
    @ Exercises negative-offset addressing anchored on SP.
subtest_13_sp_neg_imm:
    push    {r4, r5, r6, r7, lr}
    load_u32 r5, 0x11223344
    sub     sp, sp, #16
    str     r5, [sp, #8]                @ word at [sp+8]
    add     r4, sp, #12                 @ point past the word
    ldrb    r6, [r4, #-4]               @ phys[0] of word = 0x11
    cmp     r6, #0x11
    bne     .Lst13_fail
    ldrb    r6, [r4, #-1]               @ phys[3] = 0x44
    cmp     r6, #0x44
    bne     .Lst13_fail
    add     sp, sp, #16
    pop     {r4, r5, r6, r7, lr}
    bx      lr
.Lst13_fail:
    mov     r0, r6
    add     sp, sp, #16
    pop     {r4, r5, r6, r7, lr}
    hvc     #2
    fail_st 13

    @ ---- subtest 14: SP + Rm ---------------------------------------
    @ LDRB through register offset on SP.
subtest_14_sp_reg:
    push    {r4, r5, r6, r7, lr}
    load_u32 r5, 0xDEADBEEF
    sub     sp, sp, #16
    str     r5, [sp]
    @ BE-32: LDRB at A reads phys[A^3]. With SP word-aligned and
    @ 0xDEADBEEF stored at [SP]:
    @   LDRB [SP+0] -> phys[SP+3] = 0xDE
    @   LDRB [SP+2] -> phys[SP+1] = 0xBE
    @   LDRB [SP+3] -> phys[SP+0] = 0xEF
    mov     r9, #0
    ldrb    r6, [sp, r9]
    cmp     r6, #0xDE
    bne     .Lst14_fail
    mov     r9, #2
    ldrb    r6, [sp, r9]
    cmp     r6, #0xBE
    bne     .Lst14_fail
    mov     r9, #3
    ldrb    r6, [sp, r9]
    cmp     r6, #0xEF
    bne     .Lst14_fail
    add     sp, sp, #16
    pop     {r4, r5, r6, r7, lr}
    bx      lr
.Lst14_fail:
    mov     r0, r6
    add     sp, sp, #16
    pop     {r4, r5, r6, r7, lr}
    hvc     #2
    fail_st 14

    @ ---- subtest 15: pre-index writeback through SP --------------
    @ LDRB Rt, [SP, #n]! — after the insn, SP should be SP+n.
subtest_15_sp_writeback:
    push    {r4, r5, r6, r7, lr}
    load_u32 r5, 0xA0B0C0D0
    sub     sp, sp, #16
    str     r5, [sp, #4]                @ word at [sp+4]
    mov     r7, sp
    ldrb    r6, [sp, #4]!               @ writeback: SP := SP+4
    cmp     r6, #0xA0
    bne     .Lst15_fail
    sub     r4, sp, r7
    cmp     r4, #4
    bne     .Lst15_fail
    sub     sp, sp, #4                  @ restore SP for the add sp
    add     sp, sp, #16
    pop     {r4, r5, r6, r7, lr}
    bx      lr
.Lst15_fail:
    mov     r0, r6
    mov     sp, r7                      @ restore SP in failure path
    add     sp, sp, #16
    pop     {r4, r5, r6, r7, lr}
    hvc     #2
    fail_st 15

    @ ---- subtest 16: post-index through SP -----------------------
    @ LDRB Rt, [SP], #n — after the insn, SP should be SP+n.
subtest_16_sp_post_index:
    push    {r4, r5, r6, r7, lr}
    load_u32 r5, 0x01020304
    sub     sp, sp, #16
    str     r5, [sp]                    @ word at [sp]
    mov     r7, sp
    ldrb    r6, [sp], #1                @ writeback: SP := SP+1
    cmp     r6, #0x01
    bne     .Lst16_fail
    sub     r4, sp, r7
    cmp     r4, #1
    bne     .Lst16_fail
    mov     sp, r7                      @ restore original SP
    add     sp, sp, #16
    pop     {r4, r5, r6, r7, lr}
    bx      lr
.Lst16_fail:
    mov     r0, r6
    mov     sp, r7
    add     sp, sp, #16
    pop     {r4, r5, r6, r7, lr}
    hvc     #2
    fail_st 16

    @ ---- subtest 17: SWPB ----------------------------------------
    @ Exercises SWPB at each byte lane within a word.
subtest_17_swpb:
    load_u32 r4, SCRATCH_BASE
    @ Lane 0: [r4+0]
    load_u32 r5, 0x11223344
    str     r5, [r4]
    mov     r6, #0xAA
    swpb    r7, r6, [r4]                @ r7 = old byte, [r4] gets r6
    cmp     r7, #0x11                   @ old byte at r4^0^3=3 phys = 0x11
    bne     .Lst17_fail
    ldrb    r7, [r4]
    cmp     r7, #0xAA                   @ new byte readable via BE-32 LDRB
    bne     .Lst17_fail
    @ Lane 1: [r4+1]
    load_u32 r5, 0xAABBCCDD
    str     r5, [r4]
    add     r8, r4, #1
    mov     r6, #0x99
    swpb    r7, r6, [r8]
    cmp     r7, #0xBB
    bne     .Lst17_fail
    ldrb    r7, [r4, #1]
    cmp     r7, #0x99
    bne     .Lst17_fail
    @ Lane 3: [r4+3]
    load_u32 r5, 0x12345678
    str     r5, [r4]
    add     r8, r4, #3
    mov     r6, #0x42
    swpb    r7, r6, [r8]
    cmp     r7, #0x78
    bne     .Lst17_fail
    ldrb    r7, [r4, #3]
    cmp     r7, #0x42
    bne     .Lst17_fail
    bx      lr
.Lst17_fail:
    mov     r0, r7
    hvc     #2
    fail_st 17

    @ ---- subtest 18: LDRD / STRD ----------------------------------
    @ LDRD/STRD read/write an aligned 64-bit pair using a register
    @ pair Rt/Rt+1. In BE-32 this is a word-invariant pair of word
    @ accesses: the high-addr word goes to Rt+1 and the low-addr word
    @ to Rt. Our decoder currently returns None for LDRD/STRD, i.e.
    @ leaves them un-stubbed. This subtest verifies that's actually
    @ correct — if it fails we need to add LDRD/STRD stubs.
subtest_18_ldrd_verify:
    load_u32 r4, SCRATCH_BASE
    @ Write a known 64-bit pattern using two aligned word stores.
    load_u32 r5, 0x11223344            @ low word (at [r4])
    load_u32 r8, 0x55667788            @ high word (at [r4+4])
    str     r5, [r4]
    str     r8, [r4, #4]
    @ Read it back with LDRD. LDRD needs an even base register for
    @ Rt; pick r6/r7 as the pair.
    ldrd    r6, r7, [r4]
    @ For BE-32 word-invariant, the memory representation is such
    @ that each word-aligned STR / LDR sees the word it stored.
    @ So r6 should equal 0x11223344 and r7 should equal 0x55667788.
    load_u32 r8, 0x11223344
    cmp     r6, r8
    bne     .Lst18_fail
    load_u32 r8, 0x55667788
    cmp     r7, r8
    bne     .Lst18_fail
    @ Now exercise STRD: write a different pattern via STRD and
    @ read back with word loads.
    load_u32 r6, 0xAABBCCDD
    load_u32 r7, 0xEEFF1122
    strd    r6, r7, [r4]
    ldr     r5, [r4]
    load_u32 r8, 0xAABBCCDD
    cmp     r5, r8
    bne     .Lst18_fail
    ldr     r5, [r4, #4]
    load_u32 r8, 0xEEFF1122
    cmp     r5, r8
    bne     .Lst18_fail
    bx      lr
.Lst18_fail:
    mov     r0, r6
    hvc     #2
    fail_st 18

    @ ---- subtest 19: abort transparency --------------------------
    @ Performs a PATCHED LDRB whose target IPA is unmapped at stage-2.
    @ The stub triggers a stage-2 fault while executing the inner
    @ LDRB; the hypervisor must re-inject the abort to the guest with
    @ FAR showing the un-XOR'd guest-view VA and LR pointing at
    @ original_pc + 8 — exactly what the guest would have seen without
    @ the stub mechanism.
    @
    @ The custom DABT vector at VBAR+0x10 records FAR and LR to fixed
    @ RAM slots, then HVC #3 (PASS).
subtest_19_abort_transparency:
    @ Install our mini vector table at 0x04000800 in RAM. Layout:
    @   +0x00..0x0C: b . (reset/und/svc/pabt)
    @   +0x10:       ldr pc, [pc, #24] (DABT) — pulls abort_handler
    @                                           from the literal at +0x30
    @   +0x14..0x1C: b . (reserved/irq/fiq)
    @   +0x20..0x2C: (padding)
    @   +0x30:       .word abort_handler
    @
    @ We use an indirect load-to-PC rather than B since abort_handler
    @ sits in the ROM region (~0x0900), which is more than 32 MiB
    @ below the vector table in RAM — out of B's ±32 MiB reach.
    load_u32 r4, 0x04000800

    @ Vector words: b . everywhere except DABT slot.
    load_u32 r9, 0xEAFFFFFE              @ b . (branch to self)
    str     r9, [r4, #0x00]
    str     r9, [r4, #0x04]
    str     r9, [r4, #0x08]
    str     r9, [r4, #0x0C]
    @ DABT slot: `LDR PC, [PC, #24]` — reads abort_handler literal at
    @ +0x30 (24 byte offset from PC+8 at the LDR site (0x10+8=0x18)
    @ to 0x30 is 0x18).
    load_u32 r9, 0xE59FF018              @ ldr pc, [pc, #24]
    str     r9, [r4, #0x10]
    load_u32 r9, 0xEAFFFFFE
    str     r9, [r4, #0x14]
    str     r9, [r4, #0x18]
    str     r9, [r4, #0x1C]
    @ Literal pool entry at offset 0x30.
    load_sym r7, abort_handler
    str     r7, [r4, #0x30]

    @ Flush I-cache by MVA so CPU refetches the vector table bytes.
    @ Flush two cache lines: the vector table and the literal at +0x30.
    mcr     p15, 0, r4, c7, c5, 1
    add     r9, r4, #0x20
    mcr     p15, 0, r9, c7, c5, 1
    dsb
    isb

    @ Program VBAR_EL1 = 0x04000800 (CP15 c12, c0, opc2=0).
    mcr     p15, 0, r4, c12, c0, 0

    @ Reset captures in RAM.
    load_u32 r0, 0x04000900
    mov     r1, #0
    str     r1, [r0, #0]                @ captured FAR
    str     r1, [r0, #4]                @ captured LR

    @ Target unmapped IPA. 0x5000_0000 is outside our stage-2 map;
    @ both 0x5000_0000 and 0x5000_0003 (XOR'd) are unmapped.
    load_u32 r4, 0x50000000
.Lst19_faulting_pc:
    ldrb    r5, [r4]                    @ PATCHED — stage-2 fault expected

    @ If we reach here the abort wasn't delivered. That's a fail.
    mov     r0, #0x19
    hvc     #2
    fail_st 19

    .global patch_end
patch_end:
    .ltorg                              @ flush any pending literals OUTSIDE

    @ === Abort handler (outside patch range) =======================
    @ The shadow_stub scanner is called with [patch_start, patch_end),
    @ so code below patch_end is not patched. The handler itself uses
    @ only word-sized LDR/STR and MRC, so the absence of stubs is fine.
abort_handler:
    @ Running in AArch32 ABT mode after EL2 injected the abort.
    @ LR = original_pc + 8, FAR_EL1 holds the un-XOR'd guest VA.
    load_u32 r0, 0x04000900
    mrc     p15, 0, r1, c6, c0, 0       @ DFAR = FAR_EL1
    str     r1, [r0, #0]
    str     lr, [r0, #4]

    load_u32 r2, 0x50000000
    cmp     r1, r2
    bne     .Lst19_fail_far
    load_sym r2, .Lst19_faulting_pc
    add     r2, r2, #8
    cmp     lr, r2
    bne     .Lst19_fail_lr

    @ Success — signal PASS directly (we're in ABT mode; no need to
    @ return to SVC mode since this is the last subtest).
    mov     r0, #0x50                   @ 'P'
    hvc     #1
    mov     r0, #0x41                   @ 'A'
    hvc     #1
    mov     r0, #0x53                   @ 'S'
    hvc     #1
    mov     r0, #0x53                   @ 'S'
    hvc     #1
    mov     r0, #0x0a
    hvc     #1
    mov     r0, #0x19
    hvc     #3
.Lst19_hang:
    b       .Lst19_hang

.Lst19_fail_far:
    mov     r0, r1
    hvc     #2
    mov     r0, #0x91
    hvc     #4
    b       .Lst19_hang
.Lst19_fail_lr:
    mov     r0, lr
    hvc     #2
    mov     r0, #0x92
    hvc     #4
    b       .Lst19_hang
