@ tests/test_und_handler.S
@
@ Verifies the EL2 undefined-instruction handler dispatches the Newton
@ DDK UND opcodes correctly:
@   - 0xE6000010 SystemBootUND    (inline NOP marker; ELR += 4)
@   - 0xE6000510 DebuggerUND      (inline + null-terminated ASCII payload;
@                                  ELR advances past the aligned message)
@   - 0xE6000810 TapFileCntlUND   (function-entry UND, called via BL;
@                                  handler returns to the caller's LR
@                                  — matches Einstein's JIT
@                                  `POPNIL(); SETPC(GETCALLER()+4)`.)
@
@ Trampoline contract (matches src/trap.rs::handle_und):
@   - UND vector at VA 0x04 branches to our stub.
@   - Stub runs in UND mode. It stages:
@       [UND_SAVE_LR]         <- R14_und (= faulting_pc + 4)
@       [UND_SAVE_SPSR]       <- SPSR_und (= pre-UND CPSR)
@       [UND_SAVE_BANKED_LR]  <- LR of the faulting mode (the caller's
@                                return address for BL-invoked UNDs)
@     The last one is mandatory for the DDK-UND dispatch; the
@     hypervisor halts if the slot is 0 rather than silently advancing.
@   - Stub then issues `hvc #0x10` (UND_TAG).
@
@ Not tested here: SWP / SWPB. A53 under QEMU executes SWP natively
@ regardless of SCTLR.SW, so the hypervisor's emulator is bypassed on
@ this host; it stays in place for ROM-mode SWPs that do trap (or for
@ alternative hosts). The emulator's encoding decode is exercised by
@ inspection.

    .syntax unified
    .arch armv7ve

    @ Save slots must match src/trap.rs UND_SAVE_* IPAs.
    .equ UND_SAVE_LR_ADDR,         0x04005F00
    .equ UND_SAVE_SPSR_ADDR,       0x04005F04
    .equ UND_SAVE_BANKED_LR_ADDR,  0x04005F1C

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
    @ Runs in UND mode with R14_und = faulting_pc + 4, SPSR_und = pre-
    @ UND CPSR. Replicates the ROM trampoline's mode-switch dance
    @ (src/guest_mem.rs::patch_und_vector) so the hypervisor's
    @ handle_und sees banked state populated regardless of source
    @ mode. The hypervisor reads the slots via `read_word_pa` and
    @ ERETs us back to the original mode at the address the handler
    @ chose (for DDK UNDs, the faulting mode's LR = caller's return
    @ address).
_und_trampoline:
    push    {r0, r1}
    ldr     r0, =UND_SAVE_LR_ADDR
    str     lr, [r0]
    mrs     r1, spsr
    ldr     r0, =UND_SAVE_SPSR_ADDR
    str     r1, [r0]

    @ Capture the faulting mode's banked LR into UND_SAVE_BANKED_LR.
    @ Extract the mode field of SPSR and switch to it (or SYS when the
    @ source was USR, since USR/SYS share the same bank). Reading LR
    @ in that mode yields the pre-UND banked LR; store and switch back.
    and     r1, r1, #0x1F        @ r1 = SPSR.mode
    orr     r1, r1, #0xC0        @ r1 |= I|F mask (needed by MSR CPSR_c)
    cmp     r1, #0xD0            @ USR (0x10) + IF ?
    moveq   r1, #0xDF            @ → SYS (0x1F) + IF (shares USR's R13/R14 bank)
    msr     cpsr_c, r1           @ switch to faulting mode
    ldr     r0, =UND_SAVE_BANKED_LR_ADDR
    str     lr, [r0]             @ [UND_SAVE_BANKED_LR] = LR of the source mode
    msr     cpsr_c, #0xDB        @ back to UND, I=1 F=1

    pop     {r0, r1}
    hvc     #0x10                @ UND_TAG — hypervisor decodes and ERETs
    b       .Lhang               @ unreachable

test_main:
    @ ---- SystemBootUND (0xE6000010) -------------------------------
    @ Inline NOP marker: handler advances ELR by 4.
    .word   0xE6000010
    mov     r6, #0x42
    cmp     r6, #0x42
    bne     .Lfail_boot_und

    mov     r0, #1
    hvc     #5                  @ mark 1: SystemBootUND dispatch ok

    @ ---- DebuggerUND (0xE6000510) + null-terminated ASCII payload ---
    @ Inline marker + message. Handler scans from opcode+4 for a NUL
    @ byte and resumes at the next word boundary past the terminator.
    .word   0xE6000510
    .asciz  "dbg"
    .balign 4
    mov     r6, #0x55
    cmp     r6, #0x55
    bne     .Lfail_dbg_und

    mov     r0, #2
    hvc     #5                  @ mark 2: DebuggerUND dispatch ok

    @ ---- TapFileCntl (0xE6000810) — BL-invoked function entry ----
    @ The UND lives at a named ROM symbol (the kernel's BL target).
    @ Handler emulates the file-protocol dispatch — here we call with
    @ command=0 (unsupported), so the handler writes -1 to R0 as the
    @ "call failed" result — and returns to the caller's banked LR.
    @ Execution resumes at the instruction after the BL, which is
    @ exactly how the Newton kernel invokes this.
    mov     r0, #0                @ command = 0 (unsupported)
    bl      tap_file_cntl_stub
    @ <-- handler should return here (LR = address of next insn)
    cmn     r0, #1                @ r0 should be 0xFFFFFFFF (= -1)
    bne     .Lfail_tap_und

    mov     r6, #0x77
    cmp     r6, #0x77
    bne     .Lfail_tap_und

    mov     r0, #3
    hvc     #5                  @ mark 3: TapFileCntl dispatch ok

    hvc     #3                  @ PASS
    bx      lr

tap_file_cntl_stub:
    @ Single-word function body, called via BL above. On entry LR =
    @ address of the `cmn r0, #1` instruction.
    .word   0xE6000810
    b       .                    @ handler must return to LR, not fall through

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
