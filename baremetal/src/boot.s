// Entry point. Load address is per-platform (linker.ld puts us at
// 0x80000 for raspi3b, linker-fvp.ld at 0x80000000 for FVP). Multiple
// cores may arrive here at reset; we run only the unique primary
// (Aff2|Aff1|Aff0 == 0) and park everything else.

.section .text.boot, "ax"
.global _start

_start:
    // Park anything that isn't the primary. Mask to bits[23:0] of
    // MPIDR_EL1 (Aff2|Aff1|Aff0): on raspi3b this correctly parks
    // cluster0 cores 1-3; on FVP it also parks cluster1.cpu0 (Aff1=1).
    mrs     x0, mpidr_el1
    ubfx    x0, x0, #0, #24
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
