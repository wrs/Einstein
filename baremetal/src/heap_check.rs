//! iter-78: classify a Newton NS Ref against the runtime object-heap
//! bounds.
//!
//! Newton's NewtonScript Ref tag scheme (verified against
//! `IsInt__FRC6RefVar` @ ROM 0x31c6c4 and friends):
//!
//! ```text
//!   low 2 bits  meaning
//!   00          integer            value = (Ref as i32) >> 2
//!   01          real pointer       address = Ref - 1   (heap or ROM frame)
//!   10          immediate          NIL=0x02, TRUE=0x1A, char (low byte=tag)
//!   11          magic pointer      ROM-table index = Ref >> 2
//! ```
//!
//! The runtime object heap is owned by a `TObjectHeap` allocated by
//! `InitObjects__Fv` (ROM 0x31c608). The constructor stores the
//! heap's [lo, hi) bounds at offsets +8 and +12 (see
//! `__ct__11TObjectHeapFlT1` @ ROM 0x31cafc), and the resulting
//! `TObjectHeap*` is written to the global at IPA `0x0c105548`
//! (the literal at `0x31c684`). `InHeap__11TObjectHeapFl` does a
//! plain `lo <= addr < hi` check — same logic mirrored here.
//!
//! Read-the-heap reliably returns `None` early (before
//! `InitObjects__Fv` has run) so callers can fall back to a
//! tag-only classification.
//!
//! Used by the iter-75/76/77 throw / DoSend / dosend-ring probes
//! to label captured Refs as "in-heap pointer", "ROM pointer",
//! or "OUTSIDE heap" — which is the missing piece that iter-77
//! left ambiguous (we couldn't tell if the receiver / implementor
//! Refs were genuine heap objects or stale stack/register junk).

use crate::guest_mem;
use core::sync::atomic::{AtomicU32, Ordering};

/// Address of the global `TObjectHeap*` written by `InitObjects__Fv`.
const G_OBJECT_HEAP: u32 = 0x0c10_5548;

/// Cached `(lo, hi)` from the last successful `heap_bounds()` read.
/// Never goes stale once populated — the heap's outer extent is
/// fixed at boot. Sentinel `(0, 0)` means "not read yet".
static CACHED_LO: AtomicU32 = AtomicU32::new(0);
static CACHED_HI: AtomicU32 = AtomicU32::new(0);

fn read_word(va: u32) -> Option<u32> {
    guest_mem::read_word_va(va).or_else(|| guest_mem::read_word_pa(va))
}

/// Read the runtime object heap's `[lo, hi)` bounds. Returns
/// `None` before `InitObjects__Fv` has run (the global is still
/// zero) or if any of the dependent reads fail.
pub fn heap_bounds() -> Option<(u32, u32)> {
    let cached_lo = CACHED_LO.load(Ordering::Relaxed);
    let cached_hi = CACHED_HI.load(Ordering::Relaxed);
    if cached_lo != 0 || cached_hi != 0 {
        return Some((cached_lo, cached_hi));
    }
    let heap_ptr = read_word(G_OBJECT_HEAP)?;
    if heap_ptr == 0 {
        return None;
    }
    let lo = read_word(heap_ptr.wrapping_add(8))?;
    let hi = read_word(heap_ptr.wrapping_add(12))?;
    if lo == 0 && hi == 0 {
        return None;
    }
    if lo >= hi {
        return None;
    }
    CACHED_LO.store(lo, Ordering::Relaxed);
    CACHED_HI.store(hi, Ordering::Relaxed);
    Some((lo, hi))
}

/// Where a real-pointer Ref's underlying address lives.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PtrLoc {
    /// Inside `[heap_lo, heap_hi)` — a runtime-allocated NS object.
    InHeap,
    /// Outside the heap range. Could be a ROM frame (low addresses)
    /// or stale junk; `kind_label` distinguishes by ROM range.
    OutOfHeap,
    /// Heap not yet initialised — can't tell.
    HeapUnknown,
}

/// Classify the address of a real-pointer Ref against heap bounds.
pub fn classify_ptr(addr: u32) -> PtrLoc {
    match heap_bounds() {
        Some((lo, hi)) if addr >= lo && addr < hi => PtrLoc::InHeap,
        Some(_) => PtrLoc::OutOfHeap,
        None => PtrLoc::HeapUnknown,
    }
}

/// Print a one-line classification of a Ref to the kernel log,
/// prefixed by `label`. Decodes the tag bits and, for real
/// pointers, reports heap-membership and the underlying address.
///
/// Examples:
///   "    recv: ref=0x0c109f01 → real-ptr in-heap @0x0c109f00"
///   "    args: ref=0x00684085 → real-ptr ROM     @0x00684084"
///   "    meth: ref=0x00000002 → NIL"
///   "    recv: ref=0x0000003c → integer 15"
pub fn log_ref(label: &str, ref_value: u32) {
    let tag = ref_value & 0x3;
    match tag {
        0 => {
            let v = (ref_value as i32) >> 2;
            crate::kprintln!("{}: ref={:#010x} → integer {}", label, ref_value, v);
        }
        1 => {
            let addr = ref_value & !0x3; // ref - 1, but pointers are 4-aligned
            let class = match classify_ptr(addr) {
                PtrLoc::InHeap => "in-heap",
                PtrLoc::OutOfHeap if addr < 0x0100_0000 => "ROM   ",
                PtrLoc::OutOfHeap => "OUT-OF-HEAP",
                PtrLoc::HeapUnknown => "heap?",
            };
            crate::kprintln!(
                "{}: ref={:#010x} → real-ptr {} @{:#010x}",
                label, ref_value, class, addr
            );
        }
        2 => match ref_value {
            0x0000_0002 => crate::kprintln!("{}: ref={:#010x} → NIL", label, ref_value),
            0x0000_001a => crate::kprintln!("{}: ref={:#010x} → TRUE", label, ref_value),
            _ => crate::kprintln!(
                "{}: ref={:#010x} → immediate (char/special)",
                label, ref_value
            ),
        },
        3 => {
            let idx = ref_value >> 2;
            crate::kprintln!("{}: ref={:#010x} → magic-ptr {}", label, ref_value, idx);
        }
        _ => unreachable!(),
    }
}

/// One-shot summary of the heap bounds, suitable for an iter-78
/// boot-log line. Logs nothing if the heap isn't constructed yet.
pub fn log_heap_bounds_once() {
    static LOGGED: AtomicU32 = AtomicU32::new(0);
    if LOGGED.swap(1, Ordering::Relaxed) != 0 {
        return;
    }
    if let Some((lo, hi)) = heap_bounds() {
        crate::kprintln!(
            "heap_check: TObjectHeap @{:#010x} → [{:#010x}, {:#010x}) ({} KiB)",
            read_word(G_OBJECT_HEAP).unwrap_or(0),
            lo,
            hi,
            (hi - lo) / 1024,
        );
    }
}

// ----- newton-objects integration ---------------------------------------
//
// Pull-in: dump the structure of an object pointed to by a real-pointer
// Ref. Works for both heap-resident objects and ROM-resident objects
// (e.g. ROM symbols). The runtime stores objects in CPU-native byte
// order — little-endian on this Cortex-A53 — so we feed `newton-objects`
// a `Heap` configured with `Endian::Little`. Pointer Refs in the
// runtime use absolute addresses (not file offsets), so the buffer's
// load-address is the raw object address.

/// Maximum bytes copied per object dump. Big enough to cover the
/// header + class/map word + a reasonable number of slots. A frame /
/// array with more slots than this fits will report the truncation
/// rather than fail.
const DUMP_BUF_BYTES: usize = 256;

/// Print a human-readable structured dump of the object pointed to by
/// `ref_value`, using the `newton-objects` parser. Only acts on
/// real-pointer Refs (low 2 bits == 01). The pointed-to bytes are
/// read via `guest_mem::read_word_va` (with PA fallback) into a
/// stack-resident buffer, then handed to a little-endian
/// `newton_objects::Heap` view.
///
/// `indent` is prefixed to each output line.
pub fn dump_object(indent: &str, ref_value: u32) {
    if (ref_value & 0x3) != 0x1 {
        return;
    }
    let addr = ref_value & !0x3;
    let mut buf = [0u8; DUMP_BUF_BYTES];
    let n = match read_object_bytes(addr, &mut buf) {
        Some(n) => n,
        None => {
            crate::kprintln!("{}<unreadable @{:#010x}>", indent, addr);
            return;
        }
    };
    let bytes = &buf[..n];
    // Newton's ROM is byteswapped at load time to make u32 reads
    // numerically correct on a little-endian CPU; this reverses
    // bytes *within each word*. To restore the on-disk byte
    // sequence (so symbol names read in the correct order), we
    // wrote each word via `to_be_bytes` above. Parsing as
    // big-endian then gives both correct u32 values *and* correct
    // byte-level data (names, character data, raw binary blobs).
    let heap = newton_objects::Heap::with_load_addr(bytes, addr);
    match heap.object_at(addr) {
        Ok(obj) => print_object(indent, obj, /*depth=*/ 0),
        Err(e) => crate::kprintln!("{}parse error @{:#010x}: {}", indent, addr, e),
    }
}

/// Read up to `out.len()` bytes starting at `addr` into `out`. Stops
/// short on the first failed translation. Returns the number of bytes
/// actually read, or `None` if the very first word fails (i.e. the
/// address is not mapped at all).
fn read_object_bytes(addr: u32, out: &mut [u8]) -> Option<usize> {
    let mut written = 0;
    let mut cursor = addr;
    while written + 4 <= out.len() {
        let w = guest_mem::read_word_va(cursor).or_else(|| guest_mem::read_word_pa(cursor));
        let w = match w {
            Some(w) => w,
            None => break,
        };
        // Write each word as big-endian bytes so the buffer
        // mirrors the original on-disk byte order: u32 reads with
        // Endian::Big still produce the correct numeric value, but
        // byte-level data (e.g. a symbol's name) appears in the
        // intended sequential order rather than reversed within
        // each 4-byte chunk.
        out[written..written + 4].copy_from_slice(&w.to_be_bytes());
        written += 4;
        cursor = cursor.wrapping_add(4);
    }
    if written == 0 { None } else { Some(written) }
}

/// One-line object summary plus a few interior slots. The depth
/// parameter is reserved for future recursive expansion (resolving
/// frame map names etc.); for iter-78 we keep it at depth 0 to avoid
/// blowing the UART budget on cyclic structures.
fn print_object(indent: &str, obj: newton_objects::Object<'_>, _depth: u32) {
    use newton_objects::Object;
    match obj {
        Object::Binary(b) => {
            let class = b.class();
            if let Some(sym) = b.as_symbol() {
                match sym.name() {
                    Ok(n) => crate::kprintln!(
                        "{}symbol '{} (hash={:#010x}) @{:#010x} size={}",
                        indent, n, sym.hash(), b.offset(), b.size()
                    ),
                    Err(_) => crate::kprintln!(
                        "{}symbol <bad-utf8> @{:#010x} size={}",
                        indent, b.offset(), b.size()
                    ),
                }
            } else {
                crate::kprintln!(
                    "{}binary class={:?} @{:#010x} size={} (data {} B)",
                    indent, class, b.offset(), b.size(), b.data().len()
                );
            }
        }
        Object::Array(a) => {
            crate::kprintln!(
                "{}array class={:?} @{:#010x} size={} len={}",
                indent, a.class(), a.offset(), a.size(), a.len()
            );
            for (i, r) in a.iter().enumerate().take(8) {
                crate::kprintln!("{}  [{}] = {:?}", indent, i, r);
            }
            if a.len() > 8 {
                crate::kprintln!("{}  ... ({} more slots)", indent, a.len() - 8);
            }
        }
        Object::Frame(f) => {
            crate::kprintln!(
                "{}frame map={:?} @{:#010x} size={} len={}",
                indent, f.map(), f.offset(), f.size(), f.len()
            );
            for (i, r) in f.iter_slots().enumerate().take(8) {
                crate::kprintln!("{}  slot[{}] = {:?}", indent, i, r);
            }
            if f.len() > 8 {
                crate::kprintln!("{}  ... ({} more slots)", indent, f.len() - 8);
            }
        }
    }
}
