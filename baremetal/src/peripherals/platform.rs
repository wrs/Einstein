//! Platform driver — Rust port of Einstein's `TMainPlatformDriver` /
//! `TPlatformManager`.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x000001. Subfunction codes match Einstein's
//! `TNativePrimitives::ExecutePlatformDriverNative`
//! (`Emulator/TNativePrimitives.cpp:600-1050`).
//!
//! Most subfns are no-ops that just set r0=0 — they correspond to
//! power-management hooks Einstein also no-ops (event queue is empty,
//! no host dock, no backlight) plus a handful that return guest-visible
//! state: GetPCMCIAPowerSpec (0x11), FillGestaltEmulatorInfo (0x17),
//! GetUserInfo (0x1B), GetHostTimeZone (0x1C).
//!
//! Logging subfns (0x1A Log) write through to the hypervisor UART.

use crate::{cpu, guest_mem, kprintln, trap::TrapContext};

/// Platform-driver class ID in the native-primitive encoding.
pub const DRIVER_ID: u32 = 0x00_0001;

/// `kUP2Version` from `Emulator/Platform/PlatformGestalt.h:43`.
const UP2_VERSION: u32 = 0x0001_0003;

/// NewtonErrors "Call not implemented" — Einstein returns it from
/// unsupported GetPCMCIAPowerSpec selectors (TNativePrimitives.cpp:823).
const ERR_NOT_IMPLEMENTED: u32 = (-10005i32) as u32;

pub fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    match subfn {
        // No-op, no return value (New).
        0x01 => {}
        // PauseSystem (0x0D) and PowerOffSystem (0x0E) — both are
        // "halt the CPU until an event signal arrives" primitives on
        // real hardware. PauseSystem is the idle-loop primitive
        // (`SleepUntilNextWakeup` path); PowerOffSystem is called
        // from CyclePower__Fv+0xE0 inside the deep-sleep retry loop
        // body, between `IOPowerOffAll` and `PowerOnSystem`. Einstein
        // implements both as `mEmulator->PauseSystem()`
        // (TNativePrimitives.cpp:754, :756). We do the same in EL2:
        // WFI until the CNTHP heartbeat (or any wired physical IRQ)
        // wakes us. See `pause_system` below.
        //
        // Without 0x0E doing wake-wait, `CyclePower` spins at trap
        // rate after the first `SleepUntilNextWakeup`: every iteration
        // of the retry loop reads IntPresent/IntED3 and re-cycles
        // power immediately, generating ~365 k traps/sec on QEMU TCG
        // (~98 k IntCtrl writes, ~65 k IntED1 reads, ~33 k DACR
        // writes per 2 s window — see trap-hist captures in the
        // commit history).
        0x0D => pause_system(ctx, false),
        0x0E => pause_system(ctx, true),
        // No-op, r0=0 (per Einstein Emulator/TNativePrimitives.cpp:625-849):
        //   0x02 Delete, 0x03 Init, 0x04 BacklightTrigger,
        //   0x05 RegisterPowerSwitchInterrupt, 0x06 EnableSysPowerInterrupt,
        //   0x07 InterruptHandler, 0x08 TimerInterruptHandler,
        //   0x09 ResetZAPStoreCheck, 0x0A PowerOnSubsystem,
        //   0x0B PowerOffSubsystem, 0x0C PowerOffAllSubsystems,
        //   0x0F PowerOnSystem,
        //   0x10 BacklightOverride, 0x12 RegisterPowerSwitchInterrupt2,
        //   0x13 TranslatePowerEvent.
        0x02 | 0x03 | 0x04 | 0x05 | 0x06 | 0x07 | 0x08 | 0x09
        | 0x0A | 0x0B | 0x0C | 0x0F
        | 0x10 | 0x12 | 0x13 => {
            ctx.x[0] = 0;
        }
        // GetSubsystemPower — writes 0 (off) to the status pointer, r0=0.
        0x14 => get_subsystem_power(ctx, pc),
        // GetPCMCIAPowerSpec.
        0x11 => get_pcmcia_power_spec(ctx, pc),
        // GetNextEvent — always "no event" in our setup (no host event
        // queue). r0=0 (no event).
        0x15 => {
            ctx.x[0] = 0;
        }
        // FillGestaltEmulatorInfo — writes kUP2Version at [r1], r0=0.
        0x17 => fill_gestalt_emulator_info(ctx, pc),
        // LockEventQueue / UnlockEventQueue — no-op (single-writer from EL2).
        0x18 | 0x19 => {}
        // Log — read a C string at r1, emit via UART.
        0x1A => log_message(ctx, pc),
        // GetUserInfo — no user info set; write empty string to [r2], r0=0.
        0x1B => get_user_info(ctx, pc),
        // GetHostTimeZone — return 0 (UTC offset, matching "no host TZ").
        0x1C => {
            ctx.x[0] = 0;
        }
        // CalibrateTablet — no-op (no host calibration tool).
        0x1D => {}
        // Quit — Einstein toggles an mQuit flag; we treat as a no-op for
        // the guest (the hypervisor doesn't quit on guest request).
        0x1E => {}
        // DisposeBuffer — no host buffers to dispose; r0=0.
        0x1F => {
            ctx.x[0] = 0;
        }
        // CopyBufferData — no host buffers; r0=0.
        0x20 => {
            ctx.x[0] = 0;
        }
        // OpenEinsteinMenu — no menu in hypervisor; no-op.
        0x21 => {}
        // NewtonScriptCall — no NS bridge; r0=0 (nil ref).
        0x22 => {
            ctx.x[0] = 0;
        }
        _ => {
            kprintln!(
                "*** platform: unknown subfn {:#x} @PC={:#x} r1={:#x} r2={:#x} r3={:#x}",
                subfn, pc, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
            );
            cpu::halt();
        }
    }
}

/// TMainPlatformDriver::GetPCMCIAPowerSpec(slot=r1, out=r2).
/// `TMainPlatformDriver::PauseSystem` (subfn 0x0D) and `PowerOffSystem`
/// (subfn 0x0E) — both "halt the CPU until an event signal arrives"
/// primitives (Emulator/TNativePrimitives.cpp:749-758). On real
/// hardware the kernel sequence is roughly "mask IRQs; check work
/// queues; if empty, WFI"; PauseSystem is the WFI step in the idle
/// path, and PowerOffSystem is the WFI step inside CyclePower's deep-
/// sleep retry loop. Returning immediately (the previous no-op
/// behaviour) made each spin at trap rate — ~40 kHz on QEMU TCG for
/// PauseSystem, and ~365 kHz aggregate for CyclePower because each
/// retry iteration also reads/writes VIC, alarm, and FIQ-mask
/// registers — and was responsible for ≈100% of EC=0x07 (FP/SIMD)
/// traps plus the matching ≈100% of EC=0x03 (DACR writes in SWIBoot
/// exception entry/exit). See `trap-hist`.
///
/// We implement the wait directly in EL2 with `wfi`:
///
///   * Short-circuit if a vIRQ is already pending (`vic::irq_pending()`).
///     The kernel will take it on the next ERET; consuming a heartbeat
///     in WFI would just delay that.
///   * Also short-circuit on `vic::take_wake_request()`: a host-IO
///     power-switch press sets that flag (see `vic::raise_power_switch`)
///     so the guest gets a chance to leave PowerOff state even though
///     the corresponding `INT_GPIO` bit is masked out of `kPowerOffMask`.
///     Mirrors Einstein's `mEmulatorCondVar->Signal()` back-door.
///   * Otherwise unmask physical IRQs in EL2 (`PSTATE.I`) and issue
///     `wfi`. The only wired physical IRQ on this hypervisor is CNTHP
///     heartbeat (~16 ms); when it fires, the EL2 IRQ vector at offset
///     0x280 (current-EL SPx) runs `trap_irq`, which advances synthetic
///     ticks, polls Newton match registers, and may raise a vIRQ. After
///     the IRQ vector ERETs back here we re-mask and return.
///   * Loop up to `MAX_WFI_ITERS` times so a heartbeat that doesn't
///     end up raising a vIRQ doesn't return us to the guest just to
///     have it call PauseSystem again. The bound caps the worst-case
///     wait at ~128 ms wall time, which is well under any timeout the
///     kernel would notice.
///
/// SAFETY notes:
///   * WFI at EL2 is architectural (DDI 0487 D7.2.20).
///   * The nested IRQ that wakes WFI is taken while EL2 is running, so
///     it dispatches to `trap::irq_from_el2` — the slim ISR that only
///     advances synthetic ticks / DMA completions and rearms CNTHP. It
///     touches no `ctx`-derived guest state and is safe to run nested
///     inside the sync-trap handler we're in.
///   * `cpu::with_irqs_unmasked` snapshots and restores `ELR_EL2` /
///     `SPSR_EL2` around the WFI window: the CPU clobbers them on the
///     nested IRQ entry, and the outer sync-trap tail
///     (`vectors.s::restore_context_and_eret`) ERETs on whatever they
///     hold — without the restore, ERET would read the post-WFI EL2h
///     state and jump to PC=0 in EL2 instead of back to the guest.
fn pause_system(ctx: &mut TrapContext, powering_off: bool) {
    const MAX_WFI_ITERS: u32 = 8;

    // Tell the pen-input pump we're in deep-sleep so the next pen-down
    // can synthesise a power-switch press (Einstein's
    // `AndroidGlue.cpp:205-216` hack). Only for 0x0E PowerOffSystem;
    // 0x0D PauseSystem is the on-state idle loop and must NOT trigger
    // a power-switch press.
    if powering_off {
        crate::peripherals::vic::set_powered_off(true);
    }

    if !crate::peripherals::vic::irq_pending() && !crate::peripherals::vic::take_wake_request() {
        cpu::with_irqs_unmasked(|| {
            for _ in 0..MAX_WFI_ITERS {
                // SAFETY: WFI at EL2 is architectural; IRQs are
                // unmasked for the duration of this closure so a wired
                // physical IRQ (CNTHP heartbeat) wakes it.
                unsafe {
                    core::arch::asm!("wfi", options(nostack, preserves_flags));
                }
                if crate::peripherals::vic::irq_pending()
                    || crate::peripherals::vic::take_wake_request()
                {
                    break;
                }
            }
        });
    }

    if powering_off {
        crate::peripherals::vic::set_powered_off(false);
    }

    ctx.x[0] = 0;
}

///
/// Einstein returns `5` (3.3V + 5V) for slot 0, `7` (3.3V + 5V + 12V)
/// for slot 1, `kError_NotImplemented` otherwise
/// (TNativePrimitives.cpp:795-825).
fn get_pcmcia_power_spec(ctx: &mut TrapContext, pc: u32) {
    let slot = ctx.x[1] as u32;
    let out_addr = ctx.x[2] as u32;
    let value = match slot {
        0 => 5u32,
        1 => 7u32,
        _ => {
            ctx.x[0] = ERR_NOT_IMPLEMENTED as u64;
            return;
        }
    };
    if !write_guest_word(out_addr, value) {
        kprintln!(
            "*** platform.GetPCMCIAPowerSpec: cannot write at {:#x} @PC={:#x}",
            out_addr, pc
        );
        cpu::halt();
    }
    ctx.x[0] = 0;
}

/// TMainPlatformDriver::FillGestaltEmulatorInfo(out=r1).
///
/// Einstein writes `kUP2Version` as the single word at `*r1`
/// (TNativePrimitives.cpp:906-914).
fn fill_gestalt_emulator_info(ctx: &mut TrapContext, pc: u32) {
    let out_addr = ctx.x[1] as u32;
    if !write_guest_word(out_addr, UP2_VERSION) {
        kprintln!(
            "*** platform.FillGestaltEmulatorInfo: cannot write at {:#x} @PC={:#x}",
            out_addr, pc
        );
        cpu::halt();
    }
    ctx.x[0] = 0;
}

/// TMainPlatformDriver::GetSubsystemPower(subsystem=r1, out=r2).
/// Einstein writes 0 (off) to *r2 regardless of subsystem (TNP.cpp:873).
fn get_subsystem_power(ctx: &mut TrapContext, pc: u32) {
    let out_addr = ctx.x[2] as u32;
    if !write_guest_word(out_addr, 0) {
        kprintln!(
            "*** platform.GetSubsystemPower: cannot write at {:#x} @PC={:#x}",
            out_addr, pc
        );
        cpu::halt();
    }
    ctx.x[0] = 0;
}

/// TMainPlatformDriver::Log(str=r1) — ISO-encoded C string.
///
/// Einstein reads up to 512 bytes via `FastReadString` and writes to
/// its log / stdout (TNativePrimitives.cpp:924-957). We emit the same
/// via `kprintln!`.
fn log_message(ctx: &mut TrapContext, pc: u32) {
    let mut addr = ctx.x[1] as u32;
    let mut buf = [0u8; 512];
    let mut len = 0usize;
    while len < buf.len() {
        match read_guest_byte(addr) {
            Some(0) => break,
            Some(b) => {
                buf[len] = b;
                len += 1;
                addr = addr.wrapping_add(1);
            }
            None => {
                // Einstein's FastReadString completes this path, so a
                // failed guest read is a hypervisor emulation bug, not a
                // guest bug — halt loudly like every other native-prim
                // guest access (periph-L6).
                kprintln!(
                    "*** platform.Log: cannot read at {:#x} @PC={:#x}",
                    addr, pc
                );
                cpu::halt();
            }
        }
    }
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        kprintln!("platform.Log: {}", s);
    } else {
        kprintln!("platform.Log: <{} non-utf8 bytes @PC={:#x}>", len, pc);
    }
}

/// TPlatformManager::GetUserInfo(sel=r1, bufSize=r2, bufAddr=r3).
/// Einstein returns a host-side name/company/owner string; we have
/// none, so write an empty NUL-terminated string when the caller
/// provided a buffer, and return length 0.
fn get_user_info(ctx: &mut TrapContext, pc: u32) {
    let buf_size = ctx.x[2] as u32;
    let buf_addr = ctx.x[3] as u32;
    if buf_size >= 1 && !write_guest_byte(buf_addr, 0) {
        kprintln!(
            "*** platform.GetUserInfo: cannot write NUL at {:#x} @PC={:#x}",
            buf_addr, pc
        );
        cpu::halt();
    }
    ctx.x[0] = 0;
}

fn write_guest_word(addr: u32, value: u32) -> bool {
    if crate::guest_endian::guest_write_u32_va(addr, value) {
        return true;
    }
    crate::guest_endian::guest_write_u32_pa(addr, value)
}

fn read_guest_byte(addr: u32) -> Option<u8> {
    let pa = guest_mem::translate_va(addr).unwrap_or(addr);
    guest_mem::read_byte_pa(pa)
}

fn write_guest_byte(addr: u32, value: u8) -> bool {
    let pa = guest_mem::translate_va(addr).unwrap_or(addr);
    guest_mem::write_byte_pa(pa, value)
}
