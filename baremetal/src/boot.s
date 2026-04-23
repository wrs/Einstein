// Entry point. Load address is per-platform (linker.ld puts us at
// 0x80000 for raspi3b, linker-fvp.ld at 0x80000000 for FVP). Multiple
// cores may arrive here at reset; we run only the unique primary
// (Aff2|Aff1|Aff0 == 0) and park everything else.
//
// On FVP we enter at EL3 and drop to EL2 after a minimal GIC bring-up:
// the redistributor's GICR_WAKER and GICD_CTLR.DS bit are Secure-only,
// and with has_el3=0 there would be no way to wake the RD at all. On
// raspi3b (BCM2836) and under QEMU raspi3b we enter directly at EL2,
// so the EL3 path is gated on CurrentEL at runtime and is effectively
// dead on non-FVP targets.

.section .text.boot, "ax"
.global _start

_start:
    // Park anything that isn't the primary. Mask to bits[23:0] of
    // MPIDR_EL1 (Aff2|Aff1|Aff0): on raspi3b this correctly parks
    // cluster0 cores 1-3; on FVP it also parks cluster1.cpu0 (Aff1=1).
    mrs     x0, mpidr_el1
    ubfx    x0, x0, #0, #24
    cbnz    x0, .Lpark

    // Branch on current EL. FVP with has_el3=1 enters at EL3; QEMU
    // raspi3b and FVP with has_el3=0 enter at EL2. At EL1 we can't
    // run the hypervisor — halt loudly.
    mrs     x0, CurrentEL
    lsr     x0, x0, #2
    cmp     x0, #3
    b.eq    .Lfrom_el3
    cmp     x0, #2
    b.eq    .Lat_el2
    b       .Lpark          // EL0/EL1 shouldn't happen — just park.

.Lfrom_el3:
    // --- Minimal EL3 bring-up ------------------------------------
    //
    // We take EL3 only on FVP_Base_RevC with has_el3=1; see
    // scripts/fvp. Everything here is single-CPU and idempotent.
    //
    // Step 1: wake this CPU's GICv3 redistributor. With has_el3=0
    //   the RD stays asleep because GICR_WAKER is a Secure-only
    //   register and there is no Secure code to clear ProcessorSleep.
    //   We're in Secure state here, so the write lands.
    //
    //   Find the RD for MPIDR Aff0=0 at chain base 0x2F100000. Each
    //   RD frame is 128 KiB on GICv3 (256 KiB if TYPER.VLPIS=1).
    //   We assume CPU0 is always the first entry; the hypervisor's
    //   gicv3.rs walks TYPER properly later from EL2.
    movz    x9, #0x2f10, lsl #16        // x9 = 0x2F100000 (RD CPU0)

    // Clear ProcessorSleep (bit 1) in GICR_WAKER (RD + 0x14).
    add     x10, x9, #0x14
    ldr     w11, [x10]
    bic     w11, w11, #2                 // clear ProcessorSleep
    str     w11, [x10]
    dsb     sy

    // Poll ChildrenAsleep (bit 2) until it clears.
    mov     w12, #0
.Lwake_wait:
    ldr     w11, [x10]
    tst     w11, #4
    b.eq    .Lwake_done
    add     w12, w12, #1
    // Bounded spin — if we're past ~1M iterations the RD isn't there.
    mov     w13, #0x100000
    cmp     w12, w13
    b.hs    .Lpark
    b       .Lwake_wait
.Lwake_done:

    // Step 2: put the distributor into DS=1 (single security state)
    //   mode. This makes GICR_* accessible from NS-EL2 later, and
    //   collapses the Grp0/Grp1S/Grp1NS machinery into one usable
    //   group.
    //
    //   GICD_CTLR bit 6 = DS, bit 4 = ARE, bits 0..2 = enables.
    //   Write is only effective from Secure state; NS writes are
    //   WI. Once DS=1 is set it cannot be cleared.
    movz    x10, #0x2f00, lsl #16       // GICD_BASE
    mov     w11, #0x40                   // DS
    str     w11, [x10]                   // GICD_CTLR = DS=1, enables=0
    dsb     sy

    // Step 3: permit NS-EL2 to use the ICC_* system-register interface
    //   by setting ICC_SRE_EL3.SRE + ICC_SRE_EL3.Enable. Without this
    //   programming at EL2 would UNDEF back to EL3.
    mov     x10, #(1 << 0) | (1 << 3)   // ICC_SRE_EL3.SRE | .Enable
    msr     S3_6_C12_C12_5, x10          // ICC_SRE_EL3
    isb

    // Program CNTFRQ_EL0 while we're at the highest implemented EL.
    // With has_el3=1 that's EL3; EL2 can only read CNTFRQ_EL0, not
    // write it, so the previous no-EL3 fixup in fvp_base.rs is now
    // ineffective (and UNDEFs if attempted from EL2). FVP models the
    // generic timer at 100 MHz of wall time.
    mov     x10, #0
    movk    x10, #0x05f5, lsl #16
    movk    x10, #0xe100, lsl #0         // 0x05F5E100 = 100_000_000
    msr     cntfrq_el0, x10
    isb

    // Start the System Counter. Without this CNTPCT_EL0 stays at 0
    // and everything that waits on wall time (CNTHP, any spin) hangs.
    // FVP Base RevC's CNTControlBase is at 0x2A430000; CNTCR.EN is
    // bit 0, .HDBG bit 1 stays clear. In a has_el3=1 world this is
    // the hypervisor's responsibility when no TF-A is present.
    movz    x10, #0x2a43, lsl #16        // CNTControlBase
    mov     w11, #1                       // CNTCR.EN
    str     w11, [x10]
    dsb     sy

    // Clear CPTR_EL3 so FP/SIMD/trace accesses from lower ELs don't
    // trap to EL3 (where we have no vector). The reset value on FVP
    // has RES1 bits set and the important field TFP (bit 10) clear,
    // but we zero the writable portion defensively — TF-A clears it
    // the same way. CPTR_EL3.RES1 = bits [12:8,11] plus others, but
    // writing 0 is architecturally legal (hardware keeps its RES1
    // bits).
    msr     cptr_el3, xzr
    isb

    // Step 4: program SCR_EL3 so the next ERET lands at NS-EL2 AArch64
    //   with interrupts / HVC routing configured. Bits:
    //     NS  (0)  = 1   Non-secure
    //     IRQ (1)  = 0   IRQs taken locally by EL2
    //     FIQ (2)  = 0
    //     EA  (3)  = 0
    //     SMD (7)  = 1   disable SMC at EL1/EL2
    //     HCE (8)  = 1   HVC enabled
    //     RW  (10) = 1   lower EL is AArch64
    //     API (17) = 1   don't trap pointer auth  (RES0 on v8.0, safe)
    //     APK (16) = 1
    mov     x10, #0
    orr     x10, x10, #(1 << 0)     // NS
    orr     x10, x10, #(1 << 7)     // SMD
    orr     x10, x10, #(1 << 8)     // HCE
    orr     x10, x10, #(1 << 10)    // RW
    msr     scr_el3, x10
    isb

    // Step 5: ERET to EL2h with DAIF masked. SPSR_EL3 = 0x3C9 =
    //   EL2h mode, DAIF = 1111.
    mov     x10, #0x3c9
    msr     spsr_el3, x10
    adr     x10, .Lat_el2
    msr     elr_el3, x10
    eret

.Lat_el2:
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
