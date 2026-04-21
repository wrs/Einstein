@ tests/test_native_primitives.S
@
@ Verifies that CPTR_EL2.TFP trapping is wired up and that
@ peripherals::native_primitives::execute dispatches a known
@ primitive correctly.
@
@ Mechanism:
@   The guest executes `MCR p10, 0, Rd, c0, c0, 0` (= the UAL form
@   `vmov s0, Rd`). CPTR_EL2.TFP=1 routes this to EL2 with EC=0x07.
@   The hypervisor decodes the MCR, reads Rd as the "native call
@   code", and dispatches to native_primitives::execute.
@
@ The "null primitive" (driver=0, subfn=0) is a hypervisor-only
@ test hook — Einstein's real table starts at driver=0 / subfn=1
@ (TEinsteinFlashDriver::Identify). Our handler sets r0 = 0 when
@ the guest calls the null primitive, which is what we assert.

#include "common/test_runtime.S"

    .section .text, "ax"
    .global test_main
test_main:
    @ Preload r0 with a sentinel to confirm the handler writes it.
    ldr     r0, =0xDEADBEEF
    @ Null-primitive code: driver=0 (bits 31:8), subfn=0 (bits 7:0).
    mov     r3, #0

    @ MCR p10, 0, r3, c0, c0, 0. Emit the raw encoding so we don't
    @ need the VFP architecture feature enabled on this toolchain.
    .inst   0xEE003A10

    @ After the trap, the hypervisor's null-primitive handler sets
    @ r0 = 0. Verify.
    cmp     r0, #0
    bne     .Lfail_null_prim

    mov     r0, #1
    hvc     #5                  @ mark 1: null primitive dispatched ok

    hvc     #3                  @ PASS
    bx      lr

.Lfail_null_prim:
    hvc     #2                  @ print the observed (non-zero) r0
    mov     r0, #0xC0
    hvc     #4
