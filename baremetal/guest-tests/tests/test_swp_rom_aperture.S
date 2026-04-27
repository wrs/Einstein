@ tests/test_swp_rom_aperture.S
@
@ Verify the hypervisor mirrors Einstein's TMemory::WriteP behaviour
@ for stage-2 permission faults that target the ROM aperture (IPA <
@ 0x01000000): the load piece runs, the store piece is silently
@ dropped, no fault is delivered to the guest. See trap.rs
@ try_absorb_rom_write — without that path, the Newton kernel's
@ Acquire(NULL) glue in TULockingSemaphore aborts on `Swap(0, 1)`.
@
@ Mechanism: stage-1 MMU stays off, so VA == IPA. SWP r0, r1, [r0]
@ with r0 in the ROM aperture (we use offset 0, the reset vector)
@ produces a stage-2 perm fault (ROM is RW=0 in stage-2). The
@ hypervisor decodes the SWP, reads ROM[0] into Rd, drops the store,
@ and ERETs past the instruction.
@
@ Pass criteria:
@   1. SWP returns ROM[0] in Rd.
@   2. ROM[0] is unchanged after the SWP (re-read via LDR matches).
@   3. SWPB returns ROM[0] (byte) in Rd.
@   4. The data-abort vector is NOT taken.
@
@ ROM[0] under our BE→LE byteswap is the kernel's reset-branch
@ instruction (0xea0061a0 per guest_mem.rs boot log). We don't pin
@ the exact value — we just compare against a fresh LDR of ROM[0].

#include "../common/test_runtime.S"

    @ Override the runtime's data-abort handler so we can assert the
    @ SWP completes WITHOUT taking an abort. If we ever vector here,
    @ raise a distinct fail code.
    .equ ABORT_TAKEN_ADDR, 0x04000200

    .global test_main
test_main:
    @ Snapshot ROM[0] via a regular LDR — this read goes through
    @ stage-2 ROM mapping (RO, but reads succeed).
    mov     r0, #0
    ldr     r4, [r0]                 @ r4 = ROM[0] (expected SWP result)

    @ Zero the abort-taken sentinel; the runtime's .Lhalt_dabt
    @ branches to .Lhang on a real abort, so this slot only matters
    @ if we route through a custom vector. (We don't — see notes.)
    ldr     r0, =ABORT_TAKEN_ADDR
    mov     r1, #0
    str     r1, [r0]

    @ === Subtest 1: word SWP to ROM[0] =========================
    @ swp r2, r3, [r0]: load [0]→r2, store r3→[0] (dropped).
    mov     r0, #0
    ldr     r3, =0xDEADBEEF          @ value we try (and fail) to write
    swp     r2, r3, [r0]             @ r2 = ROM[0], [0] unchanged

    @ r2 must equal the LDR snapshot in r4.
    cmp     r2, r4
    bne     .Lfail_word_load

    @ ROM[0] must still equal r4 (write was dropped).
    mov     r0, #0
    ldr     r5, [r0]
    cmp     r5, r4
    bne     .Lfail_word_dropped

    @ === Subtest 2: SWP r0, r1, [r0] (Rn == Rd, the kernel's pattern) ====
    @ This is the exact instruction in `Swap` at ROM 0x3ae204 that
    @ the Newton kernel calls with r0=NULL. Per ARM ARM Rd==Rn is
    @ UNPREDICTABLE, so gas rejects the mnemonic — encode the word
    @ directly. e1000091 = swp r0, r1, [r0]. Einstein's JIT (and our
    @ absorber) compute the address from Rn first, then load → Rd,
    @ so r0 ends up with ROM[0]; the original r0 (=0) is gone but
    @ that's intentional.
    mov     r0, #0
    ldr     r1, =0x12345678
    .word   0xe1000091               @ swp r0, r1, [r0]
    cmp     r0, r4
    bne     .Lfail_alias_load

    @ === Subtest 3: byte SWPB to ROM[0] ===============================
    @ Single-byte form. ROM[0] low byte is the LSB of r4; on little-
    @ endian guest that's r4 & 0xFF.
    and     r5, r4, #0xFF
    mov     r0, #0
    mov     r3, #0x99
    swpb    r2, r3, [r0]             @ r2 = ROM[0]&0xFF, [0] unchanged
    cmp     r2, r5
    bne     .Lfail_byte_load

    @ === Subtest 4: SWP at a non-zero ROM aperture address ============
    @ Use ROM[0x100] just to prove the absorber doesn't depend on the
    @ address being literally NULL. Read once via LDR, once via SWP.
    ldr     r0, =0x100
    ldr     r4, [r0]
    mov     r0, #0
    add     r0, r0, #0x100
    ldr     r3, =0xCAFEBABE
    swp     r2, r3, [r0]
    cmp     r2, r4
    bne     .Lfail_offset_load

    @ All subtests passed.
    hvc_pass
    bx      lr

.Lfail_word_load:
    hvc_hex32 r2
    hvc_hex32 r4
    hvc_fail 0x01

.Lfail_word_dropped:
    hvc_hex32 r5
    hvc_hex32 r4
    hvc_fail 0x02

.Lfail_alias_load:
    hvc_hex32 r0
    hvc_hex32 r4
    hvc_fail 0x03

.Lfail_byte_load:
    hvc_hex32 r2
    hvc_hex32 r5
    hvc_fail 0x04

.Lfail_offset_load:
    hvc_hex32 r2
    hvc_hex32 r4
    hvc_fail 0x05
