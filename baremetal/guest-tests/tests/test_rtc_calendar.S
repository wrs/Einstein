@ tests/test_rtc_calendar.S
@
@ Verifies the RTC calendar register at IPA 0x0F181000 returns a
@ plausible "seconds since 1904" value wired to host wall-clock.
@ Einstein patches the ROM's RealClockSeconds to call host `time()`
@ (`Emulator/JIT/Generic/TJITGenericROMPatch.cpp:110`); we instead
@ service the MMIO read in `peripherals::vic::calendar_seconds`
@ using ARM-semihosting SYS_TIME captured at hypervisor boot.
@
@ Checks:
@   * calendar read is non-zero (boot-time init fired)
@   * calendar read is >= 2020-01-01 00:00:00 UTC (= 3661027200 seconds
@     since 1904), a weak modernity bound chosen so this test stays
@     valid long past current year without needing an upper bound.
@   * two successive reads are monotonic non-decreasing.

#include "../common/test_runtime.S"

    .equ HW_CALENDAR,    0x0F181000
    .equ MODERN_EPOCH_LO, 3661027200       @ 2020-01-01 relative to 1904

    .global test_main
test_main:
    ldr     r4, =HW_CALENDAR

    ldr     r0, [r4]
    cmp     r0, #0
    beq     .Lfail_zero

    ldr     r1, =MODERN_EPOCH_LO
    cmp     r0, r1                          @ Carry set iff r0 >= r1 (unsigned)
    bcc     .Lfail_pre_2020                 @ CC = "unsigned <"

    mov     r5, r0                          @ save first reading

    @ Short delay so CNTPCT advances more than a second's worth of
    @ cycles on any reasonable host. This test should never flake.
    mov     r6, #0x100000
1:  subs    r6, r6, #1
    bne     1b

    ldr     r0, [r4]
    cmp     r0, r5
    bcc     .Lfail_nondecreasing

    mov     r0, #1
    hvc     #HVC_MARK

    hvc_pass
    bx      lr

.Lfail_zero:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x80
.Lfail_pre_2020:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x81
.Lfail_nondecreasing:
    mov     r0, r5
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x82
