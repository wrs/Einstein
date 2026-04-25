@ tests/test_pcmcia.S
@
@ Exercises the PCMCIA controller stub. With no card inserted but the
@ controller silicon present (matching Einstein), each slot has:
@
@   - Card-side spaces (attribute / IO / memory, offsets
@     0x0000_0000..0x0BFF_FFFF) read 0; writes are dropped.
@   - Controller registers at offsets 0x0C00_0000..0x0C00_4400 are
@     read/write storage (so TCardSocket::GetChipInfo's a5a5/5a5a
@     pattern stuck-write check passes).
@   - Reg_1C00 reads return stored-value | 0x000C (k1C00_CardIsPresent
@     set to flag "no card inserted").
@   - Reg_4400 always reads 0xFC (Einstein hardcoded).
@
@ Both slots (0 at 0x3000_0000, 1 at 0x4000_0000) behave identically.

#include "../common/test_runtime.S"

    .equ PCMCIA_SLOT0_BASE, 0x30000000
    .equ PCMCIA_SLOT1_BASE, 0x40000000
    .equ PCMCIA_REG_OFF,    0x0C000000   @ controller-register base within a slot

    .global test_main
test_main:
    @ --- Slot 0: card-side reads return 0 (no card) ---
    ldr     r4, =PCMCIA_SLOT0_BASE

    ldr     r0, [r4, #0x00]          @ attribute space
    cmp     r0, #0
    bne     .Lfail_slot0_attr

    @ Card-side IO space (slot_base + 0x0400_0000).
    ldr     r6, =0x34000000
    ldr     r0, [r6]
    cmp     r0, #0
    bne     .Lfail_slot0_io

    @ Card-side write at attribute space is dropped, follow-up read still 0.
    ldr     r7, =0xCAFEBABE
    str     r7, [r4, #0x100]
    ldr     r0, [r4, #0x100]
    cmp     r0, #0
    bne     .Lfail_slot0_card_write

    mov     r0, #1
    hvc     #5                       @ mark 1: slot 0 card-side empty

    @ --- Slot 0: chip-detect register storage (reg_3000 / reg_3800) ---
    ldr     r5, =PCMCIA_SLOT0_BASE
    add     r5, r5, #PCMCIA_REG_OFF  @ controller register base
    add     r0, r5, #0x3000          @ reg_3000
    add     r1, r5, #0x3800          @ reg_3800

    ldr     r2, =0x0000A5A5
    ldr     r3, =0x00005A5A
    str     r2, [r0]                 @ chip-detect: write 0xA5A5 to reg_3000
    str     r3, [r1]                 @ chip-detect: write 0x5A5A to reg_3800
    ldr     ip, =0x0000FFFF
    ldr     r8, [r0]
    and     r8, r8, ip
    cmp     r8, r2
    bne     .Lfail_slot0_reg3000
    ldr     r8, [r1]
    and     r8, r8, ip
    cmp     r8, r3
    bne     .Lfail_slot0_reg3800

    mov     r0, #2
    hvc     #5                       @ mark 2: chip-detect storage works

    @ --- Slot 0: reg_1C00 returns "no card" flag (0x000C) ---
    add     r0, r5, #0x1C00
    mov     r1, #0
    str     r1, [r0]                 @ clear the storage cell
    ldr     r2, [r0]
    and     r2, r2, #0xC             @ k1C00_CardIsPresent
    cmp     r2, #0xC
    bne     .Lfail_slot0_no_card

    mov     r0, #3
    hvc     #5                       @ mark 3: no-card flag set

    @ --- Slot 0: reg_4400 returns 0xFC ---
    add     r0, r5, #0x4400
    ldr     r1, [r0]
    cmp     r1, #0xFC
    bne     .Lfail_slot0_reg4400

    mov     r0, #4
    hvc     #5                       @ mark 4: reg_4400 = 0xFC

    @ --- Slot 1: same chip-detect storage at a different base ---
    ldr     r5, =PCMCIA_SLOT1_BASE
    add     r5, r5, #PCMCIA_REG_OFF
    add     r0, r5, #0x3000
    ldr     r2, =0x0000DEAD
    str     r2, [r0]
    ldr     ip, =0x0000FFFF
    ldr     r1, [r0]
    and     r1, r1, ip
    cmp     r1, r2
    bne     .Lfail_slot1_reg3000

    mov     r0, #5
    hvc     #5                       @ mark 5: slot 1 chip-detect storage works

    @ --- Cross-slot isolation: slot 0's reg_3000 still holds 0xA5A5,
    @     not slot 1's 0xDEAD. ---
    ldr     r5, =PCMCIA_SLOT0_BASE
    add     r5, r5, #PCMCIA_REG_OFF
    add     r0, r5, #0x3000
    ldr     ip, =0x0000FFFF
    ldr     r1, [r0]
    and     r1, r1, ip
    ldr     r2, =0x0000A5A5
    cmp     r1, r2
    bne     .Lfail_cross_slot

    mov     r0, #6
    hvc     #5                       @ mark 6: slots are isolated

    hvc_pass
    bx      lr

.Lfail_slot0_attr:
    hvc     #2
    hvc_fail 0x40
.Lfail_slot0_io:
    hvc     #2
    hvc_fail 0x41
.Lfail_slot0_card_write:
    hvc     #2
    hvc_fail 0x42
.Lfail_slot0_reg3000:
    hvc     #2
    hvc_fail 0x43
.Lfail_slot0_reg3800:
    hvc     #2
    hvc_fail 0x44
.Lfail_slot0_no_card:
    hvc     #2
    hvc_fail 0x45
.Lfail_slot0_reg4400:
    hvc     #2
    hvc_fail 0x46
.Lfail_slot1_reg3000:
    hvc     #2
    hvc_fail 0x47
.Lfail_cross_slot:
    hvc     #2
    hvc_fail 0x48
