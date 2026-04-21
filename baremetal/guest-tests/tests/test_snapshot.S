@ tests/test_snapshot.S
@
@ Smoke test for the snapshot save path. The resume path can't be
@ meaningfully tested from a single guest-test run — it's exercised
@ by running this test once (which saves) and then verifying the
@ output file exists — but this test at least proves the HVC hook
@ reaches snapshot::save() without faulting.
@
@ HVC #0x20 triggers the save via QEMU semihosting to
@ /tmp/newton-snapshot.bin. The guest continues after the HVC.

#include "common/test_runtime.S"

    .section .text, "ax"
    .global test_main
test_main:
    @ Plant a sentinel in RAM so a post-run hex dump of the snapshot
    @ file can find it at the expected offset.
    ldr     r0, =0x04010000
    ldr     r1, =0xCAFEBABE
    str     r1, [r0]

    @ Trigger the save. r0..r3 go into the snapshot header's GPR
    @ slots; use a recognisable pattern.
    ldr     r0, =0x11111111
    ldr     r1, =0x22222222
    ldr     r2, =0x33333333
    ldr     r3, =0x44444444
    hvc     #0x20

    mov     r0, #1
    hvc     #5                  @ mark 1: save returned without halt

    hvc     #3                  @ PASS
    bx      lr
