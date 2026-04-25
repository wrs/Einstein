@ tests/test_tablet.S
@
@ Exercises peripherals::native_primitives::execute driver=0x000005
@ (TMainTabletDriver). Subfn semantics mirror Einstein's
@ TNativePrimitives::ExecuteTabletDriverNative
@ (Emulator/TNativePrimitives.cpp:1773-2030).
@
@ Covered:
@   * 0x03 Init                          — sets defaults (no r0)
@   * 0x04 WakeUp / 0x05 ShutDown        — r0 = 0
@   * 0x08 SetSampleRate(0x12345678)
@     followed by 0x07 GetSampleRate     — r0 = 0x12345678
@   * 0x0A SetTabletCalibration(in=r1)
@     followed by 0x09 GetTabletCalibration(out=r1) — round-trip
@     (using Einstein's offset pattern: +0x00, +0x04, +0x08, +0x10)
@   * 0x0E GetTabletState                — r0 = 0
@   * 0x12 TabletNeedsRecalibration      — r0 = 0

#include "../common/test_runtime.S"

    .equ OUT_SLOT,        0x04000300        @ 32 bytes — fits 5-word matrix
    .equ MCR_P10_R5,      0xEE005A10

    .global test_main
test_main:
    @ -------- 0x03 Init -----------------------------------------------
    ldr     r5, =0x0503
    .inst   MCR_P10_R5

    @ -------- 0x04 WakeUp ---------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0504
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_wakeup

    @ -------- 0x05 ShutDown -------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0505
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_shutdown

    mov     r0, #1
    hvc     #5                              @ mark 1: scalars ok

    @ -------- 0x08 SetSampleRate(0x12345678) --------------------------
    ldr     r1, =0x12345678
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0508
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_set_rate

    @ -------- 0x07 GetSampleRate --------------------------------------
    ldr     r5, =0x0507
    .inst   MCR_P10_R5
    ldr     r1, =0x12345678
    cmp     r0, r1
    bne     .Lfail_get_rate

    mov     r0, #2
    hvc     #5                              @ mark 2: sample rate r/w ok

    @ -------- 0x0A SetTabletCalibration -------------------------------
    @ Pre-load the calibration struct in scratch RAM.
    ldr     r4, =OUT_SLOT
    ldr     r0, =0x11111111
    str     r0, [r4, #0x00]
    ldr     r0, =0x22222222
    str     r0, [r4, #0x04]
    ldr     r0, =0x33333333
    str     r0, [r4, #0x08]
    @ +0x0C is never read by Einstein; +0x10 holds fUnknown_0C.
    ldr     r0, =0x55555555                 @ scribble +0x0C — should NOT
    str     r0, [r4, #0x0C]                 @ flow into the driver state
    ldr     r0, =0x44444444                 @ this is what Einstein reads
    str     r0, [r4, #0x10]                 @ for fUnknown_0C
    ldr     r1, =OUT_SLOT
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x050A
    .inst   MCR_P10_R5

    @ -------- 0x09 GetTabletCalibration -------------------------------
    @ Wipe scratch first.
    ldr     r0, =0xAAAAAAAA
    str     r0, [r4, #0x00]
    str     r0, [r4, #0x04]
    str     r0, [r4, #0x08]
    str     r0, [r4, #0x10]
    ldr     r1, =OUT_SLOT
    ldr     r5, =0x0509
    .inst   MCR_P10_R5
    @ Verify +0x00 round-tripped to 0x11111111.
    ldr     r0, [r4, #0x00]
    ldr     r1, =0x11111111
    cmp     r0, r1
    bne     .Lfail_cal_00
    @ Verify +0x04 round-tripped to 0x22222222.
    ldr     r0, [r4, #0x04]
    ldr     r1, =0x22222222
    cmp     r0, r1
    bne     .Lfail_cal_04
    @ Verify +0x08 round-tripped to 0x33333333.
    ldr     r0, [r4, #0x08]
    ldr     r1, =0x33333333
    cmp     r0, r1
    bne     .Lfail_cal_08
    @ Verify +0x10 round-tripped to 0x44444444.
    ldr     r0, [r4, #0x10]
    ldr     r1, =0x44444444
    cmp     r0, r1
    bne     .Lfail_cal_10

    mov     r0, #3
    hvc     #5                              @ mark 3: calibration r/w ok

    @ -------- 0x0E GetTabletState -------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x050E
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_state

    @ -------- 0x12 TabletNeedsRecalibration ---------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0512
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_needs_recal

    mov     r0, #4
    hvc     #5                              @ mark 4: state queries ok

    hvc_pass
    bx      lr

.Lfail_wakeup:
    hvc     #2
    hvc_fail 0x70
.Lfail_shutdown:
    hvc     #2
    hvc_fail 0x71
.Lfail_set_rate:
    hvc     #2
    hvc_fail 0x72
.Lfail_get_rate:
    hvc     #2
    hvc_fail 0x73
.Lfail_cal_00:
    hvc     #2
    hvc_fail 0x74
.Lfail_cal_04:
    hvc     #2
    hvc_fail 0x75
.Lfail_cal_08:
    hvc     #2
    hvc_fail 0x76
.Lfail_cal_10:
    hvc     #2
    hvc_fail 0x77
.Lfail_state:
    hvc     #2
    hvc_fail 0x78
.Lfail_needs_recal:
    hvc     #2
    hvc_fail 0x79
