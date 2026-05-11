@ tests/test_dma_irq.S
@
@ DMA enable register (0x0F09_8000) MUST NOT raise a completion IRQ
@ purely as a function of the enable write — that was the previous,
@ buggy "synthesise completion on write" behaviour which caused an
@ infinite FIQ runaway on the LocalTalk RX path (the kernel re-arms
@ DMA from inside the FIQ handler; each re-arm re-raised the IRQ
@ before the FIQ could exit).
@
@ The current contract (peripherals::dma):
@
@   ch 0 (serial 0 RX): IRQ fires only when at least one byte has been
@                       deposited into the guest's RX buffer by
@                       `poll_rx`, AND the channel is armed AND the
@                       per-channel control bit 0x02 is set. No host
@                       UART bytes → no IRQ.
@   ch 1 (serial 0 TX): IRQ fires when the per-channel control bit
@                       0x02 is set AND countdown > 0, on the enable
@                       write itself (the drain is synchronous). With
@                       control bit clear, drain is skipped and no IRQ
@                       fires.
@   ch 2..7           : never raise an IRQ from the enable write
@                       (channels 3 / 5 have a separate test-only
@                       inject path through vic::inject_sound_dma_irq
@                       for the sound-wedge probe).
@
@ This test exercises the *negative* contract: that bare enable writes
@ (with no per-channel buffer setup, no control bits) don't raise any
@ IRQ. Positive-path RX coverage will be added when we have a
@ test-only HVC to push bytes through host UART. Positive-path TX
@ coverage requires staging a buffer in guest RAM, which the test
@ runtime currently lacks.
@
@ Bit layout (TInterruptManager.h:64-83, TBasicSerialPortManager.cpp:
@ 295-302):
@   ch 0 = 0x80    (serial 0 RX)
@   ch 1 = 0x100   (serial 0 TX)
@   ch 2 = 0x200   (IR)
@   ch 3 = 0x400   (sound in)
@   ch 4 = 0x800   (audio rx)
@   ch 5 = 0x1000  (sound out)
@   ch 6 = 0x2000  (modem rx)
@   ch 7 = 0x4000  (modem tx)

#include "../common/test_runtime.S"

    .equ DMA_ENABLE,     0x0F098000
    .equ INT_CTRL,       0x0F183400
    .equ FIQ_MASK,       0x0F183C00

    .equ ALL_DMA_BITS,   0x00007F80      @ ch 0..7 = 0x80 | 0x100 | ... | 0x4000

    .global test_main
test_main:
    @ --- Route all DMA channel bits to IRQ (rather than FIQ) so a
    @ stray raise would surface as an irq_count delta, not a FIQ.
    ldr     r4, =FIQ_MASK
    mov     r5, #0
    str     r5, [r4]
    ldr     r4, =INT_CTRL
    ldr     r5, =ALL_DMA_BITS
    str     r5, [r4]
    ldr     r4, =INT_CLEAR
    mvn     r5, #0
    str     r5, [r4]

    cpu_enable_irq

    @ --- Sub-test 1: channel 0 enable with no buffer setup → no IRQ.
    @ Without the per-channel control register's bit 0x02 set and no
    @ pending host-UART bytes, poll_rx is a no-op and no completion
    @ IRQ fires. The enable write itself only sets the channel's
    @ `armed` flag.
    ldr     r4, =DMA_ENABLE
    mov     r5, #1
    str     r5, [r4]

    @ A spin window: the timer IRQ ticks every ~16 ms. We don't want
    @ the test to depend on timer cadence, so just check the count
    @ immediately — poll_rx was either not entered (no tick yet) or
    @ entered and found nothing to do (no host bytes).
    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #0
    bne     .Lfail_count_1

    ldr     r4, =IRQ_PENDING_ADDR
    ldr     r5, [r4]
    ldr     r6, =0x80
    and     r5, r5, r6
    cmp     r5, #0
    bne     .Lfail_bit_1

    mov     r0, #1
    hvc     #HVC_MARK

    @ --- Sub-test 2: channel 5 enable → no IRQ.
    @ Channel 5 (sound out) has no auto-IRQ path through dma::write.
    @ The wedge-detector in trap_irq uses vic::inject_sound_dma_irq
    @ for that case; the bare enable write must be silent.
    ldr     r4, =DMA_ENABLE
    mov     r5, #0x20                   @ bit 5
    str     r5, [r4]

    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #0
    bne     .Lfail_count_2

    ldr     r4, =IRQ_PENDING_ADDR
    ldr     r5, [r4]
    ldr     r6, =0x1000
    and     r5, r5, r6
    cmp     r5, #0
    bne     .Lfail_bit_2

    mov     r0, #2
    hvc     #HVC_MARK

    @ --- Sub-test 3: channels 3 + 7 simultaneously → no IRQ.
    @ Same contract as ch 5: neither bit synthesises a completion.
    ldr     r4, =DMA_ENABLE
    mov     r5, #0x88                   @ bits 3 and 7
    str     r5, [r4]

    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #0
    bne     .Lfail_count_3

    ldr     r4, =IRQ_PENDING_ADDR
    ldr     r5, [r4]
    ldr     r6, =0x4400
    and     r5, r5, r6
    cmp     r5, #0
    bne     .Lfail_bit_3

    mov     r0, #3
    hvc     #HVC_MARK

    cpu_disable_irq

    hvc_pass
    bx      lr

.Lfail_count_1:
    hvc_hex32 r5
    hvc_fail 0xC0
.Lfail_bit_1:
    hvc_hex32 r5
    hvc_fail 0xC1
.Lfail_count_2:
    hvc_hex32 r5
    hvc_fail 0xC2
.Lfail_bit_2:
    hvc_hex32 r5
    hvc_fail 0xC3
.Lfail_count_3:
    hvc_hex32 r5
    hvc_fail 0xC4
.Lfail_bit_3:
    hvc_hex32 r5
    hvc_fail 0xC5
