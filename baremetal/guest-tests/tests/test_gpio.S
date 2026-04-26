@ tests/test_gpio.S
@
@ Two halves:
@
@   1. Round-trip register storage for the GPIO-adjacent registers in
@      0x0F18_xxxx — these used to be inline read-as-zero stubs in
@      mmio.rs but the kernel's TGPIOInterface::DisableInterrupt at
@      ROM 0x26c468 does a read-modify-write and silently lost any
@      previously-set bits. peripherals::vic now stores them.
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
    @ --- Part 1: round-trip storage --------------------------------
    @ Pick a unique pattern per register so a copy/paste collision
    @ between fields shows up.
    ldr     r4, =GPIO_CC00
    ldr     r5, =0xAABBCCDD
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_cc00

    ldr     r4, =GPIO_D000
    ldr     r5, =0x11223344
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_d000

    ldr     r4, =GPIO_D800
    ldr     r5, =0x55667788
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_d800

    ldr     r4, =GPIO_DC00
    ldr     r5, =0x99AABBCC
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_dc00

    ldr     r4, =GPIO_E000
    ldr     r5, =0xDEADBEEF
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_e000

    ldr     r4, =GPIO_E800
    ldr     r5, =0x12345678
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_e800

    ldr     r4, =GPIO_EC00
    ldr     r5, =0xFEEDFACE
    str     r5, [r4]
    ldr     r6, [r4]
    cmp     r6, r5
    bne     .Lfail_ec00

    @ Cross-check: writing GPIO_CC00 again shouldn't have leaked into
    @ a sibling register.
    ldr     r4, =GPIO_D000
    ldr     r6, [r4]
    ldr     r5, =0x11223344
    cmp     r6, r5
    bne     .Lfail_aliasing

    mov     r0, #1
    hvc     #5                          @ mark 1: round-trip storage ok

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
    hvc     #6                          @ GPIO_TRIGGER_TAG: vic::raise(INT_GPIO)

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
    hvc     #5                          @ mark 2: GPIO IRQ delivered

    hvc_pass
    bx      lr

.Lfail_cc00:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x40
.Lfail_d000:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x41
.Lfail_d800:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x42
.Lfail_dc00:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x43
.Lfail_e000:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x44
.Lfail_e800:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x45
.Lfail_ec00:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x46
.Lfail_aliasing:
    mov     r0, r6
    hvc     #2
    hvc_fail 0x47
.Lfail_count:
    mov     r0, r5
    hvc     #2
    hvc_fail 0x48
.Lfail_bit:
    mov     r0, r5
    hvc     #2
    hvc_fail 0x49
