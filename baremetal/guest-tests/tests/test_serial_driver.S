@ tests/test_serial_driver.S
@
@ Exercises peripherals::native_primitives::execute driver=0x000006
@ (TSerialChipEinstein). Subfn semantics mirror Einstein's
@ TNativePrimitives::ExecuteSerialDriverNative
@ (Emulator/TNativePrimitives.cpp:2036-2467).
@
@ Covered:
@   * Subfn 0x10 (deprecated Voyager) — r0 = -10000 (0xFFFFD8F0)
@   * 0x33 InstallChipHandler        — r0 = 0
@   * 0x35 PutByte('A')              — r0 = 0 ('A' visible in UART log)
@   * 0x37 GetByte                   — r0 = 0 (RX buffer empty)
@   * 0x39 RxBufFull                 — r0 = 0
@   * 0x42 PowerIsOn                 — r0 = 0 (no host port → false)
@   * 0x4C InitByOption              — r0 = 0

#include "../common/test_runtime.S"

    .equ MCR_P10_R5,      0xEE005A10

    .global test_main
test_main:
    @ -------- subfn 0x10 (deprecated Voyager path) --------------------
    @ Any subfn < 0x30 should return -10000.
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0610
    .inst   MCR_P10_R5
    ldr     r1, =0xFFFFD8F0                 @ -10000 = kSerErrTimeout-ish
    cmp     r0, r1
    bne     .Lfail_voyager

    mov     r0, #1
    hvc     #HVC_MARK                              @ mark 1: voyager path ok

    @ -------- 0x33 InstallChipHandler ---------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0633
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_install

    @ -------- 0x35 PutByte('A') ---------------------------------------
    mov     r1, #0x41                       @ 'A'
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0635
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_putbyte

    @ -------- 0x37 GetByte (no RX data) -------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0637
    .inst   MCR_P10_R5
    cmp     r0, #0                          @ port == NULL short-circuit
    bne     .Lfail_getbyte

    mov     r0, #2
    hvc     #HVC_MARK                              @ mark 2: install/put/get ok

    @ -------- 0x39 RxBufFull ------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0639
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_rxfull

    @ -------- 0x42 PowerIsOn ------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0642
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_poweron

    mov     r0, #3
    hvc     #HVC_MARK                              @ mark 3: status queries ok

    @ -------- 0x4C InitByOption ---------------------------------------
    @ Einstein parses an option struct at r1; with no host port we
    @ unconditionally return 0 ("init succeeded with no observable
    @ effect").
    ldr     r1, =0
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x064C
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_init_opt

    mov     r0, #4
    hvc     #HVC_MARK                              @ mark 4: init by option ok

    hvc_pass
    bx      lr

.Lfail_voyager:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x80
.Lfail_install:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x81
.Lfail_putbyte:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x82
.Lfail_getbyte:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x83
.Lfail_rxfull:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x84
.Lfail_poweron:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x85
.Lfail_init_opt:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x86
