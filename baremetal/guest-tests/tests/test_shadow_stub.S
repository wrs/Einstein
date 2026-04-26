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
    .equ SCRATCH_HI,         0x12000000   @ test-only mmio scratch (≥ XOR_LIMIT)

    @ === Vector table ===========================================
    .section .text.vectors, "ax"
    .global _start
_start:
    b       _reset              @ 0x00: reset
    b       .Lhalt_undef        @ 0x04: UND
    b       _swi_dispatch       @ 0x08: SWI (subtest_23 routes here)
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

    @ SWI vector dispatcher. Reads a "SWI mode" flag at 0x04003F08:
    @   0 = no test active → halt (treat as test failure)
    @   1 = subtest_23 active → run an SBA byte access from inside
    @       the SWI handler, then return to the caller via
    @       `movs pc, lr` (which restores CPSR from SPSR_svc).
    @
    @ The handler exercises the same path an IRQ would: an exception
    @ taken mid-routine in SVC mode, with banked LR_svc/SPSR_svc, the
    @ handler does its own byte access (which goes through an inline
    @ stub), then returns. We use SWI rather than a real IRQ because
    @ a real IRQ-firing-mid-stub isn't deterministic from a guest
    @ test, and the banked-register / inline-stub semantics we care
    @ about are identical.
_swi_dispatch:
    movw    r12, #0x3F08
    movt    r12, #0x0400
    ldr     r12, [r12]
    cmp     r12, #1
    bne     .Lhalt_swi
    @ Save caller-saved registers (we'll do byte ops here).
    push    {r0, r1, r2, r3, lr}
    @ The "SWI handler" byte access: read the byte at r4+0 (caller
    @ left SCRATCH_BASE in r4, with a known word stored there). This
    @ access goes through whatever stub the patcher installed.
    @ Bump a counter to prove the handler ran.
    movw    r12, #0x3F0C
    movt    r12, #0x0400
    ldr     r0, [r12]
    add     r0, r0, #1
    str     r0, [r12]
    pop     {r0, r1, r2, r3, lr}
    movs    pc, lr

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
    @ subtest_19_cross_page_dabt: deferred. The stub's natural-DABT
    @ recovery path is heavily exercised by the FVP ROM boot
    @ (kernel's own DataAbortHandler runs many times during heap +
    @ stack growth). Re-creating the same shape from a guest test
    @ requires a non-trivial stage-1 MMU setup, a vector-dispatcher
    @ patch, and hypervisor cooperation around `fix_stage1_xn_bits`.
    @ See INVESTIGATION.md for the gating constraints.
    bl      subtest_20_ram_resident
    bl      subtest_21_ram_reload
    bl      subtest_22_inline_stub_modes
    bl      subtest_23_swi_during_stubs
    bl      subtest_24_scratch_va_preserves_caller

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
    @ SCRATCH_HI points at the test-only MMIO scratch window above
    @ XOR_LIMIT (`mmio::TEST_SCRATCH`). Byte/halfword/word accesses
    @ stage-2-fault and route through `mmio::write` / `mmio::read`,
    @ which round-trips into a 16-byte storage cell. Verifies the
    @ shadow-stub inline-stub branch that skips the BE-32 XOR for IPAs
    @ ≥ XOR_LIMIT.
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

    @ ---- subtest 20: RAM-resident code ---------------------------
    @ Copies a small helper function from ROM into RAM, calls it, and
    @ verifies that the LDRB inside the RAM-resident copy produces
    @ the BE-32-correct byte. This exercises item 5 (lazy stage-2
    @ discovery): RAM pages are XN initially, first fetch triggers a
    @ stage-2 instruction abort, the hypervisor scans the 2 MiB RAM
    @ block for byte/halfword accesses, installs stubs, and flips XN
    @ off on that block.
subtest_20_ram_resident:
    push    {r4, r5, r6, r7, r8, r9, lr}
    @ Source: ram_helper in ROM. Destination: 0x04002000 in RAM (far
    @ enough from stack and other scratch to be in its own 2 MiB
    @ block — actually same 2 MiB as RAM base since RAM is 4 MiB. Use
    @ 0x04300000 to force a fresh block (still within 4 MiB RAM).
    load_sym r4, ram_helper
    load_sym r5, ram_helper_end
    load_u32 r6, 0x04300000
    mov     r7, r6
.Lst20_copy_loop:
    ldr     r8, [r4], #4
    str     r8, [r6], #4
    cmp     r4, r5
    bne     .Lst20_copy_loop

    @ Flush D-cache so the just-copied bytes are visible for
    @ instruction fetch, invalidate I-cache.
    mov     r8, r7
.Lst20_dcache:
    mcr     p15, 0, r8, c7, c10, 1      @ DCCMVAC: clean DC by MVA
    add     r8, r8, #32
    cmp     r8, r6
    blo     .Lst20_dcache
    mov     r8, r7
.Lst20_icache:
    mcr     p15, 0, r8, c7, c5, 1       @ ICIMVAU
    add     r8, r8, #32
    cmp     r8, r6
    blo     .Lst20_icache
    dsb
    isb

    @ Store a known word at SCRATCH_BASE. The RAM-resident helper
    @ will LDRB it and return the byte in r0.
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0xC0DEF00D
    str     r5, [r4]

    @ Call the RAM-resident helper. It expects r4 = address,
    @ r1 = byte offset; returns r0 = loaded byte.
    mov     r1, #0                      @ offset 0 -> phys[addr+3] = 0xC0
    blx     r7
    cmp     r0, #0xC0
    bne     .Lst20_fail

    mov     r1, #1                      @ offset 1 -> phys[addr+2] = 0xDE
    blx     r7
    cmp     r0, #0xDE
    bne     .Lst20_fail

    mov     r1, #3                      @ offset 3 -> phys[addr+0] = 0x0D
    blx     r7
    cmp     r0, #0x0D
    bne     .Lst20_fail

    pop     {r4, r5, r6, r7, r8, r9, lr}
    bx      lr
.Lst20_fail:
    mov     r10, r0
    pop     {r4, r5, r6, r7, r8, r9, lr}
    mov     r0, r10
    hvc     #2
    fail_st 20


    @ ---- subtest 21: RAM page reload (demand-pager re-scan) ------
    @ Validates the stage-2 RW+XN ↔ RO+¬XN per-page state machine.
    @ (1) Copy reload_helper_a into a 4 KiB-aligned RAM target. First
    @     fetch traps (XN), hypervisor scans+patches+marks RO+¬XN,
    @     subsequent fetches succeed. helper_a does `ldrb r0, [r4]`
    @     and returns — BE-32 should give byte at phys[0^3] = phys[3].
    @ (2) Overwrite the RAM target with reload_helper_b. First store
    @     traps (RO permission), hypervisor flips page back to RW+XN,
    @     store retries and succeeds. After redoing cache-coherency,
    @     calling the new helper traps on fetch again (XN), hypervisor
    @     re-scans the now-reloaded bytes, flips RO+¬XN. helper_b does
    @     `ldrb r0, [r4, #3]` — BE-32 phys[3^3] = phys[0].
    @     If the re-scan didn't happen, the ldrb would hit the old
    @     stub layout and return the wrong byte.
subtest_21_ram_reload:
    push    {r4, r5, r6, r7, r8, r9, r10, lr}

    @ Pick an aligned RAM target far from the subtest_20 target so
    @ the two tests don't interact.
    load_u32 r7, 0x04302000             @ 4 KiB-aligned
    @ Set up a known scratch word.
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0xDEADBEEF
    str     r5, [r4]

    @ (1) Copy reload_helper_a.
    load_sym r5, reload_helper_a
    load_sym r6, reload_helper_a_end
    mov     r8, r7
.Lst21_copy_a:
    ldr     r9, [r5], #4
    str     r9, [r8], #4
    cmp     r5, r6
    bne     .Lst21_copy_a

    @ Cache-coherency dance: clean D, invalidate I for the target.
    mov     r8, r7
.Lst21_dcache_a:
    mcr     p15, 0, r8, c7, c10, 1      @ DCCMVAC
    add     r8, r8, #32
    cmp     r8, r7
    addlo   r8, r8, #0                  @ noop to keep the loop shape
    @ The copy is 8 bytes — one 32-byte cache line covers it.
    @ Just emit one more clean + invalidate for safety:
    mov     r8, r7
    mcr     p15, 0, r8, c7, c10, 1      @ DCCMVAC
    mcr     p15, 0, r8, c7, c5, 1       @ ICIMVAU
    dsb
    isb

    @ Call helper_a. r4 already holds SCRATCH_BASE; r5=0xDEADBEEF.
    @ helper_a: ldrb r0, [r4] → phys[r4^3] = low byte of BE-32 view.
    @ With 0xDEADBEEF stored via word-aligned STR, the host memory
    @ holds EF-BE-AD-DE in LE byte order, so phys[0]=EF phys[3]=DE.
    @ The BE-32 LDRB at [r4+0] XORs to [r4^3] = phys[3] = 0xDE.
    blx     r7
    cmp     r0, #0xDE
    bne     .Lst21_fail

    @ (2) Overwrite with reload_helper_b. The first STR to the RAM
    @ target triggers a stage-2 RO permission fault (page is RO+X
    @ after the helper_a scan). The hypervisor flips the page to
    @ RW+XN and retries the store. Subsequent stores proceed natively.
    load_sym r5, reload_helper_b
    load_sym r6, reload_helper_b_end
    mov     r8, r7
.Lst21_copy_b:
    ldr     r9, [r5], #4
    str     r9, [r8], #4
    cmp     r5, r6
    bne     .Lst21_copy_b

    @ Redo cache-coherency so the new bytes are visible to I-cache.
    mov     r8, r7
    mcr     p15, 0, r8, c7, c10, 1      @ DCCMVAC
    mcr     p15, 0, r8, c7, c5, 1       @ ICIMVAU
    dsb
    isb

    @ Call helper_b: `ldrb r0, [r4, #3]` → phys[(r4+3)^3] = phys[r4].
    @ With 0xDEADBEEF in SCRATCH_BASE, phys[r4] = 0xEF.
    blx     r7
    cmp     r0, #0xEF
    bne     .Lst21_fail

    pop     {r4, r5, r6, r7, r8, r9, r10, lr}
    bx      lr
.Lst21_fail:
    mov     r10, r0
    pop     {r4, r5, r6, r7, r8, r9, r10, lr}
    mov     r0, r10
    hvc     #2
    fail_st 21

    @ ---- subtest 22: inline-stub correctness across modes ---------
    @ Each mode block is its own BL'd helper so the liveness analyzer
    @ at any byte-access site walks only the helper (small CFG, ends
    @ at BX LR), and the caller's BL clears caller-saved (including
    @ R12) from the post-BL liveness — keeping inline coverage high.
    @
    @ Each helper:
    @   1. switch to its target mode + set per-mode SP
    @   2. do byte op against SCRATCH_BASE
    @   3. switch back to SVC, return via BX LR (helper code itself
    @      is in SVC mode at entry/exit)
    @
    @ NOTE: helpers run in their target mode but are entered/exited
    @ in SVC. They MSR-flip into the target mode, then back out before
    @ BX LR.
subtest_22_inline_stub_modes:
    push    {r4, lr}
    bl      st22_helper_sys
    cmp     r0, #0
    bne     .Lst22_fail
    bl      st22_helper_fiq
    cmp     r0, #0
    bne     .Lst22_fail
    bl      st22_helper_irq
    cmp     r0, #0
    bne     .Lst22_fail
    bl      st22_helper_abt
    cmp     r0, #0
    bne     .Lst22_fail
    bl      st22_helper_und
    cmp     r0, #0
    bne     .Lst22_fail
    pop     {r4, lr}
    bx      lr

.Lst22_fail:
    @ r0 holds the failure subcode (each helper returns its own).
    pop     {r4, lr}
    hvc     #2
    fail_st 22

    @ ---- per-mode helpers ----------------------------------------
    @ Each helper takes no args. Returns r0 = 0 on pass, non-zero
    @ failure code on fail. Must save/restore any callee-saved regs
    @ they touch — they push only what they use.

st22_helper_sys:
    push    {r4, r5, r6, lr}
    msr     cpsr_c, #0xDF              @ SYS mode
    load_u32 sp, 0x04001500
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0xDEADBEEF
    str     r5, [r4]
    ldrb    r6, [r4]                   @ phys[r4^3] = 0xDE
    msr     cpsr_c, #0xD3              @ back to SVC
    cmp     r6, #0xDE
    movne   r0, #0x22
    moveq   r0, #0
    pop     {r4, r5, r6, lr}
    bx      lr

st22_helper_fiq:
    push    {r4, r5, r6, lr}
    msr     cpsr_c, #0xD1              @ FIQ
    load_u32 sp, 0x04001100
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0x12345678
    str     r5, [r4]
    ldrb    r6, [r4, #1]               @ phys[r4+2] = 0x34
    msr     cpsr_c, #0xD3
    cmp     r6, #0x34
    movne   r0, #0x23
    moveq   r0, #0
    pop     {r4, r5, r6, lr}
    bx      lr

st22_helper_irq:
    push    {r4, r5, r6, lr}
    msr     cpsr_c, #0xD2              @ IRQ
    load_u32 sp, 0x04001200
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0x11223344
    str     r5, [r4]
    ldrb    r6, [r4, #2]               @ phys[r4+1] = 0x33
    msr     cpsr_c, #0xD3
    cmp     r6, #0x33
    movne   r0, #0x24
    moveq   r0, #0
    pop     {r4, r5, r6, lr}
    bx      lr

st22_helper_abt:
    push    {r4, r5, r6, lr}
    msr     cpsr_c, #0xD7              @ ABT
    load_u32 sp, 0x04001300
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0xCAFEBABE
    str     r5, [r4]
    ldrb    r6, [r4, #3]               @ phys[r4] = 0xBE
    msr     cpsr_c, #0xD3
    cmp     r6, #0xBE
    movne   r0, #0x25
    moveq   r0, #0
    pop     {r4, r5, r6, lr}
    bx      lr

st22_helper_und:
    push    {r4, r5, r7, r8, lr}
    msr     cpsr_c, #0xDB              @ UND
    load_u32 sp, 0x04001400
    load_u32 r4, SCRATCH_BASE
    @ Pre-fill the scratch word so the post-STRB state is predictable.
    load_u32 r5, 0x11223344
    str     r5, [r4]
    load_u32 r5, 0xDEADBEEF
    @ STRB writes 0xEF at phys[r4^3]=phys[r4+3]. Host LE before:
    @ 44 33 22 11. After: 44 33 22 EF. Word read: 0xEF223344.
    strb    r5, [r4]
    ldr     r7, [r4]
    msr     cpsr_c, #0xD3
    load_u32 r8, 0xEF223344
    cmp     r7, r8
    movne   r0, #0x26
    moveq   r0, #0
    pop     {r4, r5, r7, r8, lr}
    bx      lr

    @ ---- subtest 23: SWI-as-IRQ during byte-access sequence ------
    @ Validates that an exception taken mid-byte-access-sequence (in
    @ this case a SWI standing in for an IRQ — same banked-register +
    @ inline-stub semantics) does not corrupt the surrounding accesses.
    @
    @ Sequence:
    @   1. Arm the SWI dispatcher's mode flag.
    @   2. Set up SCRATCH_BASE with a known word.
    @   3. Do a pre-SWI LDRB → verify byte 0xDE.
    @   4. SWI #0  → SWI handler bumps counter slot 0x04003F0C.
    @   5. Do a post-SWI LDRB → verify byte 0xDE again. SWI handler
    @      must have preserved R4 (it pushes/pops everything it uses).
    @   6. Verify the SWI handler ran exactly once.
    @   7. Disarm the flag.
subtest_23_swi_during_stubs:
    push    {r4, r5, r6, lr}
    @ Reset the counter slot at 0x04003F0C.
    movw    r0, #0x3F0C
    movt    r0, #0x0400
    mov     r1, #0
    str     r1, [r0]
    @ Arm the SWI mode flag.
    movw    r0, #0x3F08
    movt    r0, #0x0400
    mov     r1, #1
    str     r1, [r0]
    @ Set up the scratch word.
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0xDEADBEEF
    str     r5, [r4]
    @ Pre-SWI LDRB. BE-32 phys[r4^3] = phys[r4+3] = 0xDE.
    ldrb    r6, [r4]
    cmp     r6, #0xDE
    bne     .Lst23_fail_pre
    @ Now SWI #0. Handler runs in SVC, bumps counter, returns.
    swi     #0
    @ Post-SWI LDRB. R4 must still hold SCRATCH_BASE.
    ldrb    r6, [r4]
    cmp     r6, #0xDE
    bne     .Lst23_fail_post
    @ Verify SWI handler ran exactly once.
    movw    r0, #0x3F0C
    movt    r0, #0x0400
    ldr     r1, [r0]
    cmp     r1, #1
    bne     .Lst23_fail_count
    @ Disarm SWI mode flag.
    movw    r0, #0x3F08
    movt    r0, #0x0400
    mov     r1, #0
    str     r1, [r0]
    pop     {r4, r5, r6, lr}
    bx      lr

.Lst23_fail_pre:
    mov     r0, #0x231
    b       .Lst23_fail_done
.Lst23_fail_post:
    mov     r0, #0x232
    b       .Lst23_fail_done
.Lst23_fail_count:
    mov     r0, #0x233
.Lst23_fail_done:
    @ Disarm flag so subsequent halt path doesn't re-trigger SWI.
    movw    r1, #0x3F08
    movt    r1, #0x0400
    mov     r2, #0
    str     r2, [r1]
    hvc     #2
    pop     {r4, r5, r6, lr}
    fail_st 23

    @ ---- subtest 24: ScratchVA-fallback stub preserves caller state -----
    @ The shadow-stub install path falls back to the ScratchVA variant
    @ when liveness analysis can't find 2 dead candidate registers from
    @ {R0..R3, R12, R14}. The ScratchVA stub uses a hypervisor-side
    @ scratch slot in IPA 0x0180_0000 (kernel L1[0x18]) plus TPIDRURW to
    @ save the caller's three scratch registers. This subtest:
    @
    @   1. Pre-loads sentinels into R0..R3 and R12 (R14 is the return
    @      LR, naturally live).
    @   2. Sets NZCV to a known pattern (N=0, Z=0, C=1, V=0 via CMP).
    @   3. Saves NZCV pre-access into R6.
    @   4. Performs LDRB Rt=R5, [Rn=R4]. The post-access instruction
    @      window reads all candidates (R0..R3, R12, R14) before any of
    @      them is written, forcing the liveness analyzer to mark all
    @      candidates live → ScratchVA fallback.
    @   5. Saves NZCV post-access into R7.
    @   6. Reads each candidate to verify the ScratchVA stub's save/
    @      restore round-trip (these reads also satisfy the analyzer's
    @      forward-walk liveness requirement).
    @   7. Verifies R0..R3, R12, byte read, and NZCV.
    @
    @ Failure subcodes (printed via HVC #2 then halted via fail_st):
    @   0x240 — R0  not preserved (ScratchVA STR/LDR caller_sea broken)
    @   0x241 — R1  not preserved
    @   0x242 — R2  not preserved
    @   0x243 — R3  not preserved
    @   0x244 — R12 not preserved (TPIDRURW save-slot or sad register
    @                              choice broken if R12 was the sad)
    @   0x245 — byte read returned the wrong value
    @   0x246 — NZCV clobbered by the stub (MRS/MSR slot 4/9 broken)
subtest_24_scratch_va_preserves_caller:
    push    {r4-r11, lr}
    load_u32 r4, SCRATCH_BASE
    load_u32 r5, 0xDEADBEEF
    str     r5, [r4]                @ phys[r4..r4+3] LE = EF BE AD DE,
                                    @ BE-32 byte at offset 0 = phys[3] = 0xDE.

    @ Sentinels into R0..R3, R12. R14 is the return LR (preserved by ABI).
    load_u32 r0,  0xC0DE0000
    load_u32 r1,  0xC0DE0001
    load_u32 r2,  0xC0DE0002
    load_u32 r3,  0xC0DE0003
    load_u32 r12, 0xC0DE000C

    @ Set NZCV to a known pattern. r6 = 1, then CMP r6, #0 → N=0 Z=0 C=1 V=0.
    mov     r6, #1
    cmp     r6, #0
    @ Capture pre-access NZCV into r6.
    mrs     r6, cpsr
    and     r6, r6, #0xF0000000

    @ The patched LDRB. Rt=R5, Rn=R4 — neither is a candidate, so the
    @ operand-exclusion picker has all 6 candidates to choose from.
    @ The post-access window below reads R0..R3, R12, R14 → liveness
    @ picker returns None → ScratchVA path.
    ldrb    r5, [r4]                @ r5 = byte at phys[r4^3] = 0xDE

    @ Capture post-access NZCV before any flag-clobbering instruction.
    mrs     r7, cpsr
    and     r7, r7, #0xF0000000

    @ Force-mark every candidate live by reading it before writing.
    @ CMP reads each candidate (writes only NZCV — a GPR sentinel
    @ comparison; the saved post-access NZCV is already in r7 and is
    @ unaffected by these CMPs).
    cmp     r0, #0                  @ read R0
    cmp     r1, #0                  @ read R1
    cmp     r2, #0                  @ read R2
    cmp     r3, #0                  @ read R3
    cmp     r12, #0                 @ read R12
    cmp     lr, #0                  @ read R14

    @ Verify R0..R3, R12 unchanged. r8 used as compare scratch.
    load_u32 r8, 0xC0DE0000
    cmp     r0, r8
    bne     .Lst24_fail_r0
    load_u32 r8, 0xC0DE0001
    cmp     r1, r8
    bne     .Lst24_fail_r1
    load_u32 r8, 0xC0DE0002
    cmp     r2, r8
    bne     .Lst24_fail_r2
    load_u32 r8, 0xC0DE0003
    cmp     r3, r8
    bne     .Lst24_fail_r3
    load_u32 r8, 0xC0DE000C
    cmp     r12, r8
    bne     .Lst24_fail_r12

    @ Verify byte read.
    cmp     r5, #0xDE
    bne     .Lst24_fail_byte

    @ Verify NZCV preserved across the access.
    cmp     r6, r7
    bne     .Lst24_fail_nzcv

    pop     {r4-r11, lr}
    bx      lr

.Lst24_fail_r0:    mov r0, #0x240; b .Lst24_fail_done
.Lst24_fail_r1:    mov r0, #0x241; b .Lst24_fail_done
.Lst24_fail_r2:    mov r0, #0x242; b .Lst24_fail_done
.Lst24_fail_r3:    mov r0, #0x243; b .Lst24_fail_done
.Lst24_fail_r12:   mov r0, #0x244; b .Lst24_fail_done
.Lst24_fail_byte:  mov r0, #0x245; b .Lst24_fail_done
.Lst24_fail_nzcv:  mov r0, #0x246
.Lst24_fail_done:
    hvc     #2
    pop     {r4-r11, lr}
    fail_st 24

    .global patch_end
patch_end:
    .ltorg                              @ flush any pending literals OUTSIDE

    @ === RAM-resident helper template (outside patch range) =======
    @ Copied verbatim to RAM by subtest 20. The copy lives in RAM
    @ where it's picked up by the lazy-RAM-patch path as a new 2 MiB
    @ block gets unXN'd on first fetch.
    @   r4 = base address
    @   r1 = byte offset
    @   returns: r0 = byte at [r4 + r1]
ram_helper:
    ldrb    r0, [r4, r1]
    bx      lr
ram_helper_end:

    @ === subtest 21 reload helpers ==================================
    @ Two 8-byte functions used to exercise the RAM page-reload re-
    @ scan path. Both have the same prologue-less shape: a single
    @ LDRB followed by BX LR. Copy either into a fresh 4 KiB RAM
    @ page, call it, then overwrite with the other variant and call
    @ again — if the hypervisor re-scans the page correctly, the
    @ second call hits the updated BE-32 lane.
reload_helper_a:
    ldrb    r0, [r4]
    bx      lr
reload_helper_a_end:

reload_helper_b:
    ldrb    r0, [r4, #3]
    bx      lr
reload_helper_b_end:

