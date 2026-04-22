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

// NOTE: There is no `read_sp_abt()` helper. `MRS <Xt>, SP_abt`
// (S3_4_C4_C1_1) is architecturally defined (DDI 0487 D19.2) but
// QEMU raspi3b's Cortex-A53 model takes an EC=0 UNDEFINED trap at
// EL2 on it, matching the same "AArch32 banked register accessors
// from AArch64 are unreliable" limitation that forces the UND
// trampoline's SVC bounce and the DFSR32_EL2 no-op in cp15::write_dfsr32.
// The shadow-stub abort injection therefore preserves SP_abt through
// AArch32 (an MSR CPSR mode-switch inside the trampoline, which
// doesn't touch any bank's SP) rather than reading it out of band.
// See `trap::install_abt_trampoline` for the 8-instruction stub.

/// Low-power wait loop. On a hypervisor tripwire we also ask QEMU to
/// exit via semihosting so the caller isn't left waiting on an
/// external `timeout`. If semihosting isn't available we fall through
/// to the WFE loop.
///
/// Semihosting SYS_EXIT_EXTENDED (op 0x20): x1 → [reason, exit_code].
/// Reason `0x20026` = ADP_Stopped_ApplicationExit.
pub fn halt() -> ! {
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
