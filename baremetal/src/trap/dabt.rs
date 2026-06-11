//! Data-abort (EC=0x24) handling: stage-2 fault resolution, ISV=0
//! instruction emulation, ROM-write absorb, flash-write drop, and the
//! `handle_dabt_dispatch` forwarding probe.

use crate::{cpu, guest_mem, mmio, peripherals};
use crate::diag_util::SeenSet;
use crate::trap_context::{advance_elr, read_sysreg, TrapContext};
use crate::kprintln;
use core::ptr::addr_of_mut;
use super::und::read_banked_spsr;
use super::diag::handle_diag;


// ----------------- individual handlers -----------------

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

    crate::trap_hist::record_dabt(elr, ipa as u32);

    // Stage-2 RO-permission fault on a RAM code page. Newton's
    // demand-pager is overwriting a page the hypervisor previously
    // froze RO+X after shadow-stub patching; flip the page back to
    // RW+XN and retry the write natively. The next fetch into the
    // page will trap again (XN) so the handler re-scans the fresh
    // bytes. See `src/stage2.rs::set_ram_page_{ro_x,rw_xn}`.
    let ram_base = guest_mem::RAM_IPA_BASE as u64;
    let ram_end = ram_base + guest_mem::RAM_SIZE as u64;
    let is_permission = (ifsc & 0b111100) == 0b001100;
    if wnr && is_permission && (ram_base..ram_end).contains(&ipa) {
        let page = (ipa as u32) & !0xFFF;
        // SAFETY: helper performs its own TLB maintenance.
        unsafe { crate::stage2::set_ram_page_rw_xn(page); }
        // Don't advance ELR — the CPU retries the write.
        return;
    }

    // Direct CPU writes to flash bank addresses are silently dropped
    // (matching Einstein's `TMemory::WriteP` at `Emulator/TMemory.cpp:1777`,
    // which logs and returns without touching the backing). The kernel's
    // flash chip code emits AMD-style command-sequence stores
    // (e.g. `0xAA` to magic offsets) that on real hardware are absorbed
    // by the chip's command latches and never reach the storage cells;
    // on emulation those stores have to be neutralised so the seeded
    // calibration header (`flash::seed_block`) survives. Mutations the
    // kernel actually wants to commit go through `TEinsteinFlashDriver`'s
    // native primitives → `peripherals::flash_driver` → `flash::program_word`
    // / `flash::erase_block`, which write the host backing directly and
    // bypass stage-2 entirely.
    if wnr && peripherals::flash::is_flash_pa(ipa) && drop_flash_write(ctx, iss, elr) {
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
        let mode_label = crate::arm_decode::aarch32_mode_name(mode);
        // r13/r14 of the source mode via Table D1-79 (ctx.x[13]/[14]
        // are SP_usr/LR_usr regardless of source mode).
        let cur_sp = crate::banked::sp_for_mode(ctx, spsr);
        let cur_lr = crate::banked::lr_for_mode(ctx, spsr);
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
            let via_va = crate::guest_endian::guest_read_u32_va(addr).unwrap_or(0xDEADBEEF);
            let via_pa = crate::guest_endian::guest_read_u32_pa(addr).unwrap_or(0xDEADBEEF);
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
            if let Some(w) = crate::guest_endian::guest_read_u32_va(cur_sp.wrapping_add(off * 4)) {
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
    let insn = match crate::guest_endian::guest_read_u32_va(elr) {
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
    if ipa >= 0x0100_0000 {
        return false;
    }
    // Stage-1 off (pre-MMU and the guest-test runtime) makes
    // `read_word_va` return None — fall back to a PA-direct read,
    // matching the architectural rule that VA == IPA == PA when the
    // MMU is disabled.
    let insn = match crate::guest_endian::guest_read_u32_va(elr).or_else(|| crate::guest_endian::guest_read_u32_pa(elr)) {
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
            crate::guest_endian::guest_read_u32_pa(pa)
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
    if ipa < 0x0100_0000 { return true; }
    // "Unknown bank #5" gap (between flash bank 2 end at 0x10400000
    // and PCMCIA0Base at 0x30000000). Einstein's TMemory silently
    // returns 0 here; we now do the same in mmio.rs but the kernel
    // still gets here only via uninitialised-pointer paths (e.g.
    // the TEncodingMap.+16 = 0x20000110 from the MakeString fault
    // we resolved on 2026-04-27). Surfacing the register context
    // for the first such access per boot is cheap and decisive.
    // Skip the NO_REX_PROBE sub-window (0x10400000..0x20000000) —
    // that's a known ROM-driven scan that legitimately reads zeros.
    if (0x2000_0000..0x3000_0000).contains(&ipa) { return true; }
    false
}

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
    let insn = match crate::guest_endian::guest_read_u32_va(elr) {
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
            let shifted = crate::arm_decode::arm_shift(rm_val, shift_type, imm5, guest_cpsr);
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
///     (see `guest_mem::patch_cp15_encodings`), so the kernel's
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
pub(crate) fn handle_dabt_dispatch(ctx: &mut TrapContext) {
    let far = read_sysreg!("far_el1");
    let esr_el1 = read_sysreg!("esr_el1");
    let dfsc = (esr_el1 & 0x3F) as u32;

    if dfsc == 0x01 {
        crate::unaligned::handle_align_fault(ctx);
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
        let l1 = match crate::guest_endian::guest_read_u32_pa(l1_pa) {
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
pub(crate) fn log_dabt_forward(dfsc: u32, far: u32, mode: u32, ctx: &TrapContext) {
    let spsr_abt = read_banked_spsr("abt") as u32;
    let pre_abt_mode = spsr_abt & 0x1F;
    // Cross-check `mrs spsr_abt` against the trampoline-saved SPSR_abt
    // (docs/QEMU_BUGS.md Bug #1: QEMU raspi3b returns stale spsr_abt
    // for `mrs` from EL2). The trampoline writes the slot before any
    // kernel code runs, so the slot is the architecturally-correct
    // pre-abt CPSR.
    let spsr_abt_save = crate::guest_endian::guest_read_u32_pa(guest_mem::DABT_SAVE_PA + 8).unwrap_or(0);
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
            spsr_abt_save, pre_abt_mode_save, crate::arm_decode::aarch32_mode_name(pre_abt_mode_save),
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
