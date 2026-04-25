//! Sound driver — Rust port of Einstein's `PMainSoundDriver` native
//! primitive class. Newton has no audible-sound requirement on the path
//! to `TInterpreter`, so every entry here is a no-op that mirrors the
//! return value Einstein produces in `TNativePrimitives::
//! ExecuteSoundDriverNative` (`Emulator/TNativePrimitives.cpp:1062-1400`).
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x000002. Subfns that Einstein doesn't model
//! (e.g. 0x01 New, 0x02 Delete) fall through to the default branch and
//! halt loudly so we notice if the boot path ever starts depending on
//! real sound state.

use crate::{cpu, kprintln, trap::TrapContext};

/// Sound-driver class ID in the native-primitive encoding.
pub const DRIVER_ID: u32 = 0x00_0002;

/// NewtonErrors "Sound hardware not present" — returned by
/// SetSoundHardwareInfo. Matches Einstein's constant in the 0x03 arm of
/// `ExecuteSoundDriverNative`.
const ERR_NO_SOUND_HARDWARE: u32 = (-30009i32) as u32;

pub fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    // Diagnostic: log first occurrence of each subfn so we can see what
    // the kernel exercises during sound init.
    static mut SEEN: u32 = 0;
    let bit = 1u32 << (subfn & 0x1F);
    // SAFETY: single-threaded.
    let first = unsafe {
        let v = SEEN; if (v & bit) == 0 { SEEN = v | bit; true } else { false }
    };
    if first && subfn <= 0x1F {
        kprintln!(
            "sound: first subfn {:#x} @PC={:#x} r1={:#x} r2={:#x} r3={:#x}",
            subfn, pc, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
        );
    }
    match subfn {
        // SetSoundHardwareInfo: return -30009 to tell the kernel there's
        // no configurable sound hardware. Einstein's default path.
        0x03 => {
            ctx.x[0] = ERR_NO_SOUND_HARDWARE as u64;
        }

        // GetSoundHardwareInfo: Einstein writes a 7-word info struct at
        // [r1] and returns 0. The Newton kernel reads the struct after
        // the call; returning r0=0 without writing it leaves the struct
        // holding whatever the heap allocator left there, which may
        // steer the kernel into an unexpected sample-rate / buffer-size
        // path during sound subsystem startup.
        // Einstein source: TNativePrimitives.cpp:1099-1109.
        0x04 => {
            let info_addr = ctx.x[1] as u32;
            // Constants from Einstein. The 0x54600000 at +0x0c is a
            // 32-bit fixed-point sample-rate value the Newton driver
            // expects.
            let words: [(u32, u32); 7] = [
                (0x00, 0x00000001),
                (0x04, 0x00000001),
                (0x08, 0x00000001),
                (0x0c, 0x54600000),
                (0x10, 0x00000006),
                (0x14, 0x00000010),
                (0x18, 0x00000001),
            ];
            for (off, val) in words {
                let addr = info_addr + off;
                // VA write fails when the guest's stage-1 MMU is off
                // (e.g. inside a guest test); fall back to a direct PA
                // write so the caller still sees the populated struct.
                if !crate::guest_mem::write_word_va(addr, val) {
                    let _ = crate::guest_mem::write_word_pa(addr, val);
                }
            }
            ctx.x[0] = 0;
        }

        // The rest of the sound driver — buffer setup, volume, power,
        // enable/disable — all no-op with r0=0 in Einstein.
        0x05 | 0x06 | 0x07 | 0x08 | 0x09 | 0x0A | 0x0B | 0x0C
        | 0x0D | 0x0E | 0x0F
        | 0x10 | 0x11 | 0x12 | 0x13 | 0x14 | 0x15 | 0x16 | 0x17
        | 0x18 | 0x19 | 0x1A | 0x1B | 0x1C | 0x1D | 0x1E => {
            ctx.x[0] = 0;
        }

        // NativeSetInterruptMask(r1, r2) — Einstein delegates to
        // mSoundManager->SetInterruptMask and leaves r0 unchanged.
        0x1F => {}

        _ => {
            kprintln!(
                "*** unknown sound-driver native primitive subfn={:#x} @PC={:#x}",
                subfn, pc
            );
            kprintln!(
                "    r0={:#x} r1={:#x} r2={:#x} r3={:#x}",
                ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32, ctx.x[3] as u32
            );
            kprintln!(
                "    (extend peripherals/sound.rs::handle to add this subfn)"
            );
            cpu::halt();
        }
    }
}
