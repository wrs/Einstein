//! Ring buffer of the most recent `DoSend` invocations (iter-76
//! probe). Lives separately from `trap.rs` so the dump helper has
//! a clean import path and the buffer survives across the throw
//! handler that consumes it.
//!
//! The log of every DoSend on a boot that walks NewtonScript would
//! flood the UART (the kernel's NS interpreter dispatches dozens
//! to hundreds of sends per second). Instead we keep the latest
//! `CAP` entries and dump them when `evt.ex.fr.intrp;type.ref.frame`
//! fires — that gives the call sequence leading to the bad send.

use crate::kprintln;

const CAP: usize = 16;

#[derive(Clone, Copy)]
struct Entry {
    seq: u32,
    recv: u32,
    method: u32,
    args: u32,
    argc: u32,
    caller_lr: u32,
}

const EMPTY: Entry = Entry {
    seq: 0,
    recv: 0,
    method: 0,
    args: 0,
    argc: 0,
    caller_lr: 0,
};

static mut RING: [Entry; CAP] = [EMPTY; CAP];
static mut WRITE_POS: usize = 0;
static mut FILLED: usize = 0;

pub fn record(seq: u32, recv: u32, method: u32, args: u32, argc: u32, caller_lr: u32) {
    // SAFETY: single-threaded EL2.
    unsafe {
        RING[WRITE_POS] = Entry { seq, recv, method, args, argc, caller_lr };
        WRITE_POS = (WRITE_POS + 1) % CAP;
        if FILLED < CAP {
            FILLED += 1;
        }
    }
}

/// Print the most-recent DoSend invocations in chronological order.
/// Called from the type-mismatch throw probe so the operator sees
/// the call sequence that led to the bad send.
pub fn dump(label: &str) {
    // SAFETY: single-threaded EL2.
    let (filled, write_pos) = unsafe { (FILLED, WRITE_POS) };
    if filled == 0 {
        kprintln!("dosend_ring ({}): empty", label);
        return;
    }
    kprintln!("dosend_ring ({}): last {} invocations (oldest first):", label, filled);
    let start = if filled < CAP { 0 } else { write_pos };
    for i in 0..filled {
        let idx = (start + i) % CAP;
        // SAFETY: single-threaded EL2.
        let e = unsafe { RING[idx] };
        kprintln!(
            "  #{}: recv={:#010x} meth={:#010x} args={:#010x} argc={} caller_lr={:#010x}",
            e.seq, e.recv, e.method, e.args, e.argc, e.caller_lr,
        );
    }
}
