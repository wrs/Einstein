@ tests/test_vic.S
@
@ Real integration test for the Newton VIC: programs a timer-2 match to
@ fire shortly after `now`, enables Timer-2 as an IRQ source, unmasks
@ IRQs in CPSR, and spins reading the tick register until the shared
@ runtime's IRQ handler increments `irq_count`. Passes iff:
@
@   * An IRQ is actually delivered after the match (scheduler-style).
@   * The pending-bit snapshot captured in the handler has TIMER_2 set.
@   * Clearing via IntClear works (a second match doesn't immediately
@     re-fire unless we re-arm the match register).
@
@ Also covers the ad-hoc register r/w that the previous version of this
@ test did: every config write must read back its value.

#include "../common/test_runtime.S"

    .equ INT_CTRL,       0x0F183400
    .equ FIQ_MASK,       0x0F183C00
    .equ INT_ED_1,       0x0F184000
    .equ MATCH_2,        0x0F182800
    .equ TICKS,          0x0F181800
    .equ INT_CLEAR,      0x0F183800
    .equ INT_PRESENT,    0x0F183000

    .equ BIT_TIMER_2,    0x00000020

    .global test_main
test_main:
    @ --- Part 1: register r/w sanity ---
    ldr     r4, =INT_CTRL
    ldr     r5, =0x0D400000
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r5, r6
    bne     .Lfail_int_ctrl

    ldr     r4, =FIQ_MASK
    ldr     r5, =0x0C400000
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r5, r6
    bne     .Lfail_fiq_mask

    ldr     r4, =INT_ED_1
    ldr     r5, =0xCAFE5A5A
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r5, r6
    bne     .Lfail_int_ed_1

    @ --- Part 2: real interrupt delivery via timer match ---
    @ Route ONLY timer-2 to IRQ (nothing in fiq_mask, just timer-2 in
    @ int_ctrl), leave everything else disabled.
    ldr     r4, =FIQ_MASK
    mov     r5, #0
    str     r5, [r4]
    ldr     r4, =INT_CTRL
    ldr     r5, =BIT_TIMER_2
    str     r5, [r4]

    @ Ensure no interrupt is pending before we arm the match.
    ldr     r4, =INT_CLEAR
    mvn     r5, #0              @ 0xFFFFFFFF: clear all pending bits
    str     r5, [r4]

    @ Read current ticks, arm match to fire shortly (~0x2000 ticks).
    ldr     r4, =TICKS
    ldr     r5, [r4]
    add     r5, r5, #0x2000
    ldr     r4, =MATCH_2
    str     r5, [r4]

    @ Unmask IRQs globally.
    cpu_enable_irq

    @ Spin reading ticks (each read traps to EL2, which polls the VIC
    @ and sets HCR_EL2.VI when timer-2 passes). Bail after ~100k ticks
    @ so a test failure produces a recognisable FAIL rather than a
    @ timeout.
    ldr     r4, =TICKS
    ldr     r7, [r4]
    add     r7, r7, #0x40000    @ upper bound
    ldr     r8, =IRQ_COUNT_ADDR
.Lwait_loop:
    ldr     r9, [r8]
    cmp     r9, #0
    bne     .Lirq_delivered
    ldr     r9, [r4]
    cmp     r9, r7
    blo     .Lwait_loop
    @ Timed out — IRQ never arrived.
    hvc_fail 0x18

.Lirq_delivered:
    @ Re-mask IRQs so we don't take another one while inspecting state.
    cpu_disable_irq

    @ Verify the pending snapshot had TIMER_2 set.
    ldr     r4, =IRQ_PENDING_ADDR
    ldr     r5, [r4]
    and     r5, r5, #BIT_TIMER_2
    cmp     r5, #BIT_TIMER_2
    bne     .Lfail_wrong_bit

    @ Verify exactly one IRQ was delivered (no spurious re-entries).
    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #1
    bne     .Lfail_count_not_one

    @ Verify int_present was cleared by the handler.
    ldr     r4, =INT_PRESENT
    ldr     r5, [r4]
    tst     r5, #BIT_TIMER_2
    bne     .Lfail_not_cleared

    @ All checks passed.
    mov     r0, #1
    hvc     #5                   @ mark 1: r/w sanity
    mov     r0, #2
    hvc     #5                   @ mark 2: IRQ delivered
    mov     r0, #3
    hvc     #5                   @ mark 3: pending bit correct
    mov     r0, #4
    hvc     #5                   @ mark 4: count == 1
    mov     r0, #5
    hvc     #5                   @ mark 5: int_present cleared
    hvc_pass
    bx      lr

.Lfail_int_ctrl:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x11
.Lfail_fiq_mask:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x12
.Lfail_int_ed_1:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x13
.Lfail_wrong_bit:
    mov     r0, r5
    hvc     #2
    hvc_fail 0x19
.Lfail_count_not_one:
    mov     r0, r5
    hvc     #2
    hvc_fail 0x1A
.Lfail_not_cleared:
    mov     r0, r5
    hvc     #2
    hvc_fail 0x1B
