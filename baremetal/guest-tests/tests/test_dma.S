@ tests/test_dma.S
@
@ Exercises DMA MMIO registers the Newton kernel actually touches during
@ early boot: the channel-assignment register at 0x0F08_FC00. Einstein's
@ TDMAManager is a stub for most things (enable, status, per-channel),
@ but the assignment register does have state — so that's the one we can
@ verify survives a round trip through MMIO.
@
@ Current hypervisor state: no DMA is wired in yet. These accesses go to
@ the ad-hoc mmio.rs which returns 0 for reads and drops writes. This
@ test therefore exercises the *current* behavior: read returns 0 after
@ write. When we wire in cxx-core/dma_manager in Phase 3, the readback
@ should match the written value — that will be a deliberate test update.

#include "../common/test_runtime.S"

    .equ DMA_ASSIGN,  0x0F08FC00
    .equ DMA_ENABLE,  0x0F098000   @ status register (read)
    .equ DMA_DISABLE, 0x0F098400
    .equ DMA_WORDSTS, 0x0F098800

    .global test_main
test_main:
    @ Write to the assignment register; under the ad-hoc mmio.rs the
    @ store is silently dropped and the register reads back as 0.
    ldr     r4, =DMA_ASSIGN
    ldr     r5, =0xDEADBEEF
    str     r5, [r4]
    ldr     r6, [r4]
    @ Locked-in behaviour: current hypervisor returns 0.
    cmp     r6, #0
    beq     1f
    mov     r0, r6
    hvc     #2
    hvc_fail 0x31
1:

    @ Status register reads 0.
    ldr     r4, =DMA_ENABLE
    ldr     r6, [r4]
    cmp     r6, #0
    beq     2f
    mov     r0, r6
    hvc     #2
    hvc_fail 0x32
2:

    @ Disable register: write is dropped, no state to observe.
    ldr     r4, =DMA_DISABLE
    ldr     r5, =0x0000FFFF
    str     r5, [r4]

    @ Word status register reads 0.
    ldr     r4, =DMA_WORDSTS
    ldr     r6, [r4]
    cmp     r6, #0
    beq     3f
    mov     r0, r6
    hvc     #2
    hvc_fail 0x33
3:

    hvc_pass
    bx      lr
