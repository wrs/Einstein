@ tests/test_und_handler.S
@
@ Verifies the EL2 undefined-instruction handler dispatches the three
@ Einstein UND opcodes correctly:
@   - 0xE6000010 SystemBootUND   (ELR += 8)
@   - 0xE6000510 DebuggerUND     (ELR += 8, hypervisor logs payload)
@   - 0xE6000810 TapFileCntlUND  (ELR += 8, hypervisor logs payload)
@
@ Not tested here: SWP / SWPB. A53 under QEMU executes SWP natively
@ regardless of SCTLR.SW, so the hypervisor's emulator is bypassed on
@ this host; it stays in place for ROM-mode SWPs that do trap (or for
@ alternative hosts). The emulator's encoding decode is exercised by
@ inspection.
@
@ Trampoline contract (matches src/trap.rs::handle_und):
@   - UND vector at VA 0x04 branches to our stub.
@   - Stub runs in UND mode, writes R14_und to 0x04000400 and SPSR
@     to 0x04000404, then issues `hvc #0x10` (UND_TAG).
@   - Hypervisor reads those RAM slots, decodes, emulates, and ERETs
@     us back to the original mode at the address past the UND.

    .syntax unified
    .arch armv7ve

    @ UND save slots live in the RAM-mirror-accessible portion of RAM,
    @ not at IPA 0x04000400 (which overlaps the Newton kernel's L1 table
    @ at PA 0x04000000 — writing there corrupts the page table once the
    @ ROM installs it). Match src/trap.rs UND_SAVE_LR_IPA / _SPSR_IPA.
    .equ UND_SAVE_LR_ADDR,   0x04005F00
    .equ UND_SAVE_SPSR_ADDR, 0x04005F04

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
    msr     cpsr_c, #0xDB       @ UND, I=1 F=1
    ldr     sp, =0x04006000
    msr     cpsr_c, #0xD3       @ SVC, I=1 F=1
    ldr     sp, =0x04008000

    bl      test_main
.Lhang:
    b       .Lhang

    @ ---- UND trampoline -------------------------------------------
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
    b       .Lhang              @ unreachable

test_main:
    @ ---- SystemBootUND (0xE6000010) -------------------------------
    @ Hypervisor advances PC by 8 past (opcode + payload slot).
    .word   0xE6000010
    .word   0x11111111          @ payload slot — skipped by handler
    mov     r6, #0x42
    cmp     r6, #0x42
    bne     .Lfail_boot_und

    mov     r0, #1
    hvc     #5                  @ mark 1: SystemBootUND dispatch ok

    @ ---- DebuggerUND (0xE6000510) + payload ----------------------
    .word   0xE6000510
    .word   0x22334455          @ payload — hypervisor logs it
    mov     r6, #0x55
    cmp     r6, #0x55
    bne     .Lfail_dbg_und

    mov     r0, #2
    hvc     #5                  @ mark 2: DebuggerUND dispatch ok

    @ ---- TapFileCntlUND (0xE6000810) + payload -------------------
    .word   0xE6000810
    .word   0x66778899          @ payload — hypervisor logs it
    mov     r6, #0x77
    cmp     r6, #0x77
    bne     .Lfail_tap_und

    mov     r0, #3
    hvc     #5                  @ mark 3: TapFileCntlUND dispatch ok

    hvc     #3                  @ PASS
    bx      lr

    @ === Failure paths ============================================
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
