@ tests/test_bio_bank.S
@
@ Pins the BIO interface register bank's accept/drop semantics in
@ src/mmio.rs (BIO_BANK_BASE..BIO_BANK_END at stride 0x400). Einstein's
@ TMemory returns 0 for all of these and silently drops writes
@ (TMemory.cpp:952-959 unknown-bank fallback); we mirror that to
@ unblock TBIOInterface init without modeling individual registers.
@
@ Covered:
@   * Bank 0  (0x0F050000) — write/readback drops
@   * Bank 1  (0x0F050400)
@   * Bank 12 (0x0F053000)
@   * Bank 31 (0x0F057C00) — last bank in window

#include "../common/test_runtime.S"

    .equ BIO_0,    0x0F050000
    .equ BIO_1,    0x0F050400
    .equ BIO_12,   0x0F053000
    .equ BIO_31,   0x0F057C00

    .global test_main
test_main:
    @ -------- Bank 0: write non-zero, read 0 --------------------------
    ldr     r4, =BIO_0
    ldr     r5, =0xDEADBEEF
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, #0
    bne     .Lfail_b0

    @ Sticky: a second write doesn't make the read non-zero either.
    ldr     r5, =0x12345678
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, #0
    bne     .Lfail_b0_sticky

    mov     r0, #1
    hvc     #HVC_MARK

    @ -------- Bank 1 --------------------------------------------------
    ldr     r4, =BIO_1
    ldr     r5, =0xCAFEBABE
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, #0
    bne     .Lfail_b1

    @ -------- Bank 12 -------------------------------------------------
    ldr     r4, =BIO_12
    ldr     r5, =0x0F0F0F0F
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, #0
    bne     .Lfail_b12

    @ -------- Bank 31 (last bank in window) ---------------------------
    ldr     r4, =BIO_31
    ldr     r5, =0xFFFFFFFF
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, #0
    bne     .Lfail_b31

    mov     r0, #2
    hvc     #HVC_MARK

    hvc_pass
    bx      lr

.Lfail_b0:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xE0
.Lfail_b0_sticky:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xE1
.Lfail_b1:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xE2
.Lfail_b12:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xE3
.Lfail_b31:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xE4
