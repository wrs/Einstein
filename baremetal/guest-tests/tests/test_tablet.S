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
@   * 0x16 NativeGetSample — full path via HVC_INJECT_PEN:
@         inject (sample, ticks) → INT_PRESENT.bit28 set
@         drain  → r0=1, *r1=sample, *r2=ticks
@         drain again → r0=0 (queue empty)

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
    hvc     #HVC_MARK                              @ mark 1: scalars ok

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
    hvc     #HVC_MARK                              @ mark 2: sample rate r/w ok

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
    hvc     #HVC_MARK                              @ mark 3: calibration r/w ok

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
    hvc     #HVC_MARK                              @ mark 4: state queries ok

    @ -------- 0x16 NativeGetSample (with HVC_INJECT_PEN priming) -----
    @ Inject a sample (0xCAFEBABE) with ticks 0x12345 via the debug
    @ HVC. host_io::queue::enqueue_pen_sample raises INT_TABLET in the
    @ VIC, which we observe via the int_present register.
    ldr     r0, =0xCAFEBABE
    ldr     r1, =0x00012345
    hvc     #HVC_INJECT_PEN

    @ INT_TABLET bit (0x10000000) should now be set in int_present.
    ldr     r4, =INT_PRESENT
    ldr     r0, [r4]
    ldr     r1, =0x10000000
    tst     r0, r1
    beq     .Lfail_no_tablet_irq

    @ Drain one sample. r1 = &sample_out, r2 = &ticks_out.
    ldr     r1, =OUT_SLOT
    add     r2, r1, #4
    @ Wipe the slots so a no-op write is visible.
    ldr     r0, =0xAAAAAAAA
    str     r0, [r1]
    str     r0, [r2]
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0516
    .inst   MCR_P10_R5
    cmp     r0, #1
    bne     .Lfail_drain_got
    ldr     r0, [r1]
    ldr     r3, =0xCAFEBABE
    cmp     r0, r3
    bne     .Lfail_drain_sample
    ldr     r0, [r2]
    ldr     r3, =0x00012345
    cmp     r0, r3
    bne     .Lfail_drain_ticks

    @ Second drain — queue is empty, r0 should be 0 and the out
    @ slots untouched. Pre-fill the slots with a sentinel to confirm.
    ldr     r1, =OUT_SLOT
    add     r2, r1, #4
    ldr     r0, =0x55555555
    str     r0, [r1]
    str     r0, [r2]
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0516
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_drain_empty
    ldr     r0, [r1]
    ldr     r3, =0x55555555
    cmp     r0, r3
    bne     .Lfail_drain_untouched

    mov     r0, #5
    hvc     #HVC_MARK                              @ mark 5: pen sample path ok

    hvc_pass
    bx      lr

.Lfail_no_tablet_irq:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x7A
.Lfail_drain_got:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x7B
.Lfail_drain_sample:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x7C
.Lfail_drain_ticks:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x7D
.Lfail_drain_empty:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x7E
.Lfail_drain_untouched:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x7F

.Lfail_wakeup:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x70
.Lfail_shutdown:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x71
.Lfail_set_rate:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x72
.Lfail_get_rate:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x73
.Lfail_cal_00:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x74
.Lfail_cal_04:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x75
.Lfail_cal_08:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x76
.Lfail_cal_10:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x77
.Lfail_state:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x78
.Lfail_needs_recal:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x79
