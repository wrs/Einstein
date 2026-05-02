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
//!
//! iter-78: store the actual tagged Refs (the probe now does the
//! correct double-indirection of `RefVar const&`) and dump each via
//! `heap_check::log_ref` so the operator can immediately tell
//! "this Ref is NIL", "this Ref points into the runtime heap", or
//! "this Ref points at a ROM frame".
//!
//! Field naming follows DoSend's actual signature
//! `DoSend(receiver, implementor, methodName, argc)` — iter-76's
//! "args" slot was actually the methodName; iter-77's "meth" slot
//! was actually the implementor (the FindImplementor result).

use crate::kprintln;

const CAP: usize = 16;

#[derive(Clone, Copy)]
struct Entry {
    seq: u32,
    recv: u32,
    impl_: u32,
    method: u32,
    argc: u32,
    caller_lr: u32,
}

const EMPTY: Entry = Entry {
    seq: 0,
    recv: 0,
    impl_: 0,
    method: 0,
    argc: 0,
    caller_lr: 0,
};

static mut RING: [Entry; CAP] = [EMPTY; CAP];
static mut WRITE_POS: usize = 0;
static mut FILLED: usize = 0;

pub fn record(seq: u32, recv: u32, impl_: u32, method: u32, argc: u32, caller_lr: u32) {
    // SAFETY: single-threaded EL2.
    unsafe {
        RING[WRITE_POS] = Entry { seq, recv, impl_, method, argc, caller_lr };
        WRITE_POS = (WRITE_POS + 1) % CAP;
        if FILLED < CAP {
            FILLED += 1;
        }
    }
}

/// Print the most-recent DoSend invocations in chronological order.
/// Called from the type-mismatch throw probe so the operator sees
/// the call sequence that led to the bad send.
///
/// For each entry, classifies each captured Ref via
/// `heap_check::log_ref` (tag-decoded; for real-pointer Refs,
/// reports heap-membership). When a Ref points into the runtime
/// object heap, also dumps the first 8 words at the underlying
/// address — that's where `objHeader / class / size` live.
pub fn dump(label: &str) {
    // SAFETY: single-threaded EL2.
    let (filled, write_pos) = unsafe { (FILLED, WRITE_POS) };
    if filled == 0 {
        kprintln!("dosend_ring ({}): empty", label);
        return;
    }
    kprintln!("dosend_ring ({}): last {} invocations (oldest first):", label, filled);
    crate::heap_check::log_heap_bounds_once();
    let start = if filled < CAP { 0 } else { write_pos };
    for i in 0..filled {
        let idx = (start + i) % CAP;
        // SAFETY: single-threaded EL2.
        let e = unsafe { RING[idx] };
        kprintln!(
            "  #{}: recv={:#010x} impl={:#010x} meth={:#010x} argc={} caller_lr={:#010x}",
            e.seq, e.recv, e.impl_, e.method, e.argc, e.caller_lr,
        );
        classify_and_dump("    recv", e.recv);
        classify_and_dump("    impl", e.impl_);
        classify_and_dump("    meth", e.method);
    }
}

/// Print a tag-classification for `ref_value`, and if it's a real
/// pointer (heap or ROM), dump the structured object via
/// `newton-objects`. ROM-resident pointers (e.g. a method-name
/// symbol) are dumped just like heap-resident ones because Newton
/// stores both as the same packed-object layout.
fn classify_and_dump(label: &str, ref_value: u32) {
    crate::heap_check::log_ref(label, ref_value);
    crate::heap_check::dump_object("      ", ref_value);
}
