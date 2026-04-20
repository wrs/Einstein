@ tests/test_vic.S
@
@ Integration test for the Newton VIC with *async* timer delivery. The
@ previous version of this test leaned on the fact that every LDR to the
@ TICKS MMIO register trapped to EL2 and piggy-backed a vic::poll_timer_matches
@ + HCR_EL2.VI update onto the trap return path. That masked a correctness
@ hole: a guest parked in WFI or polling a RAM word wouldn't receive a
@ timer interrupt, because nothing would trap.
@
@ This version avoids the crutch. After arming the match, the guest drops
@ into WFI and only wakes when an IRQ fires. Passing proves:
@
@   * The EL2 physical timer (CNTHP_CVAL_EL2) is armed from the guest's
@     MATCH write.
@   * CNTHPIRQ is routed via the BCM2836 per-core peripheral to the IRQ
@     pin, taken at EL2 (HCR_EL2.IMO=1), handled, and reflected back as
@     a virtual IRQ (HCR_EL2.VI).
@   * The shared runtime's _irq_entry handler services IntPresent / IntClear
@     correctly, and a second arm of MATCH_2 (followed by another WFI)
@     produces a second interrupt — no stuck state, no double-fire.

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

    mov     r0, #1
    hvc     #5                      @ mark 1: r/w sanity passed

    @ --- Part 2: first async IRQ delivery via WFI ---
    @ Route ONLY timer-2 to IRQ: clear FIQ_MASK, set INT_CTRL = BIT_TIMER_2.
    ldr     r4, =FIQ_MASK
    mov     r5, #0
    str     r5, [r4]
    ldr     r4, =INT_CTRL
    ldr     r5, =BIT_TIMER_2
    str     r5, [r4]

    @ Ensure no interrupt is pending before we arm the match.
    ldr     r4, =INT_CLEAR
    mvn     r5, #0                  @ 0xFFFFFFFF
    str     r5, [r4]

    @ Arm match-2 at now + ~0x4000 ticks (~4.4 ms at 3.6864 MHz).
    ldr     r4, =TICKS
    ldr     r5, [r4]
    add     r5, r5, #0x4000
    ldr     r4, =MATCH_2
    str     r5, [r4]

    @ Sanity: irq_count is still zero before we unmask.
    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #0
    bne     .Lfail_irq_premature

    @ Unmask IRQs globally and park in WFI. We must not touch any MMIO
    @ after this point or the trap-return poll path could deliver the IRQ
    @ for us and invalidate the test.
    cpu_enable_irq
    wfi

    @ If WFI returned without the IRQ handler running, irq_count is still 0.
    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #1
    bne     .Lfail_wfi_no_irq

    @ Verify the pending snapshot had TIMER_2 set.
    ldr     r4, =IRQ_PENDING_ADDR
    ldr     r5, [r4]
    and     r5, r5, #BIT_TIMER_2
    cmp     r5, #BIT_TIMER_2
    bne     .Lfail_wrong_bit

    @ Verify int_present was cleared by the handler's INT_CLEAR write.
    ldr     r4, =INT_PRESENT
    ldr     r5, [r4]
    tst     r5, #BIT_TIMER_2
    bne     .Lfail_not_cleared

    mov     r0, #2
    hvc     #5                      @ mark 2: async IRQ delivered via WFI

    @ --- Part 3: a second match must fire only after a re-arm ---
    @ We didn't rewrite MATCH_2, so match_fired is still latched for slot 2
    @ and no further IRQ should arrive. Re-arm to a new deadline ahead of
    @ the current tick, WFI again, expect count == 2.
    ldr     r4, =TICKS
    ldr     r5, [r4]
    add     r5, r5, #0x4000
    ldr     r4, =MATCH_2
    str     r5, [r4]

    wfi

    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #2
    bne     .Lfail_second_count

    mov     r0, #3
    hvc     #5                      @ mark 3: second IRQ after re-arm

    @ Re-mask IRQs so the final checks aren't racing against any stray
    @ pending bit.
    cpu_disable_irq

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
.Lfail_irq_premature:
    mov     r0, r5
    hvc     #2
    hvc_fail 0x17
.Lfail_wfi_no_irq:
    mov     r0, r5
    hvc     #2
    hvc_fail 0x18
.Lfail_wrong_bit:
    mov     r0, r5
    hvc     #2
    hvc_fail 0x19
.Lfail_not_cleared:
    mov     r0, r5
    hvc     #2
    hvc_fail 0x1B
.Lfail_second_count:
    mov     r0, r5
    hvc     #2
    hvc_fail 0x1C
