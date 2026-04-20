@ tests/test_flash.S
@
@ Exercises the flash region at guest IPA 0x0200_0000. The hypervisor
@ maps flash stage-2 RW (no trap) onto a zero-initialised backing that
@ peripherals::flash::init seeds with a Newton "DLDS"/"OSCD" header at
@ block 0 (offset 0) and block 1 (offset 0x10000). Tests:
@
@   * Header: the seeded words are readable at their documented offsets
@     in both blocks of bank 0.
@   * Unwritten region: reads as 0x00000000 (matches Einstein's
@     zero-initialised mmap behaviour).
@   * Plain store / load round-trips land in the backing.
@   * Byte granularity: an STRB changes only the target byte.
@   * Bank 1 (0x0240_0000) is a distinct region, not a mirror of bank 0.

#include "../common/test_runtime.S"

    .equ FLASH_BANK0, 0x02000000
    .equ FLASH_BANK1, 0x02400000

    .global test_main
test_main:
    @ --- Header, block 0 ---
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

    @ --- Header, block 1 (inside bank 0, not bank 1) ---
    @ Offsets past 4095 don't fit in the LDR immediate field, so build
    @ block-1's base address in r7 and use small offsets from there.
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

    @ --- Unwritten region reads as zero ---
    ldr     r0, [r4, #0x200]
    cmp     r0, #0
    bne     .Lfail_unwritten

    @ --- Write / readback ---
    ldr     r6, =0xDEADBEEF
    str     r6, [r4, #0x200]
    ldr     r0, [r4, #0x200]
    cmp     r0, r6
    bne     .Lfail_readback

    @ Neighbouring word untouched.
    ldr     r0, [r4, #0x204]
    cmp     r0, #0
    bne     .Lfail_neighbour

    mov     r0, #2
    hvc     #5                      @ mark 2: word write + neighbour

    @ --- Byte write changes only one byte ---
    @ Write 0xAABBCCDD at 0x300, then overwrite the LSB with 0x11 via
    @ STRB. Expect 0xAABBCC11.
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

    @ --- Bank 1 is a separate region ---
    @ Bank 1 isn't seeded by Einstein (or us), so offset 0 reads zero.
    ldr     r5, =FLASH_BANK1
    ldr     r0, [r5, #0]
    cmp     r0, #0
    bne     .Lfail_bank1_seeded

    @ A write in bank 1 doesn't affect bank 0 (no aliasing).
    ldr     r6, =0xCAFEF00D
    str     r6, [r5, #0x400]
    ldr     r0, [r4, #0x400]        @ bank 0 at same relative offset
    cmp     r0, #0
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
.Lfail_readback:
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
