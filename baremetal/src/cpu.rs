//! Minimal AArch64 system-register helpers. Will grow; v1 only needs
//! CurrentEL and a WFE halt.

/// Read the current exception level. Returns 0, 1, 2, or 3.
#[inline]
pub fn current_el() -> u32 {
    let el: u64;
    // SAFETY: `mrs CurrentEL` is unprivileged and has no side effects.
    unsafe {
        core::arch::asm!(
            "mrs {}, CurrentEL",
            out(reg) el,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((el >> 2) & 0x3) as u32
}

/// Read MPIDR_EL1 affinity level 0 (the "core ID" for Cortex-A53).
#[inline]
pub fn core_id() -> u32 {
    let v: u64;
    // SAFETY: `mrs MPIDR_EL1` is readable at EL1+ and has no side effects.
    unsafe {
        core::arch::asm!(
            "mrs {}, MPIDR_EL1",
            out(reg) v,
            options(nomem, nostack, preserves_flags),
        );
    }
    (v & 0xff) as u32
}

// ---- EL2 stack overflow guard ---------------------------------------
//
// The EL2 stack is a fixed 16 KiB region directly above `.bss` (see the
// linker scripts). `with_irqs_unmasked` permits one level of IRQ
// nesting on it, and `pi_fb::push_blit`'s bilinear loop plus
// embedded-sdmmc frames run there too, so an overflow is plausible and
// would silently corrupt whatever lands at the top of `.bss`. The
// lowest 8 bytes of the stack region hold a guard canary, seeded to
// `STACK_GUARD_MAGIC` by `boot.s`. `check_stack_guard` re-reads it; if
// the stack has descended into the canary the word no longer matches
// and we loud-halt.

extern "C" {
    static __stack_guard: u8;
}

/// Canary value seeded at `__stack_guard` by `boot.s`. Must match the
/// literal built there (movz/movk sequence in `.Lat_el2`).
pub const STACK_GUARD_MAGIC: u64 = 0x5354_4B47_5541_5244; // "STKGUARD"

/// Read the stack-guard canary word.
#[inline]
fn stack_guard_word() -> u64 {
    // SAFETY: `__stack_guard` is a valid 8-byte slot inside the image's
    // stack region; the read is aligned (the stack region is page
    // aligned) and side-effect free.
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(__stack_guard) as *const u64) }
}

/// Verify the EL2 stack hasn't overflowed into its guard canary. On
/// corruption, loud-halt with a context line — letting execution
/// continue would mean trusting a stack that has already clobbered
/// adjacent state. Cheap enough (one aligned load + compare) to call
/// from the timer-IRQ path and the halt paths.
#[inline]
pub fn check_stack_guard() {
    let w = stack_guard_word();
    if w != STACK_GUARD_MAGIC {
        // Use kprintln (masks IRQs around its own critical section) and
        // then spin without re-checking the guard.
        crate::kprintln!(
            "*** EL2 STACK OVERFLOW: guard canary = {:#018x}, expected {:#018x} \
             — the EL2 stack has descended into its guard word; halting.",
            w,
            STACK_GUARD_MAGIC,
        );
        halt();
    }
}

/// Generate a `read_<reg>()` helper for a system register.
macro_rules! read_sysreg {
    ($name:ident, $reg:literal) => {
        #[inline]
        pub fn $name() -> u64 {
            let v: u64;
            // SAFETY: reading an ID / feature register has no side effects.
            unsafe {
                core::arch::asm!(
                    concat!("mrs {}, ", $reg),
                    out(reg) v,
                    options(nomem, nostack, preserves_flags),
                );
            }
            v
        }
    };
}

read_sysreg!(id_aa64pfr0_el1, "ID_AA64PFR0_EL1");
read_sysreg!(id_aa64mmfr0_el1, "ID_AA64MMFR0_EL1");
read_sysreg!(id_aa64mmfr1_el1, "ID_AA64MMFR1_EL1");
read_sysreg!(id_aa64isar0_el1, "ID_AA64ISAR0_EL1");
read_sysreg!(midr_el1, "MIDR_EL1");
read_sysreg!(hcr_el2, "HCR_EL2");

/// Invalidate one icache line by virtual address (`IC IVAU`) and fence.
/// The VA must be accessible to EL2 — on our setup EL2's stage-1 identity
/// map covers the host view of the ROM backing, so passing the host VA
/// of the patched word works. A53's icache is PIPT so invalidating via
/// the host VA invalidates any guest alias to the same PA.
#[inline]
pub fn ic_ivau(va: u64) {
    // SAFETY: cache maintenance; `dsb ish` + `isb` fence the effect.
    unsafe {
        core::arch::asm!(
            "dc cvau, {va}",
            "dsb ish",
            "ic ivau, {va}",
            "dsb ish",
            "isb",
            va = in(reg) va,
            options(nostack, preserves_flags),
        );
    }
}

/// I-cache–coherent publish of a freshly-written code range.
///
/// After the host CPU writes instruction bytes through a Normal-WB
/// mapping (e.g. `guest_mem::load_*` populating the ROM backing, or any
/// inline patcher), those writes sit in the data cache and are not
/// visible to the I-cache fetch path on cores whose I/D caches are
/// non-coherent (Cortex-A53, AEM v8-A — i.e. both QEMU raspi3b and FVP
/// Base RevC).
///
/// This walks the range one cache line at a time and issues
/// `DC CVAU; DSB ISH; IC IVAU; DSB ISH; ISB` per line. Cache line size
/// hard-coded to 64 bytes — the value for both A53 and AEMvA. (If we
/// ever target a part with a different `CTR_EL0.IminLine`, query CTR
/// for line size instead.)
pub fn icache_publish_range(va: u64, len: usize) {
    const LINE: u64 = 64;
    let start = va & !(LINE - 1);
    let end = (va + len as u64 + LINE - 1) & !(LINE - 1);
    let mut p = start;
    while p < end {
        ic_ivau(p);
        p += LINE;
    }
}

/// Clean + invalidate a range of data cache lines to the Point of
/// Coherency.
///
/// Used for buffers that a non-coherent agent (e.g. the BCM2710
/// VideoCore via the mailbox) reads or writes via an uncached alias.
/// Our own access goes through Normal-WB DRAM; without this the VC
/// would see stale data on its reads and our subsequent reads would
/// see stale data on its writes.
///
/// `va` is the buffer base; `len` its size in bytes. We round outward
/// to the nearest cache-line boundary (64 B on A53/AEMvA). For the
/// *outbound* direction (clean before the doorbell) the rounding is
/// harmless: any adjacent data sharing the end lines is simply
/// written back too. For the *inbound* direction (invalidate after a
/// device wrote the buffer) it is NOT harmless if the buffer's end
/// lines are shared — `dc civac` cleans before it invalidates, so a
/// dirty adjacent byte on a shared line would be written back over
/// the device's freshly-DMA'd bytes. Buffers a device writes into
/// must therefore be cache-line aligned AND line-padded so their
/// lines are private (see `mailbox::Buffer`, `MaiTxRing`). `dsb sy`
/// fences the effect against subsequent device-MMIO writes (mailbox
/// doorbell).
#[allow(dead_code)] // First caller lands in src/mailbox.rs.
pub fn dc_civac_range(va: u64, len: usize) {
    const LINE: u64 = 64;
    let start = va & !(LINE - 1);
    let end = (va + len as u64 + LINE - 1) & !(LINE - 1);
    let mut p = start;
    while p < end {
        // SAFETY: cache maintenance op; `dsb sy` below fences against
        // the device-MMIO write that follows.
        unsafe {
            core::arch::asm!(
                "dc civac, {p}",
                p = in(reg) p,
                options(nostack, preserves_flags),
            );
        }
        p += LINE;
    }
    // SAFETY: barrier only; no state side-effects.
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}


/// Spin until at least `ms` ms have elapsed by CNTPCT_EL0. Used by
/// the DWC2 driver for spec-mandated reset / settle delays (e.g.
/// USB 2.0 `tDRSTR` = 50 ms after asserting port reset). The timer
/// is always running by the time anything calls this — `boot.s`
/// programs `CNTFRQ_EL0` and the generic timer is on out of reset.
/// The USB stack is its only consumer, so it's gated with it.
#[cfg(nh_input_mtouch)]
pub fn delay_ms(ms: u32) {
    let freq: u64;
    let start: u64;
    // SAFETY: sysreg reads, side-effect free.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) start,
            options(nomem, nostack, preserves_flags));
    }
    let target = start.wrapping_add((freq * ms as u64) / 1000);
    loop {
        let now: u64;
        // SAFETY: sysreg read.
        unsafe {
            core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
                options(nomem, nostack, preserves_flags));
        }
        if now.wrapping_sub(target) as i64 >= 0 {
            return;
        }
    }
}

/// Unmask EL2 physical IRQs (clear PSTATE.I) for the remainder of the
/// current execution context. Used once from `kmain` after the IRQ
/// sources we drive (BCM2835 DMA channels feeding UART TX and the HDMI
/// MAI ring, and later CNTHP) are wired up: from that point on their
/// completions arrive as real interrupts into `trap::irq_from_el2`
/// instead of being polled cooperatively, which is what lets a long
/// EL2 operation (e.g. the 5-second SD flash load) run without
/// starving the audio ring.
///
/// EL2's PSTATE.I set here only governs EL2 execution. Once the guest
/// is entered, ERET loads the guest's PSTATE; every subsequent EL2
/// entry is an exception handler, which enters with I masked again.
#[inline]
pub fn unmask_irqs_el2() {
    // SAFETY: flipping PSTATE.I only; the EL2 vector table is installed
    // and `trap::irq_from_el2` is safe to run nested in any EL2 context.
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nostack, preserves_flags));
    }
}

/// Run `f` with EL2 physical IRQs unmasked, restoring the prior DAIF
/// state afterwards. Wraps a long-running EL2 operation that executes
/// in trap-handler context (where IRQs are masked on entry) so that
/// DMA-completion and timer IRQs are serviced by `trap::irq_from_el2`
/// while `f` runs — keeping the HDMI audio ring fed and CNTHP rearmed
/// even when `f` blocks for hundreds of milliseconds.
///
/// ## Why ELR_EL2 / SPSR_EL2 are snapshotted
///
/// `save_context` in `vectors.s` spills only x0..x30, not ELR_EL2 /
/// SPSR_EL2. A nested exception (the IRQ taken inside `f`) clobbers
/// those sysregs, and the surrounding handler's
/// `restore_context_and_eret` ERETs on whatever they hold. Without
/// restoring them, ERET would jump to the post-IRQ EL2 state instead
/// of back to the guest. We re-mask IRQs *before* writing them back so
/// a late IRQ can't clobber them again after the restore. This
/// generalizes the manual snapshot/restore that `pause_system` does
/// around its WFI loop.
///
/// ## Caller invariants
///
/// - `f` must not touch any state that `trap::irq_from_el2` touches
///   (the VIC tick/match state, host_dma channel CS registers, the
///   uart TX ring tail, the audio MAI/stereo rings, or vic::raise) —
///   a nested IRQ may mutate it concurrently. kprintln is safe (it
///   masks IRQs around its own critical section).
/// - Only call after the surrounding handler has finished reading
///   ESR_EL2 / FAR_EL2: a nested exception entry overwrites those too.
pub fn with_irqs_unmasked<R>(f: impl FnOnce() -> R) -> R {
    let saved_elr: u64;
    let saved_spsr: u64;
    let saved_daif: u64;
    // SAFETY: sysreg reads, side-effect free.
    unsafe {
        core::arch::asm!(
            "mrs {0}, elr_el2",
            "mrs {1}, spsr_el2",
            "mrs {2}, daif",
            out(reg) saved_elr,
            out(reg) saved_spsr,
            out(reg) saved_daif,
            options(nomem, nostack, preserves_flags),
        );
        core::arch::asm!("msr daifclr, #2", options(nostack, preserves_flags));
    }

    let r = f();

    // Restore DAIF to its saved value (composes with already-unmasked
    // or nested windows) BEFORE restoring ELR/SPSR, so a late IRQ
    // taken during this window can't clobber them after the write.
    // SAFETY: writing DAIF / ELR_EL2 / SPSR_EL2 from EL2 is allowed;
    // the surrounding handler's ERET will consume the restored values.
    unsafe {
        core::arch::asm!(
            "msr daif, {0}",
            "msr elr_el2, {1}",
            "msr spsr_el2, {2}",
            in(reg) saved_daif,
            in(reg) saved_elr,
            in(reg) saved_spsr,
            options(nostack, preserves_flags),
        );
    }

    r
}

// NOTE: There is no `read_sp_abt()` helper. `MRS <Xt>, SP_abt`
// (S3_4_C4_C1_1) is architecturally defined (DDI 0487 D19.2) but
// QEMU raspi3b's Cortex-A53 model takes an EC=0 UNDEFINED trap at
// EL2 on it, matching the same "AArch32 banked register accessors
// from AArch64 are unreliable" limitation that forces the UND
// trampoline's SVC bounce and the DFSR32_EL2 no-op in cp15::write_dfsr32.

/// Low-power wait loop. On a hypervisor tripwire we also ask QEMU to
/// exit via semihosting so the caller isn't left waiting on an
/// external `timeout`. If semihosting isn't available we fall through
/// to the WFE loop.
///
/// Semihosting SYS_EXIT_EXTENDED (op 0x20): x1 → [reason, exit_code].
/// Reason `0x20026` = ADP_Stopped_ApplicationExit.
pub fn halt() -> ! {
    // On `no-semihost` builds (real silicon) there is no semihosting
    // host listening for SYS_EXIT_EXTENDED; HLT would either NOP or
    // generate an unintended debug exception. Fall through directly to
    // the WFE loop, which is what the comment above promises anyway.
    #[cfg(not(feature = "no-semihost"))]
    // SAFETY: HLT #0xF000 with semihosting enabled in QEMU is a
    // controlled trap that terminates QEMU. The parameter block
    // pointer lifetime spans the call.
    unsafe {
        let params: [u64; 2] = [0x20026, 1];
        core::arch::asm!(
            "hlt #0xF000",
            in("x0") 0x20u64,
            in("x1") params.as_ptr() as u64,
            options(nostack, preserves_flags),
        );
    }
    loop {
        // SAFETY: `wfe` has no operands and no memory effects.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}
