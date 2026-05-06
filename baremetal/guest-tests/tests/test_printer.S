@ tests/test_printer.S
@
@ Exercises peripherals::native_primitives::execute driver=0x00000C
@ (TPrinterManager). Subfn semantics mirror Einstein's
@ TNativePrimitives::ExecutePrinterDriverNative
@ (Emulator/TNativePrimitives.cpp:3237-3375). With no host print
@ backend, every NewtonErr arm returns r0 = 0.
@
@ Covered:
@   * 0x03 PDOpen          — r0 = 0
@   * 0x04 PDClose         — r0 = 0
@   * 0x05 PDOpenPage      — r0 = 0
@   * 0x06 PDClosePage     — r0 = 0
@   * 0x07 PDImageBand     — r0 = 0
@   * 0x09 PDIsProblemResolved — r0 = 0
@   * 0x0C PDFaxEndPage    — r0 = 0

#include "../common/test_runtime.S"

    .equ MCR_P10_R5,      0xEE005A10

    .global test_main
test_main:
    @ -------- 0x03 PDOpen ---------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0C03
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_open

    @ -------- 0x04 PDClose --------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0C04
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_close

    @ -------- 0x05 PDOpenPage -----------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0C05
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_open_page

    @ -------- 0x06 PDClosePage ----------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0C06
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_close_page

    mov     r0, #1
    hvc     #HVC_MARK                              @ mark 1: open/close ok

    @ -------- 0x07 PDImageBand ----------------------------------------
    @ Pass plausible (but unused) pointer arguments; handler doesn't
    @ dereference them.
    mov     r1, #0x100                      @ band ptr
    mov     r2, #0x200                      @ rect ptr
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0C07
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_imageband

    @ -------- 0x09 PDIsProblemResolved --------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0C09
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_problem

    @ -------- 0x0C PDFaxEndPage ---------------------------------------
    mov     r1, #1                          @ pageCount
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0C0C
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_fax

    mov     r0, #2
    hvc     #HVC_MARK                              @ mark 2: imaging path ok

    hvc_pass
    bx      lr

.Lfail_open:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC0
.Lfail_close:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC1
.Lfail_open_page:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC2
.Lfail_close_page:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC3
.Lfail_imageband:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC4
.Lfail_problem:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC5
.Lfail_fax:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0xC6
