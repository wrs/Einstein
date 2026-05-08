// Many of the helpers in this module were called only by the iter-50..89
// diagnostic probes that the BE-8 migration Phase 0 sweep removed. The
// general-purpose Ref classifier / pretty-printer is retained for future
// debugging iterations — including a one-line update to `read_object_bytes`
// in Phase 4 of the migration. Silence the now-unused warnings until then.
#![allow(dead_code)]

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

use core::sync::atomic::{AtomicU32, Ordering};

/// Address of the global `TObjectHeap*` written by `InitObjects__Fv`.
const G_OBJECT_HEAP: u32 = 0x0c10_5548;

/// Cached `(lo, hi)` from the last successful `heap_bounds()` read.
/// Never goes stale once populated — the heap's outer extent is
/// fixed at boot. Sentinel `(0, 0)` means "not read yet".
static CACHED_LO: AtomicU32 = AtomicU32::new(0);
static CACHED_HI: AtomicU32 = AtomicU32::new(0);

fn read_word(va: u32) -> Option<u32> {
    crate::guest_endian::guest_read_u32_va(va)
        .or_else(|| crate::guest_endian::guest_read_u32_pa(va))
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
///
/// (Historically also fired the iter-79 "force kernel diagnostics
/// on" sequence — see `force_kernel_diag_on` below. iter-108
/// disabled that call: it sets `gWantSerialDebugging`, which makes
/// the kernel's FPE call `WriteDebugByte` for emulation tracing,
/// and the debug ring-buffer at `obj[28]` is NULL when called from
/// UND mode → strb to address 0 → unknown-MMIO halt at PC=0x199ce8.
/// Re-enable when a real serial-debug path is wired through to the
/// EL2 UART that doesn't depend on the kernel's ring-buffer init.
/// Under `ns_trace`, the lighter-weight `force_interpreter_trace_on`
/// poke is used instead — it flips only `gInterpreter[+124]=1` so
/// the TInterpreter trace gates open without enabling the kernel's
/// IsSerialDebugging-gated paths that trip the FPE/WriteDebugByte
/// crash.)
pub fn log_heap_bounds_once() {
    static LOGGED: AtomicU32 = AtomicU32::new(0);
    if LOGGED.load(Ordering::Relaxed) != 0 {
        return;
    }
    if let Some((lo, hi)) = heap_bounds() {
        // Set the latch BEFORE logging so an IRQ-side caller and a
        // probe-side caller racing on the same tick don't double-log.
        LOGGED.store(1, Ordering::Relaxed);
        crate::kprintln!(
            "heap_check: TObjectHeap @{:#010x} → [{:#010x}, {:#010x}) ({} KiB)",
            read_word(G_OBJECT_HEAP).unwrap_or(0),
            lo,
            hi,
            (hi - lo) / 1024,
        );
        #[cfg(feature = "ns_trace")]
        force_interpreter_trace_on();
    }
    // If `heap_bounds()` returned None (heap not yet up), leave the
    // latch clear so a later poll can succeed.
}

// ----- iter-79: force-enable kernel diagnostic flags -----------------
//
// Two Newton-side flags gate most of the kernel's diagnostic
// output:
//
// 1. `gWantSerialDebugging` — a packed u32 at IPA `0x0c1017c4`
//    (the +16 field of the global at `0x0c1017b4`, set by
//    `SetgWantSerialDebugging__FUl` @ `0x199e68`). Encoding:
//    high byte must be `0x48` to validate, low 24 bits are
//    sub-flag bits queried via `IsSerialDebuggingAndFlag`
//    (`0x199e80`). On a stock boot this stays 0 because nothing
//    sends the Hammer handshake. We force it to `0x48FFFFFF`
//    so every IsSerialDebugging-gated branch (~30 sites in
//    717006) takes the "yes, log it" path.
//
// 2. `gInterpreter[124]` — the byte at offset `+0x7C` in the
//    `TInterpreter` singleton. `gInterpreter` is reachable as
//    `*0x0c105458` (cf. DoSend's literal at `0x2f06a4`). When
//    non-zero, every `DoSend / DoMessage / DoFastApply` calls
//    `TraceSend / TraceCall / TraceApply` which funnel into
//    `TraceMethod` → `Print(POutTranslator*, fmt, ...)`. With our
//    Print thunk hook in place that surfaces every NS-level
//    call into the EL2 UART.
//
// `force_kernel_diag_on` is invoked once (from
// `log_heap_bounds_once`, which fires after `InitInterpreter__Fv`
// has already run). Both writes are word-sized into kernel-data
// RAM that's mapped writable; failure to translate is logged
// and silently dropped.

const G_WANT_SERIAL_DEBUGGING: u32 = 0x0c10_17c4;
const G_INTERPRETER_PTR:       u32 = 0x0c10_5458;
const TINTERPRETER_TRACE_OFF:  u32 = 124;

fn write_word(va: u32, value: u32) -> bool {
    if crate::guest_endian::guest_write_u32_va(va, value) {
        return true;
    }
    crate::guest_endian::guest_write_u32_pa(va, value)
}

/// Subset of `force_kernel_diag_on` that pokes ONLY
/// `gInterpreter[+124] = 1` — the TInterpreter trace gate. This
/// causes every `DoSend / DoMessage / DoFastApply` to call
/// `TraceSend / TraceCall / TraceApply`, which funnel into
/// `TraceMethod → Print(POutTranslator*, fmt, ...)`. With the
/// `ns_trace` feature's TraceSetOptions ROM patch in place plus
/// the always-on PHammerOutTranslator body patches routing
/// Print → EL2 UART, every NS-level call surfaces in the log.
///
/// Deliberately does NOT touch `gWantSerialDebugging`: setting
/// that triggers `WriteDebugByte` calls from the kernel's FPE
/// handler running in UND mode, where the debug ring-buffer
/// pointer at obj[28] is NULL → strb to address 0 → unknown-MMIO
/// halt at PC=0x199ce8. See iter-108 for the regression history.
#[cfg(feature = "ns_trace")]
fn force_interpreter_trace_on() {
    match read_word(G_INTERPRETER_PTR) {
        Some(p) if p != 0 => {
            if write_word(p.wrapping_add(TINTERPRETER_TRACE_OFF), 1) {
                crate::kprintln!(
                    "force_diag: TInterp_trace=on (gInterpreter={:#010x})",
                    p,
                );
            } else {
                crate::kprintln!(
                    "force_diag: TInterp_trace=ERR (gInterpreter={:#010x}, write failed)",
                    p,
                );
            }
        }
        _ => {
            crate::kprintln!("force_diag: TInterp_trace=skip (gInterpreter not init)");
        }
    }
}

fn force_kernel_diag_on() {
    let mut summary = [0u8; 96];
    let mut n = 0usize;

    // (1) gWantSerialDebugging — 0x48 sentinel + all sub-flags on.
    if write_word(G_WANT_SERIAL_DEBUGGING, 0x48FF_FFFF) {
        summary[n..n + 13].copy_from_slice(b"WantSerial=on");
        n += 13;
    } else {
        summary[n..n + 14].copy_from_slice(b"WantSerial=ERR");
        n += 14;
    }

    // (2) TInterpreter trace flag.
    summary[n] = b' ';
    n += 1;
    match read_word(G_INTERPRETER_PTR) {
        Some(p) if p != 0 => {
            if write_word(p.wrapping_add(TINTERPRETER_TRACE_OFF), 1) {
                summary[n..n + 11].copy_from_slice(b"TInterp=on ");
                n += 11;
            } else {
                summary[n..n + 12].copy_from_slice(b"TInterp=ERR ");
                n += 12;
            }
        }
        _ => {
            summary[n..n + 17].copy_from_slice(b"TInterp=not-init ");
            n += 17;
        }
    }

    let s = core::str::from_utf8(&summary[..n]).unwrap_or("<utf8>");
    crate::kprintln!("force_diag: {}", s);
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
/// read via `guest_endian::guest_read_bytes_va` into a
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
    let n = match crate::guest_endian::guest_read_bytes_va(addr, &mut buf) {
        Some(n) => n,
        None => {
            crate::kprintln!("{}<unreadable @{:#010x}>", indent, addr);
            return;
        }
    };
    let bytes = &buf[..n];
    // `guest_read_bytes_va` returns Newton-side logical-byte order,
    // which is the on-disk byte sequence (high byte first within a
    // word). Parsing as big-endian gives both correct u32 values and
    // correct byte-level data (symbol names, character data, raw
    // binary blobs).
    let heap = newton_objects::Heap::with_load_addr(bytes, addr);
    match heap.object_at(addr) {
        Ok(obj) => print_object(indent, obj, /*depth=*/ 0),
        Err(e) => crate::kprintln!("{}parse error @{:#010x}: {}", indent, addr, e),
    }
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

// ---------------- compact pretty printer ------------------------------
//
// One-line NewtonScript-style rendering of a Ref:
//
//   integer       → 1234
//   character     → $a   (or $É for non-ASCII)
//   true / nil    → true / nil
//   special       → <? #hex>
//   magic-ptr     → @table.index
//   symbol        → 'name
//   'string       → "text"          (UCS-2 BE on Newton, ASCII subset
//                                    decoded; non-printable → \uXXXX)
//   array         → [a, b, c, ...]
//   frame         → {key: v, key: v, ...}
//   unreadable / parse error / unknown binary → <? #hex>
//
// `depth` controls how many levels of *structure* to expand. At
// depth=0 every pointer Ref prints as `#hex` — including symbols
// and strings; turn the dial up to see contents. At depth=N, an
// array/frame is expanded once and each of its slot Refs is
// rendered at depth=N-1.
//
// Per recursion level we read a fresh 256-byte buffer from guest
// memory (≈ stack budget); keep `depth` ≤ 4.

/// Pretty-print a NewtonScript Ref on a single line, with
/// `depth` levels of structural expansion (default 0 — pointers
/// render as `#hex`). Emits "label: <ref>\n"; pass `""` for a
/// bare-line print. Inline composition (no newline, no label) is
/// available via [`pretty_print_ref_inline`].
pub fn pretty_print_ref(label: &str, ref_value: u32, depth: u32) {
    if !label.is_empty() {
        crate::kprint!("{}: ", label);
    }
    write_ref(ref_value, depth);
    crate::kprintln!();
}

/// As [`pretty_print_ref`], but emits only the compact rendering —
/// no label, no trailing newline. Use to compose probe headers
/// like `kprint!("StorePermObject[{}]: ", n);
/// pretty_print_ref_inline(r, 1); kprintln!(" lr={:#x}", lr);`.
pub fn pretty_print_ref_inline(ref_value: u32, depth: u32) {
    write_ref(ref_value, depth);
}

fn write_ref(ref_value: u32, depth: u32) {
    use newton_objects::{Ref, RefKind};
    let r = Ref(ref_value);
    if r.0 == Ref::TRUE.0 {
        crate::kprint!("true");
        return;
    }
    if r.is_nil() {
        crate::kprint!("nil");
        return;
    }
    match r.kind() {
        RefKind::Integer(i) => crate::kprint!("{}", i),
        RefKind::Character(c) => write_char_literal(c),
        RefKind::Special(_) => crate::kprint!("<? #{:x}>", ref_value),
        RefKind::MagicPointer { table, index } => crate::kprint!("@{}.{}", table, index),
        RefKind::Pointer(addr) => write_pointee(addr, ref_value, depth),
    }
}

/// Object header: high 24 bits of word 0 = size (bytes incl. header
/// + class/map + body), low 8 bits = flags (`0x01` = slotted,
/// `0x02` = frame, `0x40` = base bit, GC bits in the high nibble).
/// Word 1 is the GC/refcount field (not consulted here). Class or
/// map Ref sits at word 2 (offset +8), body slots/data start at +12.
fn read_obj_header(addr: u32) -> Option<(u32 /*size*/, u8 /*flags*/, u32 /*class_or_map*/)> {
    let w0 = crate::guest_endian::guest_read_u32_va(addr)?;
    let class = crate::guest_endian::guest_read_u32_va(addr.wrapping_add(8))?;
    let size = w0 >> 8;
    let flags = (w0 & 0xFF) as u8;
    if size < 12 { return None; }
    Some((size, flags, class))
}

const KOBJ_SLOTTED: u8 = 0x01;
const KOBJ_FRAME: u8 = 0x02;
/// Forwarding-pointer flag in the header byte. The "object" is a
/// 12-byte stub: header + (unused) word + the forwarding Ref at
/// the slot normally used for class/map. Newton emits these when
/// it relocates an object during GC/compaction so existing Refs
/// to the old address keep resolving via one extra hop.
const KOBJ_FORWARDED: u8 = 0x20;
const MAX_FORWARD_HOPS: u32 = 8;

/// Render the pointee of a pointer Ref. Reads the object header
/// directly from guest memory (one word at a time) instead of
/// buffering the whole body, so arbitrarily-sized objects (fault
/// blocks, big slot arrays) work without an upper-bound buffer.
fn write_pointee(addr: u32, ref_value: u32, depth: u32) {
    if depth == 0 {
        crate::kprint!("#{:x}", ref_value);
        return;
    }
    // Resolve forwarding chain. Each hop emits a `→` and follows
    // the forwarding Ref (stored at the class-or-map slot of the
    // 12-byte forwarding stub). Forwarding is transparent — depth
    // is not decremented — so the user sees the actual object as
    // if directly referenced. Bounded to MAX_FORWARD_HOPS to
    // protect against a self-referential forwarding bug.
    let mut cur_addr = addr;
    let mut cur_ref = ref_value;
    let (size, flags, class_or_map) = {
        let mut hops = 0u32;
        loop {
            let (s, f, c) = match read_obj_header(cur_addr) {
                Some(x) => x,
                None => { write_squirrely_at(cur_addr, cur_ref); return; }
            };
            if (f & KOBJ_FORWARDED) == 0 {
                break (s, f, c);
            }
            crate::kprint!("→");
            hops += 1;
            if hops > MAX_FORWARD_HOPS {
                crate::kprint!(" <fwd-loop?>");
                return;
            }
            let next_ref = c;
            let next = newton_objects::Ref(next_ref);
            match next.pointer_offset() {
                Some(a) => { cur_addr = a; cur_ref = next_ref; }
                None => {
                    // Forwarded to an immediate Ref — render it
                    // and stop.
                    write_ref(next_ref, depth);
                    return;
                }
            }
        }
    };
    let addr = cur_addr;
    let ref_value = cur_ref;
    let body_bytes = size - 12;
    let slot_count = (body_bytes / 4) as usize;

    let slotted = (flags & KOBJ_SLOTTED) != 0;
    let is_frame = (flags & KOBJ_FRAME) != 0;

    if !slotted {
        write_binary_at(addr, ref_value, size, class_or_map);
        return;
    }

    let (open, close) = if is_frame { ('{', '}') } else { ('[', ']') };
    crate::kprint!("{}", open);
    const LIMIT: usize = 8;
    let take = slot_count.min(LIMIT);
    for i in 0..take {
        if i > 0 { crate::kprint!(", "); }
        if is_frame {
            let mut name_buf = [0u8; 64];
            let nn = resolve_slot_name_into(class_or_map, i, slot_count, &mut name_buf);
            if nn > 0 {
                let name = core::str::from_utf8(&name_buf[..nn]).unwrap_or("?");
                crate::kprint!("{}: ", name);
            } else {
                // Map walk failed (NIL / broken / unknown) — fall back
                // to a positional placeholder that's clearly not a
                // resolved name so the reader doesn't mistake it for
                // a real frame key.
                crate::kprint!("?{}: ", i);
            }
        }
        let slot_va = addr.wrapping_add(12).wrapping_add((i as u32) * 4);
        match crate::guest_endian::guest_read_u32_va(slot_va) {
            Some(s) => write_ref(s, depth - 1),
            None => crate::kprint!("<? #?>"),
        }
    }
    if slot_count > LIMIT { crate::kprint!(", ..."); }
    crate::kprint!("{}", close);
}

/// Binary body. Symbols (class == `kSymbolClass` = 0x55552) →
/// `'name`. Strings (class is a pointer to the symbol `'string`)
/// → `"text"`. Anything else → `<bin 'classname N bytes>` (or
/// `<bin class=#hex N bytes>` if the class symbol can't be
/// resolved). The class lookup is forwarding-aware.
fn write_binary_at(addr: u32, ref_value: u32, size: u32, class_ref: u32) {
    let _ = ref_value;
    if class_ref == newton_objects::SYMBOL_CLASS.raw() {
        write_symbol_name_at(addr, size, ref_value);
        return;
    }
    let mut name_buf = [0u8; 32];
    let nn = read_symbol_name_into(newton_objects::Ref(class_ref), &mut name_buf);
    let class_name = if nn > 0 {
        core::str::from_utf8(&name_buf[..nn]).unwrap_or("")
    } else {
        ""
    };
    if class_name == "string" {
        write_string_body_at(addr, size);
        return;
    }
    let body_bytes = size.saturating_sub(12);
    if class_name.is_empty() {
        crate::kprint!("<bin class=#{:x} {} bytes>", class_ref, body_bytes);
    } else {
        crate::kprint!("<bin '{} {} bytes>", class_name, body_bytes);
    }
}

/// Symbol body layout: 4-byte hash at +12, NUL-terminated UTF-8
/// name at +16. Read up to a small fixed cap (symbols are short).
fn write_symbol_name_at(addr: u32, size: u32, ref_value: u32) {
    const NAME_CAP: usize = 96;
    let name_bytes = (size.saturating_sub(16) as usize).min(NAME_CAP);
    // Round up to a 4-byte boundary: `guest_read_bytes_va` only
    // writes whole words, so an odd byte count is silently
    // truncated down to the previous multiple of 4 — chopping 1–3
    // characters off the end of any name whose length isn't already
    // word-aligned.
    let read_len = ((name_bytes + 3) & !3).min(NAME_CAP);
    let mut buf = [0u8; NAME_CAP];
    if crate::guest_endian::guest_read_bytes_va(addr.wrapping_add(16), &mut buf[..read_len]).is_none() {
        write_squirrely_at(addr, ref_value);
        return;
    }
    let end = buf[..name_bytes].iter().position(|&b| b == 0).unwrap_or(name_bytes);
    match core::str::from_utf8(&buf[..end]) {
        Ok(s) => crate::kprint!("'{}", s),
        Err(_) => write_squirrely_at(addr, ref_value),
    }
}

/// Read the first chunk of a `'string` body and emit it as
/// `"text"`, decoding UCS-2 BE word-by-word so we don't depend on
/// the full body fitting in a buffer. Caps at MAX_CHARS units.
fn write_string_body_at(addr: u32, size: u32) {
    crate::kprint!("\"");
    const MAX_CHARS: usize = 48;
    let body_chars = size.saturating_sub(12) / 2;
    let take = (body_chars as usize).min(MAX_CHARS);
    let base = addr.wrapping_add(12);
    let mut emitted = 0usize;
    for w in 0..((take + 1) / 2) {
        let word_va = base.wrapping_add((w as u32) * 4);
        let word = match crate::guest_endian::guest_read_u32_va(word_va) {
            Some(w) => w,
            None => break,
        };
        // BE byte order: hi-hi, hi-lo, lo-hi, lo-lo.
        let bytes = [(word >> 24) as u8, (word >> 16) as u8, (word >> 8) as u8, word as u8];
        for pair in bytes.chunks_exact(2) {
            if emitted == take { break; }
            let c = ((pair[0] as u16) << 8) | (pair[1] as u16);
            if c == 0 { emitted = take; break; }
            write_string_char(c);
            emitted += 1;
        }
    }
    if (body_chars as usize) > MAX_CHARS { crate::kprint!("..."); }
    crate::kprint!("\"");
}

/// Diagnostic emission when a pointer Ref's pointee can't be
/// recognized: prints `<? #ref [w0 w1 w2 w3 w4 w5 w6 w7]>` with
/// the first 8 words at `addr` so the caller can eyeball the raw
/// header / class / body and figure out why decoding bailed.
/// Words are shown in Newton-numerical (BE-equivalent) form, the
/// same form `read_obj_header` sees; unreadable slots show as
/// `--------`.
fn write_squirrely_at(addr: u32, ref_value: u32) {
    crate::kprint!("<? #{:x} [", ref_value);
    for i in 0..8u32 {
        if i > 0 { crate::kprint!(" "); }
        match crate::guest_endian::guest_read_u32_va(addr.wrapping_add(i * 4)) {
            Some(w) => crate::kprint!("{:08x}", w),
            None => crate::kprint!("--------"),
        }
    }
    crate::kprint!("]>");
}


fn write_string_char(c: u16) {
    let cu = c as u32;
    if c == b'\\' as u16 {
        crate::kprint!("\\\\");
    } else if c == b'"' as u16 {
        crate::kprint!("\\\"");
    } else if (0x20..0x7f).contains(&cu) {
        crate::kprint!("{}", c as u8 as char);
    } else {
        crate::kprint!("\\u{:04x}", c);
    }
}

fn write_char_literal(c: u16) {
    let cu = c as u32;
    if (0x20..0x7f).contains(&cu) {
        crate::kprint!("${}", c as u8 as char);
    } else {
        crate::kprint!("$\\u{:04x}", c);
    }
}

/// Follow forwarding pointers starting at `addr`, returning the
/// (final-address, size, flags, class_or_map) of the underlying
/// non-forwarded object, or `None` if the chain breaks (unreadable
/// header, hop limit exceeded, forwarded to a non-pointer Ref).
fn resolve_forwarding(addr: u32) -> Option<(u32, u32, u8, u32)> {
    let mut a = addr;
    for _ in 0..=MAX_FORWARD_HOPS {
        let (size, flags, c) = read_obj_header(a)?;
        if (flags & KOBJ_FORWARDED) == 0 {
            return Some((a, size, flags, c));
        }
        let next = newton_objects::Ref(c);
        a = next.pointer_offset()?;
    }
    None
}

/// Read a symbol's name bytes via direct guest reads (forwarding-
/// aware). Returns the number of bytes written into `out`, or 0 if
/// `r` isn't a pointer Ref, the chain isn't a binary with class
/// `SYMBOL_CLASS`, or the read fails.
fn read_symbol_name_into(r: newton_objects::Ref, out: &mut [u8]) -> usize {
    let addr = match r.pointer_offset() { Some(a) => a, None => return 0 };
    let (final_addr, size, flags, class) = match resolve_forwarding(addr) {
        Some(x) => x,
        None => return 0,
    };
    if (flags & KOBJ_SLOTTED) != 0 { return 0; }
    if class != newton_objects::SYMBOL_CLASS.raw() { return 0; }
    let name_bytes = (size.saturating_sub(16) as usize).min(out.len());
    // Round up to 4 — see `write_symbol_name_at` for why.
    let read_len = ((name_bytes + 3) & !3).min(out.len());
    if crate::guest_endian::guest_read_bytes_va(
        final_addr.wrapping_add(16), &mut out[..read_len]
    ).is_none() {
        return 0;
    }
    out[..name_bytes].iter().position(|&b| b == 0).unwrap_or(name_bytes)
}

/// Resolve the symbol name for frame slot `slot_idx` by walking
/// the map chain rooted at `map_ref_value`. Writes the symbol's
/// name bytes into `out`; returns 0 on any failure (NIL map,
/// parse error, slot out of range, non-symbol name slot, broken
/// forwarding chain).
///
/// Map convention: slot[0] is the supermap (NIL terminates),
/// slots[1..N] name the *last* N positional slots of the frame.
/// Maps are read via direct word loads with forwarding-pointer
/// resolution at each level, so a relocated map still resolves.
fn resolve_slot_name_into(
    map_ref_value: u32,
    slot_idx: usize,
    frame_len: usize,
    out: &mut [u8],
) -> usize {
    let mut current = newton_objects::Ref(map_ref_value);
    let mut frame_slots_remaining = frame_len;
    let slot_offset_from_top = slot_idx;
    for _ in 0..8 {
        if !current.is_pointer() { return 0; }
        let raw_addr = match current.pointer_offset() { Some(a) => a, None => return 0 };
        let (map_addr, map_size, map_flags, _) = match resolve_forwarding(raw_addr) {
            Some(x) => x,
            None => return 0,
        };
        if (map_flags & KOBJ_SLOTTED) == 0 { return 0; } // map must be an array
        let arr_len = (map_size.saturating_sub(12) / 4) as usize;
        let local_count = arr_len.saturating_sub(1);
        let super_count = frame_slots_remaining.saturating_sub(local_count);
        if slot_offset_from_top < super_count {
            // Descend into supermap (slot 0 of the map array).
            let supermap_va = map_addr.wrapping_add(12);
            let supermap_ref = match crate::guest_endian::guest_read_u32_va(supermap_va) {
                Some(s) => s,
                None => return 0,
            };
            current = newton_objects::Ref(supermap_ref);
            frame_slots_remaining = super_count;
            continue;
        }
        let local_idx = slot_offset_from_top - super_count;
        if local_idx >= local_count { return 0; }
        let name_va = map_addr.wrapping_add(12 + ((1 + local_idx) as u32) * 4);
        let name_ref_value = match crate::guest_endian::guest_read_u32_va(name_va) {
            Some(r) => r,
            None => return 0,
        };
        return read_symbol_name_into(newton_objects::Ref(name_ref_value), out);
    }
    0
}
