//! Minimal GICv3 bring-up for FVP Base RevC (single CPU, runs at EL2).
//!
//! We run the model with `has_el3=1`, so `boot.s`'s EL3 stub has already
//! done the Secure-only part: it clears GICR_WAKER.ProcessorSleep on
//! this CPU's redistributor and sets GICD_CTLR.DS (single security
//! state) + ICC_SRE_EL3 so NS-EL2 can reach the GICR_* / distributor /
//! ICC_* registers at all. Everything else — ICC_SRE_EL2, the
//! distributor enables, the CPU interface, per-PPI config — is still
//! ours to program here from EL2, before any ICC_* system-register
//! access at EL1 (they UNDEF until ICC_SRE_EL2 permits them) and before
//! any GICR_* PPI-frame write. `wake_redistributor` is repeated here so
//! the sequence reads as self-contained, but on the shipped has_el3=1
//! config the RD is already awake when we arrive (it is idempotent;
//! `boot.s` is the ground truth for what the stub did).
//!
//! Init ordering (matters — deviates from Linux's order because Linux
//! assumes firmware has already woken the RD):
//!   1. `wake_redistributor` — clear ProcessorSleep, poll ChildrenAsleep.
//!      MUST come first: any ICC_* sysreg access before this is
//!      UNPREDICTABLE (FVP flags it loudly; delivery silently breaks).
//!   2. `init_cpu_if_el2`  — ICC_SRE_EL2 = SRE|Enable, ICH_HCR_EL2 = 0
//!   3. `init_distributor` — GICD_CTLR = 0, wait RWP, enable ARE_NS|G1|G1A
//!   4. `init_cpu_if_el1`  — ICC_PMR_EL1, ICC_BPR1_EL1, ICC_CTLR_EL1=0,
//!                           ICC_AP1R0_EL1=0, ICC_IGRPEN1_EL1=1
//!   5. `enable_ppi(intid)` — IGROUPR0, IPRIORITYR, ICFGR1, ISENABLER0 on
//!                           the SGI frame (RD_BASE + 0x10000)
//!
//! The arm-gic crate skips the has_el3=0 path: it assumes TF-A has done
//! step 1 and offers no EL2-side helper. We hand-roll instead.
//!
//! All ICC_* accesses go via raw `msr` / `mrs` with the S3_* encodings;
//! LLVM's AArch64 assembler accepts the short mnemonics only on newer
//! cores, so the encoded form is the portable option.

use crate::kprintln;

// ---- MMIO base addresses (FVP Base RevC default map) -----------------------

/// Distributor.
const GICD_BASE: usize = 0x2F00_0000;

/// Base of the redistributor chain. The RD for THIS CPU is located
/// by walking TYPER.Last at runtime in `find_redistributor_for_this_cpu`.
const GICR_CHAIN_BASE: usize = 0x2F10_0000;

/// RD frame discovered for this CPU. Set once in `init` so `enable_ppi`
/// and subsequent per-PPI work hit the same frame. AtomicUsize rather
/// than UnsafeCell because the read path is called from trap handlers.
static RD_BASE: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static SGI_BASE: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

// ---- GICD register offsets (only what we touch) ----------------------------

const GICD_CTLR: usize = 0x0000;
const GICD_TYPER: usize = 0x0004;
const GICD_IGROUPR: usize = 0x0080;       // per-32-INTID, index 0 is SGI/PPI (ignored for SPIs)
const GICD_ICENABLER: usize = 0x0180;
const GICD_ICACTIVER: usize = 0x0380;
const GICD_IPRIORITYR: usize = 0x0400;

const GICD_CTLR_ENABLE_G1: u32 = 1 << 0;
const GICD_CTLR_ENABLE_G1A: u32 = 1 << 1;
const GICD_CTLR_ARE_NS: u32 = 1 << 4;
const GICD_CTLR_RWP: u32 = 1 << 31;

// ---- GICR (RD frame) register offsets --------------------------------------

const GICR_CTLR: usize = 0x0000;
const GICR_TYPER: usize = 0x0008;   // 64-bit: affinity[63:32], last@bit4, VLPIS@bit1
const GICR_WAKER: usize = 0x0014;

const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const GICR_CTLR_RWP: u32 = 1 << 3;

// ---- GICR SGI-frame offsets (PPI/SGI state lives here on GICv3) ------------
//
// Same layout as the legacy GICD for INTIDs 0..31 but located on the
// redistributor so each core has its own bank.
const GICR_IGROUPR0: usize = 0x0080;
const GICR_ISENABLER0: usize = 0x0100;
const GICR_ICPENDR0: usize = 0x0280;
const GICR_ICACTIVER0: usize = 0x0380;
const GICR_IPRIORITYR: usize = 0x0400;   // byte-addressable, INTID index
const GICR_ICFGR1: usize = 0x0C04;       // PPIs (INTID 16..31); 2 bits per INTID

// ---- ICC_* system-register encodings (S3_op1_CRn_CRm_op2) ------------------
//
// LLVM accepts the short mnemonics (e.g. `icc_sre_el2`) on armv8.4+ only;
// we spell every access with the S3_* encoding so this builds on the
// stock aarch64-unknown-none-softfloat target.

macro_rules! sysreg_read_u64 {
    ($name:literal) => {{
        let v: u64;
        unsafe {
            core::arch::asm!(concat!("mrs {}, ", $name), out(reg) v,
                options(nomem, nostack, preserves_flags));
        }
        v
    }};
}
macro_rules! sysreg_write_u64 {
    ($name:literal, $val:expr) => {{
        unsafe {
            core::arch::asm!(concat!("msr ", $name, ", {}"),
                in(reg) ($val as u64),
                options(nostack, preserves_flags));
        }
    }};
}

// ---- MMIO helpers ----------------------------------------------------------

#[inline(always)]
fn read32(pa: usize) -> u32 {
    // SAFETY: GIC MMIO is identity-mapped Device-nGnRE by mmu::init().
    unsafe { core::ptr::read_volatile(pa as *const u32) }
}

#[inline(always)]
fn write32(pa: usize, v: u32) {
    // SAFETY: same mapping as read32.
    unsafe { core::ptr::write_volatile(pa as *mut u32, v) }
}

#[inline(always)]
fn write8(pa: usize, v: u8) {
    // SAFETY: IPRIORITYR tolerates byte writes on Device-nGnRE.
    unsafe { core::ptr::write_volatile(pa as *mut u8, v) }
}

#[inline(always)]
fn dsb_sy() {
    // SAFETY: barrier instruction with no memory side effect of its own.
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)); }
}

#[inline(always)]
fn isb() {
    // SAFETY: barrier instruction.
    unsafe { core::arch::asm!("isb", options(nostack, preserves_flags)); }
}

// ---- Public INTIDs for the hypervisor --------------------------------------

/// CNTHP (EL2 physical timer) is PPI INTID 26. Matches the ARM base
/// system architecture and what FVP reports via CNTHPIRQ.
pub const INTID_CNTHP: u32 = 26;

/// Spurious INTID returned by ICC_IAR1_EL1 when nothing is pending.
pub const INTID_SPURIOUS: u32 = 1023;

// ---------------------------------------------------------------------------
// Init stages
// ---------------------------------------------------------------------------

/// Step 1: program the EL2 CPU-interface gate so ICC_* EL1 sysregs are
/// live, and disable all maintenance-IRQ / LR traps we'd take via
/// ICH_HCR_EL2 (none until we wire up a vGIC). Also read-back to prove
/// a GICv3 is present — if SRE didn't stick, the System Register
/// interface isn't implemented and we'd wedge the first time we touched
/// an ICC_* register below.
fn init_cpu_if_el2() {
    const ICC_SRE_EL2_SRE: u64 = 1 << 0;
    const ICC_SRE_EL2_ENABLE: u64 = 1 << 3;

    let mut sre = sysreg_read_u64!("S3_4_C12_C9_5"); // ICC_SRE_EL2
    sre |= ICC_SRE_EL2_SRE | ICC_SRE_EL2_ENABLE;
    sysreg_write_u64!("S3_4_C12_C9_5", sre);
    isb();

    let sre_rb = sysreg_read_u64!("S3_4_C12_C9_5");
    if sre_rb & ICC_SRE_EL2_SRE == 0 {
        panic!("gicv3: ICC_SRE_EL2.SRE read back 0 — GICv3 not present?");
    }

    // ICH_HCR_EL2 = 0: no vGIC LR traps, no maintenance IRQ from us.
    sysreg_write_u64!("S3_4_C12_C11_0", 0u64);
    isb();
}

/// Poll the distributor or redistributor RWP (register-write-pending)
/// bit until it clears, with a bounded spin so a wedged GIC panics
/// instead of hanging forever.
fn wait_bit_clear(pa: usize, bit: u32, what: &str) {
    let mut spins: u32 = 0;
    while read32(pa) & bit != 0 {
        spins = spins.wrapping_add(1);
        if spins > 10_000_000 {
            panic!("gicv3: {} RWP timeout at {:#x}", what, pa);
        }
    }
}

/// Step 2: disable the distributor, zero all SPI enables / pending /
/// active, set all SPI INTIDs to Group 1 NS with a moderate priority,
/// then enable it with ARE_NS | Grp1 | Grp1A. FVP only has ~224 SPIs
/// (GICD_TYPER.ITLinesNumber = 6 → 32 × (6+1) = 224 INTIDs total),
/// we read TYPER so we don't write off the end on smaller configs.
fn init_distributor() {
    write32(GICD_BASE + GICD_CTLR, 0);
    wait_bit_clear(GICD_BASE + GICD_CTLR, GICD_CTLR_RWP, "GICD_CTLR");

    let typer = read32(GICD_BASE + GICD_TYPER);
    let it_lines = ((typer & 0x1F) + 1) * 32; // max INTID + 1

    // SPIs start at INTID 32. Walk 32-INTID-wide banks.
    let mut i: u32 = 32;
    while i < it_lines {
        let off = (i / 32) as usize * 4;
        write32(GICD_BASE + GICD_ICENABLER + off, !0);
        write32(GICD_BASE + GICD_ICACTIVER + off, !0);
        write32(GICD_BASE + GICD_IGROUPR + off, !0);
        i += 32;
    }
    // Moderate priority (0xA0) on every SPI byte. Lower numeric value =
    // higher priority; 0xA0 sits comfortably below PMR=0xF0.
    let mut j: u32 = 32;
    while j < it_lines {
        write8(GICD_BASE + GICD_IPRIORITYR + j as usize, 0xA0);
        j += 1;
    }

    let ctlr = GICD_CTLR_ARE_NS | GICD_CTLR_ENABLE_G1 | GICD_CTLR_ENABLE_G1A;
    write32(GICD_BASE + GICD_CTLR, ctlr);
    wait_bit_clear(GICD_BASE + GICD_CTLR, GICD_CTLR_RWP, "GICD_CTLR (enable)");
}

/// Walk the redistributor chain via GICR_TYPER.Last (bit 4) looking for
/// the RD frame whose affinity matches this CPU's MPIDR_EL1. Each RD
/// frame is 128 KiB on GICv3 (the RD + SGI pages), or 256 KiB on
/// GICv4 (RD + SGI + VLPI_base + reserved). We don't support VLPIs
/// ourselves, but GICv4-capable redistributors still use 256 KiB per
/// frame, so probe TYPER.VLPIS (bit 1) for the correct stride.
fn find_redistributor_for_this_cpu() -> usize {
    let mpidr: u64;
    // SAFETY: read-only sysreg.
    unsafe {
        core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr,
            options(nomem, nostack, preserves_flags));
    }
    // Affinity is stored in GICR_TYPER[63:32] as Aff3|Aff2|Aff1|Aff0
    // matching MPIDR_EL1[39:32]|[23:0].
    let want_aff = ((mpidr & 0xFF_0000_0000) >> 8) | (mpidr & 0x00FF_FFFF);

    let mut base = GICR_CHAIN_BASE;
    loop {
        // SAFETY: Device-nGnRE MMIO, 64-bit TYPER read.
        let typer = unsafe { core::ptr::read_volatile((base + GICR_TYPER) as *const u64) };
        let aff = (typer >> 32) & 0xFFFF_FFFF;
        if aff == want_aff {
            return base;
        }
        if typer & (1 << 4) != 0 {
            panic!("gicv3: no RD for MPIDR {:#x} (aff={:#x})", mpidr, want_aff);
        }
        // Stride: 128 KiB on v3, 256 KiB on v4. VLPIS = bit 1.
        base += if typer & (1 << 1) != 0 { 0x4_0000 } else { 0x2_0000 };
    }
}

/// Step 1: clear GICR_WAKER.ProcessorSleep on THIS CPU's redistributor
/// and wait for the children-asleep flag to drop. After this the SGI/
/// PPI frame accepts writes and any ICC_* sysreg access is defined.
fn wake_redistributor(rd_base: usize) {
    let waker = rd_base + GICR_WAKER;
    let before = read32(waker);
    let mut v = before;
    v &= !GICR_WAKER_PROCESSOR_SLEEP;
    write32(waker, v);

    let mut spins: u32 = 0;
    while read32(waker) & GICR_WAKER_CHILDREN_ASLEEP != 0 {
        spins = spins.wrapping_add(1);
        if spins > 10_000_000 {
            panic!(
                "gicv3: RD@{:#x} ChildrenAsleep never cleared (waker before={:#x}, after={:#x})",
                rd_base, before, read32(waker)
            );
        }
    }
    dsb_sy();
}

/// Step 4: CPU interface via the ICC_* EL1 aliases. EL2 sees these as
/// the physical CPU-interface registers (since ICH_HCR_EL2 is 0).
///  - PMR = 0xF0 → accept any priority < 0xF0 (we use 0xA0 on PPIs).
///  - BPR1 = 0 → no pre-emption grouping.
///  - CTLR = 0 → EOImode=0 (EOIR1 both drops priority and deactivates).
///  - AP1R0 = 0 → clear preempted-active-priority state.
///  - IGRPEN1 = 1 → enable Group 1 NS delivery to the CPU.
fn init_cpu_if_el1() {
    sysreg_write_u64!("S3_0_C4_C6_0", 0xF0u64);        // ICC_PMR_EL1
    sysreg_write_u64!("S3_0_C12_C12_3", 0u64);         // ICC_BPR1_EL1
    sysreg_write_u64!("S3_0_C12_C12_4", 0u64);         // ICC_CTLR_EL1
    sysreg_write_u64!("S3_0_C12_C9_0", 0u64);          // ICC_AP1R0_EL1
    isb();
    sysreg_write_u64!("S3_0_C12_C12_7", 1u64);         // ICC_IGRPEN1_EL1
    isb();
}

/// Configure and enable a PPI (INTID 16..31) on CPU0's redistributor.
/// Puts it in Group 1 NS at priority 0xA0 and marks it level-sensitive
/// (CNTHP asserts while CNTPCT_EL0 >= CNTHP_CVAL_EL2).
pub fn enable_ppi(intid: u32) {
    assert!((16..32).contains(&intid), "enable_ppi: {} is not a PPI", intid);
    let rd = RD_BASE.load(core::sync::atomic::Ordering::Acquire);
    let sgi = SGI_BASE.load(core::sync::atomic::Ordering::Acquire);
    assert!(rd != 0 && sgi != 0, "enable_ppi called before gicv3::init");

    // Group: bit in GICR_IGROUPR0.
    let mut g = read32(sgi + GICR_IGROUPR0);
    g |= 1 << intid;
    write32(sgi + GICR_IGROUPR0, g);

    // Priority: one byte per INTID.
    write8(sgi + GICR_IPRIORITYR + intid as usize, 0xA0);

    // ICFGR1 covers INTIDs 16..31; two bits each, bit[1]=1 → edge,
    // bit[1]=0 → level. CNTHP is level-triggered.
    let mut cfg = read32(sgi + GICR_ICFGR1);
    let shift = (intid - 16) * 2;
    cfg &= !(0b11 << shift);
    write32(sgi + GICR_ICFGR1, cfg);

    // Clear any stale pending/active state before enabling.
    write32(sgi + GICR_ICPENDR0, 1 << intid);
    write32(sgi + GICR_ICACTIVER0, 1 << intid);

    // Enable.
    write32(sgi + GICR_ISENABLER0, 1 << intid);

    // Wait for GICR_CTLR.RWP so the enable is live before the caller
    // arms the underlying timer/peripheral.
    wait_bit_clear(rd + GICR_CTLR, GICR_CTLR_RWP, "GICR_CTLR");
    dsb_sy();
}

/// ACK the highest-priority pending Group-1 interrupt and return its
/// INTID. Called from `trap_irq` at the top of the handler. Returns
/// `INTID_SPURIOUS` (1023) when nothing is pending.
pub fn ack() -> u32 {
    let iar = sysreg_read_u64!("S3_0_C12_C12_0"); // ICC_IAR1_EL1
    (iar & 0xFF_FFFF) as u32
}

/// Signal end-of-interrupt for an INTID previously returned by `ack`.
/// No-op for `INTID_SPURIOUS`. With ICC_CTLR_EL1.EOImode=0 this both
/// drops the running priority and deactivates the INTID.
pub fn eoi(intid: u32) {
    if intid == INTID_SPURIOUS {
        return;
    }
    sysreg_write_u64!("S3_0_C12_C12_1", intid as u64); // ICC_EOIR1_EL1
}

/// Full GICv3 bring-up. Idempotent in the sense that calling it twice
/// on a live system is safe (RWP polls, no blind OR into stale state),
/// but there's no reason to — call once from the FVP platform init.
pub fn init() {
    let rd = find_redistributor_for_this_cpu();
    let sgi = rd + 0x1_0000;
    RD_BASE.store(rd, core::sync::atomic::Ordering::Release);
    SGI_BASE.store(sgi, core::sync::atomic::Ordering::Release);

    // Idempotent wake — the EL3 stub in boot.s already clears
    // ProcessorSleep from Secure, but doing it again from NS-EL2 in
    // DS=1 mode is harmless and keeps the init readable as a
    // self-contained sequence.
    wake_redistributor(rd);
    init_cpu_if_el2();
    init_distributor();
    init_cpu_if_el1();
    kprintln!(
        "gicv3: initialized (GICD {:#x}, RD {:#x}, SGI frame {:#x})",
        GICD_BASE, rd, sgi
    );
}
