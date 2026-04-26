@ tests/test_alarm.S
@
@ RTC alarm match → INT_RTC_ALARM (bit 0x4) → virtual IRQ delivery.
@
@ Setup:
@   * Clear any spurious bits in INT_PRESENT.
@   * Enable INT_RTC_ALARM in INT_CTRL, clear FIQ_MASK so it's IRQ-class.
@   * Read the live calendar register (seconds since 1904) and write that
@     value to the alarm register — `vic::poll_alarm` is supposed to fire
@     when calendar >= alarm. With both equal, the next CNTHP heartbeat
@     (~16 ms wall) latches INT_RTC_ALARM and delivers VIRQ.
@
@ The shared _irq_entry handler increments IRQ_COUNT_ADDR and snapshots
@ INT_PRESENT into IRQ_PENDING_ADDR before clearing it via INT_CLEAR.

#include "../common/test_runtime.S"

    .equ CALENDAR_REG,   0x0F181000
    .equ ALARM_REG,      0x0F181400
    .equ INT_CTRL,       0x0F183400
    .equ FIQ_MASK,       0x0F183C00

    .equ BIT_RTC_ALARM,  0x00000004

    .global test_main
test_main:
    @ --- Clear any pending interrupts and route INT_RTC_ALARM to IRQ. ---
    ldr     r4, =FIQ_MASK
    mov     r5, #0
    str     r5, [r4]
    ldr     r4, =INT_CLEAR
    mvn     r5, #0
    str     r5, [r4]

    ldr     r4, =INT_CTRL
    ldr     r5, =BIT_RTC_ALARM
    str     r5, [r4]

    @ --- Arm the alarm at the current calendar value. ---
    ldr     r4, =CALENDAR_REG
    ldr     r5, [r4]
    ldr     r4, =ALARM_REG
    str     r5, [r4]

    @ Sanity: irq_count is still zero before we unmask.
    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #0
    bne     .Lfail_premature

    @ --- Wait for the next heartbeat to latch + deliver the alarm IRQ. ---
    cpu_enable_irq
.Lwfi_loop:
    wfi
    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #1
    blt     .Lwfi_loop
    bne     .Lfail_irq_count

    @ Verify pending snapshot had INT_RTC_ALARM set.
    ldr     r4, =IRQ_PENDING_ADDR
    ldr     r5, [r4]
    and     r5, r5, #BIT_RTC_ALARM
    cmp     r5, #BIT_RTC_ALARM
    bne     .Lfail_wrong_bit

    @ Verify int_present was cleared by the handler.
    ldr     r4, =INT_PRESENT
    ldr     r5, [r4]
    tst     r5, #BIT_RTC_ALARM
    bne     .Lfail_not_cleared

    cpu_disable_irq

    mov     r0, #1
    hvc     #5

    hvc_pass
    bx      lr

.Lfail_premature:
    mov     r0, r5
    hvc     #2
    hvc_fail 0xF0
.Lfail_irq_count:
    mov     r0, r5
    hvc     #2
    hvc_fail 0xF1
.Lfail_wrong_bit:
    mov     r0, r5
    hvc     #2
    hvc_fail 0xF2
.Lfail_not_cleared:
    mov     r0, r5
    hvc     #2
    hvc_fail 0xF3
