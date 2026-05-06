@ tests/test_host_call.S
@
@ Exercises peripherals::native_primitives::execute driver=0x000009
@ (TEinsteinNativeCalls). Subfn semantics mirror Einstein's
@ TNativePrimitives::ExecuteHostCallNative
@ (Emulator/TNativePrimitives.cpp:2515-2882).
@
@ Without a host FFI runtime, every recognised subfn returns r0 = 0
@ (or leaves r0 untouched for void-return subfns). Unknown subfns
@ would halt the hypervisor — those are not exercised here.

#include "../common/test_runtime.S"

    .equ MCR_P10_R5,      0xEE005A10

    .global test_main
test_main:
    @ -------- 0x03 OpenLib --------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0903
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_openlib

    @ -------- 0x05 PrepareFFIStructure --------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0905
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_prep_ffi

    @ -------- 0x14 SetArgValue_uint32 ---------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0914
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_set_uint32

    mov     r0, #1
    hvc     #HVC_MARK                              @ mark 1: arg setters ok

    @ -------- 0x20 SetResultType --------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0920
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_set_result

    @ -------- 0x30 Call_void ------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0930
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_call_void

    @ -------- 0x40 GetErrno -------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0940
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_get_errno

    mov     r0, #2
    hvc     #HVC_MARK                              @ mark 2: call/result/errno ok

    hvc_pass
    bx      lr

.Lfail_openlib:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xA0
.Lfail_prep_ffi:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xA1
.Lfail_set_uint32:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xA2
.Lfail_set_result:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xA3
.Lfail_call_void:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xA4
.Lfail_get_errno:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xA5
