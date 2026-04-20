@ tests/test_dma.S
@
@ Exercises the DMA MMIO surface the Newton kernel touches during
@ early boot. Our peripherals::dma port mirrors Einstein's TDMAManager
@ semantics:
@
@   * Channel-assignment register at 0x0F08_FC00 — R/W, writes latch
@     and a subsequent read returns the written value.
@   * Enable/status (0x0F09_8000), disable (0x0F09_8400), and word-
@     status (0x0F09_8800) have no observable state. Reads return 0;
@     writes are dropped.
@   * Per-channel registers (banks at 0x0F08_0000 and 0x0F09_0000 with
@     channel stride 0x2000, register stride 0x400) read as 0 and drop
@     writes. Einstein's channel-0 / channel-1 paths would eventually
@     delegate to an external-serial DMA driver, but that peripheral
@     isn't wired up yet.

#include "../common/test_runtime.S"

    .equ DMA_ASSIGN,    0x0F08FC00
    .equ DMA_ENABLE,    0x0F098000
    .equ DMA_DISABLE,   0x0F098400
    .equ DMA_WORDSTS,   0x0F098800
    @ Per-channel probes: bank 1, channel 0, register 0 & register 2.
    .equ DMA_CH0_R0,    0x0F080000
    .equ DMA_CH0_R2,    0x0F080800

    .global test_main
test_main:
    @ --- Assignment register: write/read round-trip ---
    ldr     r4, =DMA_ASSIGN
    ldr     r5, =0xDEADBEEF
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_assign_rw

    @ A second write overwrites.
    ldr     r5, =0x12345678
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_assign_rewrite

    mov     r0, #1
    hvc     #5                      @ mark 1: assignment reg R/W

    @ --- Enable/status: read returns 0 regardless of prior write ---
    ldr     r4, =DMA_ENABLE
    ldr     r5, =0xFFFFFFFF
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, #0
    bne     .Lfail_enable_nonzero

    @ --- Disable register: write is dropped (no readable state) ---
    ldr     r4, =DMA_DISABLE
    str     r5, [r4]                @ no observable effect

    @ --- Word-status: always 0 ---
    ldr     r4, =DMA_WORDSTS
    ldr     r6, [r4]
    cmp     r6, #0
    bne     .Lfail_wordsts_nonzero

    mov     r0, #2
    hvc     #5                      @ mark 2: chip-wide stubs

    @ --- Per-channel stub: reads zero, writes dropped ---
    ldr     r4, =DMA_CH0_R0
    ldr     r5, =0xAABBCCDD
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, #0
    bne     .Lfail_ch_stub

    ldr     r4, =DMA_CH0_R2
    ldr     r6, [r4]
    cmp     r6, #0
    bne     .Lfail_ch_stub

    mov     r0, #3
    hvc     #5                      @ mark 3: per-channel stubs

    hvc_pass
    bx      lr

.Lfail_assign_rw:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x30
.Lfail_assign_rewrite:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x31
.Lfail_enable_nonzero:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x32
.Lfail_wordsts_nonzero:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x33
.Lfail_ch_stub:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x34
