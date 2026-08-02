//! Data-abort (EC=0x24) handling: stage-2 fault resolution, ISV=0
//! instruction emulation, and the ROM-write absorb. The flash-write
//! drop and the `HVC #DabtDispatch` forwarding probe are guest-OS
//! bodies reached through the hooks.

use crate::{arch::cpu, hv::guest_mem, hv::layout, hv::mmio};
use crate::arch::trap_context::{advance_elr, read_sysreg, TrapContext};
use crate::{dprintln, kprintln};
use core::sync::atomic::{AtomicU32, Ordering};
use crate::hv::hooks::{ActiveGuest, GuestOs};


// ----------------- individual handlers -----------------

/// Count of absorbed ROM-aperture stores, for log rate-limiting: the
/// first few are kprintln'd (each one is a guest null-pointer-class
/// write worth seeing), the rest go through `dprintln!`.
static ROM_WRITE_DROPS: AtomicU32 = AtomicU32::new(0);

/// Resolve the IPA of a stage-2 fault.
///
/// HPFAR_EL2 is the architectural source, but on the Cortex-A53 (and
/// other ARMv8.0 cores) it can be **invalid for non-S1PTW permission
/// faults** — empirically on the Pi Zero 2 W (BCM2710A1) the silicon
/// reports the post-stage-2 host PA in HPFAR's FIPA field instead of
/// the IPA. The classic symptom is a guest write to IPA `0x0F18_xxxx`
/// (the Newton tick page) emerging at HPFAR-derived IPA
/// `0x0168_xxxx` (the host PA we mapped it to).
///
/// The standard fix (Linux/KVM and Jailhouse both ship this) is to
/// fall back to `AT S1E1{R,W}` for non-S1PTW permission faults: the
/// instruction translates the FAR through the guest's stage-1 regime
/// only, depositing the resulting IPA in PAR_EL1. With guest stage-1
/// disabled (SCTLR_EL1.M=0) this is the identity; with it enabled
/// AT correctly walks the guest tables.
///
/// `iss` is ESR_EL2.ISS[24:0]. `wnr` selects W vs R for AT (instruction
/// aborts always pass false). Returns the resolved IPA.
pub(crate) fn resolve_ipa(iss: u32, wnr: bool) -> u64 {
    let far: u64 = read_sysreg!("far_el2");
    let s1ptw = ((iss >> 7) & 1) != 0;
    let xfsc = iss & 0x3f;
    // DFSC/IFSC permission fault levels 0..3 occupy 0b001100..0b001111.
    let is_permission = (xfsc & 0b111100) == 0b001100;

    if !s1ptw && is_permission {
        let par: u64;
        // SAFETY: AT is a side-effecting system instruction that
        // writes PAR_EL1; ISB orders the MRS that follows. Runs at
        // EL2 with the guest's EL1 translation regime in effect.
        unsafe {
            if wnr {
                core::arch::asm!(
                    "at s1e1w, {0}",
                    "isb",
                    "mrs {1}, par_el1",
                    in(reg) far,
                    out(reg) par,
                    options(nostack, preserves_flags),
                );
            } else {
                core::arch::asm!(
                    "at s1e1r, {0}",
                    "isb",
                    "mrs {1}, par_el1",
                    in(reg) far,
                    out(reg) par,
                    options(nostack, preserves_flags),
                );
            }
        }
        if (par & 1) == 0 {
            // F=0: success. PAR[51:12] holds the IPA[51:12].
            return (par & 0xFFFF_FFFF_F000) | (far & 0xFFF);
        }
        // F=1: AT itself faulted (shouldn't happen for a genuine
        // stage-2 perm fault). Fall through to HPFAR — best effort.
    }

    let hpfar: u64 = read_sysreg!("hpfar_el2");
    ((hpfar >> 4) << 12) | (far & 0xFFF)
}

pub(crate) fn handle_data_abort(ctx: &mut TrapContext, iss: u32) {
    let far = read_sysreg!("far_el2");
    let isv = (iss >> 24) & 1;
    let wnr = ((iss >> 6) & 1) != 0;
    let ipa = resolve_ipa(iss, wnr);
    let sas = ((iss >> 22) & 3) as u8;
    let srt = ((iss >> 16) & 0x1F) as usize;
    let ifsc = (iss & 0x3f) as u32;

    // ISS.SRT (ESR_EL2 bits[20:16]) names the AArch64 register the
    // transfer used. For AArch32 guest traps the mapped register is
    // always X0..X14 (R0..R14), so SRT == 31 (the WZR/XZR encoding) is
    // architecturally impossible here. `ctx.x` only has slots 0..30, so
    // an unexpected 31 would panic on the index below; halt loudly with
    // context instead.
    if srt == 31 {
        kprintln!(
            "*** handle_data_abort: ISS.SRT == 31 (XZR) on AArch32 trap — \
             impossible; iss={:#010x} FAR={:#010x} IPA={:#010x} ***",
            iss, far as u32, ipa as u32,
        );
        cpu::halt();
    }

    let elr = read_sysreg!("elr_el2") as u32;

    crate::diag::trap_hist::record_dabt(elr, ipa as u32);

    // Stage-2 RO-permission fault on a RAM code page. Newton's
    // demand-pager is overwriting a page the hypervisor previously
    // froze RO+X after shadow-stub patching; flip the page back to
    // RW+XN and retry the write natively. The next fetch into the
    // page will trap again (XN) so the handler re-scans the fresh
    // bytes. See `src/stage2.rs::set_ram_page_{ro_x,rw_xn}`.
    let is_permission = (ifsc & 0b111100) == 0b001100;
    if wnr && is_permission && layout::ram_range().contains(&ipa) {
        let page = (ipa as u32) & !0xFFF;
        // SAFETY: helper performs its own TLB maintenance.
        unsafe { crate::hv::stage2::set_ram_page_rw_xn(page); }
        // Don't advance ELR — the CPU retries the write.
        return;
    }

    // Direct CPU writes to flash bank addresses are silently dropped
    // (matching Einstein's `TMemory::WriteP` at `Emulator/TMemory.cpp:1777`,
    // which logs and returns without touching the backing). The
    // flash-window check and the writeback-fixup decode are guest-OS
    // logic — see the `maybe_drop_flash_write` hook impl.
    if wnr && ActiveGuest::maybe_drop_flash_write(ctx, iss, ipa, elr) {
        advance_elr(4);
        return;
    }

    // Decodable stores (ISV=1) into the ROM aperture: mirror Einstein's
    // `TMemory::WriteP` (Emulator/TMemory.cpp:1755-1766), which logs and
    // drops every write below kHighROMEnd. Newton null-pointer writes
    // land here: VA 0 maps to ROM page 0 and a Manager-domain stage-1
    // mapping skips the AP check, so the store sails through to the
    // stage-2 RO ROM mapping (first seen: the internal store's
    // `KillBlock` @0x31230c storing through a NULL free-list pointer).
    // Real hardware's mask ROM ignores the write; so do we. ISV=1 has
    // no writeback, so there's no guest register state to fix up.
    // ISV=0 shapes (SWP) are absorbed by `try_absorb_rom_write` below.
    if wnr && isv == 1 && is_permission && ipa < layout::high_rom_end() {
        let n = ROM_WRITE_DROPS.fetch_add(1, Ordering::Relaxed);
        if n < 4 {
            kprintln!(
                "dabt: ignored write to ROM IPA={:#010x} value={:#010x} @PC={:#010x} (#{})",
                ipa as u32, ctx.x[srt] as u32, elr, n + 1
            );
        } else {
            dprintln!(
                "dabt: ignored write to ROM IPA={:#010x} value={:#010x} @PC={:#010x} (#{})",
                ipa as u32, ctx.x[srt] as u32, elr, n + 1
            );
        }
        advance_elr(4);
        return;
    }

    if isv == 0 {
        // No decodable syndrome — typically LDR/STR with writeback,
        // LDM/STM, or exclusive access. The Newton kernel uses
        // pre-indexed-with-writeback LDR (`ldr Rd, [Rn, #imm]!`) for
        // PCMCIA controller register access (e.g. `DisableSocketInterrupt`
        // at 0x55208). Try to fetch the instruction and emulate the
        // simple LDR/STR-immediate forms; fall through to halt on
        // anything we can't handle so the failure stays loud.
        if try_emulate_isv0_dabt(ctx, ipa, wnr, elr) {
            advance_elr(4);
            return;
        }
        // Mirror Einstein's `TMemory::WriteP` (Emulator/TMemory.cpp:1755-
        // 1766): writes to anywhere `< kHighROMEnd` (0x01000000) are
        // silently dropped, no fault raised. The Newton kernel's PCMCIA
        // path ends up calling `Swap(0, 1)` (atomic SWP via `Acquire`'s
        // semaphore-acquire helper) when `gPowerSemaphore[idx]` is NULL,
        // so the SWP loads ROM[0] and the kernel spins on a non-zero
        // value — matching that behaviour keeps the boot walking.
        if wnr && try_absorb_rom_write(ctx, ipa, elr) {
            advance_elr(4);
            return;
        }
        let spsr = read_sysreg!("spsr_el2");
        let sctlr_el1 = read_sysreg!("sctlr_el1");
        kprintln!(
            "*** data abort ISV=0 at ELR={:#x} SPSR={:#x} IPA={:#x} FAR={:#x} iss={:#x}",
            elr, spsr, ipa, far, iss
        );
        kprintln!(
            "    SCTLR_EL1 (guest) M-bit = {} (stage-1 {})",
            sctlr_el1 & 1,
            if (sctlr_el1 & 1) != 0 { "ON" } else { "OFF" }
        );
        cpu::halt();
    }

    // Before dispatching an "unknown IPA" write to the MMIO halt path,
    // dump the caller context. Cheap enough (runs once, then halt) and
    // decisive for diagnosing MCR-then-STR patterns where the faulting
    // instruction is in a tight helper far from where the bad address
    // was computed. The check mirrors the regions mmio::write would
    // silently accept — anything outside an MMIO window AND outside
    // the stage-2 RW RAM/flash/FB blocks is obviously unreachable.
    if is_obviously_unreachable_ipa(ipa) {
        let spsr = read_sysreg!("spsr_el2") as u32;
        let mode = spsr & 0x1F;
        let mode_label = crate::arch::arm_decode::aarch32_mode_name(mode);
        // r13/r14 of the source mode via Table D1-79 (ctx.x[13]/[14]
        // are SP_usr/LR_usr regardless of source mode).
        let cur_sp = crate::arch::banked::sp_for_mode(ctx, spsr);
        let cur_lr = crate::arch::banked::lr_for_mode(ctx, spsr);
        let dir = if wnr { "writing" } else { "reading" };
        let val = if wnr { ctx.x[srt] as u32 } else { 0 };
        kprintln!(
            "dabt-trip: PC={:#010x} mode={} {} {:#010x} -> IPA={:#x}",
            elr, mode_label, dir, val, ipa
        );
        kprintln!(
            "           r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x}",
            ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
        );
        kprintln!(
            "           r4={:#010x} r5={:#010x} r6={:#010x} r7={:#010x}",
            ctx.x[4] as u32, ctx.x[5] as u32, ctx.x[6] as u32, ctx.x[7] as u32
        );
        kprintln!(
            "           r8={:#010x} r9={:#010x} r10={:#010x} r11={:#010x}",
            ctx.x[8] as u32, ctx.x[9] as u32, ctx.x[10] as u32, ctx.x[11] as u32
        );
        kprintln!(
            "           r12={:#010x} sp({})={:#010x} lr({})={:#010x}",
            ctx.x[12] as u32, mode_label, cur_sp, mode_label, cur_lr
        );
        // Dump the instruction word at the faulting PC + 1 word of
        // surrounding context, both via stage-1 (so we honour the
        // kernel's view) and direct PA (in case stage-1 is off).
        // Helps when the PC is past the disassembly's coverage —
        // e.g. the post-SearchFreeList halt at 0xf76368.
        for off in [-4i32, 0, 4, 8] {
            let addr = elr.wrapping_add(off as u32);
            let via_va = crate::hv::guest_endian::guest_read_u32_va(addr).unwrap_or(0xDEADBEEF);
            let via_pa = crate::hv::guest_endian::guest_read_u32_pa(addr).unwrap_or(0xDEADBEEF);
            kprintln!(
                "           insn[pc{:+#3x}] @{:#010x} = via-va:{:#010x}  via-pa:{:#010x}",
                off, addr, via_va, via_pa,
            );
        }
        // Walk a few words of the source-mode stack via stage-1 — the
        // top entry is normally the caller's saved LR after a leaf
        // function's `stmfd sp!, {lr}` prologue. Also walk the access
        // base register so the table-pointer dereference is visible
        // even when the bad value was already overwritten in `ctx`.
        for off in 0..8u32 {
            if let Some(w) = crate::hv::guest_endian::guest_read_u32_va(cur_sp.wrapping_add(off * 4)) {
                kprintln!(
                    "           stack[sp+{:#04x}] @{:#010x} = {:#010x}",
                    off * 4, cur_sp.wrapping_add(off * 4), w
                );
            }
        }
    }

    if wnr {
        let value = ctx.x[srt] as u32;
        mmio::write(ctx, ipa, sas, value as u32, elr as u64);
    } else {
        let value = mmio::read(ctx, ipa, sas, elr as u64);
        // Sign-extension (SSE) is ignored for stub reads — everything we
        // return here is either zero or a known non-negative constant.
        ctx.x[srt] = value as u64;
    }

    // Advance past the 32-bit ARM instruction that faulted.
    advance_elr(4);
}

/// Attempt to emulate an ISV=0 stage-2 data abort. Used when the
/// faulting instruction is an LDR/STR (immediate, A1) form whose
/// stage-2 syndrome can't carry the destination register — most
/// commonly the pre-indexed-with-writeback variant the Newton kernel
/// uses for PCMCIA-controller register access. Returns true on
/// successful emulation; the caller advances ELR. Returns false if
/// the instruction isn't a form we recognise — caller halts loudly.
///
/// We only handle the unconditional and a small set of common
/// conditional encodings; LDM/STM, exclusives, and register-offset
/// LDR/STR all return false on purpose so they keep halting.
fn try_emulate_isv0_dabt(ctx: &mut TrapContext, ipa: u64, wnr: bool, elr: u32) -> bool {
    let insn = match crate::hv::guest_endian::guest_read_u32_va(elr) {
        Some(v) => v,
        None => return false,
    };
    // Cache-maintenance MCR by MVA via CP15 c7 (DC IVAC, DC CIVAC,
    // DC CVAC, IC IVAU, etc.). These check the target line's stage-2
    // permissions and trap with ISV=0 when the line maps to a RO
    // stage-2 page (which is our intent for ROM/flash regions — see
    // the IPA permission map in `stage2::init`). The op is meaningless
    // on emulated MMIO/flash because no host-side cache state needs
    // to change, so we just advance ELR past it.
    //
    // Encoding mask: cond 1110 0000 CRn=c7 Rt 1111 opc2 1 CRm
    //   bits[27:24] = 1110 (MCR opcode group)
    //   bits[23:20] = 0000 (opc1 = 0; bit 20 = 0 = MCR not MRC)
    //   bits[19:16] = 0111 (CRn = c7)         ← was masked out before
    //   bits[11:8]  = 1111 (coproc = p15)
    //   bit[4]      = 1    (MCR/MRC, not CDP)
    //   cond / Rt / CRm / opc2 are any.
    if (insn & 0x0FFF_0F10) == 0x0E07_0F10 {
        let _ = ctx;
        let _ = ipa;
        let _ = wnr;
        return true;
    }
    // Decode LDR/STR (immediate, A1): cond 010 P U 0 W L Rn Rt imm12.
    // We require word access (B=0); halfword/byte forms have
    // different bit 22 values and we don't support them yet.
    if (insn & 0x0E40_0000) != 0x0400_0000 {
        return false;
    }
    let cond = (insn >> 28) & 0xF;
    if cond != 0xE {
        // Conditional: caller already trapped because the access
        // happened, so the condition was true. Same emulation works
        // regardless of which condition was used; allow any cond.
    }
    let p = (insn >> 24) & 1 != 0;
    let u = (insn >> 23) & 1 != 0;
    let w = (insn >> 21) & 1 != 0;
    let l = (insn >> 20) & 1 != 0;
    let rn = ((insn >> 16) & 0xF) as usize;
    let rt = ((insn >> 12) & 0xF) as usize;
    let imm12 = insn & 0xFFF;
    if l != !wnr {
        // Syndrome WnR disagrees with insn L bit — instruction must
        // not be the one we think; bail.
        return false;
    }
    if rn == 15 || rt == 15 {
        // PC-relative or PC-target — too tricky for the simple path.
        return false;
    }
    let writeback = (!p) || w;
    let signed_off: i32 = if u { imm12 as i32 } else { -(imm12 as i32) };
    let pre_rn = ctx.x[rn] as u32;
    let post_rn = pre_rn.wrapping_add(signed_off as u32);

    if l {
        let value = mmio::read(ctx, ipa, 2 /* word */, elr as u64);
        ctx.x[rt] = value as u64;
    } else {
        let value = ctx.x[rt] as u32;
        mmio::write(ctx, ipa, 2 /* word */, value, elr as u64);
    }
    if writeback {
        ctx.x[rn] = post_rn as u64;
    }
    true
}

/// Mirror Einstein's `TMemory::WriteP` (Emulator/TMemory.cpp:1755-1766)
/// for stage-2 permission faults that target the ROM aperture
/// (`IPA < kHighROMEnd = 0x01000000`). Einstein logs and drops every
/// such write without raising a fault; we map ROM RO at stage-2 so the
/// same writes surface as ISV=0 stage-2 perm faults (no decodable
/// syndrome — SWP, LDM/STM with a base in ROM, etc.).
///
/// For atomic `SWP/SWPB` we still have to run the load piece — the
/// Newton kernel's lock-acquire glue calls `Swap(addr, val)` with
/// `addr = gPowerSemaphore[idx]`, which is NULL on a fresh PCMCIA path,
/// and spins on the loaded value. The load returns `ROM[ipa]` (here
/// `ROM[0]` = the reset vector), the store is dropped.
///
/// Returns `true` if the instruction shape was recognised and the write
/// has been absorbed; the caller advances ELR. Returns `false` for
/// anything we don't recognise so the loud halt path stays the trip-
/// wire for novel cases (pre/post-indexed STR with writeback, LDM/STM,
/// inline-stub byte/halfword stores, …).
fn try_absorb_rom_write(ctx: &mut TrapContext, ipa: u64, elr: u32) -> bool {
    if ipa >= layout::high_rom_end() {
        return false;
    }
    // Stage-1 off (pre-MMU and the guest-test runtime) makes
    // `read_word_va` return None — fall back to a PA-direct read,
    // matching the architectural rule that VA == IPA == PA when the
    // MMU is disabled.
    let insn = match crate::hv::guest_endian::guest_read_u32_va(elr).or_else(|| crate::hv::guest_endian::guest_read_u32_pa(elr)) {
        Some(v) => v,
        None => return false,
    };
    // SWP / SWPB (A1):  cond 00010 B 00 Rn Rd SBZ 1001 Rm
    // Mask leaves cond, B (bit 22), Rn, Rd, Rm free; fixes everything
    // else. Rd holds the loaded data on completion; Rm holds the value
    // to write (which we drop). Rn holds the address.
    if (insn & 0x0FB0_0FF0) == 0x0100_0090 {
        let b = ((insn >> 22) & 1) != 0;
        let rn = ((insn >> 16) & 0xF) as usize;
        let rd = ((insn >> 12) & 0xF) as usize;
        if rn == 15 || rd == 15 {
            return false;
        }
        let pa = ipa as u32;
        let loaded = if b {
            guest_mem::read_byte_pa(pa).map(|v| v as u32)
        } else {
            // Plain SWP zero-extends bits[31:0] of the loaded word into
            // Rd; `read_word_pa` already returns a u32 in the guest's
            // little-endian view (matches the BE→LE byteswap done at
            // ROM load time).
            crate::hv::guest_endian::guest_read_u32_pa(pa)
        };
        let value = match loaded {
            Some(v) => v,
            None => {
                kprintln!(
                    "*** try_absorb_rom_write: SWP{} load PA={:#010x} outside guest memory \
                     (insn={:#010x} @ELR={:#010x}) ***",
                    if b { "B" } else { "" }, pa, insn, elr,
                );
                cpu::halt();
            }
        };
        ctx.x[rd] = value as u64;
        return true;
    }
    false
}

/// IPA ranges that the stage-2 map intentionally leaves as fault /
/// read-only and that no peripheral module owns. A write here is
/// almost certainly a wild pointer — worth dumping context before
/// halting.
fn is_obviously_unreachable_ipa(ipa: u64) -> bool {
    // Inside ROM (stage-2 RO). Any write is doomed.
    if ipa < layout::high_rom_end() { return true; }
    // "Unknown bank #5" gap (between flash bank 2 end at 0x10400000
    // and PCMCIA0Base at 0x30000000). Einstein's TMemory silently
    // returns 0 here; we now do the same in mmio.rs but the kernel
    // still gets here only via uninitialised-pointer paths (e.g.
    // the TEncodingMap.+16 = 0x20000110 from the MakeString fault
    // we resolved on 2026-04-27). Surfacing the register context
    // for the first such access per boot is cheap and decisive.
    // Skip the NO_REX_PROBE sub-window (0x10400000..0x20000000) —
    // that's a known ROM-driven scan that legitimately reads zeros.
    if layout::UNKNOWN_BANK5.contains(ipa) { return true; }
    false
}

