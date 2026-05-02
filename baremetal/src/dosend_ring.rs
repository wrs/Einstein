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
///
/// For each entry, also dumps the receiver and implementor heap
/// objects' first few words via stage-1-translated reads — that's
/// where the type-mismatch evidence lives (the implementor's
/// header word == 2 is exactly the trip-wire DoSend hits).
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
        dump_heap_obj("    recv", e.recv);
        dump_heap_obj("    meth (FindImplementor result)", e.method);
        dump_heap_obj("    args (methodName symbol)", e.args);
    }
}

/// Dump the first 8 words of a heap object at the given Ref-tagged
/// address. Skips the dump for non-pointer refs (low 2 bits != 00)
/// and for addresses outside the readable RAM/ROM regions.
fn dump_heap_obj(label: &str, ref_value: u32) {
    if (ref_value & 0x3) != 0 {
        kprintln!("{}: ref={:#010x} (immediate, not a pointer)", label, ref_value);
        return;
    }
    if ref_value == 0 {
        kprintln!("{}: ref=NULL", label);
        return;
    }
    let addr = ref_value & !0x3;
    kprintln!("{}: ref={:#010x} → header+words at {:#010x}:", label, ref_value, addr);
    for off in 0..8u32 {
        let p = addr.wrapping_add(off * 4);
        let v = crate::guest_mem::read_word_va(p)
            .or_else(|| crate::guest_mem::read_word_pa(p))
            .unwrap_or(0xDEADBEEF);
        kprintln!("      [{:+#04x}] @{:#010x} = {:#010x}", off * 4, p, v);
    }
}
