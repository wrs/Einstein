//! Sound driver — Rust port of Einstein's `PMainSoundDriver` native
//! primitive class.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x000002. Each subfn mirrors the return
//! value Einstein produces in `TNativePrimitives::ExecuteSoundDriverNative`
//! (`Emulator/TNativePrimitives.cpp:1062-1400`) and, where relevant,
//! forwards to the active [`crate::audio`] backend so the buffer
//! actually reaches a host audio device. With the null backend (the
//! default) the boot path to `TInterpreter` runs identically to the
//! Einstein "no sound" emulation — Einstein returns success for most
//! of these stubs because Newton has no audible-sound requirement on
//! that path, so the kernel proceeds even when nothing actually
//! plays. Subfns that Einstein doesn't model (e.g. 0x01 New, 0x02
//! Delete) fall through to the default branch and halt loudly so we
//! notice if the boot path ever starts depending on real sound
//! state.

use crate::{audio, cpu, kprintln, peripherals::vic, trap::TrapContext};

/// Sound-driver class ID in the native-primitive encoding.
pub const DRIVER_ID: u32 = 0x00_0002;

/// NewtonErrors "Sound hardware not present" — returned by
/// SetSoundHardwareInfo. Matches Einstein's constant in the 0x03 arm of
/// `ExecuteSoundDriverNative`.
const ERR_NO_SOUND_HARDWARE: u32 = (-30009i32) as u32;

/// Per-subfn invocation count. Read by the wedge probe so a runaway
/// poll on, e.g., subfn 0x13 (OutputIsRunning) is visible even when
/// the per-subfn "first call" log filter has long since suppressed it.
static mut SUBFN_COUNT: [u32; 32] = [0; 32];

pub fn snapshot_subfn_counts() -> [u32; 32] {
    // SAFETY: single-threaded EL2.
    unsafe { SUBFN_COUNT }
}

pub fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    // Diagnostic: log first occurrence of each subfn so we can see what
    // the kernel exercises during sound init. Also tally per-subfn
    // invocation counts so a tight kernel poll loop on a sound subfn
    // (e.g., OutputIsRunning 0x13) shows up as a runaway count in the
    // task dump even though the "first" filter suppresses repeats.
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
    if subfn < 32 {
        // SAFETY: single-threaded EL2.
        unsafe { SUBFN_COUNT[subfn as usize] = SUBFN_COUNT[subfn as usize].saturating_add(1); }
    }
    let subfn_count = if subfn < 32 {
        // SAFETY: single-threaded EL2.
        unsafe { SUBFN_COUNT[subfn as usize] }
    } else {
        0
    };
    // Trace the sound state machine's load-bearing subfns: schedule
    // (0x07), start (0x0D), stop (0x0F), running? (0x13), the
    // IRQ-handler path (0x1D), and mask install (0x1F). First 32
    // calls each, then 1-in-64.
    let is_traced_subfn = matches!(subfn, 0x07 | 0x0D | 0x0F | 0x13 | 0x1D | 0x1F);
    if is_traced_subfn && (subfn_count <= 32 || (subfn_count & 0x3F) == 0) {
        kprintln!(
            "sound: subfn {:#04x} #{} r1={:#x} r2={:#x} r3={:#x} ipres={:#x}",
            subfn,
            subfn_count,
            ctx.x[1] as u32,
            ctx.x[2] as u32,
            ctx.x[3] as u32,
            vic::int_present_raw()
        );
    }
    // Subfn arms below mirror Einstein's `ExecuteSoundDriverNative`
    // (`Emulator/TNativePrimitives.cpp:1062-1400`) one-for-one. The
    // Newton kernel picks `case 0x03` first; once it sees
    // `kError_SoundHardware_NoHardware` it skips most of the rest of
    // the sound bring-up, so the boot path to `TInterpreter` doesn't
    // exercise the delegating subfns. Each "STUB" comment is a
    // deviation from Einstein we accept on that basis — convert them
    // to real implementations once the kernel actually drives them.
    match subfn {
        // 0x03 SetSoundHardwareInfo — Einstein returns -30009 to tell
        // the kernel there's no configurable sound hardware. Verbatim.
        0x03 => {
            ctx.x[0] = ERR_NO_SOUND_HARDWARE as u64;
        }

        // 0x04 GetSoundHardwareInfo — Einstein writes a 7-word info
        // struct at [r1] and returns 0. The 0x54600000 at +0x0c is a
        // 32-bit fixed-point sample-rate value the Newton driver
        // expects. Einstein source: TNativePrimitives.cpp:1093-1110.
        0x04 => {
            let info_addr = ctx.x[1] as u32;
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
                if !crate::guest_endian::guest_write_u32_va(addr, val) {
                    let _ = crate::guest_endian::guest_write_u32_pa(addr, val);
                }
            }
            ctx.x[0] = 0;
        }

        // 0x05 SetOutputBuffers(r1, r2, r3, [sp+4]) — Einstein
        // (TNativePrimitives.cpp:1112-1132) stores
        //   mSoundOutputBuffer1Addr = r1
        //   mSoundOutputBuffer2Addr = r3
        // for subfn 0x07 to read back. We hand both VAs to the
        // audio backend so the ping-pong 0x07 calls can pick the
        // right one.
        0x05 => {
            audio::set_output_buffers(ctx.x[1] as u32, ctx.x[3] as u32);
            ctx.x[0] = 0;
        }

        // 0x06 SetInputBuffers — Einstein logs only and returns 0.
        // (TNativePrimitives.cpp:1134-1149.) Verbatim.
        0x06 => {
            ctx.x[0] = 0;
        }

        // 0x07 ScheduleOutputBuffer(r1=which, r2=amount) — Einstein
        // (TNativePrimitives.cpp:1151-1172) picks
        // `mSoundOutputBuffer{1,2}Addr` per `r1` and calls
        // `mSoundManager->ScheduleOutputBuffer(buffer, amount)`. We
        // delegate to the audio backend, which reads the indicated
        // half of the ping-pong, resamples + SPDIF-encodes, and
        // queues a buffer-complete IRQ once the tail catches up.
        0x07 => {
            audio::schedule_output(ctx.x[1] as u32, ctx.x[2] as u32);
            ctx.x[0] = 0;
        }

        // 0x08 ScheduleInputBuffer — Einstein logs only and returns 0.
        // (TNativePrimitives.cpp:1174-1183.) Verbatim.
        0x08 => {
            ctx.x[0] = 0;
        }

        // 0x09..0x0C PowerOutputOn / PowerOutputOff / PowerInputOn /
        // PowerInputOff — Einstein logs only and returns 0 for all
        // four (TNativePrimitives.cpp:1185-1217). Verbatim.
        0x09 | 0x0A | 0x0B | 0x0C => {
            ctx.x[0] = 0;
        }

        // 0x0D StartOutput — Einstein calls
        // `mSoundManager->StartOutput()` and returns 0
        // (TNativePrimitives.cpp:1219-1226). We arm MAI_CTL.ENABLE
        // on the active audio backend so HDMI audio packets start
        // streaming.
        0x0D => {
            audio::start_output();
            ctx.x[0] = 0;
        }

        // 0x0E StartInput — Einstein logs only and returns 0
        // (TNativePrimitives.cpp:1228-1234). Verbatim.
        0x0E => {
            ctx.x[0] = 0;
        }

        // 0x0F StopOutput — Einstein calls
        // `mSoundManager->StopOutput()` and returns 0
        // (TNativePrimitives.cpp:1236-1243). Drop the MAI ENABLE bit
        // so the HDMI receiver doesn't keep hearing residual ring
        // contents between Newton clips.
        0x0F => {
            audio::stop_output();
            ctx.x[0] = 0;
        }

        // 0x10 StopInput — Einstein logs only and returns 0
        // (TNativePrimitives.cpp:1245-1251). Verbatim.
        0x10 => {
            ctx.x[0] = 0;
        }

        // 0x11 OutputIsEnabled, 0x12 InputIsEnabled — Einstein logs
        // only and returns 0 (TNativePrimitives.cpp:1253-1267).
        // Verbatim.
        0x11 | 0x12 => {
            ctx.x[0] = 0;
        }

        // 0x13 OutputIsRunning — Einstein returns
        // `mSoundManager->OutputIsRunning()` (TNativePrimitives.cpp:
        // 1269-1275). Defers to the active audio backend.
        0x13 => {
            ctx.x[0] = if audio::output_is_running() { 1 } else { 0 };
        }

        // 0x14 InputIsRunning, 0x15 CurrentOutputPtr,
        // 0x16 CurrentInputPtr — Einstein logs only and returns 0
        // (TNativePrimitives.cpp:1277-1299). Verbatim.
        0x14 | 0x15 | 0x16 => {
            ctx.x[0] = 0;
        }

        // 0x17 OutputVolume(r1) — Einstein calls
        // `mSoundManager->OutputVolume(r1)` and returns 0
        // (TNativePrimitives.cpp:1301-1310). We stash the value on
        // the backend so the matching 0x18 getter sees the same
        // fader the kernel just set, even though the HDMI MAI
        // hardware has no software fader of its own.
        0x17 => {
            audio::output_volume_set(ctx.x[1] as u32);
            ctx.x[0] = 0;
        }

        // 0x18 OutputVolume getter — Einstein returns
        // `mSoundManager->OutputVolume()` (TNativePrimitives.cpp:
        // 1312-1318). Returns the value passed to the most recent
        // 0x17, or kOutputVolume_Max if the kernel queried first.
        0x18 => {
            ctx.x[0] = audio::output_volume_get() as u64;
        }

        // 0x19 InputVolume(r1) — Einstein clamps r1 to 0xFF and
        // stores it in `mInputVolume`, returns 0
        // (TNativePrimitives.cpp:1320-1336). STUB: dropped; pairs
        // with 0x1A reading 0 below.
        0x19 => {
            ctx.x[0] = 0;
        }

        // 0x1A InputVolume getter — Einstein returns the cached
        // `mInputVolume` set by 0x19 (TNativePrimitives.cpp:1338-1344).
        // STUB: we never stored it, so report 0.
        0x1A => {
            ctx.x[0] = 0;
        }

        // 0x1B EnableExtSoundSource, 0x1C DisableExtSoundSource —
        // Einstein logs only and returns 0 (TNativePrimitives.cpp:
        // 1346-1360). Verbatim.
        0x1B | 0x1C => {
            ctx.x[0] = 0;
        }

        // 0x1D OutputIntHandler, 0x1E InputIntHandler — Einstein
        // logs only and returns 0 (TNativePrimitives.cpp:1362-1376).
        // Verbatim.
        0x1D | 0x1E => {
            ctx.x[0] = 0;
        }

        // 0x1F NativeSetInterruptMask(r1=in_mask, r2=out_mask) —
        // Einstein calls `mSoundManager->SetInterruptMask(r1, r2)`
        // and explicitly leaves r0 unchanged (no `SetRegister(0,…)`
        // in TNativePrimitives.cpp:1378-1389). The output mask is
        // what the backend raises through `vic::raise` after a
        // buffer's worth of samples has drained, so the kernel
        // calls 0x07 again with the next half of the ping-pong. We
        // mirror Einstein's "r0 untouched" detail.
        0x1F => {
            audio::set_interrupt_mask(ctx.x[1] as u32, ctx.x[2] as u32);
        }

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
