@ tests/test_flash_driver.S
@
@ Exercises `peripherals::native_primitives::execute` driver=0x000000
@ (TEinsteinFlashDriver) via `MCR p10, 0, r5, c0, c0, 0` traps. Mirrors
@ the Einstein subfunction semantics at
@ `Emulator/TNativePrimitives.cpp:263-528`.
@
@ Subfunctions covered:
@   * 0x01 Identify             — recognised mask returns ID struct
@   * 0x01 Identify             — bogus mask returns r0 = 0
@   * 0x03 Init                 — r0 = 0
@   * 0x0B IsEraseComplete      — r0 = 1, *r3 = 0
@   * 0x0D BeginWrite           — flash addr → r0 = 0
@   * 0x0D BeginWrite           — non-flash addr → kError_Flash_AddressOutOfRange
@   * 0x09 StartErase (32-bit)  — fills block with 0xFFFFFFFF
@
@ MMU is off in this test; the hypervisor's `read_guest_word` /
@ `write_guest_word` helpers fall back to PA when stage-1 translation
@ fails, so every guest address here is an IPA.
@
@ NOTE: subfn 0x08 (Write) is not covered here. It reads its
@ `flashRange` argument from `[SP+4]`, and QEMU raspi3b doesn't
@ propagate the AArch32 banked R13 into AArch64 x13 / the
@ `MRS SP_svc` alias reliably enough for a test to set up a
@ predictable stack frame for it. The Write path is exercised
@ implicitly by the real Newton kernel during ROM boot.

#include "../common/test_runtime.S"

    .equ FLASH_BANK0,      0x02000000
    .equ ID_STRUCT_ADDR,   0x04000300      @ scratch in RAM
    .equ FLASH_RANGE_ADDR, 0x04000400      @ scratch: first word = vtable
    .equ VTABLE_32BIT,     0x0001E3D4      @ MP2100US 32-bit
    .equ MCR_P10_R5,       0xEE005A10      @ MCR p10, 0, r5, c0, c0, 0

    .global test_main
test_main:
    @ -------- 0x01 Identify with recognised mask -----------------------
    @ Zero the id-struct area so a stale read doesn't pass.
    ldr     r4, =ID_STRUCT_ADDR
    mov     r0, #0
    str     r0, [r4, #0x00]
    str     r0, [r4, #0x04]
    str     r0, [r4, #0x08]
    str     r0, [r4, #0x0C]
    str     r0, [r4, #0x10]
    str     r0, [r4, #0x14]

    ldr     r0, =0xDEADBEEF                 @ sentinel in r0
    ldr     r1, =0x34000000                 @ chipAddr (unused by handler)
    ldr     r2, =0xFF000000                 @ mask — recognised
    ldr     r3, =ID_STRUCT_ADDR             @ idStruct out-pointer
    mov     r5, #1                          @ native_insn = (0<<8)|0x01
    .inst   MCR_P10_R5

    cmp     r0, #1                          @ r0 == 1 on recognised
    bne     .Lfail_identify_r0

    ldr     r0, [r4, #0x00]
    ldr     r1, =0x00000089                 @ manufacturer = Intel
    cmp     r0, r1
    bne     .Lfail_identify_mfr

    ldr     r0, [r4, #0x10]
    ldr     r1, =0x00200000                 @ chip size
    cmp     r0, r1
    bne     .Lfail_identify_size

    ldr     r0, [r4, #0x14]
    ldr     r1, =0x00010000                 @ block size
    cmp     r0, r1
    bne     .Lfail_identify_block

    mov     r0, #1
    hvc     #HVC_MARK

    @ -------- 0x01 Identify with bogus mask ---------------------------
    ldr     r2, =0xDEAD0000                 @ not a recognised single-lane mask
    ldr     r3, =ID_STRUCT_ADDR
    mov     r5, #1
    .inst   MCR_P10_R5
    cmp     r0, #0                          @ r0 == 0 on miss
    bne     .Lfail_identify_miss

    mov     r0, #2
    hvc     #HVC_MARK

    @ -------- 0x03 Init ----------------------------------------------
    ldr     r0, =0xDEADBEEF
    mov     r5, #3
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_init

    mov     r0, #3
    hvc     #HVC_MARK

    @ -------- 0x0B IsEraseComplete -----------------------------------
    ldr     r3, =ID_STRUCT_ADDR             @ reuse scratch for the status out
    ldr     r6, =0xAAAAAAAA
    str     r6, [r3]                        @ prime with non-zero
    mov     r5, #0x0B
    .inst   MCR_P10_R5
    cmp     r0, #1                          @ r0 = 1 (complete)
    bne     .Lfail_erase_complete_r0
    ldr     r0, [r3]
    cmp     r0, #0                          @ *r3 set to 0 (no error)
    bne     .Lfail_erase_complete_status

    mov     r0, #4
    hvc     #HVC_MARK

    @ -------- 0x0D BeginWrite — valid flash address -------------------
    ldr     r2, =0x02000100                 @ inside bank 0
    mov     r5, #0x0D
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_begin_write_ok

    @ -------- 0x0D BeginWrite — non-flash address ---------------------
    ldr     r2, =0x04001234                 @ RAM, not flash
    ldr     r1, =0xFFFFD6BE                 @ -10562 = kError_Flash_AddressOutOfRange
    mov     r5, #0x0D
    .inst   MCR_P10_R5
    cmp     r0, r1
    bne     .Lfail_begin_write_err

    mov     r0, #5
    hvc     #HVC_MARK

    @ -------- 0x09 StartErase (32-bit lane, 0x20000 block) ------------
    @ flashRange is in r1; first word of *r1 = vtable selects 32-bit.
    ldr     r6, =FLASH_RANGE_ADDR
    ldr     r7, =VTABLE_32BIT
    str     r7, [r6]

    @ Drop a marker inside the block we're about to erase so we can
    @ verify the fill landed.
    ldr     r4, =FLASH_BANK0
    add     r7, r4, #0x10000                @ offset 0x10000 inside bank 0
    ldr     r0, =0xDEADDEAD
    str     r0, [r7]

    @ r1 = flashRange ptr, r2 = addr to erase
    ldr     r1, =FLASH_RANGE_ADDR
    add     r2, r4, #0x10000
    mov     r5, #0x09
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_erase_ret

    ldr     r0, [r7]
    ldr     r1, =0xFFFFFFFF
    cmp     r0, r1
    bne     .Lfail_erase_value

    mov     r0, #6
    hvc     #HVC_MARK

    hvc_pass
    bx      lr

.Lfail_identify_r0:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x30
.Lfail_identify_mfr:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x31
.Lfail_identify_size:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x32
.Lfail_identify_block:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x33
.Lfail_identify_miss:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x34
.Lfail_init:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x35
.Lfail_erase_complete_r0:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x36
.Lfail_erase_complete_status:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x37
.Lfail_begin_write_ok:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x38
.Lfail_begin_write_err:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x39
.Lfail_erase_ret:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x3C
.Lfail_erase_value:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x3D
