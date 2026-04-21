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

    .global patch_end
patch_end:
    .ltorg                              @ flush any pending literals OUTSIDE
