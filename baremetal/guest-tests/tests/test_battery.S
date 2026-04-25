@ tests/test_battery.S
@
@ Exercises peripherals::native_primitives::execute driver=0x000003
@ (PMainBatteryDriver). Subfn semantics mirror Einstein's
@ TNativePrimitives::ExecuteBatteryDriverNative
@ (Emulator/TNativePrimitives.cpp:1406-1558).
@
@ Covered:
@   * 0x03 Init             — r0 = 0
@   * 0x04 WakeUp           — r0 = 0
@   * 0x05 ShutDown         — r0 = 0
@   * 0x06 Count            — r0 = 0 (Einstein returns 0, not 1)
@   * 0x07 Status (out=r2)  — fills 13-word PowerPlantStatus, r0 = 0
@   * 0x08 RawStatus        — fills raw-voltage variant, r0 = 0
@   * 0x0B ReadADCVoltage   — r0 = 0
@
@ Status struct first three fields verified against Einstein:
@   +0x00 mBatteryType   = 0x00000003
@   +0x04 mVoltage1      = 0x000587C0
@   +0x08 mBatteryLevel  = 0x00000064

#include "../common/test_runtime.S"

    .equ OUT_SLOT,        0x04000300
    .equ MCR_P10_R5,      0xEE005A10      @ MCR p10, 0, r5, c0, c0, 0

    .global test_main
test_main:
    @ -------- 0x03 Init -----------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0303
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_init

    @ -------- 0x04 WakeUp ---------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0304
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_wakeup

    @ -------- 0x05 ShutDown -------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0305
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_shutdown

    @ -------- 0x06 Count ----------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0306
    .inst   MCR_P10_R5
    cmp     r0, #0                          @ Einstein returns 0 here
    bne     .Lfail_count

    mov     r0, #1
    hvc     #5                              @ mark 1: scalars ok

    @ -------- 0x07 Status ---------------------------------------------
    @ Pre-fill scratch with sentinel so we can verify the writes.
    ldr     r4, =OUT_SLOT
    ldr     r1, =0xAAAAAAAA
    str     r1, [r4, #0x00]
    str     r1, [r4, #0x04]
    str     r1, [r4, #0x08]
    @ r2 = struct address (Einstein reads from r2, NOT r1).
    ldr     r2, =OUT_SLOT
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0307
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_status_r0
    @ Verify mBatteryType (+0x00) = 0x00000003.
    ldr     r0, [r4, #0x00]
    cmp     r0, #3
    bne     .Lfail_status_type
    @ Verify mVoltage1 (+0x04) = 0x000587C0.
    ldr     r0, [r4, #0x04]
    ldr     r1, =0x000587C0
    cmp     r0, r1
    bne     .Lfail_status_v1
    @ Verify mBatteryLevel (+0x08) = 0x00000064.
    ldr     r0, [r4, #0x08]
    cmp     r0, #0x64
    bne     .Lfail_status_level

    mov     r0, #2
    hvc     #5                              @ mark 2: Status struct ok

    @ -------- 0x08 RawStatus ------------------------------------------
    ldr     r1, =0xBBBBBBBB
    str     r1, [r4, #0x04]                 @ scribble +0x04 first
    ldr     r2, =OUT_SLOT
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0308
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_raw_r0
    @ RawStatus mVoltage1 = 0x0C97D000.
    ldr     r0, [r4, #0x04]
    ldr     r1, =0x0C97D000
    cmp     r0, r1
    bne     .Lfail_raw_v1

    mov     r0, #3
    hvc     #5                              @ mark 3: RawStatus ok

    @ -------- 0x0B ReadADCVoltage -------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x030B
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_adc

    mov     r0, #4
    hvc     #5                              @ mark 4: ADC ok

    hvc_pass
    bx      lr

.Lfail_init:
    hvc     #2
    hvc_fail 0x60
.Lfail_wakeup:
    hvc     #2
    hvc_fail 0x61
.Lfail_shutdown:
    hvc     #2
    hvc_fail 0x62
.Lfail_count:
    hvc     #2
    hvc_fail 0x63
.Lfail_status_r0:
    hvc     #2
    hvc_fail 0x64
.Lfail_status_type:
    hvc     #2
    hvc_fail 0x65
.Lfail_status_v1:
    hvc     #2
    hvc_fail 0x66
.Lfail_status_level:
    hvc     #2
    hvc_fail 0x67
.Lfail_raw_r0:
    hvc     #2
    hvc_fail 0x68
.Lfail_raw_v1:
    hvc     #2
    hvc_fail 0x69
.Lfail_adc:
    hvc     #2
    hvc_fail 0x6A
