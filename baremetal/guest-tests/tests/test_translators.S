@ tests/test_translators.S
@
@ Exercises peripherals::native_primitives::execute drivers 0x07
@ (PEinsteinInTranslator) and 0x08 (PEinsteinOutTranslator). Both
@ Einstein implementations are stubs that return 0 for every subfn
@ (Emulator/TNativePrimitives.cpp:2475-2509); we enumerate the
@ protocol method indices and verify each returns r0 = 0.
@
@ In-translator subfns covered:  0x00 New, 0x02 Init, 0x05 ProduceFrame
@ Out-translator subfns covered: 0x00 New, 0x02 Init, 0x07 Print, 0x0C ExceptionNotify

#include "../common/test_runtime.S"

    .equ MCR_P10_R5,    0xEE005A10      @ MCR p10, 0, r5, c0, c0, 0

    .global test_main
test_main:
    @ -------- 0x07 In-translator: 0x00 New ---------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0700
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_in_new

    @ -------- 0x07 In-translator: 0x02 Init --------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0702
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_in_init

    @ -------- 0x07 In-translator: 0x05 ProduceFrame ------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0705
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_in_produce

    mov     r0, #1
    hvc     #5                              @ mark 1: in-translator subfns ok

    @ -------- 0x08 Out-translator: 0x00 New --------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0800
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_out_new

    @ -------- 0x08 Out-translator: 0x02 Init -------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0802
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_out_init

    @ -------- 0x08 Out-translator: 0x07 Print ------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0807
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_out_print

    @ -------- 0x08 Out-translator: 0x0C ExceptionNotify --------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x080C
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_out_excnotify

    mov     r0, #2
    hvc     #5                              @ mark 2: out-translator subfns ok

    hvc_pass
    bx      lr

.Lfail_in_new:
    hvc     #2
    hvc_fail 0x90
.Lfail_in_init:
    hvc     #2
    hvc_fail 0x91
.Lfail_in_produce:
    hvc     #2
    hvc_fail 0x92
.Lfail_out_new:
    hvc     #2
    hvc_fail 0x93
.Lfail_out_init:
    hvc     #2
    hvc_fail 0x94
.Lfail_out_print:
    hvc     #2
    hvc_fail 0x95
.Lfail_out_excnotify:
    hvc     #2
    hvc_fail 0x96
