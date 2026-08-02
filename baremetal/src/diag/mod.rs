//! Diagnostics: halt-path dumps, trap history, symbolication, guest
//! breakpoints, tracer. Reachable from any layer.

pub mod diag_util;
pub mod guest_bp;
pub mod heap_check;
pub mod rep_print;
pub mod symbols;
pub mod task_dump;
#[cfg(feature = "trace")]
pub mod tracer;
pub mod trap_diag;
pub mod trap_hist;
