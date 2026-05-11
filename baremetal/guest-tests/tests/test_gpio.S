@ tests/test_gpio.S
@
@ Two halves:
@
@   1. Drop-on-write / read-as-zero for the GPIO-adjacent registers in
@      0x0F18_xxxx (CC00..EC00). Einstein's TMemory Bank #3 read path
@      has no handler for these and falls through to the default = 0
@      (TMemory.cpp:950-960), and the write path likewise has no
@      handler and drops at TMemory.cpp:1903-1913. peripherals::vic
@      matches that behaviour: writes silently drop, reads always
@      return 0. (A previous iteration tried round-trip storage
@      because TGPIOInterface::DisableInterrupt at ROM 0x26c468 does
@      a read-modify-write, but Einstein's r-m-w sees 0 on every read,
@      so ours must too — see commit 499ffe42.)
@
@   2. HVC-trigger raise of INT_GPIO (bit 0x01000000) → virtual IRQ
@      delivery, verifying the test-only HVC GPIO_TRIGGER_TAG (#6)
@      hooks vic::raise correctly.

#include "../common/test_runtime.S"

    .equ GPIO_CC00,    0x0F18CC00
    .equ GPIO_D000,    0x0F18D000
    .equ GPIO_D800,    0x0F18D800
    .equ GPIO_DC00,    0x0F18DC00
    .equ GPIO_E000,    0x0F18E000
    .equ GPIO_E800,    0x0F18E800
    .equ GPIO_EC00,    0x0F18EC00

    .equ INT_CTRL,     0x0F183400
    .equ FIQ_MASK,     0x0F183C00

    .equ BIT_GPIO,     0x01000000

    .global test_main
test_main:
    @ --- Part 1: writes drop, reads return 0 ------------------------
    @ Pick a unique pattern per register so a stray storage bug shows
    @ up as the pattern leaking back out of the read. Each register
    @ must read as 0 regardless of what we just wrote.
    mov     r5, #0                              @ expected read value
    ldr     r4, =GPIO_CC00
    ldr     r7, =0xAABBCCDD
    str     r7, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_cc00

    ldr     r4, =GPIO_D000
    ldr     r7, =0x11223344
    str     r7, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_d000

    ldr     r4, =GPIO_D800
    ldr     r7, =0x55667788
    str     r7, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_d800

    ldr     r4, =GPIO_DC00
    ldr     r7, =0x99AABBCC
    str     r7, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_dc00

    ldr     r4, =GPIO_E000
    ldr     r7, =0xDEADBEEF
    str     r7, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_e000

    ldr     r4, =GPIO_E800
    ldr     r7, =0x12345678
    str     r7, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_e800

    ldr     r4, =GPIO_EC00
    ldr     r7, =0xFEEDFACE
    str     r7, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_ec00

    @ Cross-check: after writing every register, none must have leaked
    @ a stored pattern into a sibling. Re-read GPIO_D000; still 0.
    ldr     r4, =GPIO_D000
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_aliasing

    mov     r0, #1
    hvc     #HVC_MARK                          @ mark 1: drop/read-as-zero ok

    @ --- Part 2: HVC-trigger → INT_GPIO virtual IRQ -----------------
    ldr     r4, =FIQ_MASK
    mov     r5, #0
    str     r5, [r4]
    ldr     r4, =INT_CTRL
    ldr     r5, =BIT_GPIO
    str     r5, [r4]
    ldr     r4, =INT_CLEAR
    mvn     r5, #0
    str     r5, [r4]

    cpu_enable_irq
    hvc     #HVC_GPIO_TRIGGER                          @ GPIO_TRIGGER_TAG: vic::raise(INT_GPIO)

    @ HVC entry sets ELR to the next insn; on ERET the VIRQ is taken
    @ before the next instruction executes.
    ldr     r4, =IRQ_COUNT_ADDR
    ldr     r5, [r4]
    cmp     r5, #1
    bne     .Lfail_count

    ldr     r4, =IRQ_PENDING_ADDR
    ldr     r5, [r4]
    ldr     r6, =BIT_GPIO
    and     r5, r5, r6
    cmp     r5, r6
    bne     .Lfail_bit

    cpu_disable_irq

    mov     r0, #2
    hvc     #HVC_MARK                          @ mark 2: GPIO IRQ delivered

    hvc_pass
    bx      lr

.Lfail_cc00:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x40
.Lfail_d000:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x41
.Lfail_d800:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x42
.Lfail_dc00:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x43
.Lfail_e000:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x44
.Lfail_e800:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x45
.Lfail_ec00:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x46
.Lfail_aliasing:
    mov     r0, r6
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x47
.Lfail_count:
    mov     r0, r5
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x48
.Lfail_bit:
    mov     r0, r5
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x49
