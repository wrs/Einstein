@ tests/test_flash.S
@
@ Exercises the flash regions at guest IPAs 0x0200_0000 (bank 0,
@ Einstein kFlashBank1) and 0x1000_0000 (bank 1, kFlashBank2). Each
@ bank is 4 MiB, mapped stage-2 *read-only* onto its half of the single
@ 8 MiB backing in peripherals::flash. Bank 0 is seeded with Einstein's
@ "DLDS" / "OSCD" header at block 0 (offset 0) and block 1 (offset
@ 0x10000); every other byte of both banks starts at 0x00, matching
@ Einstein's mmap-O_CREAT backing (TFlash.cpp:66-70). Real flash would
@ read 0xFF for erased bytes, but the Newton kernel was tested against
@ Einstein's zero-fill, so peripherals::flash::GUEST_FLASH and this
@ test follow Einstein, not real hardware. Direct CPU writes are
@ silently dropped (matches Einstein's TMemory::WriteP); flash
@ mutations only happen via the TEinsteinFlashDriver native primitives.
@ Tests:
@
@   * Bank 0 header readable at its documented offsets.
@   * Unwritten region in bank 0 reads as 0x00000000 (zero-fill).
@   * Direct stores to flash are dropped (read-back returns the
@     unmodified backing — zero pattern or the seeded value).
@   * Byte / halfword stores likewise dropped.
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
    hvc     #HVC_MARK                      @ mark 1: header intact

    @ --- Unwritten region of bank 0 reads 0x00000000 (Einstein zero-fill) ---
    ldr     r0, [r4, #0x200]
    ldr     r1, =0x00000000
    cmp     r0, r1
    bne     .Lfail_unwritten

    @ --- Bank 0: direct write is silently dropped ---
    ldr     r6, =0xDEADBEEF
    str     r6, [r4, #0x200]
    ldr     r0, [r4, #0x200]
    ldr     r1, =0x00000000         @ store dropped → still zero-fill
    cmp     r0, r1
    bne     .Lfail_bank0_drop

    @ Seeded region: write should also be dropped (header preserved).
    ldr     r6, =0xDEADBEEF
    str     r6, [r4, #0x00]
    ldr     r0, [r4, #0x00]
    ldr     r1, =0x444C4453         @ "DLDS" — unchanged
    cmp     r0, r1
    bne     .Lfail_seed_clobbered

    mov     r0, #2
    hvc     #HVC_MARK                      @ mark 2: bank 0 store dropped

    @ --- Byte / halfword stores are likewise dropped ---
    add     r2, r4, #0x300
    mov     r1, #0x11
    strb    r1, [r2]
    mov     r1, #0x22
    strh    r1, [r2, #4]
    ldr     r0, [r4, #0x300]
    ldr     r1, =0x00000000
    cmp     r0, r1
    bne     .Lfail_byte_drop

    mov     r0, #3
    hvc     #HVC_MARK                      @ mark 3: byte/halfword stores dropped

    @ --- Bank 1 (kFlashBank2) is a separate 4 MiB region (zero-fill) ---
    ldr     r5, =FLASH_BANK1
    ldr     r0, [r5, #0]
    ldr     r1, =0x00000000
    cmp     r0, r1
    bne     .Lfail_bank1_seeded

    ldr     r6, =0xCAFEF00D
    str     r6, [r5, #0x400]
    ldr     r0, [r4, #0x400]        @ bank 0 at same offset still zero-fill
    ldr     r1, =0x00000000
    cmp     r0, r1
    bne     .Lfail_bank_alias

    ldr     r0, [r5, #0x400]        @ bank 1 store likewise dropped
    ldr     r1, =0x00000000
    cmp     r0, r1
    bne     .Lfail_bank1_drop

    mov     r0, #4
    hvc     #HVC_MARK                      @ mark 4: bank independence + drop

    hvc_pass
    bx      lr

.Lfail_dlds_block0:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x20
.Lfail_oscd_block0:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x21
.Lfail_magic_block0:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x22
.Lfail_n_block0:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x23
.Lfail_dlds_block1:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x24
.Lfail_n_block1:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x25
.Lfail_unwritten:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x26
.Lfail_bank0_drop:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x27
.Lfail_seed_clobbered:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x28
.Lfail_byte_drop:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x29
.Lfail_bank1_seeded:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x2A
.Lfail_bank_alias:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x2B
.Lfail_bank1_drop:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x2C
