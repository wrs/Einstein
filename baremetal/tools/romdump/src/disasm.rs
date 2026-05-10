//! NewtonScript bytecode disassembler.
//!
//! Decodes the byte stream of an `'instructions` binary per *Newton
//! Formats* §2 (NewtonScript Bytecode Interpreter Specification) and
//! formats it as text, optionally resolving literal/locals references
//! against the parent CodeBlock frame's `literals` array and
//! `argFrame` frame.

use std::fmt::Write as _;

use newton_objects::{Array, Binary, Frame, Heap, Object, Ref};

use crate::short_ref;

// ----------------------------------------------------------------- Decoder

/// A decoded NewtonScript instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instr {
    Simple(SimpleOp),
    Push { lit_idx: u16 },
    PushConstant { value: Ref },
    Call { nargs: u16 },
    Invoke { nargs: u16 },
    Send { nargs: u16 },
    SendIfDefined { nargs: u16 },
    Resend { nargs: u16 },
    ResendIfDefined { nargs: u16 },
    Branch { target: u16 },
    BranchIfTrue { target: u16 },
    BranchIfFalse { target: u16 },
    FindVar { lit_idx: u16 },
    GetVar { local_idx: u16 },
    MakeFrame { nslots: u16 },
    /// `B == 0xFFFF` is the pop-size form (size popped from stack);
    /// otherwise the value is the literal slot count.
    MakeArray { nslots_or_ffff: u16 },
    GetPath { check: bool },
    SetPath { push: bool },
    SetVar { local_idx: u16 },
    FindAndSetVar { lit_idx: u16 },
    IncrVar { local_idx: u16 },
    BranchIfLoopNotDone { target: u16 },
    FreqFunc { prim_idx: u16 },
    NewHandlers { npairs: u16 },
    Unknown { a: u8, b: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimpleOp {
    Pop,
    Dup,
    Return,
    PushSelf,
    SetLexScope,
    IterNext,
    IterDone,
    /// Encoded as `00 00 01` (the 3-byte form of A=0/B=7 with B=1).
    PopHandlers,
    Unknown(u16),
}

/// One decoded instruction together with its raw bytes and start PC.
#[derive(Clone, Copy, Debug)]
pub struct Decoded<'a> {
    pub pc: u32,
    pub raw: &'a [u8],
    pub instr: Instr,
}

/// Iterator that walks a bytecode stream from PC 0 to the end of the
/// buffer. Truncated 3-byte instructions stop iteration.
pub struct Decoder<'a> {
    bytes: &'a [u8],
    pc: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pc: 0 }
    }
}

impl<'a> Iterator for Decoder<'a> {
    type Item = Decoded<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pc >= self.bytes.len() {
            return None;
        }
        let start = self.pc;
        let byte0 = self.bytes[start];
        let a = byte0 >> 3;
        let mut b = (byte0 & 0b111) as u16;
        let three_byte = b == 7;
        let raw_end = if three_byte {
            if start + 3 > self.bytes.len() {
                // Truncated; emit nothing further.
                return None;
            }
            b = ((self.bytes[start + 1] as u16) << 8) | (self.bytes[start + 2] as u16);
            start + 3
        } else {
            start + 1
        };

        let instr = decode(a, b, three_byte);
        self.pc = raw_end;
        Some(Decoded {
            pc: start as u32,
            raw: &self.bytes[start..raw_end],
            instr,
        })
    }
}

fn decode(a: u8, b: u16, three_byte: bool) -> Instr {
    match a {
        0 => {
            // Simple instructions: A=0, B selects op. Spec uses the
            // 3-byte form only for pop-handlers (B=1 in extended B).
            if three_byte {
                if b == 1 {
                    Instr::Simple(SimpleOp::PopHandlers)
                } else {
                    Instr::Simple(SimpleOp::Unknown(b))
                }
            } else {
                let op = match b {
                    0 => SimpleOp::Pop,
                    1 => SimpleOp::Dup,
                    2 => SimpleOp::Return,
                    3 => SimpleOp::PushSelf,
                    4 => SimpleOp::SetLexScope,
                    5 => SimpleOp::IterNext,
                    6 => SimpleOp::IterDone,
                    other => SimpleOp::Unknown(other),
                };
                Instr::Simple(op)
            }
        }
        3 => Instr::Push { lit_idx: b },
        4 => {
            // push-constant: B is signed; the spec restricts valid B to
            // an immediate Ref (low bits 00 or 10) or a magic pointer
            // index 0..4095. We pass the raw bits through as a Ref.
            let value = Ref(sign_extend_b(b, three_byte));
            Instr::PushConstant { value }
        }
        5 => Instr::Call { nargs: b },
        6 => Instr::Invoke { nargs: b },
        7 => Instr::Send { nargs: b },
        8 => Instr::SendIfDefined { nargs: b },
        9 => Instr::Resend { nargs: b },
        10 => Instr::ResendIfDefined { nargs: b },
        11 => Instr::Branch { target: b },
        12 => Instr::BranchIfTrue { target: b },
        13 => Instr::BranchIfFalse { target: b },
        14 => Instr::FindVar { lit_idx: b },
        15 => Instr::GetVar { local_idx: b },
        16 => Instr::MakeFrame { nslots: b },
        17 => Instr::MakeArray { nslots_or_ffff: b },
        18 => Instr::GetPath { check: b == 1 },
        19 => Instr::SetPath { push: b == 1 },
        20 => Instr::SetVar { local_idx: b },
        21 => Instr::FindAndSetVar { lit_idx: b },
        22 => Instr::IncrVar { local_idx: b },
        23 => Instr::BranchIfLoopNotDone { target: b },
        24 => Instr::FreqFunc { prim_idx: b },
        25 => Instr::NewHandlers { npairs: b },
        _ => Instr::Unknown { a, b },
    }
}

/// Sign-extend the B field for `push-constant`. In the 1-byte form
/// only the low 3 bits are present (and the high bits are zero — the
/// spec gives no negative 1-byte push-constants); in the 3-byte form
/// the 16-bit B is interpreted as signed (i.e. 0xFFFF means -1, which
/// in Ref terms is the integer Ref `Ref(0xFFFF_FFFF)`).
fn sign_extend_b(b: u16, three_byte: bool) -> u32 {
    if three_byte {
        b as i16 as i32 as u32
    } else {
        b as u32
    }
}

// ------------------------------------------------------ Primitive table

/// Spec name for a `freq-func` primitive index, per *Newton Formats*
/// Table 1-3. Returns `None` for indexes the spec does not define.
pub fn freq_func_name(idx: u16) -> Option<&'static str> {
    Some(match idx {
        0 => "add",
        1 => "subtract",
        2 => "aref",
        3 => "set-aref",
        4 => "equals",
        5 => "not",
        6 => "not-equals",
        7 => "multiply",
        8 => "divide",
        9 => "div",
        10 => "less-than",
        11 => "greater-than",
        12 => "greater-or-equal",
        13 => "less-or-equal",
        14 => "bit-and",
        15 => "bit-or",
        16 => "bit-not",
        17 => "new-iterator",
        18 => "length",
        19 => "clone",
        20 => "set-class",
        21 => "add-array-slot",
        22 => "stringer",
        23 => "has-path",
        24 => "class-of",
        _ => return None,
    })
}

// --------------------------------------------------------------- Formatter

/// Resolved context from the parent CodeBlock frame.
pub struct CodeBlockCtx<'a> {
    pub literals: Option<Array<'a>>,
    pub arg_frame: Option<Frame<'a>>,
}

impl<'a> CodeBlockCtx<'a> {
    /// Build a `CodeBlockCtx` from a frame that has already been
    /// confirmed to be a CodeBlock. Returns slots resolved to their
    /// underlying Array/Frame objects (or `None` for nil/missing).
    pub fn from_codeblock(heap: &Heap<'a>, f: &Frame<'a>) -> Self {
        let literals = f
            .lookup("literals")
            .and_then(|r| heap.deref(r).ok())
            .and_then(|o| o.as_array().ok());
        let arg_frame = f
            .lookup("argFrame")
            .and_then(|r| heap.deref(r).ok())
            .and_then(|o| o.as_frame().ok());
        Self { literals, arg_frame }
    }
}

/// Render the bytecodes of `bin` as a multi-line listing into `out`.
///
/// Output starts with a header comment line, then one line per
/// instruction indented by `indent` spaces. The last line has no
/// trailing newline; the caller appends slot punctuation.
pub fn format_disasm(
    out: &mut String,
    bin: Binary<'_>,
    ctx: Option<&CodeBlockCtx<'_>>,
    heap: &Heap<'_>,
    indent: usize,
) {
    let data = bin.data();
    write!(
        out,
        "// bytecodes ({} bytes) at 0x{:08x}",
        data.len(),
        bin.offset()
    )
    .unwrap();

    for d in Decoder::new(data) {
        out.push('\n');
        for _ in 0..indent {
            out.push(' ');
        }
        write!(out, "{:>4}  ", d.pc).unwrap();
        write_raw_octal(out, d.raw);
        out.push_str("  ");
        write_instr(out, d.instr, ctx, heap);
    }
}

/// Format the raw instruction bytes as space-separated 3-digit octal,
/// padded to the width of a 3-byte instruction (`NNN NNN NNN`) so that
/// 1-byte and 3-byte lines have aligned mnemonic columns.
fn write_raw_octal(out: &mut String, raw: &[u8]) {
    for (i, byte) in raw.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        write!(out, "{:03o}", byte).unwrap();
    }
    // Pad short lines so 3-byte and 1-byte instructions align.
    let pad = 3usize.saturating_sub(raw.len());
    for _ in 0..pad {
        out.push_str("    "); // 3 spaces for "NNN" + 1 separator
    }
}

fn write_instr(
    out: &mut String,
    instr: Instr,
    ctx: Option<&CodeBlockCtx<'_>>,
    heap: &Heap<'_>,
) {
    match instr {
        Instr::Simple(op) => write_simple(out, op),
        Instr::Push { lit_idx } => {
            write!(out, "push {}", lit_idx).unwrap();
            append_literal_comment(out, ctx, heap, lit_idx);
        }
        Instr::PushConstant { value } => {
            write!(out, "push-constant {}", short_ref(heap, value)).unwrap();
        }
        Instr::Call { nargs } => write!(out, "call {}", nargs).unwrap(),
        Instr::Invoke { nargs } => write!(out, "invoke {}", nargs).unwrap(),
        Instr::Send { nargs } => write!(out, "send {}", nargs).unwrap(),
        Instr::SendIfDefined { nargs } => write!(out, "send-if-defined {}", nargs).unwrap(),
        Instr::Resend { nargs } => write!(out, "resend {}", nargs).unwrap(),
        Instr::ResendIfDefined { nargs } => write!(out, "resend-if-defined {}", nargs).unwrap(),
        Instr::Branch { target } => write!(out, "branch {}", target).unwrap(),
        Instr::BranchIfTrue { target } => write!(out, "branch-if-true {}", target).unwrap(),
        Instr::BranchIfFalse { target } => write!(out, "branch-if-false {}", target).unwrap(),
        Instr::FindVar { lit_idx } => {
            write!(out, "find-var {}", lit_idx).unwrap();
            append_literal_comment(out, ctx, heap, lit_idx);
        }
        Instr::GetVar { local_idx } => {
            write!(out, "get-var {}", local_idx).unwrap();
            append_local_comment(out, ctx, local_idx);
        }
        Instr::MakeFrame { nslots } => write!(out, "make-frame {}", nslots).unwrap(),
        Instr::MakeArray { nslots_or_ffff } => {
            if nslots_or_ffff == 0xFFFF {
                out.push_str("make-array (size from stack)");
            } else {
                write!(out, "make-array {}", nslots_or_ffff).unwrap();
            }
        }
        Instr::GetPath { check } => {
            out.push_str(if check { "get-path 1" } else { "get-path 0" });
            if check {
                out.push_str("  ; throw on nil");
            }
        }
        Instr::SetPath { push } => {
            out.push_str(if push { "set-path 1" } else { "set-path 0" });
            if push {
                out.push_str("  ; leave value on stack");
            }
        }
        Instr::SetVar { local_idx } => {
            write!(out, "set-var {}", local_idx).unwrap();
            append_local_comment(out, ctx, local_idx);
        }
        Instr::FindAndSetVar { lit_idx } => {
            write!(out, "find-and-set-var {}", lit_idx).unwrap();
            append_literal_comment(out, ctx, heap, lit_idx);
        }
        Instr::IncrVar { local_idx } => {
            write!(out, "incr-var {}", local_idx).unwrap();
            append_local_comment(out, ctx, local_idx);
        }
        Instr::BranchIfLoopNotDone { target } => {
            write!(out, "branch-if-loop-not-done {}", target).unwrap();
        }
        Instr::FreqFunc { prim_idx } => {
            write!(out, "freq-func {}", prim_idx).unwrap();
            if let Some(name) = freq_func_name(prim_idx) {
                write!(out, "  ; {}", name).unwrap();
            }
        }
        Instr::NewHandlers { npairs } => write!(out, "new-handlers {}", npairs).unwrap(),
        Instr::Unknown { a, b } => write!(out, "<unknown a={} b={}>", a, b).unwrap(),
    }
}

fn write_simple(out: &mut String, op: SimpleOp) {
    let name = match op {
        SimpleOp::Pop => "pop",
        SimpleOp::Dup => "dup",
        SimpleOp::Return => "return",
        SimpleOp::PushSelf => "push-self",
        SimpleOp::SetLexScope => "set-lex-scope",
        SimpleOp::IterNext => "iter-next",
        SimpleOp::IterDone => "iter-done",
        SimpleOp::PopHandlers => "pop-handlers",
        SimpleOp::Unknown(b) => {
            write!(out, "<simple b={}>", b).unwrap();
            return;
        }
    };
    out.push_str(name);
}

fn append_literal_comment(
    out: &mut String,
    ctx: Option<&CodeBlockCtx<'_>>,
    heap: &Heap<'_>,
    idx: u16,
) {
    let lits = match ctx.and_then(|c| c.literals) {
        Some(a) => a,
        None => return,
    };
    let slot = match lits.slot(idx as usize) {
        Some(r) => r,
        None => return,
    };
    write!(out, "  ; literals[{}] = {}", idx, short_ref(heap, slot)).unwrap();
}

fn append_local_comment(out: &mut String, ctx: Option<&CodeBlockCtx<'_>>, idx: u16) {
    let af = match ctx.and_then(|c| c.arg_frame.as_ref()) {
        Some(f) => f,
        None => return,
    };
    if let Some(sym) = af.name(idx as usize) {
        if let Ok(name) = sym.name() {
            write!(out, "  ; arg_frame[{}] = {}", idx, name).unwrap();
            return;
        }
    }
    write!(out, "  ; local{}", idx).unwrap();
}

// True iff the slot Ref dereferences to a binary of class `'instructions`.
pub fn slot_is_instructions_binary<'a>(heap: &Heap<'a>, slot: Ref) -> Option<Binary<'a>> {
    let off = slot.pointer_offset()?;
    let obj = heap.object_at(off).ok()?;
    let bin = obj.as_binary().ok()?;
    let class_off = bin.class().pointer_offset()?;
    let class_obj = heap.object_at(class_off).ok()?;
    let class_bin = class_obj.as_binary().ok()?;
    let sym = class_bin.as_symbol()?;
    if sym.name().ok()?.eq_ignore_ascii_case("instructions") {
        Some(bin)
    } else {
        None
    }
}

/// Newton's `kPlainFuncClass` — the special-immediate the runtime
/// stores in the `class` slot of a real CodeBlock function frame
/// instead of the `'CodeBlock` symbol. Encoded as `RefKind::Special(0xC)`
/// (raw bits `0x0000_0032`). Templates use the symbol form; production
/// instances use this immediate.
pub const PLAIN_FUNC_CLASS: Ref = Ref(0x0000_0032);

/// Returns `true` if `f`'s `class` slot identifies it as a CodeBlock.
///
/// Two encodings qualify:
///
/// 1. **Symbol form.** Class slot is a pointer Ref to symbol
///    `'CodeBlock`. Used by the empty templates the ROM stores.
/// 2. **Special-immediate form.** Class slot is `Ref(0x32)` =
///    `RefKind::Special(0xC)`. Used by every production function
///    frame in the ROM; the runtime carries the class as a tag rather
///    than a per-instance symbol pointer.
///
/// Both rely on `Frame::lookup`, which walks the supermap — so frames
/// that share a CodeBlock map are detected just like per-instance
/// ones.
pub fn frame_is_codeblock(heap: &Heap<'_>, f: &Frame<'_>) -> bool {
    let class = match f.lookup("class") {
        Some(r) => r,
        None => return false,
    };
    if class == PLAIN_FUNC_CLASS {
        return true;
    }
    let off = match class.pointer_offset() {
        Some(o) => o,
        None => return false,
    };
    let obj = match heap.object_at(off) {
        Ok(o) => o,
        Err(_) => return false,
    };
    let Object::Binary(b) = obj else { return false };
    match b.as_symbol().and_then(|s| s.name().ok()) {
        Some(name) => name.eq_ignore_ascii_case("codeblock"),
        None => false,
    }
}

// ------------------------------------------------------------------- Tests

#[cfg(test)]
mod tests {
    use super::*;
    use newton_objects::{flags, RefKind, SYMBOL_CLASS};

    /// Minimal local copy of the heap-builder pattern from
    /// `newton-objects/src/tests.rs`. Only the operations we need
    /// for CodeBlock fixtures are reproduced.
    struct Builder {
        bytes: Vec<u8>,
    }

    impl Builder {
        fn new() -> Self {
            Self { bytes: Vec::new() }
        }
        fn align4(&mut self) {
            while !self.bytes.len().is_multiple_of(4) {
                self.bytes.push(0);
            }
        }
        fn ptr(off: u32) -> Ref {
            assert!(off & 0b11 == 0);
            Ref(off | 0b01)
        }
        fn header(&mut self, size: u32, flags: u8) {
            let w0 = (size << 8) | flags as u32;
            self.bytes.extend_from_slice(&w0.to_be_bytes());
            self.bytes.extend_from_slice(&[0u8; 4]);
        }
        fn binary(&mut self, class: Ref, data: &[u8]) -> u32 {
            self.align4();
            let off = self.bytes.len() as u32;
            let size = 8 + 4 + data.len() as u32;
            self.header(size, flags::HEADER_BASE);
            self.bytes.extend_from_slice(&class.0.to_be_bytes());
            self.bytes.extend_from_slice(data);
            self.align4();
            off
        }
        fn symbol(&mut self, name: &str) -> u32 {
            use newton_objects::Symbol;
            let mut data = Vec::new();
            let hash = Symbol::hash_of(name.as_bytes());
            data.extend_from_slice(&hash.to_be_bytes());
            data.extend_from_slice(name.as_bytes());
            data.push(0);
            self.binary(SYMBOL_CLASS, &data)
        }
        fn array(&mut self, class: Ref, slots: &[Ref]) -> u32 {
            self.align4();
            let off = self.bytes.len() as u32;
            let size = 8 + 4 + (slots.len() as u32) * 4;
            self.header(size, flags::HEADER_BASE | flags::KOBJ_SLOTTED);
            self.bytes.extend_from_slice(&class.0.to_be_bytes());
            for s in slots {
                self.bytes.extend_from_slice(&s.0.to_be_bytes());
            }
            off
        }
        fn frame(&mut self, map: Ref, slots: &[Ref]) -> u32 {
            self.align4();
            let off = self.bytes.len() as u32;
            let size = 8 + 4 + (slots.len() as u32) * 4;
            let f = flags::HEADER_BASE | flags::KOBJ_SLOTTED | flags::KOBJ_FRAME;
            self.header(size, f);
            self.bytes.extend_from_slice(&map.0.to_be_bytes());
            for s in slots {
                self.bytes.extend_from_slice(&s.0.to_be_bytes());
            }
            off
        }
    }

    /// Build a heap holding only an `'instructions` binary with the
    /// given byte stream. Returns the binary and a heap that owns it.
    fn instr_only_heap(bytecodes: &[u8]) -> (Vec<u8>, u32) {
        let mut b = Builder::new();
        let cls = b.symbol("instructions");
        let off = b.binary(Builder::ptr(cls), bytecodes);
        (b.bytes, off)
    }

    fn lines(out: &str) -> Vec<&str> {
        out.lines().collect()
    }

    // ------ Decoder unit tests --------------------------------------

    #[test]
    fn simple_ops_decode() {
        let bytes = [0o00, 0o01, 0o02, 0o03, 0o04, 0o05, 0o06];
        let decoded: Vec<_> = Decoder::new(&bytes).map(|d| d.instr).collect();
        assert_eq!(
            decoded,
            vec![
                Instr::Simple(SimpleOp::Pop),
                Instr::Simple(SimpleOp::Dup),
                Instr::Simple(SimpleOp::Return),
                Instr::Simple(SimpleOp::PushSelf),
                Instr::Simple(SimpleOp::SetLexScope),
                Instr::Simple(SimpleOp::IterNext),
                Instr::Simple(SimpleOp::IterDone),
            ]
        );
    }

    #[test]
    fn pop_handlers_three_byte() {
        let bytes = [0o07, 0x00, 0x01];
        let decoded: Vec<_> = Decoder::new(&bytes).collect();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].instr, Instr::Simple(SimpleOp::PopHandlers));
        assert_eq!(decoded[0].raw, &bytes);
    }

    #[test]
    fn one_byte_parameterized() {
        // A=3 (push), B=2  →  byte = (3 << 3) | 2 = 0x1A = octal 032.
        let bytes = [0x1A];
        let decoded: Vec<_> = Decoder::new(&bytes).map(|d| d.instr).collect();
        assert_eq!(decoded, vec![Instr::Push { lit_idx: 2 }]);
    }

    #[test]
    fn three_byte_parameterized() {
        // A=3 (push), B=10 via 3-byte form.
        let bytes = [0x1F, 0x00, 0x0A];
        let decoded: Vec<_> = Decoder::new(&bytes).collect();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].instr, Instr::Push { lit_idx: 10 });
        assert_eq!(decoded[0].raw, &bytes);
    }

    #[test]
    fn push_constant_signed_b() {
        // A=4 (push-constant), B=0xFFFC in 3-byte form → sign-extended
        // to Ref(0xFFFFFFFC), which decodes as integer -1 (low 2 bits
        // = 00 = integer tag, top 30 bits = -1).
        let bytes = [0x27, 0xFF, 0xFC];
        let decoded: Vec<_> = Decoder::new(&bytes).map(|d| d.instr).collect();
        let expected_ref = Ref(0xFFFFFFFCu32);
        assert_eq!(decoded, vec![Instr::PushConstant { value: expected_ref }]);
        assert_eq!(expected_ref.kind(), RefKind::Integer(-1));
    }

    #[test]
    fn freq_func_known_and_unknown() {
        // A=24 → byte = 24<<3 = 0xC0 = octal 0300.
        // B=18 (length) one-byte requires B<=6, so use 3-byte form.
        let bytes = [0xC7, 0x00, 0x12, 0xC7, 0x00, 0x63];
        let decoded: Vec<_> = Decoder::new(&bytes).map(|d| d.instr).collect();
        assert_eq!(
            decoded,
            vec![
                Instr::FreqFunc { prim_idx: 18 },
                Instr::FreqFunc { prim_idx: 99 },
            ]
        );
        assert_eq!(freq_func_name(18), Some("length"));
        assert_eq!(freq_func_name(99), None);
    }

    #[test]
    fn make_array_pop_size_form() {
        // A=17, B=0xFFFF.
        let bytes = [0x8F, 0xFF, 0xFF];
        let decoded: Vec<_> = Decoder::new(&bytes).map(|d| d.instr).collect();
        assert_eq!(
            decoded,
            vec![Instr::MakeArray { nslots_or_ffff: 0xFFFF }]
        );
    }

    #[test]
    fn truncated_three_byte_stops_iteration() {
        // First byte starts a 3-byte instruction but only one byte is
        // present after it.
        let bytes = [0x1F, 0x00];
        let decoded: Vec<_> = Decoder::new(&bytes).collect();
        assert!(decoded.is_empty());
    }

    // ------ Formatter tests -----------------------------------------

    #[test]
    fn formats_simple_ops_with_pc_and_octal() {
        let (heap_bytes, bin_off) = instr_only_heap(&[0o00, 0o02]);
        let heap = Heap::new(&heap_bytes);
        let bin = heap.object_at(bin_off).unwrap().as_binary().unwrap();

        let mut out = String::new();
        format_disasm(&mut out, bin, None, &heap, 0);

        let ls = lines(&out);
        assert_eq!(ls.len(), 3, "header + 2 instructions, got: {out}");
        assert!(ls[0].starts_with("// bytecodes (2 bytes) at 0x"));
        assert!(ls[1].contains("000"), "{}", ls[1]);
        assert!(ls[1].contains("pop"), "{}", ls[1]);
        assert!(ls[2].contains("002"), "{}", ls[2]);
        assert!(ls[2].contains("return"), "{}", ls[2]);
        // PC column.
        assert!(ls[1].trim_start().starts_with("0  "));
        assert!(ls[2].trim_start().starts_with("1  "));
    }

    #[test]
    fn end_to_end_with_literals_resolves_inline() {
        // Build literals = ['foo, 'bar], instructions = [push 0, push 1, return],
        // wrapped in a CodeBlock frame.
        let mut b = Builder::new();
        let foo = b.symbol("foo");
        let bar = b.symbol("bar");
        let lits_class = b.symbol("literals");
        let lits = b.array(
            Builder::ptr(lits_class),
            &[Builder::ptr(foo), Builder::ptr(bar)],
        );
        let instr_class = b.symbol("instructions");
        // push 0 = 0x18, push 1 = 0x19, return = 0x02.
        let bin = b.binary(Builder::ptr(instr_class), &[0x18, 0x19, 0x02]);
        let codeblock_sym = b.symbol("CodeBlock");
        let class_sym = b.symbol("class");
        let instr_sym = b.symbol("instructions");
        let lits_sym = b.symbol("literals");
        let map = b.array(
            Ref(0),
            &[
                Ref::NIL,
                Builder::ptr(class_sym),
                Builder::ptr(instr_sym),
                Builder::ptr(lits_sym),
            ],
        );
        let frame_off = b.frame(
            Builder::ptr(map),
            &[
                Builder::ptr(codeblock_sym),
                Builder::ptr(bin),
                Builder::ptr(lits),
            ],
        );

        let heap = Heap::new(&b.bytes);
        let frame = heap.object_at(frame_off).unwrap().as_frame().unwrap();
        assert!(frame_is_codeblock(&heap, &frame));
        let ctx = CodeBlockCtx::from_codeblock(&heap, &frame);
        assert!(ctx.literals.is_some());

        let bin = heap.object_at(bin).unwrap().as_binary().unwrap();
        let mut out = String::new();
        format_disasm(&mut out, bin, Some(&ctx), &heap, 0);

        assert!(out.contains("literals[0] = 'foo"), "{out}");
        assert!(out.contains("literals[1] = 'bar"), "{out}");
        assert!(out.contains("return"), "{out}");
    }

    #[test]
    fn end_to_end_with_arg_frame_resolves_locals() {
        // argFrame map: [supermap=NIL, _nextArgFrame, _parent, _implementor, a, b]
        // get-var 3 → a, set-var 4 → b, return.
        let mut b = Builder::new();
        let s_naf = b.symbol("_nextArgFrame");
        let s_par = b.symbol("_parent");
        let s_imp = b.symbol("_implementor");
        let s_a = b.symbol("a");
        let s_b = b.symbol("b");
        let af_map = b.array(
            Ref(0),
            &[
                Ref::NIL,
                Builder::ptr(s_naf),
                Builder::ptr(s_par),
                Builder::ptr(s_imp),
                Builder::ptr(s_a),
                Builder::ptr(s_b),
            ],
        );
        let arg_frame = b.frame(
            Builder::ptr(af_map),
            &[Ref::NIL, Ref::NIL, Ref::NIL, Ref::NIL, Ref::NIL],
        );

        let instr_class = b.symbol("instructions");
        // get-var 3 = (15<<3)|3 = 0x7B; set-var 4 = (20<<3)|4 = 0xA4; return = 0x02.
        let bin = b.binary(Builder::ptr(instr_class), &[0x7B, 0xA4, 0x02]);

        let codeblock_sym = b.symbol("CodeBlock");
        let class_sym = b.symbol("class");
        let instr_sym = b.symbol("instructions");
        let af_sym = b.symbol("argFrame");
        let map = b.array(
            Ref(0),
            &[
                Ref::NIL,
                Builder::ptr(class_sym),
                Builder::ptr(instr_sym),
                Builder::ptr(af_sym),
            ],
        );
        let frame_off = b.frame(
            Builder::ptr(map),
            &[
                Builder::ptr(codeblock_sym),
                Builder::ptr(bin),
                Builder::ptr(arg_frame),
            ],
        );

        let heap = Heap::new(&b.bytes);
        let frame = heap.object_at(frame_off).unwrap().as_frame().unwrap();
        assert!(frame_is_codeblock(&heap, &frame));
        let ctx = CodeBlockCtx::from_codeblock(&heap, &frame);
        assert!(ctx.arg_frame.is_some());

        let bin = heap.object_at(bin).unwrap().as_binary().unwrap();
        let mut out = String::new();
        format_disasm(&mut out, bin, Some(&ctx), &heap, 0);

        assert!(out.contains("get-var 3"), "{out}");
        assert!(out.contains("arg_frame[3] = a"), "{out}");
        assert!(out.contains("set-var 4"), "{out}");
        assert!(out.contains("arg_frame[4] = b"), "{out}");
    }

    #[test]
    fn frame_is_codeblock_recognizes_special_immediate_class() {
        // Build a CodeBlock-shaped frame whose `class` slot is the
        // special-immediate Ref(0x32) (= Special(0xC)) — the encoding
        // every production function frame in the ROM uses.
        let mut b = Builder::new();
        let class_sym = b.symbol("class");
        let map = b.array(Ref(0), &[Ref::NIL, Builder::ptr(class_sym)]);
        let frame_off = b.frame(Builder::ptr(map), &[PLAIN_FUNC_CLASS]);

        let heap = Heap::new(&b.bytes);
        let frame = heap.object_at(frame_off).unwrap().as_frame().unwrap();
        assert!(frame_is_codeblock(&heap, &frame));
    }

    #[test]
    fn slot_is_instructions_binary_detects_class() {
        let (heap_bytes, bin_off) = instr_only_heap(&[0o02]);
        let heap = Heap::new(&heap_bytes);
        let r = Builder::ptr(bin_off);
        assert!(slot_is_instructions_binary(&heap, r).is_some());

        // A non-instructions binary returns None.
        let mut b = Builder::new();
        let other = b.symbol("string");
        let bin = b.binary(Builder::ptr(other), &[0, 0]);
        let heap2 = Heap::new(&b.bytes);
        assert!(slot_is_instructions_binary(&heap2, Builder::ptr(bin)).is_none());
    }
}
