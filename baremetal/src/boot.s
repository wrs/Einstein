// Entry point. Loaded at 0x80000 by QEMU raspi3b and by Pi firmware.
// All four cores start here; we run core 0 and park the others.

.section .text.boot, "ax"
.global _start

_start:
    // Read CPU ID (affinity level 0) from MPIDR_EL1; park cores 1-3.
    mrs     x0, mpidr_el1
    and     x0, x0, #0xff
    cbnz    x0, .Lpark

    // Install our stack pointer.
    adrp    x0, __stack_top
    add     x0, x0, #:lo12:__stack_top
    mov     sp, x0

    // Zero .bss. Linker aligns both ends to 16 so we can store pairs.
    adrp    x0, __bss_start
    add     x0, x0, #:lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, #:lo12:__bss_end
.Lbss_loop:
    cmp     x0, x1
    b.hs    .Lbss_done
    stp     xzr, xzr, [x0], #16
    b       .Lbss_loop
.Lbss_done:

    bl      kmain

    // kmain is `-> !`, but guard against a bug in the signature.
.Lhang:
    wfe
    b       .Lhang

.Lpark:
    wfe
    b       .Lpark
