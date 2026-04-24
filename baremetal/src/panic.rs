use core::panic::PanicInfo;
use core::sync::atomic::{compiler_fence, Ordering};

use crate::kprintln;

// When building under `cargo test --target=<host>`, std provides the
// panic_impl lang item; gate ours out so we don't hit a duplicate.
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Best-effort: the UART may be the thing that panicked, but retrying
    // costs us nothing.
    kprintln!();
    kprintln!("*** PANIC ***");
    if let Some(loc) = info.location() {
        kprintln!("  at {}:{}:{}", loc.file(), loc.line(), loc.column());
    }
    kprintln!("  {}", info.message());
    kprintln!("*** HALTED ***");

    compiler_fence(Ordering::SeqCst);

    loop {
        // SAFETY: `wfe` has no operands and no memory effects.
        unsafe { core::arch::asm!("wfe", options(nomem, nostack, preserves_flags)); }
    }
}
