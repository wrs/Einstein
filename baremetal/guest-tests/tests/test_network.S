@ tests/test_network.S
@
@ Exercises peripherals::native_primitives::execute driver=0x00000A
@ (TNetworkManager). Subfn semantics mirror Einstein's
@ TNativePrimitives::ExecuteNetworkManagerNative
@ (Emulator/TNativePrimitives.cpp:2889-3151).
@
@ Covered:
@   * 0x04 Init / 0x05 Enable / 0x06 Disable — no observable r0
@   * 0x0A SendPacket(buf=r1, size=r2)       — accepts/drops packet
@   * 0x0B GetDeviceAddress(dst=r1, size=r2) — writes the default MAC
@                                              (58:b0:35:77:d7:22) and
@                                              returns r0 = 0
@   * 0x14 DataAvailable                     — r0 = 0 (no data)

#include "../common/test_runtime.S"

    .equ OUT_SLOT,        0x04000300
    .equ MCR_P10_R5,      0xEE005A10

    .global test_main
test_main:
    @ -------- 0x04 Init -----------------------------------------------
    ldr     r5, =0x0A04
    .inst   MCR_P10_R5

    @ -------- 0x05 Enable ---------------------------------------------
    ldr     r5, =0x0A05
    .inst   MCR_P10_R5

    @ -------- 0x06 Disable --------------------------------------------
    ldr     r5, =0x0A06
    .inst   MCR_P10_R5

    mov     r0, #1
    hvc     #5                              @ mark 1: lifecycle ok

    @ -------- 0x0A SendPacket(buf=OUT_SLOT, size=8) -------------------
    @ Pre-fill the buffer with a few bytes so the handler has something
    @ to potentially read. Our handler drops on the floor without
    @ reading, but that's fine for a stub.
    ldr     r4, =OUT_SLOT
    mov     r0, #0xAA
    str     r0, [r4]
    ldr     r1, =OUT_SLOT
    mov     r2, #8
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0A0A
    .inst   MCR_P10_R5
    @ r0 untouched per Einstein's no-manager path; just don't crash.

    mov     r0, #2
    hvc     #5                              @ mark 2: SendPacket ok

    @ -------- 0x0B GetDeviceAddress(dst=OUT_SLOT, size=6) -------------
    @ Wipe the destination first.
    mov     r0, #0
    str     r0, [r4, #0]
    str     r0, [r4, #4]
    ldr     r1, =OUT_SLOT
    mov     r2, #6
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0A0B
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_getmac_r0

    @ Verify the 6 MAC bytes: 58 B0 35 77 D7 22.
    @ Little-endian word at offset 0 packs mac[0..4] as
    @   0x58 | (0xB0<<8) | (0x35<<16) | (0x77<<24) = 0x7735B058.
    ldr     r0, [r4, #0]
    ldr     r1, =0x7735B058
    cmp     r0, r1
    bne     .Lfail_getmac_w0

    @ Word at offset 4 holds mac[4] | (mac[5]<<8) in its low halfword;
    @ the upper two bytes were not written so they remain at the zero
    @ we pre-stored above. Expected: 0x000022D7.
    ldr     r0, [r4, #4]
    ldr     r1, =0x000022D7
    cmp     r0, r1
    bne     .Lfail_getmac_w1

    mov     r0, #3
    hvc     #5                              @ mark 3: MAC bytes verified

    @ -------- 0x14 DataAvailable --------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0A14
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_avail

    mov     r0, #4
    hvc     #5                              @ mark 4: DataAvailable ok

    hvc_pass
    bx      lr

.Lfail_getmac_r0:
    hvc     #2
    hvc_fail 0xB0
.Lfail_getmac_w0:
    hvc     #2
    hvc_fail 0xB1
.Lfail_getmac_w1:
    hvc     #2
    hvc_fail 0xB2
.Lfail_avail:
    hvc     #2
    hvc_fail 0xB3
