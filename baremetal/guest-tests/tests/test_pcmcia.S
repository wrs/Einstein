@ tests/test_pcmcia.S
@
@ Exercises the PCMCIA slot windows. With no card-emulation peripheral
@ wired in, peripherals::pcmcia::{read,write} return 0xFFFFFFFF for
@ every read (Newton's "no card" signal) and drop every write. Both
@ slots (0 at 0x3000_0000, 1 at 0x4000_0000) behave identically.

#include "../common/test_runtime.S"

    .equ PCMCIA_SLOT0_BASE, 0x30000000
    .equ PCMCIA_SLOT1_BASE, 0x40000000

    .global test_main
test_main:
    @ --- Slot 0: probe the CIS window, expect all-ones ---
    ldr     r4, =PCMCIA_SLOT0_BASE
    ldr     r5, =0xFFFFFFFF

    ldr     r0, [r4, #0x00]
    cmp     r0, r5
    bne     .Lfail_slot0_base

    @ An arbitrary offset inside the slot window also reads as 0xFFFFFFFF.
    @ 0x38000000 is the specific probe site the kernel hits during boot.
    ldr     r6, =0x38000000
    ldr     r0, [r6]
    cmp     r0, r5
    bne     .Lfail_slot0_offset

    @ Writes are silently dropped; a follow-up read still returns 0xFFFFFFFF.
    ldr     r7, =0xCAFEBABE
    str     r7, [r4, #0x100]
    ldr     r0, [r4, #0x100]
    cmp     r0, r5
    bne     .Lfail_slot0_write

    mov     r0, #1
    hvc     #5                      @ mark 1: slot 0 empty + drop writes

    @ --- Slot 1: same behaviour at a different base ---
    ldr     r4, =PCMCIA_SLOT1_BASE
    ldr     r0, [r4]
    cmp     r0, r5
    bne     .Lfail_slot1_base

    str     r7, [r4, #0x200]
    ldr     r0, [r4, #0x200]
    cmp     r0, r5
    bne     .Lfail_slot1_write

    mov     r0, #2
    hvc     #5                      @ mark 2: slot 1 empty + drop writes

    hvc_pass
    bx      lr

.Lfail_slot0_base:
    mov     r0, r0
    hvc     #2
    hvc_fail 0x40
.Lfail_slot0_offset:
    hvc     #2
    hvc_fail 0x41
.Lfail_slot0_write:
    hvc     #2
    hvc_fail 0x42
.Lfail_slot1_base:
    hvc     #2
    hvc_fail 0x43
.Lfail_slot1_write:
    hvc     #2
    hvc_fail 0x44
