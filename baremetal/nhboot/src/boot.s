// nhboot entry. The Pi firmware loads kernel8.img at 0x80000 and the
// AArch64 armstub enters it at EL2h on core 0 with x0 = DTB pointer
// (docs/REAL_HW_BRINGUP.md, "EL handoff"). The image is linked at
// 0x10000000 (linker.ld), so the first job is to move ourselves there:
// the hypervisor payload we later copy to 0x80000 would otherwise
// overwrite the code doing the copying.
//
// Everything before `.Lrelocated` runs at the *entered* address, so it
// must be position-independent: `adr` (PC-relative) and literal-pool
// loads of link-time constants only. No `adrp` to link-address symbols.

.section .text.boot, "ax"
.global _start

_start:
    // Firmware hands us the DTB pointer in x0; keep it for the
    // payload's entry (x19 is callee-saved, and main() receives it)
    // before anything below touches x0. Also remember where we were
    // entered (x20) for the banner.
    mov     x19, x0
    adr     x20, _start

    // Park anything that isn't the primary core. MPIDR_EL1 bits
    // [23:0] are Aff2|Aff1|Aff0 (ARM ARM D23.2.113); the armstub
    // leaves cores 1-3 in its own spin table, but a QEMU boot can
    // still bring them here.
    mrs     x1, mpidr_el1
    ubfx    x1, x1, #0, #24
    cbnz    x1, .Lpark

    // ---- Self-relocation ------------------------------------------
    // x1 = running address of _start, x2 = link address, x3 = link
    // address of the end of the loaded bytes (.data end, 16-aligned).
    adr     x1, _start
    ldr     x2, =_start
    ldr     x3, =__data_end
    cmp     x1, x2
    b.eq    .Lrelocated_pi          // Already in place (e.g. a loader
                                    // that honoured the ELF address).
.Lcopy:
    cmp     x2, x3
    b.hs    .Lcopy_done
    ldp     x4, x5, [x1], #16
    stp     x4, x5, [x2], #16
    b       .Lcopy
.Lcopy_done:
    // The copy went through the data side; make the instruction side
    // see it before we execute from it. With the MMU off the data
    // accesses were Non-cacheable, so a barrier plus an I-cache
    // invalidate is sufficient (ARM ARM D7.4.7 / "IC IALLU").
    dsb     sy
    ic      iallu
    dsb     sy
    isb
.Lrelocated_pi:
    ldr     x4, =.Lrelocated
    br      x4

    // ---- Running at the link address from here -------------------
.Lrelocated:
    adrp    x0, __stack_top
    add     x0, x0, #:lo12:__stack_top
    mov     sp, x0

    // Stack guard canary, same magic the hypervisor uses
    // (0x5354_4B47_5541_5244, "STKGUARD"); panic.rs reports it.
    adrp    x1, __stack_guard
    add     x1, x1, #:lo12:__stack_guard
    movz    x2, #0x5244
    movk    x2, #0x5541, lsl #16
    movk    x2, #0x4b47, lsl #32
    movk    x2, #0x5354, lsl #48
    str     x2, [x1]

    // Zero .bss (linker aligns both ends to 16).
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

    mov     x0, x19                 // dtb
    mov     x1, x20                 // entered_at
    bl      main

    // main is `-> !`; guard against a signature bug.
.Lpark:
    wfe
    b       .Lpark
