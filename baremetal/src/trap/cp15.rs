//! CP15 (EC=0x03) trap handling: the SCTLR / TLBI / cache-op shim, the
//! `cp15` accessor sub-module, and the flash-checksum reseed hook.

use crate::{cpu, guest_mem};
use crate::diag_util::SeenSet;
use crate::trap_context::{advance_elr, read_sysreg, TrapContext};
use crate::kprintln;
use core::ptr::addr_of_mut;


pub(crate) fn log_cp15_strongarm_clock(pc: u32) {
    static mut LOG_BUDGET: usize = 2;
    // SAFETY: single-threaded.
    let ok = unsafe {
        if LOG_BUDGET > 0 {
            LOG_BUDGET -= 1;
            true
        } else {
            false
        }
    };
    if ok {
        kprintln!("und: MCR p15,0,Rt,c15,c1,2 (StrongARM clock) @PC={:#x} — no-op", pc);
    }
}

pub(crate) fn log_cp15_deprecated_cache_all(pc: u32) {
    static mut LOG_BUDGET: usize = 2;
    // SAFETY: single-threaded.
    let ok = unsafe {
        if LOG_BUDGET > 0 {
            LOG_BUDGET -= 1;
            true
        } else {
            false
        }
    };
    if ok {
        kprintln!(
            "und: MCR p15,0,Rt,c7,c7,0 (deprecated invalidate-unified-cache) @PC={:#x} — emulated as ICIALLU",
            pc
        );
    }
}

// ISS layout for EC=0x03 (trapped MCR/MRC to CP15):
//   [19:17]  Opc2
//   [16:14]  Opc1
//   [13:10]  CRn
//   [9:5]    Rt   (guest register operand)
//   [4:1]    CRm
//   [0]      Direction: 0 = write (MCR), 1 = read (MRC)
pub(crate) fn handle_cp15_trap(ctx: &mut TrapContext, iss: u32) {
    let is_read = (iss & 1) != 0;
    let _crm = ((iss >> 1) & 0xF) as u32;
    let rt = ((iss >> 5) & 0x1F) as usize;
    // ISS.Rt[9:5] names the AArch64 register operand. An AArch32
    // MCR/MRC maps Rt to X0..X14, so 31 (XZR/WZR) cannot occur here and
    // would panic on the `ctx.x[rt]` index below; halt loudly instead.
    if rt == 31 {
        kprintln!(
            "*** handle_cp15_trap: ISS.Rt == 31 (XZR) on AArch32 trap — \
             impossible; iss={:#010x} ***",
            iss,
        );
        cpu::halt();
    }
    let crn = ((iss >> 10) & 0xF) as u32;
    let opc1 = ((iss >> 14) & 0x7) as u32;
    let opc2 = ((iss >> 17) & 0x7) as u32;
    let crm = _crm;

    crate::trap_hist::record_cp15(
        crate::trap_hist::cp15_key(opc1, crn, crm, opc2, is_read),
        read_sysreg!("elr_el2") as u32,
    );

    // Budget-limited CP15 logging for bring-up diagnostics. Prints only the
    // first N unique (CRn, CRm, Opc1, Opc2, dir) tuples.
    static mut CP15_SEEN: SeenSet<u32, 32> = SeenSet::new(0);
    let key = ((is_read as u32) << 13)
        | (crn << 9)
        | (crm << 5)
        | (opc1 << 2)
        | opc2;
    // SAFETY: single-core EL2; see diag_util module docs.
    let should_log = unsafe { (*addr_of_mut!(CP15_SEEN)).first_time(key) };
    if should_log {
        let value_log = if is_read { 0 } else { ctx.x[rt] as u32 };
        let elr = read_sysreg!("elr_el2");
        kprintln!(
            "cp15: {} p15,{},Rt=r{},c{},c{},{{{}}} val={:#010x} @ELR={:#x}",
            if is_read { "MRC" } else { "MCR" },
            opc1, rt, crn, crm, opc2, value_log, elr
        );
    }

    // Dispatch on the full (opc1, CRn, CRm, opc2, dir) tuple. The
    // surface is fixed at 15 tuples for the 717006 ROM (see
    // probe/FINDINGS.md §16.4). The load-time CP15 patcher in
    // guest_mem.rs rewrites the StrongARM lax CRm=CRn encodings for
    // CRn ∈ {1,2,3,5,6} to the ARMv7 canonical CRm=0 form before the
    // guest runs, so we only see the ARMv7 encodings here; the three
    // cache and TLB groups (CRn=7, CRn=8) and the one-off StrongARM
    // clock-control write (CRn=15, CRm=1, opc2=2) keep their native
    // encodings.
    // Writes to virtual-memory CP15 regs (SCTLR/TTBR/DACR/FSR/FAR)
    // trap via HCR_EL2.TVM. Reads of the same registers are NOT
    // trapped (we don't set TRVM): the hardware already holds the
    // right values — for SCTLR/TTBR/DACR because we synced them on
    // the trapped write, for DFSR/DFAR because the CPU writes them
    // when it takes an EL1 stage-1 abort. Guest MRC reads go straight
    // to hardware and return the real values.
    //
    // Cache-maintenance (CRn=7) and TLB invalidation (CRn=8) are not
    // covered by TVM; they trap via HCR_EL2.TIDCP / TSW.
    let tuple = (opc1, crn, crm, opc2, is_read);
    match tuple {
        // --- writes to virtual-memory CP15 regs ---
        (0, 1, 0, 0, false) => {
            let value = ctx.x[rt] as u32;
            // Detect M=0→M=1 transitions and re-walk the stage-1 tables
            // then. The TTBR-write pass catches what was reachable at
            // that moment but misses coarse L1 entries populated after.
            // (ARMv4 small-page descriptors use bits[11:4] as four
            // subpage AP fields; ARMv7 reinterprets bit 9 as AP[2] and
            // bits[5:4] as AP[1:0], so entries like 0x04007F0E read as
            // AP[2:0]=100 (reserved) = no-access on A53 and writes
            // permission-fault.) Running fix on every M=1 write would
            // cost ~60k calls/sec under task switching, so we gate it
            // on the rising edge only. The rewrite is idempotent.
            let prev_sctlr = cp15::read_sctlr_el1() as u32;
            let was_off = (prev_sctlr & 1) == 0;
            let now_on = (value & 1) != 0;
            // Force SCTLR.A=1 on the guest so unaligned LDR/STR raises
            // an alignment fault at EL1. The DABT-vector trampoline
            // routes alignment faults to unaligned::handle_align_fault.
            //
            // Under BE-8 also force EE (bit 25) and E0E (bit 24) so the
            // kernel's SCTLR writes (which never set EE) don't drop us
            // back into LE data mode mid-boot. Guest-test builds keep
            // the kernel's value verbatim so LE flat-binary tests work.
            #[cfg(not(nh_guest_test))]
            let value_with_a = value | 0x2 | (1u32 << 25) | (1u32 << 24);
            #[cfg(nh_guest_test)]
            let value_with_a = value | 0x2;
            cp15::write_sctlr_el1(value_with_a as u64);
            // One-time cross-check: read SCTLR back to verify the A-bit
            // stuck on the first guest SCTLR write.
            static LOGGED_SCTLR_A_ONCE: core::sync::atomic::AtomicBool =
                core::sync::atomic::AtomicBool::new(false);
            if !LOGGED_SCTLR_A_ONCE.swap(true, core::sync::atomic::Ordering::Relaxed) {
                let readback = cp15::read_sctlr_el1() as u32;
                kprintln!(
                    "sctlr: first guest write {:#010x} → hw {:#010x} (A={}, M={}, V={})",
                    value, readback, (readback >> 1) & 1, readback & 1,
                    (readback >> 13) & 1,
                );
            }
            log_sctlr_write(value);
            if was_off && now_on {
                // Drop HCR_EL2.DC. While the guest ran with stage-1
                // off, DC=1 gave its data accesses Normal-WB semantics
                // so they hit the same cache lines the hypervisor
                // writes. But DC=1 also suppresses the guest's stage-1
                // translation from EL2's side (DDI 0487 D13.2.50):
                // leaving it set past this point means every non-
                // identity VA → IPA mapping the guest sets up (the
                // UND trampoline's save slot being the first one we
                // hit) falls through as VA=IPA and stage-2-faults.
                crate::guest::set_dc_for_stage1_off(false);
                // The XN-rewrite walks RAM[0..0x4000] interpreting it
                // as the L1 table — that's correct only when the
                // guest's TTBR0 actually points there. Guest tests
                // that pick a different L1 base (e.g. their own table
                // at 0x04004000) would otherwise have RAM[0..0x4000]
                // (stack / scratch) corrupted by the walker. Gate on
                // the live TTBR0 value.
                if (cp15::read_ttbr0_el1() as u32 & 0xFFFF_C000) == 0x0400_0000 {
                    let rom_dirty = guest_mem::fix_stage1_xn_bits();
                    guest_mem::install_scratch_pool_l1_section();
                    if rom_dirty {
                        reseed_flash_checksums_if_needed();
                    }
                }
                // No cache maintenance here: the TTBR0 write handler
                // below OR's Inner/Outer-WB cacheability bits into
                // every guest TTBR0 write, so stage-1 walks share the
                // D-cache view of the producer (kernel's own page-
                // table writes, and our in-place rewrites in
                // fix_stage1_xn_bits). Producer + walker matched-
                // attributes keeps them coherent per ARM ARM §B2.8
                // without any DC CVAC burst. See the comment block at
                // the (0, 2, 0, 0) CP15-write case for the full
                // rationale.
                maybe_dump_l1_once();
                // Swap the UND trampoline's save-slot literal to the
                // kernel VA that L1[0xC0] maps to the RAM slot. Done
                // outside `enable_patches()` so a soft-reboot that
                // cycles M=1→0→1 re-applies the swap (the tracer
                // gates its UDF install on a one-shot flag, but the
                // literal needs to track every MMU transition).
                // SAFETY: single-word ROM-backing write under the
                // paused-guest invariant.
                unsafe { guest_mem::install_und_vector_swap_post_mmu(); }
            }
            // M=1→M=0: the guest is turning its stage-1 MMU off
            // (typically the SWIBoot→ROMBoot soft-reset path). Revert
            // the UND trampoline's save-slot literal to the pre-MMU
            // RAM IPA so any UND taken before MMU re-enable lands in
            // a stage-2-mapped IPA. Without this, the first trace-UDF
            // after a soft reboot stores to VA 0x0C00_4F0C with MMU
            // off, which faults at an unmapped IPA.
            if !was_off && !now_on {
                // Soft reboot: the guest turned its stage-1 MMU off.
                // Re-enable HCR_EL2.DC so data accesses stay Normal-WB
                // cacheable while we're back in the "MMU off" regime.
                crate::guest::set_dc_for_stage1_off(true);
                // SAFETY: single-word ROM-backing write under the
                // same paused-guest invariant as the original patch.
                unsafe { guest_mem::install_und_vector_swap_pre_mmu(); }
            }
        }
        (0, 2, 0, 0, false) => {
            // The 717006 kernel writes TTBR0 = 0x0400_0000 with the
            // low 14 bits cleared — IRGN = RGN = S = 0, so stage-1
            // page-table walks are Normal **Non-cacheable**
            // Non-shareable (DDI 0406C §B4.1.154 TTBR0 layout +
            // Table B3-17 attribute encodings). The kernel populates
            // its page tables via cacheable Normal-WB mappings (DC=1
            // while MMU is off, normal sections once MMU is on);
            // fix_stage1_xn_bits also edits L2 entries in place
            // through the hypervisor's Normal-WB view. Cacheable
            // producer + Non-cacheable walker is an ARM ARM §B2.8
            // mismatched-memory-attributes case: the walker bypasses
            // the D-cache and reads stale DRAM until a DC CVAC to
            // PoC. StrongARM's simpler cache model let the Newton ROM
            // get away without any table-side cache maintenance; on
            // Cortex-A53 under FVP the first walk after SCTLR.M=1
            // fetches pre-guest zeros (or post-rewrite L2 bytes that
            // never made it to DRAM) and prefetch-aborts on whatever
            // VA it tries, wedging the guest in a PABT-vector loop.
            //
            // Rather than bursting DC CVAC over all of RAM + ROM on
            // every M=0→M=1 (which costs ~64 Ki maintenance ops and
            // still leaves later, untraceable kernel-side table
            // updates to hit the same trap), rewrite the walker
            // attributes to match the producer: OR Inner WB/WA and
            // Outer WB/WA cacheability into every guest TTBR0 write.
            // Walker and producer then share the D-cache and stay
            // coherent without any explicit maintenance — including
            // for the kernel's runtime page-table updates we don't
            // trap.
            //
            // Fields set:
            //   bit 0 IRGN[1] = 0, bit 6 IRGN[0] = 1 → IRGN = 0b01 = WB/WA
            //   bits[4:3] RGN[1:0] = 0b01           → ORGN = 0b01 = WB/WA
            // (Encoding per DDI 0406C Table B3-17.)
            // S (bit 1) left as the guest wrote it: the boot kernel
            // leaves it 0 = Non-shareable, which matches our single-
            // core stage-2 RAM mapping's effective shareability.
            //
            // Guest MRC reads of TTBR0 aren't trapped (HCR_EL2.TRVM
            // is off) so they go through to hardware and see the
            // modified value. The Newton kernel writes TTBR0 during
            // MMU setup and doesn't read it back in a way that
            // inspects cacheability bits, so the asymmetry is
            // invisible to it in practice.
            const TTBR_WB_WA: u32 = (1 << 6) | (1 << 3);
            let raw = ctx.x[rt] as u32;
            // The EL2-side stage-1 walkers (`translate_va`,
            // `fix_stage1_xn_bits`, the L1 dumpers) all assume the
            // kernel L1 table lives at the start of guest RAM
            // (0x0400_0000) per the 717006 probe, rather than reading
            // TTBR0 back. Enforce that invariant: if the Newton kernel
            // ever programs a different root, those walkers would
            // silently read the wrong table. Guest tests legitimately
            // pick their own L1 base, so the assertion is dev/ROM-only.
            #[cfg(not(nh_guest_test))]
            if (raw & 0xFFFF_C000) != 0x0400_0000 {
                kprintln!(
                    "trap: guest programmed TTBR0={:#010x} (base {:#010x}); EL2 stage-1 walkers assume 0x0400_0000",
                    raw, raw & 0xFFFF_C000,
                );
                cpu::halt();
            }
            let value = raw | TTBR_WB_WA;
            cp15::write_ttbr0_el1(value as u64);
            // First TTBR write locks in the guest's stage-1 table
            // location. Walk it once and normalise the XN / SBZ bits
            // before the guest turns stage-1 on.
            static mut TTBR_FIXED: bool = false;
            // SAFETY: single-threaded.
            let already = unsafe {
                let v = TTBR_FIXED;
                TTBR_FIXED = true;
                v
            };
            if !already && (raw & 0xFFFF_C000) == 0x0400_0000 {
                let rom_dirty = guest_mem::fix_stage1_xn_bits();
                guest_mem::install_scratch_pool_l1_section();
                if rom_dirty {
                    reseed_flash_checksums_if_needed();
                }
            }
        }
        (0, 3, 0, 0, false) => cp15::write_dacr32(ctx.x[rt]),
        (0, 5, 0, 0, false) => {
            // Guest writes to DFSR — pass through to hardware so
            // subsequent guest reads see the intended value.
            cp15::write_dfsr32(ctx.x[rt]);
        }
        (0, 6, 0, 0, false) => {
            // Guest writes to DFAR — pass through to FAR_EL1.
            cp15::write_far_el1(ctx.x[rt]);
        }

        // Cache maintenance (CRn=7). Per probe/FINDINGS.md §16.7:
        //   c7, c6, op2=0  Invalidate entire data cache
        //   c7, c6, op2=1  Clean+invalidate DC line (MVA)
        //   c7, c7, op2=0  Invalidate unified cache
        //   c7, c10, op2=1 Clean DC line (MVA)
        //   c7, c10, op2=4 Drain write buffer / DSB
        // A53 handles coherency natively for our config, so a DSB is
        // the only operation we actually need to preserve ordering
        // the guest expects. The other c7 ops are no-ops.
        (0, 7, _, _, false) => cp15::cache_maintenance_barrier(),

        // TLB invalidation (CRn=8):
        //   c8, c5, op2=0  ITLB invalidate all
        //   c8, c6, op2=1  DTLB invalidate by MVA
        //   c8, c7, op2=0  TLB invalidate all
        (0, 8, _, _, false) => cp15::invalidate_tlb(),

        // StrongARM-specific clock-control write (c15, c1, op1=0, op2=2).
        // Fires exactly once at boot; no observable effect from EL2.
        (0, 15, 1, 2, false) => { /* nop */ }

        // Guest VBAR_EL1 write (CP15 c12, c0, opc1=0, opc2=0). Needed
        // so tests that want a non-default exception-vector table can
        // install one; the real Newton ROM never writes VBAR (it uses
        // low vectors at 0).
        (0, 12, 0, 0, false) => {
            let value = ctx.x[rt] as u64;
            // SAFETY: VBAR_EL1 is writable at EL2; on ERET the guest
            // sees it as its own CP15 VBAR.
            unsafe {
                core::arch::asm!(
                    "msr vbar_el1, {}",
                    "isb",
                    in(reg) value,
                    options(nostack, preserves_flags),
                );
            }
        }

        _ => {
            // Unrecognised tuple — Phase A contract: halt loudly so
            // we model it here rather than silently returning zero /
            // dropping the write. probe/FINDINGS.md §16.4 enumerates
            // the 15 tuples 717006 uses.
            halt_unknown_cp15(is_read, opc1, crn, crm, opc2, rt, ctx);
        }
    }

    advance_elr(4);
}

fn log_sctlr_write(value: u32) {
    static mut SCTLR_N: usize = 0;
    // SAFETY: single-threaded.
    let n = unsafe { let v = SCTLR_N; SCTLR_N += 1; v };
    if n < 6 {
        let sctlr_now = cp15::read_sctlr_el1();
        kprintln!(
            "cp15.sctlr[{}] wrote {:#010x} (M={} V={} C={} I={})",
            n, value,
            value & 1,
            (value >> 13) & 1,
            (value >> 2) & 1,
            (value >> 12) & 1,
        );
        kprintln!("   SCTLR_EL1 after write = {:#018x}", sctlr_now);
    }
}

fn maybe_dump_l1_once() {
    #[cfg(feature = "log_mmu")]
    {
        static mut L1_DUMPS: usize = 0;
        // SAFETY: single-threaded.
        let n = unsafe { let v = L1_DUMPS; L1_DUMPS += 1; v };
        if n < 10 {
            guest_mem::dump_guest_l1_table();
        }
    }
}

/// Re-seed the flash ROM/REx checksums after `fix_stage1_xn_bits` has
/// modified ROM-resident L2 page tables. The original boot-time seed
/// (in main.rs) computed checksums over the unpatched ROM bytes; once
/// the kernel writes TTBR0 we walk its L1 table, find L2 tables that
/// live in ROM, and rewrite them in place to ARMv7-compatible form
/// (XN/AP/CB normalisation). That mutation invalidates the seeded
/// checksums — the kernel then sees flash[0x64..0x8C] mismatch its own
/// runtime CalculateROMREXCheckSums result and takes the heavyweight
/// `UpdateBlock0FromBlock1 → erase → rewrite` recovery path, which
/// diverges heap state and feeds the downstream "newt" UnhandledException.
/// Re-running the seed function recomputes over the post-mutation ROM
/// and overwrites flash[0x64..0x8C] so the kernel's later comparison
/// passes. Idempotent: subsequent calls (the kernel re-enables MMU on
/// every task switch) recompute the same value.
fn reseed_flash_checksums_if_needed() {
    // Idempotent: each call recomputes from the current ROM bytes and
    // writes flash[0x64..0x8C]. Subsequent calls (the kernel re-enables
    // MMU on every task switch) recompute, and any further L2-table
    // mutations in ROM get reflected before the kernel reaches the
    // checksum comparison in TReservedBlockAccessor::CheckIfRecoveryIsNeeded.
    crate::peripherals::flash::seed_rom_rex_checksums(
        guest_mem::rom_host_pa() as *const u32,
        guest_mem::ROM_SIZE,
    );
}

fn halt_unknown_cp15(is_read: bool, opc1: u32, crn: u32, crm: u32, opc2: u32, rt: usize, ctx: &TrapContext) -> ! {
    let value = if is_read { 0 } else { ctx.x[rt] as u32 };
    let elr = read_sysreg!("elr_el2");
    kprintln!();
    kprintln!("*** unhandled CP15 access halted (no silent stub per Phase A) ***");
    kprintln!(
        "  {} p15,{},Rt=r{},c{},c{},{{{}}}  val={:#010x}  @ELR={:#x}",
        if is_read { "MRC" } else { "MCR" },
        opc1, rt, crn, crm, opc2, value, elr
    );
    kprintln!(
        "  (extend handle_cp15_trap in trap.rs to service this tuple; cross-reference"
    );
    kprintln!(
        "   probe/FINDINGS.md §16.4 for the 15 tuples the 717006 ROM exercises.)"
    );
    cpu::halt();
}

// Small inline module with the raw sysreg touches, kept close to the
// dispatch above so the trap handler stays readable.
pub(crate) mod cp15 {
    // Only the write paths are used by the hypervisor: we intercept
    // guest MCRs to these CP15 registers via HCR_EL2.TVM and mirror
    // the value into the corresponding EL2 sysreg. Guest reads are
    // not trapped (we don't set TRVM) so they go straight to hardware
    // and return the current value, which is either what we synced
    // on the last trapped write (SCTLR/TTBR/DACR) or what the CPU
    // wrote on the last EL1 abort (DFSR/DFAR).

    pub fn write_sctlr_el1(v: u64) { sysreg_write!("sctlr_el1", v); sync(); }
    pub fn write_ttbr0_el1(v: u64) { sysreg_write!("ttbr0_el1", v); sync(); }
    pub fn write_dacr32(v: u64) { sysreg_write!("dacr32_el2", v); sync(); }

    /// AArch32 DFSR via DFSR32_EL2 (op0=3 op1=4 CRn=5 CRm=0 op2=0,
    /// ARM ARM D10.2.32). Both MRS and MSR to this register take an
    /// EC=0 (UNDEFINED) exception at EL2 on Cortex-A53 under QEMU
    /// raspi3b, despite the ARM ARM saying it should be accessible
    /// from EL2 AArch64 when a lower EL supports AArch32 (which
    /// ID_AA64PFR0_EL1.EL1=0x2 confirms it does). So `write_dfsr32`
    /// is a no-op — we swallow the write. The hardware maintains
    /// DFSR correctly at EL1 when it takes an abort, which is what
    /// a kernel's abort handler needs. Guest writes are rare and
    /// typically just attempts to clear the register; losing them
    /// has no functional impact since the next abort will overwrite.
    pub fn write_dfsr32(_v: u64) { /* DFSR32_EL2 MSR UNDEFs on A53 */ }

    pub fn write_far_el1(v: u64) { sysreg_write!("far_el1", v); sync(); }

    pub fn read_sctlr_el1() -> u64 { sysreg_read!("sctlr_el1") }
    pub fn read_ttbr0_el1() -> u64 { sysreg_read!("ttbr0_el1") }

    pub fn cache_maintenance_barrier() {
        // StrongARM c7 cache ops don't all map cleanly to A53 encodings
        // and A53 handles coherency natively for our configuration. A
        // `dsb ish` covers the write-buffer-drain encoding the guest
        // issues most often; the rest are no-ops.
        sync();
    }

    pub fn invalidate_tlb() {
        // SAFETY: TLBI variants are defined sysreg writes.
        unsafe {
            core::arch::asm!(
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
    }

    pub fn invalidate_icache_all() {
        // ARMv8 equivalent of ARMv4 `MCR p15, 0, Rt, c7, c7, 0`
        // (invalidate unified cache). A53 has split I/D caches with
        // broadcast; `IC IALLUIS` covers the inner-shareable domain.
        // The D-cache is handled by A53's native coherency for our
        // config, so no explicit DCCISW loop is needed here.
        // SAFETY: cache maintenance sysreg writes.
        unsafe {
            core::arch::asm!(
                "dsb ish",
                "ic ialluis",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
    }

    fn sync() {
        // SAFETY: barrier instructions only.
        unsafe {
            core::arch::asm!(
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
    }

    macro_rules! sysreg_read {
        ($reg:literal) => {{
            let v: u64;
            unsafe {
                core::arch::asm!(
                    concat!("mrs {}, ", $reg),
                    out(reg) v,
                    options(nomem, nostack, preserves_flags),
                );
            }
            v
        }};
    }
    macro_rules! sysreg_write {
        ($reg:literal, $val:expr) => {{
            unsafe {
                core::arch::asm!(
                    concat!("msr ", $reg, ", {}"),
                    in(reg) $val,
                    options(nostack, preserves_flags),
                );
            }
        }};
    }
    pub(crate) use {sysreg_read, sysreg_write};
}

// Re-export the sysreg accessors at this module's level so external
// callers keep the original `crate::trap::cp15::{invalidate_tlb,
// invalidate_icache_all}` paths (the nested `mod cp15` is an
// implementation detail of this file).
pub(crate) use cp15::{invalidate_icache_all, invalidate_tlb};
