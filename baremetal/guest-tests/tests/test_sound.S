@ tests/test_sound.S
@
@ Exercises peripherals::native_primitives::execute driver=0x000002
@ (PMainSoundDriver). Pins the existing "no sound hardware" stub in
@ src/peripherals/sound.rs against Einstein's behaviour
@ (Emulator/TNativePrimitives.cpp:1063-1400).
@
@ Covered:
@   * 0x03 SetSoundHardwareInfo  — r0 = -30009 (0xFFFF8AC7)
@   * 0x04 GetSoundHardwareInfo  — r0 = 0; struct at *r1 holds
@         seven words: 1, 1, 1, 0x54600000, 6, 0x10, 1
@   * 0x05, 0x10                  — r0 = 0 (no-op subfns)

#include "../common/test_runtime.S"

    .equ OUT_SLOT,        0x04000300
    .equ MCR_P10_R5,      0xEE005A10

    .global test_main
test_main:
    @ -------- 0x03 SetSoundHardwareInfo -------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0203
    .inst   MCR_P10_R5
    ldr     r1, =0xFFFF8AC7                 @ -30009
    cmp     r0, r1
    bne     .Lfail_set_hw

    @ -------- 0x04 GetSoundHardwareInfo -------------------------------
    @ Pre-fill scratch with sentinel.
    ldr     r4, =OUT_SLOT
    ldr     r0, =0xAAAAAAAA
    str     r0, [r4, #0x00]
    str     r0, [r4, #0x04]
    str     r0, [r4, #0x08]
    str     r0, [r4, #0x0C]
    str     r0, [r4, #0x10]
    str     r0, [r4, #0x14]
    str     r0, [r4, #0x18]

    ldr     r1, =OUT_SLOT
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0204
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_get_hw_r0

    @ Verify the seven words.
    ldr     r0, [r4, #0x00]
    cmp     r0, #1
    bne     .Lfail_get_hw_w0

    ldr     r0, [r4, #0x04]
    cmp     r0, #1
    bne     .Lfail_get_hw_w1

    ldr     r0, [r4, #0x08]
    cmp     r0, #1
    bne     .Lfail_get_hw_w2

    ldr     r0, [r4, #0x0C]
    ldr     r1, =0x54600000
    cmp     r0, r1
    bne     .Lfail_get_hw_w3

    ldr     r0, [r4, #0x10]
    cmp     r0, #6
    bne     .Lfail_get_hw_w4

    ldr     r0, [r4, #0x14]
    cmp     r0, #0x10
    bne     .Lfail_get_hw_w5

    ldr     r0, [r4, #0x18]
    cmp     r0, #1
    bne     .Lfail_get_hw_w6

    mov     r0, #1
    hvc     #HVC_MARK                              @ mark 1: hw info ok

    @ -------- 0x05 SetOutputBuffers (no-op) ---------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0205
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_subfn_05

    @ -------- 0x10 StopOutput (no-op) ---------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0210
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_subfn_10

    mov     r0, #2
    hvc     #HVC_MARK                              @ mark 2: no-op subfns ok

    hvc_pass
    bx      lr

.Lfail_set_hw:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xD0
.Lfail_get_hw_r0:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xD1
.Lfail_get_hw_w0:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xD2
.Lfail_get_hw_w1:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xD3
.Lfail_get_hw_w2:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xD4
.Lfail_get_hw_w3:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xD5
.Lfail_get_hw_w4:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xD6
.Lfail_get_hw_w5:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xD7
.Lfail_get_hw_w6:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xD8
.Lfail_subfn_05:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xD9
.Lfail_subfn_10:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xDA
