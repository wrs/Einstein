@ tests/test_flash.S
@
@ Exercises the flash regions at guest IPAs 0x0200_0000 (bank 0,
@ Einstein kFlashBank1) and 0x1000_0000 (bank 1, kFlashBank2). Each
@ bank is 4 MiB, mapped stage-2 RW onto its half of the single 8 MiB
@ backing in peripherals::flash. Bank 0 is seeded with Einstein's
@ "DLDS" / "OSCD" header at block 0 (offset 0) and block 1 (offset
@ 0x10000); bank 1 is left zero. Tests:
@
@   * Bank 0 header readable at its documented offsets.
@   * Unwritten region in bank 0 reads as 0.
@   * Word store + load round-trips (bank 0 and bank 1).
@   * Byte granularity: STRB changes only the target byte.
@   * Bank 0 and bank 1 are independent (no aliasing).

#include "../common/test_runtime.S"

    .equ FLASH_BANK0, 0x02000000
    .equ FLASH_BANK1, 0x10000000

    .global test_main
test_main:
    @ --- Header, block 0 of bank 0 ---
    ldr     r4, =FLASH_BANK0
    ldr     r0, [r4, #0x00]
    ldr     r1, =0x444C4453         @ "DLDS"
    cmp     r0, r1
    bne     .Lfail_dlds_block0

    ldr     r0, [r4, #0x04]
    ldr     r1, =0x4F534344         @ "OSCD"
    cmp     r0, r1
    bne     .Lfail_oscd_block0

    ldr     r0, [r4, #0x08]
    ldr     r1, =0x0000010C
    cmp     r0, r1
    bne     .Lfail_magic_block0

    ldr     r0, [r4, #0x58]
    ldr     r1, =0xFFFFFFFC
    cmp     r0, r1
    bne     .Lfail_n_block0

    @ --- Header, block 1 within bank 0 (offset 0x10000, not bank 1) ---
    add     r7, r4, #0x10000
    ldr     r0, [r7, #0]
    ldr     r1, =0x444C4453
    cmp     r0, r1
    bne     .Lfail_dlds_block1

    ldr     r0, [r7, #0x58]
    ldr     r1, =0xFFFFFFF0         @ differs from block 0's 0xFFFFFFFC
    cmp     r0, r1
    bne     .Lfail_n_block1

    mov     r0, #1
    hvc     #5                      @ mark 1: header intact

    @ --- Unwritten region of bank 0 reads 0xFFFFFFFF (erased state) ---
    ldr     r0, [r4, #0x200]
    ldr     r1, =0xFFFFFFFF
    cmp     r0, r1
    bne     .Lfail_unwritten

    @ --- Bank 0: write / readback ---
    ldr     r6, =0xDEADBEEF
    str     r6, [r4, #0x200]
    ldr     r0, [r4, #0x200]
    cmp     r0, r6
    bne     .Lfail_bank0_readback

    ldr     r0, [r4, #0x204]        @ neighbour untouched (erased)
    ldr     r1, =0xFFFFFFFF
    cmp     r0, r1
    bne     .Lfail_neighbour

    mov     r0, #2
    hvc     #5                      @ mark 2: bank 0 word write

    @ --- Byte granularity: STRB changes only the target byte ---
    ldr     r6, =0xAABBCCDD
    str     r6, [r4, #0x300]
    mov     r1, #0x11
    strb    r1, [r4, #0x300]
    ldr     r0, [r4, #0x300]
    ldr     r1, =0xAABBCC11
    cmp     r0, r1
    bne     .Lfail_byte_write

    mov     r0, #3
    hvc     #5                      @ mark 3: byte granularity

    @ --- Bank 1 (kFlashBank2) is a separate 4 MiB region (erased) ---
    ldr     r5, =FLASH_BANK1
    ldr     r0, [r5, #0]
    ldr     r1, =0xFFFFFFFF
    cmp     r0, r1
    bne     .Lfail_bank1_seeded

    ldr     r6, =0xCAFEF00D
    str     r6, [r5, #0x400]
    ldr     r0, [r4, #0x400]        @ bank 0 at same offset must be unaffected (erased)
    ldr     r1, =0xFFFFFFFF
    cmp     r0, r1
    bne     .Lfail_bank_alias

    ldr     r0, [r5, #0x400]
    cmp     r0, r6
    bne     .Lfail_bank1_readback

    mov     r0, #4
    hvc     #5                      @ mark 4: bank independence

    hvc_pass
    bx      lr

.Lfail_dlds_block0:
    hvc     #2
    hvc_fail 0x20
.Lfail_oscd_block0:
    hvc     #2
    hvc_fail 0x21
.Lfail_magic_block0:
    hvc     #2
    hvc_fail 0x22
.Lfail_n_block0:
    hvc     #2
    hvc_fail 0x23
.Lfail_dlds_block1:
    hvc     #2
    hvc_fail 0x24
.Lfail_n_block1:
    hvc     #2
    hvc_fail 0x25
.Lfail_unwritten:
    hvc     #2
    hvc_fail 0x26
.Lfail_bank0_readback:
    hvc     #2
    hvc_fail 0x27
.Lfail_neighbour:
    hvc     #2
    hvc_fail 0x28
.Lfail_byte_write:
    hvc     #2
    hvc_fail 0x29
.Lfail_bank1_seeded:
    hvc     #2
    hvc_fail 0x2A
.Lfail_bank_alias:
    hvc     #2
    hvc_fail 0x2B
.Lfail_bank1_readback:
    hvc     #2
    hvc_fail 0x2C
