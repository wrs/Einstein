//! `NewtonOs` — the [`crate::hv::hooks::GuestOs`] implementation for
//! Newton OS 2.x. The hook bodies here are the Newton-specific halves
//! of the hv core's trap tails, MMU rituals, and probe dispatch; the
//! hv side calls them through `hooks::ActiveGuest::…` (static, no dyn).
//!
//! Host-backend entry points (input pumps, audio tick, splash
//! progress) are consumed through [`HostPumpOps`] installed from
//! `main.rs` — the newton layer may use arch / hv / peripherals but
//! not host, so the host fns arrive by registration, mirroring the
//! `peripherals::sound::AudioOps` idiom.

use crate::arch::cpu;
use crate::arch::trap_context::{read_sysreg, TrapContext};
use crate::diag::diag_util::SeenSet;
use crate::diag::trap_diag::handle_diag;
use crate::hv::guest_mem;
use crate::hv::guest_mem::{read_pt_entry, write_pt_entry};
use crate::hv::hooks::{GuestOs, UndHvcOutcome};
use crate::hv::hvc_imm::HvcImm;
use crate::hv::trap::und::read_banked_spsr;
use crate::peripherals::{native_primitives, vic};
use crate::{dprintln, kprintln};
use core::ptr::addr_of_mut;

use super::guest_trampolines;
use super::probes::{self, ThunkKind};
use super::rom_patches;
use super::unaligned;

/// The Newton OS 2.x guest. Zero-sized; every hook is an associated
/// fn so `hooks::ActiveGuest::…` monomorphizes to direct calls.
pub struct NewtonOs;

// ---------------------------------------------------------------------
// Host-service pumps (installed from main.rs)
// ---------------------------------------------------------------------

/// Host entry points the trap-tail hooks drive. Installed once from
/// `main.rs` boot wiring; the no-op defaults only matter for the
/// window before wiring, during which the guest cannot be running.
pub struct HostPumpOps {
    /// host-io backend input pump (drain viewer pen events into the
    /// queue and raise INT_TABLET).
    pub host_io_pump_input: fn(),
    /// Real-hw input-source pump (USB touchscreen) — feeds the same
    /// queue as the host-io pump.
    pub input_pump: fn(),
    /// Audio backend tick: the null backend fires armed buffer-
    /// completion IRQs here once playback duration has elapsed.
    pub audio_tick: fn(),
    /// Boot-splash progress advance; called with the sync-trap count.
    #[cfg(all(feature = "platform-raspi3b", nh_host_io_pi_fb))]
    pub splash_progress: fn(u64),
}

fn noop() {}

struct HostPumpCell(core::cell::UnsafeCell<HostPumpOps>);
// SAFETY: written once by `install_host_pumps` from kmain on core 0
// before the guest runs; read-only afterwards from the single EL2
// trap handler.
unsafe impl Sync for HostPumpCell {}

static HOST_PUMPS: HostPumpCell = HostPumpCell(core::cell::UnsafeCell::new(HostPumpOps {
    host_io_pump_input: noop,
    input_pump: noop,
    audio_tick: noop,
    #[cfg(all(feature = "platform-raspi3b", nh_host_io_pi_fb))]
    splash_progress: |_| {},
}));

/// Install the host pump entry points. Called once from `main.rs`.
pub fn install_host_pumps(ops: HostPumpOps) {
    // SAFETY: single-core EL2, called before the guest runs.
    unsafe {
        *HOST_PUMPS.0.get() = ops;
    }
}

fn host_pumps() -> &'static HostPumpOps {
    // SAFETY: see HostPumpCell.
    unsafe { &*HOST_PUMPS.0.get() }
}

// ---------------------------------------------------------------------
// Tick-page update logic (VIC advance/poll + publish)
// ---------------------------------------------------------------------

/// Sync-trap path: advance synthetic ticks, poll match crossings, and
/// republish the non-trapping tick / calendar registers. The
/// `tick_advance` here is what makes the tick rate track guest
/// progress rather than wall clock — see `vic::SYNTH_TICKS`.
fn tick_page_update_from_sync_trap() {
    vic::tick_advance_sync_trap();
    tick_page_publish();
}

/// Poll match/alarm crossings and republish the current tick +
/// calendar values into the stage-2 tick page (`stage2::tick_page`
/// owns the table mechanics: BE-8 encode, volatile write, cache
/// clean). The heartbeat path calls this WITHOUT advancing ticks (so
/// the heartbeat can detect "no guest progress" by SYNTH_TICKS being
/// unchanged); forward-progress fast-forward is handled in
/// `vic::heartbeat_tick_update`.
fn tick_page_publish() {
    vic::poll_timer_matches();
    vic::poll_alarm();
    crate::hv::stage2::tick_page::publish(vic::ticks(), vic::calendar_seconds());
}

/// Boot-time tick-page seed. Called twice from `main.rs`: once after
/// `stage2::init` (so any read before the first timer IRQ returns
/// something non-zero-but-consistent) and once after `vic::init` (to
/// re-publish now that `calendar_seconds()` returns a real value —
/// the first seed ran while the calendar baseline was still zero).
pub fn seed_tick_page() {
    tick_page_update_from_sync_trap();
}

// ---------------------------------------------------------------------
// Stage-1 MMU ritual helpers
// ---------------------------------------------------------------------

/// Walk the guest's stage-1 L1 table at TTBR=0x0400_0000 and, for every
/// coarse L2 table we can reach, clear the XN (execute-never) bit on
/// entries whose type field is large/small page.
///
/// Rationale: ARMv4 second-level descriptors treat bit 15 as SBZ, but
/// ARMv7/v8 short-descriptor re-interpret the same bit as XN. The
/// 717006 ROM's prebuilt L2 tables happen to have bit 15 set in many
/// entries, so A53's stage-1 walker treats the corresponding ROM code
/// pages as non-executable and every instruction fetch aborts.
///
/// We walk the tables once, when the guest first writes TTBR0 (CP15
/// c2 c0 0). Tables in ROM are modified via our backing store — guests
/// see ROM as stage-2 read-only, but from EL2 we own the bytes.
///
/// Returns `true` iff this call actually wrote bytes into the ROM
/// backing store (an L2 entry inside ROM was rewritten). The flash
/// ROM/REx checksums only need re-seeding when ROM has changed, so
/// callers gate `reseed_flash_checksums_if_needed` on the return.
fn fix_stage1_xn_bits() -> bool {
    let ram = guest_mem::ram_host_pa() as *mut u32;
    let rom = guest_mem::rom_host_pa() as *mut u32;

    let mut rom_writes = 0usize;

    let scratch_l1_idx = (crate::hv::layout::SCRATCH_POOL_VA >> 20) as usize;

    // L1 sits at the start of guest RAM (TTBR0 = 0x0400_0000 per probe).
    for i in 0..4096 {
        // Skip the shadow-stub scratch L1 slot — it's owned by
        // `install_scratch_pool_l1_section`, which installs a section
        // with XN=1. The section-normalisation block below would clear
        // XN every M-toggle, forcing the installer to re-set it on
        // each task switch. Leave the slot alone; the installer
        // handles it.
        if i == scratch_l1_idx {
            continue;
        }

        // SAFETY: L1 is 16 KiB = 4096 × 4 bytes, at RAM[0..16384].
        let entry = unsafe { read_pt_entry(ram.add(i)) };
        let typ = entry & 3;

        // Rewrite fine-table (0b11) descriptors to fault (0b00). The ARMv4
        // fine-table format was dropped in ARMv7 short descriptors; A53's
        // walker treats it as UNPREDICTABLE. The 717006 ROM installs three
        // fine-table L1 entries at VA 0x78000000 / 0x90000000 / 0xAC000000
        // as PCMCIA placeholders whose L2 slots are all fault (see
        // probe/FINDINGS.md). Converting to an L1 fault preserves intent:
        // any access to those VAs must raise a stage-1 translation fault
        // our abort handler can dispatch.
        if typ == 3 {
            // SAFETY: i < 4096.
            unsafe { write_pt_entry(ram.add(i), 0); }
            continue;
        }

        // Normalise section descriptor to minimal-valid ARMv7 form:
        // preserve PA (bits 31:20) + domain (8:5), clear XN/AP[2]/TEX/S/nG,
        // force AP[1:0] = 0b11 (RW both levels) + C/B = 1.
        if typ == 2 {
            let new = (entry & 0xFFF0_01E0) | 0x0000_0C0E;
            if new != entry {
                // SAFETY: i < 4096.
                unsafe { write_pt_entry(ram.add(i), new); }
            }
        }

        // Normalise coarse descriptor: preserve L2 ptr (bits 31:10) + domain
        // (8:5), clear the ARMv4 SBO bits (4) and NS (3).
        if typ == 1 {
            let new = (entry & 0xFFFF_FC00) | (entry & 0x0000_01E0) | 0x01;
            if new != entry {
                // SAFETY: i < 4096.
                unsafe { write_pt_entry(ram.add(i), new); }
            }
        }

        if typ != 1 {
            continue; // only coarse L2 tables for the XN-on-page-entries pass
        }
        let l2_pa = (entry & 0xFFFF_FC00) as usize;
        // Pick backing store pointer by region.
        let (base, region_start, region_size) = if l2_pa < guest_mem::ROM_SIZE {
            (rom, 0usize, guest_mem::ROM_SIZE)
        } else if (0x04000000..0x04000000 + guest_mem::RAM_SIZE as u64)
            .contains(&(l2_pa as u64))
        {
            (ram, 0x04000000usize, guest_mem::RAM_SIZE)
        } else {
            continue;
        };
        let is_rom = region_start == 0;
        let l2_idx_start = (l2_pa - region_start) / 4;
        if l2_idx_start + 256 > region_size / 4 {
            continue;
        }

        // Coarse L2 has 256 entries, each 4 bytes. Rewrite each non-fault
        // entry into minimal valid ARMv7 form: preserve the PA, force
        // AP = 0b11 (RW both levels), C = B = 1, XN = 0. This strips the
        // ARMv4 subpage-permission bits which ARMv7 would reinterpret as
        // XN/AP[2]/TEX etc.
        for j in 0..256 {
            // SAFETY: bounds checked above.
            let ptr = unsafe { base.add(l2_idx_start + j) };
            let e = unsafe { read_pt_entry(ptr) };
            let typ = e & 3;
            let new = match typ {
                0 => continue,                         // fault, leave alone
                1 => (e & 0xFFFF_0000) | 0x0000_003D,  // large page, RW/RW, CB
                2 | 3 => (e & 0xFFFF_F000) | 0x0000_003E, // small page, XN=0
                _ => unreachable!(),
            };

            if new != e {
                unsafe { write_pt_entry(ptr, new); }
                if is_rom {
                    rom_writes += 1;
                }
            }
        }
    }

    rom_writes > 0
}

/// ARMv7 short-descriptor section attributes for the shadow-stub
/// ScratchVA carve-out installed at the kernel VA
/// `crate::hv::layout::SCRATCH_POOL_VA`. The section's PA bits encode
/// the IPA `SCRATCH_POOL_IPA`, which stage-2 then translates to the
/// host SCRATCH_POOL backing.
///
///   PA[31:20] = SCRATCH_POOL_IPA[31:20]  (stage-1 outputs this IPA)
///   AP[1:0] = 0b11   (RW from any mode, including USR)
///   AP[2]   = 0
///   domain  = 0      (matches kernel domain 0)
///   TEX     = 0, C/B = 0b11  (Normal cacheable WB)
///   XN      = 1      (instruction fetches PABT — defensive: scratch
///                    is data-only)
///   nG / S / NS = 0  (matches kernel section defaults)
///   bit[1] = 1, bit[0] = 0  (Section, PXN = 0)
///
/// Lower-19 attribute bits are 0x0C1E. Bit-by-bit cross-check against
/// DDI 0406C B3-19.
const SCRATCH_POOL_L1_SECTION_ATTRS: u32 = 0x0000_0C1E;
fn scratch_pool_l1_section() -> u32 {
    crate::hv::layout::SCRATCH_POOL_IPA | SCRATCH_POOL_L1_SECTION_ATTRS
}

/// Install the kernel-side L1 mapping for the shadow-stub ScratchVA
/// scratch carve-out at VA `crate::hv::layout::SCRATCH_POOL_IPA`. The
/// section descriptor identity-maps the VA to itself; stage-2 then
/// translates that IPA to the host `SCRATCH_POOL` backing.
///
/// Idempotent: rewrites the slot to `SCRATCH_POOL_L1_SECTION` even if
/// `fix_stage1_xn_bits` has just normalised it (clearing XN), so the
/// XN=1 invariant survives a re-walk.
///
/// Halts loud if the kernel has independently populated L1[0x18] with a
/// non-fault, non-matching entry (would mean a ROM revision actually
/// uses VA 0x0180_0000 — the plan's assumption breaks and a different
/// VA must be picked).
fn install_scratch_pool_l1_section() {
    let ram = guest_mem::ram_host_pa() as *mut u32;
    let idx = (crate::hv::layout::SCRATCH_POOL_VA >> 20) as usize;

    // SAFETY: idx < 4096; GUEST_RAM holds the kernel L1 at TTBR0 = 0x0400_0000.
    let entry = unsafe { read_pt_entry(ram.add(idx)) };

    let installed = scratch_pool_l1_section();
    // Acceptable pre-states:
    //   * Any type-0 (fault) entry — bits[1:0] == 0. The 717006 kernel
    //     leaves stray non-zero bits in unused L1 slots after soft-
    //     reset (e.g. observed `L1[0x18] = 0x00000010` on the second
    //     M=0→M=1 transition); the upper bits of a fault descriptor
    //     are don't-care for translation.
    //   * `installed` — our previous install survived re-walk
    //     untouched.
    //   * Normalised by fix_stage1_xn_bits to (entry & 0xFFF0_01E0) |
    //     0x0C0E — the walker flipped XN=1 → 0 inside our section.
    let normalised_after_walker: u32 =
        (installed & 0xFFF0_01E0) | 0x0000_0C0E;
    let is_fault_entry = (entry & 3) == 0;
    let acceptable =
        is_fault_entry
        || entry == installed
        || entry == normalised_after_walker;

    if !acceptable {
        kprintln!(
            "shadow_stub scratch: FATAL — kernel L1[{:#x}] = {:#010x}, type bits {:#x}; \
             not a fault entry and not our installed section. ROM revision uses VA {:#x}? \
             Pick a different SCRATCH_POOL_VA.",
            idx, entry, entry & 3, crate::hv::layout::SCRATCH_POOL_VA,
        );
        cpu::halt();
    }

    if entry != installed {
        // SAFETY: idx < 4096.
        unsafe { write_pt_entry(ram.add(idx), installed); }
        dprintln!(
            "shadow_stub scratch: installed kernel L1[{:#x}] = {:#010x} (was {:#010x})",
            idx, installed, entry,
        );
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

// ---------------------------------------------------------------------
// DABT bodies (flash-write drop, DABT-trampoline dispatch)
// ---------------------------------------------------------------------

/// Drop a guest write to the flash bank IPA window. Stage-2 maps the
/// banks RO to surface AMD-style command-sequence stores (the kernel's
/// flash chip code emits `0xAA` / `0x55` / `0x80` to magic offsets);
/// Einstein's `TMemory::WriteP` ignores them, so we do too.
///
/// For ISV=1 syndromes (simple LDR/STR-immediate without writeback):
/// nothing to update on the guest side, just advance ELR.
///
/// For ISV=0 syndromes (writeback or register-offset addressing): we
/// fetch the instruction at ELR, decode the destination register and
/// any base-register writeback, and update Rn so the kernel observes
/// the same post-instruction CPU state it would have if the store had
/// been silently absorbed by the flash chip's command latch. The store
/// itself is dropped.
///
/// Returns false on instruction shapes we don't recognise (LDM/STM,
/// load-exclusive, vector loads, …) so the caller halts loudly. Drop
/// in fresh forms here as the kernel turns out to use them.
fn drop_flash_write(ctx: &mut TrapContext, iss: u32, elr: u32) -> bool {
    let isv = (iss >> 24) & 1;
    if isv != 0 {
        // Simple LDR/STR-immediate or LDR/STR-byte/halfword without
        // writeback — no register state changes besides the (dropped)
        // memory store. Caller advances ELR.
        return true;
    }

    // ISV=0: writeback or unusual addressing. Decode the faulting
    // instruction enough to apply the writeback to Rn (if any).
    let insn = match crate::hv::guest_endian::guest_read_u32_va(elr) {
        Some(v) => v,
        None => return false,
    };

    // STR (immediate, A1): cond 010 P U B W L Rn Rt imm12, L=0.
    // Word B=0, byte B=1. Writeback when (P=0) || (W=1).
    if (insn & 0x0E10_0000) == 0x0400_0000 {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let rn = ((insn >> 16) & 0xF) as usize;
        let imm12 = insn & 0xFFF;
        if rn == 15 {
            return false;
        }
        let writeback = (!p) || w;
        if writeback {
            let signed_off: i32 = if u { imm12 as i32 } else { -(imm12 as i32) };
            let pre_rn = ctx.x[rn] as u32;
            ctx.x[rn] = pre_rn.wrapping_add(signed_off as u32) as u64;
        }
        return true;
    }

    // STRH (immediate, A1): cond 000 P U 1 W 0 Rn Rt imm4H 1011 imm4L.
    // imm = (imm4H << 4) | imm4L. Writeback when (P=0) || (W=1).
    if (insn & 0x0E40_00F0) == 0x0040_00B0 {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let rn = ((insn >> 16) & 0xF) as usize;
        let imm = ((insn >> 4) & 0xF0) | (insn & 0xF);
        if rn == 15 {
            return false;
        }
        let writeback = (!p) || w;
        if writeback {
            let signed_off: i32 = if u { imm as i32 } else { -(imm as i32) };
            let pre_rn = ctx.x[rn] as u32;
            ctx.x[rn] = pre_rn.wrapping_add(signed_off as u32) as u64;
        }
        return true;
    }

    // STR (register, A1): cond 011 P U B W L Rn Rt imm5 type Rm, L=0.
    // Bit 4 must be 0 (else it's a register-shift form we don't decode).
    if (insn & 0x0E10_0010) == 0x0600_0000 {
        let p = (insn >> 24) & 1 != 0;
        let u = (insn >> 23) & 1 != 0;
        let w = (insn >> 21) & 1 != 0;
        let rn = ((insn >> 16) & 0xF) as usize;
        let rm = (insn & 0xF) as usize;
        let imm5 = (insn >> 7) & 0x1F;
        let shift_type = (insn >> 5) & 0x3;
        if rn == 15 || rm == 15 {
            return false;
        }
        let writeback = (!p) || w;
        if writeback {
            // Guest CPSR at the data abort = SPSR_EL2; RRX writeback needs
            // its carry flag (arm_decode::arm_shift reads CPSR.C).
            let guest_cpsr = read_sysreg!("spsr_el2") as u32;
            let rm_val = ctx.x[rm] as u32;
            let shifted = crate::arch::arm_decode::arm_shift(rm_val, shift_type, imm5, guest_cpsr);
            let pre_rn = ctx.x[rn] as u32;
            let post_rn = if u {
                pre_rn.wrapping_add(shifted)
            } else {
                pre_rn.wrapping_sub(shifted)
            };
            ctx.x[rn] = post_rn as u64;
        }
        return true;
    }

    false
}

/// DABT-fast-trampoline fall-through. The trampoline at
/// `DABT_TRAMP_OFFSET` runs in ABT mode after a data abort; on
/// `DFSR.status != 1` (i.e. anything but alignment) it falls through
/// to `HVC #DabtDispatch` and lands here. Three outcomes:
///
///   * `DFSC=0x01` — alignment. The trampoline's BEQ should have
///     caught this and routed to `HVC #Align`, but the legacy
///     `mrc p15,0,Rt,c5,c0,0` has been observed to miss in at least
///     one site (DrText LDR-rotate at `0x0035c554`). Cross-check
///     ESR_EL1 here and dispatch to `handle_align_fault`
///     unconditionally instead of halting on a known-handleable
///     fault.
///   * Forwardable DFSC (translation / permission / access-flag,
///     codes 0x03 / 0x05 / 0x06 / 0x07 / 0x0D / 0x0F) — forward to
///     the kernel's `DataAbortHandler` at VA `0x0039_3114` (the
///     original target of the ROM's VA 0x10 branch before our DABT
///     trampoline insertion). Lets the kernel handle routine faults
///     like stack-collision growth without the hypervisor needing
///     to model on-demand paging.
///   * Anything else — delegate to `handle_diag` for the diagnostic
///     halt + register dump.
///
/// For the forwardable case:
///   * R0/R1 were clobbered by the trampoline (which stashed them
///     in TPIDRURW / TPIDRRO and then loaded DFSR / SPSR_abt into
///     them). Restore from those scratch slots so the kernel's
///     handler sees the pre-abort register state. LR_abt / SP_abt /
///     SPSR_abt are already in their post-DABT-entry values (the
///     trampoline reads them but does not modify them).
///   * ARMv7 leaves DFSR.Domain UNK for DFSC=5 (translation,
///     section) — see ARMv7 ARM B4.1.51. The 717006 kernel was
///     written for StrongARM, where the equivalent register (CP15
///     c5,c5,0) always carried the L1 entry's domain regardless of
///     fault status. Our hypervisor rewrites the kernel's
///     `mrc c5,c5,0` to `mrc c5,c0,0` (= DFSR_EL1) at ROM-load time
///     (see `loader::patch_cp15_encodings`), so the kernel's
///     later DAH read picks up whatever ARMv7 hardware put in
///     DFSR.Domain — which is 0 for DFSC=5. The kernel then
///     computes domain := 0 and asks
///     `GetDomainAndFaultMonitorFromDomainNumber(0)`, which has no
///     monitor → returns `scratch[0]=0` → `FaultMonitorEntry(r0=0)`
///     → -10015 → reboot. Empirical wedge: qemu13.log fault #2
///     shows `task[+0x58]=0x05` (DFSR=0x05, domain=0) where every
///     other recovered abort had `task[+0x58]=0x47` (DFSR=0x47,
///     domain=4). Fix: synthesise the StrongARM-style domain field
///     by reading the L1 entry for the FAR's section and writing
///     its bits[8:5] into DFSR_EL1.bits[7:4]. Idempotent for
///     valid-domain DFSCs (the bits already match).
fn handle_dabt_dispatch(ctx: &mut TrapContext) {
    let far = read_sysreg!("far_el1");
    let esr_el1 = read_sysreg!("esr_el1");
    let dfsc = (esr_el1 & 0x3F) as u32;

    if dfsc == 0x01 {
        unaligned::handle_align_fault(ctx);
        return;
    }
    let forwardable = matches!(dfsc, 0x03 | 0x05 | 0x06 | 0x07 | 0x0D | 0x0F);
    if !forwardable {
        handle_diag(ctx);
        return;
    }

    if dfsc == 0x05 || dfsc == 0x07 || dfsc == 0x0D || dfsc == 0x0F {
        let l1_pa = 0x0400_0000u32 + ((far as u32) >> 20) * 4;
        // The L1 entry's domain bits are synthesised into the DFSR the
        // kernel's DataAbortHandler reads — a fabricated entry would
        // steer the kernel's fault-monitor lookup, so halt loudly if
        // the table read fails.
        let l1 = match crate::hv::guest_endian::guest_read_u32_pa(l1_pa) {
            Some(v) => v,
            None => {
                kprintln!(
                    "*** handle_dabt_dispatch: L1 entry @PA={:#010x} unreadable \
                     (FAR={:#010x} DFSC={:#x} ELR_EL2={:#x}) ***",
                    l1_pa, far as u32, dfsc, read_sysreg!("elr_el2"),
                );
                cpu::halt();
            }
        };
        let l1_domain = (l1 >> 5) & 0xF;
        let mut dfsr_el1: u64;
        // SAFETY: sysreg read of DFSR_EL1 (= ESR_EL1's AArch32 alias
        // for data aborts when EL1 is AArch32). On Cortex-A53 in our
        // config, DFSR_EL1 == ESR_EL1 for AArch32 EL1 abort entries,
        // so update both via ESR_EL1.
        unsafe {
            core::arch::asm!("mrs {}, esr_el1", out(reg) dfsr_el1,
                options(nomem, nostack, preserves_flags));
        }
        dfsr_el1 = (dfsr_el1 & !(0xF << 4)) | ((l1_domain as u64) << 4);
        unsafe {
            core::arch::asm!("msr esr_el1, {}", in(reg) dfsr_el1,
                options(nostack, preserves_flags));
            core::arch::asm!("isb", options(nostack, preserves_flags));
        }
    }
    let spsr_el2 = read_sysreg!("spsr_el2");
    let hvc_src_mode = (spsr_el2 as u32) & 0x1F;
    log_dabt_forward(dfsc, far as u32, hvc_src_mode, ctx);
    let saved_r0: u64;
    let saved_r1: u64;
    unsafe {
        core::arch::asm!(
            "mrs {}, tpidr_el0",
            out(reg) saved_r0,
            options(nomem, nostack, preserves_flags),
        );
        core::arch::asm!(
            "mrs {}, tpidrro_el0",
            out(reg) saved_r1,
            options(nomem, nostack, preserves_flags),
        );
    }
    ctx.x[0] = saved_r0;
    ctx.x[1] = saved_r1;
    const DATA_ABORT_HANDLER_VA: u32 = 0x0039_3114;
    unsafe {
        core::arch::asm!(
            "msr elr_el2, {elr}",
            "isb",
            elr = in(reg) DATA_ABORT_HANDLER_VA as u64,
            options(nostack, preserves_flags),
        );
    }
}

/// Budgeted log for the DABT→kernel forward path. Prints once per unique
/// (FAR, hvc_src_mode, pre_abt_mode) tuple so we see each distinct fault
/// site without flooding on tight-loop faults (e.g. a page-table walk
/// the kernel is filling in one entry at a time).
///
/// Including `pre_abt_mode` (`SPSR_abt & 0x1F`) in the dedup key
/// distinguishes a USR-pre-abt fault from an SVC-pre-abt fault at the
/// same FAR.
fn log_dabt_forward(dfsc: u32, far: u32, mode: u32, ctx: &TrapContext) {
    let spsr_abt = read_banked_spsr("abt") as u32;
    let pre_abt_mode = spsr_abt & 0x1F;
    // Cross-check `mrs spsr_abt` against the trampoline-saved SPSR_abt
    // (docs/QEMU_BUGS.md Bug #1: QEMU raspi3b returns stale spsr_abt
    // for `mrs` from EL2). The trampoline writes the slot before any
    // kernel code runs, so the slot is the architecturally-correct
    // pre-abt CPSR.
    let spsr_abt_save = crate::hv::guest_endian::guest_read_u32_pa(guest_trampolines::DABT_SAVE_PA + 8).unwrap_or(0);
    let pre_abt_mode_save = spsr_abt_save & 0x1F;
    static mut SEEN: SeenSet<(u32, u32, u32), 16> = SeenSet::new((0, 0, 0));
    // Dedup on the saved-slot mode (architecturally correct) so a single
    // physical fault doesn't double-print just because `mrs spsr_abt`
    // reads a different (stale) value than the saved slot.
    let dedup_mode = pre_abt_mode_save;
    // SAFETY: single-core EL2; see diag_util module docs.
    let first = unsafe { (*addr_of_mut!(SEEN)).first_time((far, mode, dedup_mode)) };
    if first {
        // Capture more context: LR_abt (faulting PC + 8) tells us *where*
        // the kernel was when the abort happened — critical when
        // mode=ABT (recursive abort) because the FAR alone doesn't
        // identify the kernel-side instruction that wandered into the
        // unmapped VA. SPSR_abt names the mode the abort was taken from
        // (i.e. the mode that was running before this abort). For mode=ABT
        // (recursive) SPSR_abt also reads ABT — confirming the
        // double-fault.
        let lr_abt = ctx.x[20] as u32;
        let sp_abt = ctx.x[21] as u32;
        let lr_usr = ctx.x[14] as u32;
        let sp_usr = ctx.x[13] as u32;
        let lr_svc = ctx.x[18] as u32;
        let sp_svc = ctx.x[19] as u32;
        // For ARM-mode DABT, faulting_pc = LR_abt - 8.
        let faulting_pc = lr_abt.wrapping_sub(8);
        kprintln!(
            "dabt: forwarding to kernel DataAbortHandler — DFSC={:#x} FAR={:#010x} mode={:#x}",
            dfsc, far, mode
        );
        kprintln!(
            "  LR_abt={:#010x} (faulting PC={:#010x}) SP_abt={:#010x} SPSR_abt={:#010x} (pre-abt mode={:#x}){}",
            lr_abt, faulting_pc, sp_abt, spsr_abt, spsr_abt & 0x1F,
            if pre_abt_mode_save != pre_abt_mode {
                "  [mrs] -- mrs DIVERGES FROM SAVED SLOT --"
            } else { "" },
        );
        kprintln!(
            "  saved-slot SPSR_abt={:#010x} (pre-abt mode={:#x} = {})",
            spsr_abt_save, pre_abt_mode_save, crate::arch::arm_decode::aarch32_mode_name(pre_abt_mode_save),
        );
        kprintln!(
            "  USR sp={:#010x} lr={:#010x}   SVC sp={:#010x} lr={:#010x}",
            sp_usr, lr_usr, sp_svc, lr_svc
        );
        kprintln!(
            "  r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} r12={:#010x}",
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32, ctx.x[12] as u32
        );
        // Dump the stage-1 walk for the FAR. Crucial for distinguishing
        // "L1 entry missing" (DFSC=5) from "L2 entry missing"
        // (DFSC=7) — both would otherwise look the same in a brief log.
        guest_mem::dump_stage1_walk(far);
        // For DFSC=5 (section fault), also show the neighbouring L1
        // entries so we can see whether this section was an isolated
        // hole vs. a wider gap. Lazy "non-zero fault" descriptors
        // (e.g. 0x90 — type=00 with bit-7/bit-4 set) are a kernel
        // bookkeeping shape worth eyeballing across a window.
        #[cfg(feature = "log_mmu")]
        if dfsc == 5 {
            guest_mem::dump_l1_neighbourhood(far);
        }
    }
}

// ---------------------------------------------------------------------
// GuestOs implementation
// ---------------------------------------------------------------------

impl GuestOs for NewtonOs {
    fn on_sync_trap_exit(_ctx: &mut TrapContext) {
        // Drain any pen events from the host viewer before update_virq,
        // so a freshly raised INT_TABLET gets reflected into HCR_EL2.VI
        // on this trap exit instead of waiting for the next CNTHP
        // heartbeat. Cheap: backend self-throttles to 16 ms wall.
        (host_pumps().host_io_pump_input)();
        (host_pumps().input_pump)();
        // (audio used to be pumped here from the trap tail. With cyclic
        // DMA driving MAI from a hardware-paced CB chain, audio refills
        // happen from `audio::on_mai_dma_done` — the DMA
        // period-completion IRQ — and from `schedule_output` when the
        // kernel queues a new buffer. Liveness no longer depends on
        // trap rate, which is something the rest of the hypervisor is
        // trying to reduce.)

        // Guest MMIO writes to IntCtrl / FIQMask / IntClear change the
        // effective (`int_present & int_ctrl & ~fiq_mask`) pending set and
        // must be reflected into HCR_EL2.VI / VF before ERET, or a cleared
        // interrupt keeps re-firing (or an unmasked one never delivers).
        crate::hv::trap::update_virq();

        // Refresh the non-trapping tick page on every sync-trap exit so the
        // guest's tight delay loops (e.g. TSerialNumberROM::Init at 0x1dd8d0,
        // bit-bang protocol with cmp-against-#20-tick deadlines) see a fresh
        // tick value on the next read instead of spinning until the 16 ms
        // CNTHP heartbeat fires. Without this each delay loop runs ~heartbeat
        // wall time regardless of the requested delay, which on QEMU TCG (with
        // tracer overhead amplifying per-trap wall) makes us run ~4x more
        // delay-loop iterations than Einstein for the same kernel logic — and
        // the resulting trace-count drift is what causes the heap-allocator
        // divergence at TStackInfo::Init #12.
        tick_page_update_from_sync_trap();
    }

    fn on_irq_tail(_ctx: &mut TrapContext) {
        // Pump host PL011 -> guest extr-port RX DMA buffer. No-op when
        // DMA ch0 is not armed. See peripherals/dma.rs::poll_rx.
        crate::peripherals::dma::poll_rx();
        // Continue any in-flight guest extr-port TX DMA past the per-call
        // 4 KiB drain cap. No-op when DMA ch1 is not armed. See
        // peripherals/dma.rs::poll_tx.
        crate::peripherals::dma::poll_tx();
        // Pump the host-io backend: drain any pen events the viewer
        // posted, enqueue them, and raise INT_TABLET. Must run BEFORE
        // update_virq so the IRQ it raises lands in HCR_EL2.VI on this
        // trap exit, not the next one. The input pump is the parallel
        // path for real-hw pen sources (USB touchscreen) — it feeds the
        // same queue.
        (host_pumps().host_io_pump_input)();
        (host_pumps().input_pump)();
        // Audio tick: the null backend fires armed buffer-completion IRQs
        // here once a scheduled buffer's playback duration has elapsed,
        // raising the kernel's sound-output interrupt mask. Must run
        // BEFORE update_virq so a raised IRQ lands in HCR_EL2.VI on this
        // trap exit. The pi_hdmi backend ignores this and completes from
        // its own DMA-period IRQ (`audio::on_mai_dma_done`) instead.
        (host_pumps().audio_tick)();
        crate::hv::trap::update_virq();
        // Advance the boot-splash progress bar (no-op once the guest's
        // first blit has frozen the splash, and on platforms without
        // pi_fb). Driven from the timer IRQ tail so the bar grows on a
        // steady ~16 ms cadence regardless of trap-rate variation.
        #[cfg(all(feature = "platform-raspi3b", nh_host_io_pi_fb))]
        (host_pumps().splash_progress)(crate::diag::trap_hist::sync_count());
    }

    fn on_heartbeat() {
        // If the guest has made no sync-trap progress since the last
        // heartbeat, push synthetic ticks past the next pending match so
        // the deadline fires here instead of waiting for guest progress
        // that won't come (WFI / long busy-wait). No-op when the guest is
        // making progress.
        // Heartbeat path bumps SYNTH_TICKS by Δ_heartbeat (so non-trapping
        // busy-waits make progress) and fast-forwards past any pending
        // match deadline if the guest is parked.
        vic::heartbeat_tick_update();
        // Republish ticks + poll match crossings.
        tick_page_publish();
    }

    fn virq_lines() -> (bool, bool) {
        (vic::irq_pending(), vic::fiq_pending())
    }

    fn massage_sctlr(value: u32) -> u32 {
        // Force SCTLR.A=1 on the guest so unaligned LDR/STR raises
        // an alignment fault at EL1. The DABT-vector trampoline
        // routes alignment faults to unaligned::handle_align_fault.
        //
        // Under BE-8 also force EE (bit 25) and E0E (bit 24) so the
        // kernel's SCTLR writes (which never set EE) don't drop us
        // back into LE data mode mid-boot. Guest-test builds keep
        // the kernel's value verbatim so LE flat-binary tests work.
        #[cfg(not(nh_guest_test))]
        {
            value | 0x2 | (1u32 << 25) | (1u32 << 24)
        }
        #[cfg(nh_guest_test)]
        {
            value | 0x2
        }
    }

    fn on_stage1_mmu_enable(_ctx: &mut TrapContext, ttbr0: u32) {
        // Drop HCR_EL2.DC. While the guest ran with stage-1
        // off, DC=1 gave its data accesses Normal-WB semantics
        // so they hit the same cache lines the hypervisor
        // writes. But DC=1 also suppresses the guest's stage-1
        // translation from EL2's side (DDI 0487 D13.2.50):
        // leaving it set past this point means every non-
        // identity VA → IPA mapping the guest sets up (the
        // UND trampoline's save slot being the first one we
        // hit) falls through as VA=IPA and stage-2-faults.
        crate::hv::guest::set_dc_for_stage1_off(false);
        // The XN-rewrite walks RAM[0..0x4000] interpreting it
        // as the L1 table — that's correct only when the
        // guest's TTBR0 actually points there. Guest tests
        // that pick a different L1 base (e.g. their own table
        // at 0x04004000) would otherwise have RAM[0..0x4000]
        // (stack / scratch) corrupted by the walker. Gate on
        // the live TTBR0 value.
        if (ttbr0 & 0xFFFF_C000) == 0x0400_0000 {
            let rom_dirty = fix_stage1_xn_bits();
            install_scratch_pool_l1_section();
            if rom_dirty {
                reseed_flash_checksums_if_needed();
            }
        }
        // No cache maintenance here: the TTBR0 write handler
        // OR's Inner/Outer-WB cacheability bits into every guest
        // TTBR0 write, so stage-1 walks share the D-cache view of
        // the producer (kernel's own page-table writes, and our
        // in-place rewrites in fix_stage1_xn_bits). Producer +
        // walker matched-attributes keeps them coherent per ARM
        // ARM §B2.8 without any DC CVAC burst. See the comment
        // block at the (0, 2, 0, 0) CP15-write case in
        // `hv::trap::cp15` for the full rationale.
        maybe_dump_l1_once();
        // Swap the UND trampoline's save-slot literal to the
        // kernel VA that L1[0xC0] maps to the RAM slot. Done
        // outside `enable_patches()` so a soft-reboot that
        // cycles M=1→0→1 re-applies the swap (the tracer
        // gates its UDF install on a one-shot flag, but the
        // literal needs to track every MMU transition).
        // SAFETY: single-word ROM-backing write under the
        // paused-guest invariant.
        unsafe { guest_trampolines::install_und_vector_swap_post_mmu(); }
    }

    fn on_stage1_mmu_disable(_ctx: &mut TrapContext) {
        // Soft reboot: the guest turned its stage-1 MMU off.
        // Re-enable HCR_EL2.DC so data accesses stay Normal-WB
        // cacheable while we're back in the "MMU off" regime.
        crate::hv::guest::set_dc_for_stage1_off(true);
        // SAFETY: single-word ROM-backing write under the
        // same paused-guest invariant as the original patch.
        unsafe { guest_trampolines::install_und_vector_swap_pre_mmu(); }
    }

    fn on_stage1_ttbr0_write(raw: u32) {
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
            let rom_dirty = fix_stage1_xn_bits();
            install_scratch_pool_l1_section();
            if rom_dirty {
                reseed_flash_checksums_if_needed();
            }
        }
    }

    fn handle_und_hvc(
        ctx: &mut TrapContext,
        insn: u32,
        faulting_pc: u32,
        spsr_und: u64,
    ) -> UndHvcOutcome {
        match insn {
            // BootOs / ROMBoot canary (rom_patches::BOOTOS_PC = 0x0001_8688).
            // The initial hypervisor-ERET lands here in SVC mode (HVC traps
            // normally to EL2). Any later entry from USR mode is a software
            // reset reached via a task branching to the reset vector — HVC
            // from EL0 is UNDEFINED and arrives here instead of handle_hvc.
            // Route into the same handler so the canary's "2nd+ entry →
            // halt" logic applies regardless of the source mode.
            _ if insn == HvcImm::BootOs.insn()
                && faulting_pc == rom_patches::BOOTOS_PC =>
            {
                probes::handle_bootos_canary(ctx);
                UndHvcOutcome::Done
            }
            // L1[0xCD] investigation probes: the patched HVC instructions
            // sit inside `Remember` and `AllocatePageTable`, which the
            // kernel calls from both SVC (kernel-side fault chain) and USR
            // (user-mode wrappers like the post-ship patch table). HVC
            // from USR is UNDEFINED, so those calls land here. Pass the
            // trampoline-saved `spsr_und` (= the original USR-caller CPSR)
            // directly to the inner probe so its SP/LR lookups land on the
            // right banked register, then advance ELR via the UND-return
            // stub since UND entry doesn't auto-advance.
            _ if insn == HvcImm::RememberSwiret.insn() => {
                probes::handle_remember_swiret_probe(ctx);
                UndHvcOutcome::Resume { pc: (faulting_pc + 4) as u64, spsr: spsr_und }
            }
            // StorePermObject entry probe — first instruction (`mov ip,
            // sp`) was replaced with HVC. Reached here when StorePermObject
            // is called from USR mode (the typical NS-runtime path);
            // SVC-mode calls go through the direct HVC dispatch.
            #[cfg(feature = "log_store")]
            _ if insn == HvcImm::StorePermObjEntry.insn() => {
                probes::handle_store_perm_obj_entry_probe(ctx);
                ctx.x[12] = crate::arch::banked::sp_for_mode(ctx, spsr_und as u32) as u64;
                UndHvcOutcome::Resume { pc: (faulting_pc + 4) as u64, spsr: spsr_und }
            }
            // LoadPermObject return-site probe — `mov r0, r4` was
            // replaced with HVC. Same USR-vs-SVC routing rationale as
            // the StorePermObject entry probe above.
            #[cfg(feature = "log_store")]
            _ if insn == HvcImm::LoadPermObjRet.insn() => {
                probes::handle_load_perm_obj_ret_probe(ctx);
                ctx.x[0] = ctx.x[4];
                UndHvcOutcome::Resume { pc: (faulting_pc + 4) as u64, spsr: spsr_und }
            }
            // PHammerOutTranslator concrete-body patches. The kernel's
            // debug-print path is reached from USR for any task that
            // runs through the NS interpreter (DoSend / DoMessage /
            // DoFastApply); HVC from USR is UNDEFINED, so those firings
            // come through here. Pass the trampoline-saved spsr_und so
            // the SP/LR lookup lands on the right banked register, then
            // advance ELR via the UND-return stub since UND entry
            // doesn't auto-advance.
            _ if insn == HvcImm::HammerPrint.insn() => {
                probes::handle_hammer_print_with(ctx, spsr_und as u32);
                UndHvcOutcome::Resume { pc: (faulting_pc + 4) as u64, spsr: spsr_und }
            }
            _ if insn == HvcImm::HammerPutc.insn() => {
                probes::handle_hammer_thunk(ctx, ThunkKind::Putc);
                UndHvcOutcome::Resume { pc: (faulting_pc + 4) as u64, spsr: spsr_und }
            }
            _ if insn == HvcImm::HammerFlush.insn() => {
                probes::handle_hammer_thunk(ctx, ThunkKind::Flush);
                UndHvcOutcome::Resume { pc: (faulting_pc + 4) as u64, spsr: spsr_und }
            }
            _ if insn == HvcImm::HammerStackTrace.insn() => {
                probes::handle_hammer_thunk(ctx, ThunkKind::StackTrace);
                UndHvcOutcome::Resume { pc: (faulting_pc + 4) as u64, spsr: spsr_und }
            }
            _ if insn == HvcImm::HammerExceptionNotify.insn() => {
                probes::handle_hammer_thunk(ctx, ThunkKind::ExceptionNotify);
                UndHvcOutcome::Resume { pc: (faulting_pc + 4) as u64, spsr: spsr_und }
            }
            _ => UndHvcOutcome::NotMine,
        }
    }

    fn und_resume(ctx: &mut TrapContext, pc: u64, spsr: u64) {
        guest_trampolines::return_to_guest_from_und(ctx, pc, spsr);
    }

    fn handle_hvc_probe(ctx: &mut TrapContext, imm: u32) -> bool {
        match imm {
            v if v == HvcImm::BootOs as u32 => {
                probes::handle_bootos_canary(ctx);
            }
            v if v == HvcImm::RememberSwiret as u32 => {
                probes::handle_remember_swiret_probe(ctx);
            }
            v if v == HvcImm::DahMrsSpsr as u32 => {
                probes::handle_dah_mrs_spsr_patch(ctx);
            }
            #[cfg(feature = "log_store")]
            v if v == HvcImm::StorePermObjEntry as u32 => {
                probes::handle_store_perm_obj_entry_probe(ctx);
                // Emulate the patched-out `mov ip, sp` (R12 = SP for
                // the source AArch32 mode). HVC entry already advanced
                // ELR_EL2 past the trap, so no ELR adjustment needed.
                let spsr_el2 = read_sysreg!("spsr_el2") as u32;
                ctx.x[12] = crate::arch::banked::sp_for_mode(ctx, spsr_el2) as u64;
            }
            #[cfg(feature = "log_store")]
            v if v == HvcImm::LoadPermObjRet as u32 => {
                probes::handle_load_perm_obj_ret_probe(ctx);
                // Emulate the patched-out `mov r0, r4`. R0/R4 are not
                // banked across modes, so a direct GPR copy is correct
                // regardless of source mode.
                ctx.x[0] = ctx.x[4];
            }
            v if v == HvcImm::HammerPrint as u32 => {
                probes::handle_hammer_print(ctx);
            }
            v if v == HvcImm::HammerPutc as u32 => {
                probes::handle_hammer_thunk(ctx, ThunkKind::Putc);
            }
            v if v == HvcImm::HammerFlush as u32 => {
                probes::handle_hammer_thunk(ctx, ThunkKind::Flush);
            }
            v if v == HvcImm::HammerStackTrace as u32 => {
                probes::handle_hammer_thunk(ctx, ThunkKind::StackTrace);
            }
            v if v == HvcImm::HammerExceptionNotify as u32 => {
                probes::handle_hammer_thunk(ctx, ThunkKind::ExceptionNotify);
            }
            v if v == HvcImm::GpioTrigger as u32 => {
                vic::raise(vic::INT_GPIO);
            }
            _ => return false,
        }
        true
    }

    /// FP / SIMD access trap from a lower EL (EC=0x07), routed to EL2 by
    /// CPTR_EL2.TFP. On Newton this is how native-primitive calls arrive:
    /// the guest executes `MCR p10, 0, Rd, cN, cM, {opc2}` and Einstein's
    /// convention is that the CPU register Rd holds the "native call code"
    /// (driver ID << 8 | sub-function). We read the named register and
    /// hand it to peripherals::native_primitives::execute.
    ///
    /// MRC reads from CP10/CP11 (and any other FP/SIMD shape we don't
    /// expect from Newton OS) halt loudly — extend the handler when a
    /// ROM boot trips one.
    fn handle_native_call(ctx: &mut TrapContext, insn: u32, elr: u32) {
        // Decode ARMv7 MCR / MRC (load/store to coprocessor, single
        // register). Encoding: cond 1110 opc1 L CRn Rd coproc opc2 1 CRm
        // Mask for (MCR or MRC) with bit 4 = 1 and the fixed 1110 prefix
        // is (insn & 0x0F00_0010) == 0x0E00_0010.
        let is_mcr_mrc = (insn & 0x0F00_0010) == 0x0E00_0010;
        let cop = (insn >> 8) & 0xF;
        let l_bit = (insn >> 20) & 1; // 0 = MCR, 1 = MRC

        if !(is_mcr_mrc && (cop == 10 || cop == 11)) {
            kprintln!(
                "*** fp_simd trap on unexpected instruction {:#010x} @PC={:#x}, halting",
                insn, elr
            );
            cpu::halt();
        }

        let rd = ((insn >> 12) & 0xF) as usize;
        let crn = (insn >> 16) & 0xF;
        let crm = insn & 0xF;
        let opc1 = (insn >> 21) & 0x7;
        let opc2 = (insn >> 5) & 0x7;

        if l_bit != 0 {
            kprintln!(
                "*** MRC from CP{} not supported: insn={:#010x} @PC={:#x} (opc1={} Rd=r{} CRn=c{} CRm=c{} opc2={})",
                cop, insn, elr, opc1, rd, crn, crm, opc2
            );
            cpu::halt();
        }

        // Einstein's NativeCoprocRegisterTransfer reads CPU register Rd as
        // the "native call" code. ARMv4 MCR with Rd=PC reads PC+12, but
        // the Newton kernel never uses PC there; flag it if we ever see
        // one so we can match Einstein's quirk.
        if rd == 15 {
            kprintln!(
                "*** MCR p{}: Rd=PC is an Einstein quirk (mCurrentRegisters[15]+4); halting to investigate",
                cop
            );
            cpu::halt();
        }

        let native_insn = ctx.x[rd] as u32;
        native_primitives::execute(ctx, native_insn, elr);
    }

    fn handle_dabt_dispatch(ctx: &mut TrapContext) {
        handle_dabt_dispatch(ctx);
    }

    fn handle_align_fault(ctx: &mut TrapContext) {
        unaligned::handle_align_fault(ctx);
    }

    fn maybe_drop_flash_write(ctx: &mut TrapContext, iss: u32, ipa: u64, elr: u32) -> bool {
        if !crate::peripherals::flash::is_flash_pa(ipa) {
            return false;
        }
        drop_flash_write(ctx, iss, elr)
    }
}
