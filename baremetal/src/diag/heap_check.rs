//! Classify a Newton NS Ref against the runtime object-heap bounds and
//! pretty-print it for the `log_store` / `ns_trace` probes.
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
//! `heap_bounds()` reliably returns `None` early (before
//! `InitObjects__Fv` has run) so callers can fall back to a
//! tag-only classification.

use core::sync::atomic::{AtomicU32, Ordering};

/// Address of the global `TObjectHeap*` written by `InitObjects__Fv`.
const G_OBJECT_HEAP: u32 = 0x0c10_5548;

/// Cached `(lo, hi)` from the last successful `heap_bounds()` read.
/// Never goes stale once populated — the heap's outer extent is
/// fixed at boot. Sentinel `(0, 0)` means "not read yet".
static CACHED_LO: AtomicU32 = AtomicU32::new(0);
static CACHED_HI: AtomicU32 = AtomicU32::new(0);

fn read_word(va: u32) -> Option<u32> {
    crate::hv::guest_endian::guest_read_u32_va(va)
        .or_else(|| crate::hv::guest_endian::guest_read_u32_pa(va))
}

/// Read the runtime object heap's `[lo, hi)` bounds. Returns
/// `None` before `InitObjects__Fv` has run (the global is still
/// zero) or if any of the dependent reads fail.
///
/// The result is cached permanently once read, so the dependent
/// reads go through the stage-1 VA walk only — never the PA
/// fallback `read_word` uses. A PA-fallback read of a kernel-VA
/// global like `G_OBJECT_HEAP` lands on unrelated physical memory
/// and would poison the permanent cache with garbage bounds.
pub fn heap_bounds() -> Option<(u32, u32)> {
    use crate::hv::guest_endian::guest_read_u32_va;
    let cached_lo = CACHED_LO.load(Ordering::Relaxed);
    let cached_hi = CACHED_HI.load(Ordering::Relaxed);
    if cached_lo != 0 || cached_hi != 0 {
        return Some((cached_lo, cached_hi));
    }
    let heap_ptr = guest_read_u32_va(G_OBJECT_HEAP)?;
    if heap_ptr == 0 {
        return None;
    }
    let lo = guest_read_u32_va(heap_ptr.wrapping_add(8))?;
    let hi = guest_read_u32_va(heap_ptr.wrapping_add(12))?;
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

/// One-shot summary of the heap bounds. Logs nothing if the heap
/// isn't constructed yet. Under `ns_trace`, also flips the
/// TInterpreter trace gate via `force_interpreter_trace_on`.
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

// `gInterpreter[124]` — the byte at offset `+0x7C` in the
// `TInterpreter` singleton (`gInterpreter` is reachable as
// `*0x0c105458`, cf. DoSend's literal at `0x2f06a4`). When non-zero,
// every `DoSend / DoMessage / DoFastApply` calls `TraceSend /
// TraceCall / TraceApply`, which funnel into `TraceMethod →
// Print(POutTranslator*, fmt, ...)`. With the `ns_trace` Print thunk
// hook in place that surfaces every NS-level call into the EL2 UART.
#[cfg(feature = "ns_trace")]
const G_INTERPRETER_PTR: u32 = 0x0c10_5458;
#[cfg(feature = "ns_trace")]
const TINTERPRETER_TRACE_OFF: u32 = 124;

#[cfg(feature = "ns_trace")]
fn write_word(va: u32, value: u32) -> bool {
    if crate::hv::guest_endian::guest_write_u32_va(va, value) {
        return true;
    }
    crate::hv::guest_endian::guest_write_u32_pa(va, value)
}

/// Poke `gInterpreter[+124] = 1` — the TInterpreter trace gate.
///
/// Deliberately does NOT touch `gWantSerialDebugging`: setting that
/// triggers `WriteDebugByte` calls from the kernel's FPE handler
/// running in UND mode, where the debug ring-buffer pointer at
/// obj[28] is NULL → strb to address 0 → unknown-MMIO halt at
/// PC=0x199ce8.
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

#[cfg(feature = "log_store")]
/// Pretty-print a NewtonScript Ref inline — no label, no trailing
/// newline — with `depth` levels of structural expansion (default
/// 0 — pointers render as `#hex`). Use to compose probe headers
/// like `kprint!("StorePermObject[{}]: ", n);
/// pretty_print_ref_inline(r, 1); kprintln!(" lr={:#x}", lr);`.
pub fn pretty_print_ref_inline(ref_value: u32, depth: u32) {
    write_ref(ref_value, depth);
}

#[cfg(feature = "log_store")]
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

#[cfg(feature = "log_store")]
/// Object header: high 24 bits of word 0 = size (bytes incl. header
/// + class/map + body), low 8 bits = flags (`0x01` = slotted,
/// `0x02` = frame, `0x40` = base bit, GC bits in the high nibble).
/// Word 1 is the GC/refcount field (not consulted here). Class or
/// map Ref sits at word 2 (offset +8), body slots/data start at +12.
fn read_obj_header(addr: u32) -> Option<(u32 /*size*/, u8 /*flags*/, u32 /*class_or_map*/)> {
    let w0 = crate::hv::guest_endian::guest_read_u32_va(addr)?;
    let class = crate::hv::guest_endian::guest_read_u32_va(addr.wrapping_add(8))?;
    let size = w0 >> 8;
    let flags = (w0 & 0xFF) as u8;
    if size < 12 { return None; }
    Some((size, flags, class))
}

#[cfg(feature = "log_store")]
const KOBJ_SLOTTED: u8 = 0x01;
#[cfg(feature = "log_store")]
const KOBJ_FRAME: u8 = 0x02;
#[cfg(feature = "log_store")]
/// Forwarding-pointer flag in the header byte. The "object" is a
/// 12-byte stub: header + (unused) word + the forwarding Ref at
/// the slot normally used for class/map. Newton emits these when
/// it relocates an object during GC/compaction so existing Refs
/// to the old address keep resolving via one extra hop.
const KOBJ_FORWARDED: u8 = 0x20;
#[cfg(feature = "log_store")]
const MAX_FORWARD_HOPS: u32 = 8;

#[cfg(feature = "log_store")]
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
        match crate::hv::guest_endian::guest_read_u32_va(slot_va) {
            Some(s) => write_ref(s, depth - 1),
            None => crate::kprint!("<? #?>"),
        }
    }
    if slot_count > LIMIT { crate::kprint!(", ..."); }
    crate::kprint!("{}", close);
}

#[cfg(feature = "log_store")]
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

#[cfg(feature = "log_store")]
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
    if crate::hv::guest_endian::guest_read_bytes_va(addr.wrapping_add(16), &mut buf[..read_len]).is_none() {
        write_squirrely_at(addr, ref_value);
        return;
    }
    let end = buf[..name_bytes].iter().position(|&b| b == 0).unwrap_or(name_bytes);
    match core::str::from_utf8(&buf[..end]) {
        Ok(s) => crate::kprint!("'{}", s),
        Err(_) => write_squirrely_at(addr, ref_value),
    }
}

#[cfg(feature = "log_store")]
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
        let word = match crate::hv::guest_endian::guest_read_u32_va(word_va) {
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

#[cfg(feature = "log_store")]
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
        match crate::hv::guest_endian::guest_read_u32_va(addr.wrapping_add(i * 4)) {
            Some(w) => crate::kprint!("{:08x}", w),
            None => crate::kprint!("--------"),
        }
    }
    crate::kprint!("]>");
}


#[cfg(feature = "log_store")]
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

#[cfg(feature = "log_store")]
fn write_char_literal(c: u16) {
    let cu = c as u32;
    if (0x20..0x7f).contains(&cu) {
        crate::kprint!("${}", c as u8 as char);
    } else {
        crate::kprint!("$\\u{:04x}", c);
    }
}

#[cfg(feature = "log_store")]
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

#[cfg(feature = "log_store")]
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
    if crate::hv::guest_endian::guest_read_bytes_va(
        final_addr.wrapping_add(16), &mut out[..read_len]
    ).is_none() {
        return 0;
    }
    out[..name_bytes].iter().position(|&b| b == 0).unwrap_or(name_bytes)
}

#[cfg(feature = "log_store")]
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
            let supermap_ref = match crate::hv::guest_endian::guest_read_u32_va(supermap_va) {
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
        let name_ref_value = match crate::hv::guest_endian::guest_read_u32_va(name_va) {
            Some(r) => r,
            None => return 0,
        };
        return read_symbol_name_into(newton_objects::Ref(name_ref_value), out);
    }
    0
}
