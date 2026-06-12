@ tests/test_str_rom_aperture.S
@
@ Verify the hypervisor mirrors Einstein's TMemory::WriteP behaviour
@ for plain decodable stores (ISV=1) into the ROM aperture (IPA <
@ 0x01000000): the store is silently dropped, no fault is delivered
@ to the guest, execution continues past the instruction. See
@ trap/dabt.rs handle_data_abort — the Newton internal store's
@ KillBlock writes through a NULL free-list pointer (VA 0 → ROM page
@ 0 under a Manager-domain stage-1 mapping), which real mask ROM
@ absorbs. The SWP-shaped sibling lives in test_swp_rom_aperture.S.
@
@ Mechanism: stage-1 MMU stays off, so VA == IPA. STR to a ROM-
@ aperture address produces a stage-2 perm fault with a decodable
@ (ISV=1) syndrome; the hypervisor drops the write and ERETs past
@ the instruction.
@
@ Pass criteria:
@   1. STR to ROM[0] completes without an abort.
@   2. ROM[0] is unchanged afterwards (re-read via LDR matches).
@   3. Same for STRB (byte form).
@   4. Same for STR at a non-zero ROM address (0x100).

#include "../common/test_runtime.S"

    .global test_main
test_main:
    @ Snapshot ROM[0] and ROM[0x100] via regular LDRs (reads through
    @ the stage-2 RO ROM mapping succeed natively).
    mov     r0, #0
    ldr     r4, [r0]                 @ r4 = ROM[0]
    ldr     r0, =0x100
    ldr     r6, [r0]                 @ r6 = ROM[0x100]

    @ === Subtest 1: word STR to ROM[0] =========================
    mov     r0, #0
    ldr     r1, =0xDEADBEEF
    str     r1, [r0]                 @ dropped by the hypervisor

    mov     r0, #0
    ldr     r5, [r0]
    cmp     r5, r4
    bne     .Lfail_word_dropped

    @ === Subtest 2: byte STRB to ROM[1] ========================
    mov     r0, #0
    mov     r1, #0x99
    strb    r1, [r0, #1]             @ dropped

    mov     r0, #0
    ldr     r5, [r0]
    cmp     r5, r4
    bne     .Lfail_byte_dropped

    @ === Subtest 3: word STR at a non-zero ROM address =========
    @ Prove the drop doesn't depend on the address being literally
    @ NULL.
    ldr     r0, =0x100
    ldr     r1, =0xCAFEBABE
    str     r1, [r0]                 @ dropped

    ldr     r0, =0x100
    ldr     r5, [r0]
    cmp     r5, r6
    bne     .Lfail_offset_dropped

    @ All subtests passed (reaching here at all also proves no
    @ abort was delivered).
    hvc_pass
    bx      lr

.Lfail_word_dropped:
    hvc_hex32 r5
    hvc_hex32 r4
    hvc_fail 0x01

.Lfail_byte_dropped:
    hvc_hex32 r5
    hvc_hex32 r4
    hvc_fail 0x02

.Lfail_offset_dropped:
    hvc_hex32 r5
    hvc_hex32 r6
    hvc_fail 0x03
