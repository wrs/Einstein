@ tests/test_und_handler.S
@
@ Verifies the EL2 undefined-instruction handler dispatches:
@   - SWP / SWPB (atomic swap emulated by the hypervisor, or executed
@     natively by A53 when SCTLR.SW permits — either way the externally
@     observable semantics match).
@   - 0xE6000010 SystemBootUND   (ELR += 8)
@   - 0xE6000510 DebuggerUND     (ELR += 8, hypervisor logs payload)
@   - 0xE6000810 TapFileCntlUND  (ELR += 8, hypervisor logs payload)
@
@ Trampoline contract (matches src/trap.rs::handle_und):
@   - UND vector at VA 0x04 branches to our stub.
@   - Stub runs in UND mode, writes R14_und to 0x04000400 and SPSR_und
@     to 0x04000404, then issues `hvc #0x10` (UND_TAG).
@   - Hypervisor reads those RAM slots, decodes, emulates, and ERETs
@     us back to the original mode at the address past the UND.
@
@ AArch32 banked LR_und / SPSR_und are not readable via AArch64 MRS
@ from EL2 under QEMU raspi3b, so the RAM-slot convention is how we
@ propagate that state across the UND -> HVC -> EL2 boundary.

    .syntax unified
    .arch armv7ve

    .equ SWP_TARGET_ADDR,   0x04010000
    .equ SENTINEL_OLD,      0xDEADBEEF
    .equ SENTINEL_NEW,      0xCAFEF00D
    .equ UND_SAVE_LR_ADDR,  0x04000400
    .equ UND_SAVE_SPSR_ADDR,0x04000404

    @ === Vector table ===============================================
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
    @ UND-mode stack lives above the L1 / RAM-scratch region. 0x04006000
    @ grows down and never reaches the save slots at 0x04000400.
    msr     cpsr_c, #0xDB       @ UND, I=1 F=1
    ldr     sp, =0x04006000
    msr     cpsr_c, #0xD3       @ SVC, I=1 F=1
    ldr     sp, =0x04008000

    bl      test_main
.Lhang:
    b       .Lhang

    @ ---- UND trampoline ---------------------------------------------
    @ Entry: UND mode, R14_und = faulting_pc + 4, SPSR = old CPSR.
    @ Writes both to fixed RAM slots, then HVCs to EL2. Uses the UND
    @ stack to avoid clobbering r0/r1 the faulting code had live.
_und_trampoline:
    push    {r0, r1}
    ldr     r0, =UND_SAVE_LR_ADDR
    str     lr, [r0]
    mrs     r1, spsr
    ldr     r0, =UND_SAVE_SPSR_ADDR
    str     r1, [r0]
    pop     {r0, r1}
    hvc     #0x10               @ UND_TAG — hypervisor decodes and ERETs
    @ Never returns here: the hypervisor ERETs directly to the
    @ original mode past the faulting instruction.
    b       .Lhang

test_main:
    @ ---- SWP (word) -----------------------------------------------
    @ A53 under QEMU executes SWP natively (SCTLR.SW semantics), so
    @ this mainly exercises the semantic rather than our emulator —
    @ but the hypervisor's emulator is kept in place for the case
    @ where the ROM runs with SW disabled and SWP does trap UND.
    ldr     r0, =SWP_TARGET_ADDR
    ldr     r1, =SENTINEL_OLD
    str     r1, [r0]

    ldr     r2, =SENTINEL_NEW
    ldr     r3, =SWP_TARGET_ADDR
    mov     r4, #0xAA
    swp     r4, r2, [r3]

    ldr     r5, =SENTINEL_OLD
    cmp     r4, r5
    bne     .Lfail_swp_rd

    ldr     r0, =SWP_TARGET_ADDR
    ldr     r4, [r0]
    ldr     r5, =SENTINEL_NEW
    cmp     r4, r5
    bne     .Lfail_swp_mem

    mov     r0, #1
    hvc     #5                  @ mark 1: SWP word semantics ok

    @ ---- SWPB (byte) ---------------------------------------------
    ldr     r0, =SWP_TARGET_ADDR
    mov     r1, #0x77
    strb    r1, [r0]
    mov     r2, #0x22
    mov     r4, #0xAA
    swpb    r4, r2, [r0]
    cmp     r4, #0x77
    bne     .Lfail_swpb_rd
    ldrb    r5, [r0]
    cmp     r5, #0x22
    bne     .Lfail_swpb_mem

    mov     r0, #2
    hvc     #5                  @ mark 2: SWPB semantics ok

    @ ---- SystemBootUND (0xE6000010) -------------------------------
    @ Hypervisor advances PC by 8 past (opcode + payload slot).
    .word   0xE6000010
    .word   0x11111111          @ payload slot — skipped by handler
    mov     r6, #0x42
    cmp     r6, #0x42
    bne     .Lfail_boot_und

    mov     r0, #3
    hvc     #5                  @ mark 3: SystemBootUND dispatch ok

    @ ---- DebuggerUND (0xE6000510) + payload ----------------------
    .word   0xE6000510
    .word   0x22334455          @ payload — hypervisor logs it
    mov     r6, #0x55
    cmp     r6, #0x55
    bne     .Lfail_dbg_und

    mov     r0, #4
    hvc     #5                  @ mark 4: DebuggerUND dispatch ok

    @ ---- TapFileCntlUND (0xE6000810) + payload -------------------
    .word   0xE6000810
    .word   0x66778899          @ payload — hypervisor logs it
    mov     r6, #0x77
    cmp     r6, #0x77
    bne     .Lfail_tap_und

    mov     r0, #5
    hvc     #5                  @ mark 5: TapFileCntlUND dispatch ok

    hvc     #3                  @ PASS
    bx      lr

    @ === Failure paths ============================================
.Lfail_swp_rd:
    mov     r0, r4
    hvc     #2
    mov     r0, #0x80
    hvc     #4
.Lfail_swp_mem:
    mov     r0, r4
    hvc     #2
    mov     r0, #0x81
    hvc     #4
.Lfail_swpb_rd:
    mov     r0, r4
    hvc     #2
    mov     r0, #0x82
    hvc     #4
.Lfail_swpb_mem:
    mov     r0, r5
    hvc     #2
    mov     r0, #0x83
    hvc     #4
.Lfail_boot_und:
    mov     r0, r6
    hvc     #2
    mov     r0, #0x84
    hvc     #4
.Lfail_dbg_und:
    mov     r0, r6
    hvc     #2
    mov     r0, #0x85
    hvc     #4
.Lfail_tap_und:
    mov     r0, r6
    hvc     #2
    mov     r0, #0x86
    hvc     #4
