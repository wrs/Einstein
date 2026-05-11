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
@   * Per-channel registers for channels 0/1 (the external-serial
@     port) round-trip through the modeled `TBasicSerialPortManager`
@     state — bank 1 reg 0 = mRx/TxDMAPhysicalBufferStart, etc. See
@     `Emulator/Serial/TBasicSerialPortManager.cpp:642-891`.
@   * Per-channel registers for channels 2-7 are unmodeled (no host
@     backend wired up): reads return 0, writes drop.
@
@ This test only probes the simplest round-trip register (bank 1 reg 0,
@ "buffer start PA") on channel 0; deeper TX/RX flow coverage is in
@ test_dma_irq and (eventually) a serial-data integration test.

#include "../common/test_runtime.S"

    .equ DMA_ASSIGN,    0x0F08FC00
    .equ DMA_ENABLE,    0x0F098000
    .equ DMA_DISABLE,   0x0F098400
    .equ DMA_WORDSTS,   0x0F098800
    @ Per-channel probes:
    @   channel 0 (serial 0 RX): bank 1, reg 0 — round-trips
    @   channel 2 (IR):          bank 1, reg 0 — stub (reads 0)
    .equ DMA_CH0_B1R0,  0x0F080000
    .equ DMA_CH2_B1R0,  0x0F084000      @ channel stride = 0x2000

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
    hvc     #HVC_MARK                      @ mark 1: assignment reg R/W

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
    hvc     #HVC_MARK                      @ mark 2: chip-wide stubs

    @ --- Channel 0 (serial 0 RX) round-trips through the modeled
    @ TBasicSerialPortManager state for bank 1 reg 0 (buf_start). ---
    ldr     r4, =DMA_CH0_B1R0
    ldr     r5, =0xAABBCCDD
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_ch0_rw

    @ --- Channel 2 (IR) is unmodeled: write drops, read returns 0. ---
    ldr     r4, =DMA_CH2_B1R0
    ldr     r5, =0x11223344
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, #0
    bne     .Lfail_ch2_stub

    mov     r0, #3
    hvc     #HVC_MARK                      @ mark 3: per-channel mix

    hvc_pass
    bx      lr

.Lfail_assign_rw:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x30
.Lfail_assign_rewrite:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x31
.Lfail_enable_nonzero:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x32
.Lfail_wordsts_nonzero:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x33
.Lfail_ch0_rw:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x34
.Lfail_ch2_stub:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x35
