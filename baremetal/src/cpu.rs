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

/// Low-power wait loop. Never returns.
pub fn halt() -> ! {
    loop {
        // SAFETY: `wfe` has no operands and no memory effects.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}
