@ tests/test_flash.S
@
@ Exercises the flash region at guest IPA 0x0200_0000. Currently the
@ hypervisor backs this with a stage-2-RW mapping onto a static byte
@ array initialised to 0xFF (erased flash). Tests:
@   * A fresh read returns 0xFFFFFFFF at a few offsets.
@   * A write followed by a read sees the written value.
@   * Adjacent words are unaffected.
@
@ When we port TFlash into cxx-core (Phase 2.1 C++ side already done;
@ Phase 3 wires the FFI into the MMIO dispatch), this test will have
@ to change to match Einstein's "DLDS"/"OSCD" seeded header at offset
@ 0. That's expected and will be a deliberate update.

#include "../common/test_runtime.S"

    .equ FLASH_BASE, 0x02000000

    .global test_main
test_main:
    @ Read a few words from a fresh flash — every one should be 0xFF.
    ldr     r4, =FLASH_BASE
    ldr     r5, =0xFFFFFFFF

    ldr     r0, [r4]
    cmp     r0, r5
    bne     .Lfail_read_at_0

    ldr     r0, [r4, #4]
    cmp     r0, r5
    bne     .Lfail_read_at_4

    ldr     r0, [r4, #0x100]
    cmp     r0, r5
    bne     .Lfail_read_at_100

    @ Write a known pattern and read it back.
    ldr     r6, =0xDEADBEEF
    str     r6, [r4, #0x200]
    ldr     r0, [r4, #0x200]
    cmp     r0, r6
    bne     .Lfail_readback

    @ Neighbour cells untouched.
    ldr     r0, [r4, #0x204]
    cmp     r0, r5
    bne     .Lfail_neighbour

    hvc_pass
    bx      lr

.Lfail_read_at_0:
    hvc     #2
    hvc_fail 0x21
.Lfail_read_at_4:
    hvc     #2
    hvc_fail 0x22
.Lfail_read_at_100:
    hvc     #2
    hvc_fail 0x23
.Lfail_readback:
    hvc     #2
    hvc_fail 0x24
.Lfail_neighbour:
    hvc     #2
    hvc_fail 0x25
