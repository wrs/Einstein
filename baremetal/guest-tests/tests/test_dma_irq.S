@ tests/test_dma_irq.S
@
@ DMA enable register (0x0F09_8000) → per-channel completion IRQ
@ delivery via the synthetic "completes immediately" model in
@ peripherals::dma::write. Writing a bit N into the enable register
@ raises INT_DMA_CH{N} (bit `0x80 << N`) in int_present; the next
@ ERET delivers a virtual IRQ if INT_CTRL allows it.
@
@ Bit layout (TInterruptManager.h:64-83):
@   ch 0 = 0x80    (serial 0 RX)
@   ch 1 = 0x100   (serial 0 TX)
@   ch 2 = 0x200   (IR)
@   ch 3 = 0x400   (sound in)
@   ch 4 = 0x800   (audio rx)
@   ch 5 = 0x1000  (sound out)
@   ch 6 = 0x2000  (modem rx)
@   ch 7 = 0x4000  (modem tx)
@
@ Three sub-tests:
@   1. Enable channel 0 → IRQ with pending bit 0x80, count=1.
@   2. Enable channel 5 (after re-clearing) → IRQ with bit 0x1000, count=2.
@   3. Enable channels 3 + 7 simultaneously → IRQ with bits 0x4400, count=3.

#include "../common/test_runtime.S"

    .equ DMA_ENABLE,     0x0F098000
    .equ INT_CTRL,       0x0F183400
    .equ FIQ_MASK,       0x0F183C00

    .equ ALL_DMA_BITS,   0x00007F80      @ ch 0..7 = 0x80 | 0x100 | ... | 0x4000

    .global test_main
test_main:
    @ --- Route all DMA channel bits to IRQ. ---
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

    @ --- Sub-test 1: channel 0 (bit 0 of enable, INT bit 0x80) ---
    ldr     r4, =DMA_ENABLE
    mov     r5, #1
    str     r5, [r4]                    @ raises INT_DMA_CH0; VIRQ on ERET

    @ The store above traps to EL2; on ERET the IRQ fires before the
    @ next instruction. After the IRQ handler returns, irq_count = 1.
    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #1
    bne     .Lfail_count_1

    ldr     r4, =IRQ_PENDING_ADDR
    ldr     r5, [r4]
    ldr     r6, =0x80
    and     r5, r5, r6
    cmp     r5, r6
    bne     .Lfail_bit_1

    mov     r0, #1
    hvc     #HVC_MARK

    @ --- Sub-test 2: channel 5 (bit 5 of enable, INT bit 0x1000) ---
    ldr     r4, =DMA_ENABLE
    mov     r5, #0x20                   @ bit 5
    str     r5, [r4]

    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #2
    bne     .Lfail_count_2

    ldr     r4, =IRQ_PENDING_ADDR
    ldr     r5, [r4]
    ldr     r6, =0x1000
    and     r5, r5, r6
    cmp     r5, r6
    bne     .Lfail_bit_2

    mov     r0, #2
    hvc     #HVC_MARK

    @ --- Sub-test 3: channels 3 + 7 simultaneously (bits 3 and 7 of
    @ enable; INT bits 0x400 | 0x4000 = 0x4400) ---
    ldr     r4, =DMA_ENABLE
    mov     r5, #0x88                   @ bits 3 and 7
    str     r5, [r4]

    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #3
    bne     .Lfail_count_3

    ldr     r4, =IRQ_PENDING_ADDR
    ldr     r5, [r4]
    ldr     r6, =0x4400
    and     r5, r5, r6
    cmp     r5, r6
    bne     .Lfail_bit_3

    mov     r0, #3
    hvc     #HVC_MARK

    cpu_disable_irq

    hvc_pass
    bx      lr

.Lfail_count_1:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC0
.Lfail_bit_1:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC1
.Lfail_count_2:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC2
.Lfail_bit_2:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC3
.Lfail_count_3:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC4
.Lfail_bit_3:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC5
