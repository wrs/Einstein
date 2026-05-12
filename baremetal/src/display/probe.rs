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

    // Wait 2 s before painting so any firmware-side display
    // initialization (splash, status indicators, mode setup) has
    // a chance to finish before we touch the framebuffer. Earlier
    // halt-mode runs flickered on some boots, suggesting a race
    // with firmware activity; this gates that hypothesis.
    let freq = cntfrq();
    let wait_ticks = freq * 2;
    kprintln!("fb: waiting 2 s for firmware to quiesce...");
    let deadline = cntpct().wrapping_add(wait_ticks);
    while cntpct() < deadline {
        // SAFETY: yield has no side effects.
        unsafe { asm!("yield", options(nomem, nostack, preserves_flags)) };
    }

    kprintln!("fb: painting red → green gradient + top-rows blue, then halt");
    fb::fill_h_gradient(&info, red, green);
    fb::fill_top_rows(&info, 32, blue);

    kprintln!("fb: paint done; halt");
    cpu::halt();
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
