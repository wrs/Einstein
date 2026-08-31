@ tests/test_rotate_ldr.S
@
@ Exercises the hypervisor's SA-1100 rotate-LDR emulator
@ (src/unaligned.rs). With SCTLR.A=1 every unaligned AArch32 LDR/STR
@ raises a stage-1 alignment fault at EL1. The test installs a DABT
@ handler at VA 0x10 that matches the hypervisor's production
@ trampoline shape (src/guest_mem.rs::patch_dabt_vector) — save
@ R0 → TPIDRURW, R1 → TPIDRRO, and LR / SP / SPSR to the DABT_SAVE
@ slot at 0x0600_00A0 (`guest_trampolines::DABT_SAVE_PA`, in the
@ identity-mapped scratch pool) — then HVCs `#ALIGN_TAG` (0x17) to
@ enter EL2.
@ The EL2 emulator decodes the faulting LDR, performs the aligned
@ word load + rotate-right, writes the result to ctx.x[Rt], and ERETs
@ past the faulting insn.
@
@ Checks (source word = 0x11223344 at 0x04010000):
@   offset +1:  expect 0x44112233  (ROR 8)
@   offset +2:  expect 0x33441122  (ROR 16)
@   offset +3:  expect 0x22334411  (ROR 24)
@   offset +0 (aligned): unchanged 0x11223344
@
@ We don't rely on the hypervisor's own patch_dabt_vector — that
@ would collide with test_cp15_fault_regs' handler. The handler is
@ fully self-contained.

    .syntax unified
    .arch armv7ve

#include "../common/hvc_abi.S"

    .equ DABT_SAVE_PA,  0x060000A0

    .equ SRC_ADDR,      0x04010000
    .equ SRC_WORD,      0x11223344

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
    @ SVC and ABT stacks (abort handler doesn't use SP but establish one
    @ so the banked-SP save path produces a sane value).
    msr     cpsr_c, #0xD7       @ ABT mode, I=1 F=1
    ldr     sp, =0x04003000
    msr     cpsr_c, #0xD3       @ SVC mode, I=1 F=1
    ldr     sp, =0x04008000

    @ SCTLR.A = 1 so unaligned LDR/STR raises an alignment fault. The
    @ hypervisor also ORs A=1 into every SCTLR write (see trap.rs), so
    @ this is belt-and-braces.
    mrc     p15, 0, r0, c1, c0, 0
    orr     r0, r0, #(1 << 1)
    mcr     p15, 0, r0, c1, c0, 0

    @ Seed the source word (aligned — no fault).
    ldr     r0, =SRC_ADDR
    ldr     r1, =SRC_WORD
    str     r1, [r0]

    bl      test_main
.Lhang:
    b       .Lhang

    @ === DABT trampoline (matches src/guest_mem.rs::patch_dabt_vector) ==
    @
    @ Persist R0/R1 to TPIDR scratch regs and LR/SP/SPSR to the DABT_SAVE
    @ area, then HVC #ALIGN_TAG. The hypervisor reads them back in
    @ `unaligned::handle_align_fault`. Deliberately identical shape so
    @ the test exercises the production EL2 path end-to-end.
_dabt_trampoline:
    mcr     p15, 0, r0,  c13, c0, 2  @ save R0  → TPIDRURW
    mcr     p15, 0, r1,  c13, c0, 3  @ save R1  → TPIDRRO
    ldr     r0, =DABT_SAVE_PA
    str     lr,  [r0]                @ LR_abt    +0x00
    str     sp,  [r0, #4]            @ SP_abt    +0x04
    mrs     r1,  spsr
    str     r1,  [r0, #8]            @ SPSR_abt  +0x08
    hvc     #HVC_ALIGN
    b       .                        @ guard (handler ERETs past the fault)

test_main:
    @ --- offset +1 (ROR 8) ------------------------------------------
    ldr     r0, =SRC_ADDR
    add     r0, r0, #1
    ldr     r2, [r0]
    ldr     r3, =0x44112233
    cmp     r2, r3
    bne     .Lfail_ror8

    mov     r0, #1
    hvc     #HVC_MARK

    @ --- offset +2 (ROR 16) -----------------------------------------
    ldr     r0, =SRC_ADDR
    add     r0, r0, #2
    ldr     r2, [r0]
    ldr     r3, =0x33441122
    cmp     r2, r3
    bne     .Lfail_ror16

    mov     r0, #2
    hvc     #HVC_MARK

    @ --- offset +3 (ROR 24) -----------------------------------------
    ldr     r0, =SRC_ADDR
    add     r0, r0, #3
    ldr     r2, [r0]
    ldr     r3, =0x22334411
    cmp     r2, r3
    bne     .Lfail_ror24

    mov     r0, #3
    hvc     #HVC_MARK

    @ --- offset +0 (aligned — should NOT fault) --------------------
    ldr     r0, =SRC_ADDR
    ldr     r2, [r0]
    ldr     r3, =SRC_WORD
    cmp     r2, r3
    bne     .Lfail_aligned

    mov     r0, #4
    hvc     #HVC_MARK

    hvc     #HVC_PASS                  @ PASS
    bx      lr

    @ === Failure paths ==============================================
.Lfail_ror8:
    mov     r0, r2
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x70
    hvc     #HVC_FAIL
.Lfail_ror16:
    mov     r0, r2
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x71
    hvc     #HVC_FAIL
.Lfail_ror24:
    mov     r0, r2
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x72
    hvc     #HVC_FAIL
.Lfail_aligned:
    mov     r0, r2
    hvc     #HVC_PRINT_HEX
    mov     r0, #0x73
    hvc     #HVC_FAIL
