//! Real-hardware bring-up probe for the VC framebuffer path.
//!
//! Called from `kmain` under `#[cfg(feature = "fb-probe")]`. Allocates
//! a framebuffer at the panel's native resolution, paints a known
//! gradient, halts. If a monitor is attached over mini-HDMI you should
//! see a left-to-right red → green gradient covering the whole
//! display. Anything else means we got pixel order, pitch, or
//! channel packing wrong.

use core::arch::asm;

use crate::cpu;
use crate::kprintln;

use super::fb;

pub fn run() -> ! {
    kprintln!("\r\n=== fb probe ===");

    let info = match fb::alloc_native() {
        Ok(i) => i,
        Err(e) => {
            kprintln!("fb: alloc FAILED: {:?}", e);
            cpu::halt();
        }
    };
    kprintln!(
        "fb: {}x{} {} bpp pitch={} pa=0x{:x} size={}",
        info.width, info.height, info.bpp, info.pitch, info.pa, info.size
    );

    // Red on the left, green on the right. RGB pixel-order request
    // means byte 0 = R, byte 1 = G, byte 2 = B, byte 3 = pad. So
    // red = 0x0000_00FF in u32 little-endian — but the pixel write
    // takes a u32 with byte 0 in the low byte. Build via to_le_bytes
    // to keep the channel mapping explicit.
    let red = u32::from_le_bytes([0xFF, 0x00, 0x00, 0x00]);
    let green = u32::from_le_bytes([0x00, 0xFF, 0x00, 0x00]);

    let blue = u32::from_le_bytes([0x00, 0x00, 0xFF, 0x00]);

    // Continuous repaint at ~60 Hz. Earlier runs showed intermittent
    // flicker on some boots; this loop diagnoses whether the flicker
    // is something *else* writing into our FB (then a fast repaint
    // should win the race and the image stays steady) or our writes
    // not landing (then flicker continues regardless).
    //
    // No `halt` — the diagnostic IS the loop. Power-cycle to exit.
    kprintln!("fb: continuous repaint loop (Ctrl-C / power off to stop)");
    let freq = cntfrq();
    let interval_ticks = freq / 60; // ~16.6 ms
    let mut next = cntpct().wrapping_add(interval_ticks);
    let mut frame = 0u64;
    loop {
        fb::fill_h_gradient(&info, red, green);
        fb::fill_top_rows(&info, 32, blue);
        frame += 1;
        if frame.is_multiple_of(60) {
            // Once a second so we can see the loop is alive over serial.
            kprintln!("fb: frame {}", frame);
        }
        while cntpct() < next {
            // SAFETY: ISB has no side effects; spinning is fine.
            unsafe { asm!("yield", options(nomem, nostack, preserves_flags)) };
        }
        next = next.wrapping_add(interval_ticks);
    }
}

#[inline]
fn cntpct() -> u64 {
    let v: u64;
    // SAFETY: MRS of a RO sysreg has no side effects.
    unsafe {
        asm!("mrs {}, cntpct_el0", out(reg) v,
             options(nomem, nostack, preserves_flags));
    }
    v
}

#[inline]
fn cntfrq() -> u64 {
    let v: u64;
    // SAFETY: as above.
    unsafe {
        asm!("mrs {}, cntfrq_el0", out(reg) v,
             options(nomem, nostack, preserves_flags));
    }
    v
}
