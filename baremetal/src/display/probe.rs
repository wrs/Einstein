//! Real-hardware bring-up probe for the VC framebuffer path.
//!
//! Called from `kmain` under `#[cfg(feature = "fb-probe")]`. Allocates
//! a framebuffer at the panel's native resolution, paints a known
//! gradient, halts. If a monitor is attached over mini-HDMI you should
//! see a left-to-right red → green gradient covering the whole
//! display. Anything else means we got pixel order, pitch, or
//! channel packing wrong.

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

    kprintln!("fb: painting red → green gradient...");
    fb::fill_h_gradient(&info, red, green);
    kprintln!("fb: paint done; halt");

    cpu::halt();
}
