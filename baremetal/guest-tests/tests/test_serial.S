@ tests/test_serial.S
@
@ Verifies the four Newton TSerialChipVoyager windows respond to
@ the read/write patterns the 717006 boot path exercises:
@
@   - Reading the +0x4400 status register reports TX FIFO empty
@     (bit 7 set) — this is what polling loops wait on before
@     writing a byte.
@   - Writing to +0x6000 (TX byte) is consumed silently; the
@     hypervisor logs up to TX_LOG_MAX bytes per port to the
@     console.
@   - Reading +0x7000 (RX byte) returns 0 — no data.
@
@ Ports (from TMemoryConsts.h:128-131):
@   extr 0x0F1C_0000  infr 0x0F1D_0000  tblt 0x0F1E_0000  mdem 0x0F1F_0000

#include "common/test_runtime.S"

    .section .text, "ax"
    .global test_main
test_main:
    @ ---- Status register read from each port ---------------------
    ldr     r4, =0x0F1C4400     @ extr +0x4400
    ldr     r5, [r4]
    tst     r5, #0x80           @ TX empty bit
    beq     .Lfail_status_extr

    ldr     r4, =0x0F1D4400     @ infr
    ldr     r5, [r4]
    tst     r5, #0x80
    beq     .Lfail_status_infr

    ldr     r4, =0x0F1E4400     @ tblt
    ldr     r5, [r4]
    tst     r5, #0x80
    beq     .Lfail_status_tblt

    ldr     r4, =0x0F1F4400     @ mdem
    ldr     r5, [r4]
    tst     r5, #0x80
    beq     .Lfail_status_mdem

    mov     r0, #1
    hvc     #5                  @ mark 1: all four status regs reported TX empty

    @ ---- TX byte writes ------------------------------------------
    @ Spell "hi" out of each port to exercise the TX consume path.
    ldr     r4, =0x0F1C6000
    mov     r5, #'H'
    str     r5, [r4]
    mov     r5, #'i'
    str     r5, [r4]

    ldr     r4, =0x0F1D6000
    mov     r5, #'I'
    str     r5, [r4]
    mov     r5, #'R'
    str     r5, [r4]

    ldr     r4, =0x0F1E6000
    mov     r5, #'B'
    str     r5, [r4]
    mov     r5, #'I'
    str     r5, [r4]

    ldr     r4, =0x0F1F6000
    mov     r5, #'M'
    str     r5, [r4]
    mov     r5, #'D'
    str     r5, [r4]

    mov     r0, #2
    hvc     #5                  @ mark 2: TX writes accepted

    @ ---- RX byte read returns zero --------------------------------
    ldr     r4, =0x0F1C7000
    ldr     r5, [r4]
    cmp     r5, #0
    bne     .Lfail_rx_nonzero

    mov     r0, #3
    hvc     #5                  @ mark 3: RX read returned 0

    @ ---- Unknown offset inside a window returns 0 -----------------
    @ The hypervisor should log it once and return zero; the test
    @ only asserts the zero return.
    ldr     r4, =0x0F1C9100     @ not any documented offset
    ldr     r5, [r4]
    cmp     r5, #0
    bne     .Lfail_unknown_nonzero

    mov     r0, #4
    hvc     #5                  @ mark 4: unknown offset returned 0

    hvc     #3                  @ PASS
    bx      lr

.Lfail_status_extr:
    mov     r0, r5
    hvc     #2
    mov     r0, #0xA0
    hvc     #4
.Lfail_status_infr:
    mov     r0, r5
    hvc     #2
    mov     r0, #0xA1
    hvc     #4
.Lfail_status_tblt:
    mov     r0, r5
    hvc     #2
    mov     r0, #0xA2
    hvc     #4
.Lfail_status_mdem:
    mov     r0, r5
    hvc     #2
    mov     r0, #0xA3
    hvc     #4
.Lfail_rx_nonzero:
    mov     r0, r5
    hvc     #2
    mov     r0, #0xA4
    hvc     #4
.Lfail_unknown_nonzero:
    mov     r0, r5
    hvc     #2
    mov     r0, #0xA5
    hvc     #4
