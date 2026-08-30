use core::panic::PanicInfo;

use crate::println;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!();
    println!("*** nhboot PANIC ***");
    if let Some(loc) = info.location() {
        println!("  at {}:{}:{}", loc.file(), loc.line(), loc.column());
    }
    println!("  {}", info.message());
    if crate::stack_guard_intact() {
        println!("*** HALTED ***");
    } else {
        println!("*** HALTED (stack guard clobbered — overflow) ***");
    }
    crate::park()
}
