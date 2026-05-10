//! romdump — pretty-print a NewtonScript object graph rooted at a file
//! offset, using the `newton-objects` library.
//!
//!     romdump <file> <hex-offset> [--depth N] [--max-bytes N]

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use newton_objects::{Array, Binary, Frame, Heap, Object, Ref, RefKind, Symbol, SYMBOL_CLASS};

mod disasm;

const DEFAULT_DEPTH: u32 = 0;
const DEFAULT_MAX_BYTES: usize = 64;

struct Args {
    file: PathBuf,
    offset: u32,
    depth: u32,
    max_bytes: usize,
    flat: bool,
    dump_func_frames: bool,
    align: u32,
    load_addr: u32,
}

fn parse_offset(s: &str) -> Result<u32, String> {
    let s = s.trim();
    let (radix, digits) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(rest) => (16, rest),
        None => (16, s), // bare hex by default
    };
    u32::from_str_radix(digits, radix).map_err(|e| format!("bad offset {s:?}: {e}"))
}

fn parse_args() -> Result<Args, String> {
    let mut file: Option<PathBuf> = None;
    let mut offset: Option<u32> = None;
    let mut depth = DEFAULT_DEPTH;
    let mut max_bytes = DEFAULT_MAX_BYTES;
    let mut flat = false;
    let mut dump_func_frames = false;
    let mut align: u32 = 4;
    let mut load_addr: u32 = 0;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--depth" => {
                let v = it.next().ok_or("--depth requires a value")?;
                depth = v.parse().map_err(|e| format!("bad --depth {v:?}: {e}"))?;
            }
            "--max-bytes" => {
                let v = it.next().ok_or("--max-bytes requires a value")?;
                max_bytes = v.parse().map_err(|e| format!("bad --max-bytes {v:?}: {e}"))?;
            }
            "--flat" => flat = true,
            "--dumpfuncframes" => dump_func_frames = true,
            "--align" => {
                let v = it.next().ok_or("--align requires a value")?;
                align = v.parse().map_err(|e| format!("bad --align {v:?}: {e}"))?;
                if align != 4 && align != 8 {
                    return Err(format!("--align must be 4 or 8, got {align}"));
                }
            }
            "--loadaddr" => {
                let v = it.next().ok_or("--loadaddr requires a value")?;
                load_addr = parse_offset(&v)?;
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            _ if file.is_none() => file = Some(PathBuf::from(a)),
            _ if offset.is_none() => offset = Some(parse_offset(&a)?),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }

    Ok(Args {
        file: file.ok_or("missing <file>")?,
        offset: offset.ok_or("missing <hex-offset>")?,
        depth,
        max_bytes,
        flat,
        dump_func_frames,
        align,
        load_addr,
    })
}

fn print_usage() {
    eprintln!(
        "romdump <file> <hex-addr> [--depth N] [--max-bytes N]\n\
         \x20      [--flat | --dumpfuncframes] [--align 4|8] [--loadaddr HEX]\n\
         \n\
         Default: walks the NewtonScript object graph rooted at <hex-addr>\n\
         within <file> and pretty-prints it as a tree.\n\
         \n\
         --depth N: in tree mode, expand pointer Refs up to N levels deep.\n\
         Default 0 — show the root only; pointer Refs to other objects are\n\
         summarised as <...at 0x...>. Symbols always inline regardless of depth.\n\
         \n\
         --flat: instead of a tree walk, iterate packed objects starting at\n\
         <hex-addr>, advancing by each object's size rounded up to --align,\n\
         and print one summary line per object. Stops at the first parse error.\n\
         \n\
         --dumpfuncframes: like --flat, but for every frame encountered that\n\
         is NOT a CodeBlock function frame, run the tree pretty-printer on it\n\
         (using --depth / --max-bytes). Non-frame objects and CodeBlock frames\n\
         are skipped silently. Useful for inventorying soup/settings/layout\n\
         frames in a heap region without the noise of bytecode binaries.\n\
         \n\
         <hex-addr> is an absolute address in the heap's load-address space,\n\
         i.e. the same space the on-disk pointer Refs use. With --loadaddr\n\
         HEX, a Ref to loadaddr+X resolves to file offset X, and a <hex-addr>\n\
         of loadaddr+X likewise reads file offset X. Default --loadaddr is 0,\n\
         in which case <hex-addr> is just a file offset (what xxd shows).\n"
    );
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    let bytes = match fs::read(&args.file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: read {}: {e}", args.file.display());
            return ExitCode::from(1);
        }
    };

    let heap = Heap::with_load_addr(&bytes, args.load_addr);

    // CLI address is already in the heap's load-address space (matches
    // on-disk Refs). The heap subtracts load_addr internally to find
    // the file offset.
    let entry_abs = args.offset;

    if args.flat {
        return dump_flat(heap, entry_abs, args.align, args.max_bytes);
    }

    if args.dump_func_frames {
        return dump_func_frames(heap, entry_abs, args.align, args.depth, args.max_bytes);
    }

    let root = match heap.object_at(entry_abs) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: parse object at 0x{:08x}: {e}", entry_abs);
            return ExitCode::from(1);
        }
    };

    let mut p = Printer {
        heap,
        depth_left: args.depth,
        max_bytes: args.max_bytes,
        seen: HashSet::new(),
        out: String::new(),
    };
    p.dump_object(root, 0, "");
    println!("{}", p.out);
    ExitCode::SUCCESS
}

/// Walk packed objects starting at `offset`. For every Frame that is
/// NOT a CodeBlock, run the tree pretty-printer with the given depth
/// budget; everything else is silently skipped. Stops at the first
/// parse error (returned via the `Err` arm of the iterator).
fn dump_func_frames(
    heap: Heap<'_>,
    offset: u32,
    align: u32,
    depth: u32,
    max_bytes: usize,
) -> ExitCode {
    let mut scanned: u32 = 0;
    let mut printed: u32 = 0;
    let mut last_err: Option<newton_objects::ParseError> = None;
    for item in heap.iter_from(offset, align) {
        match item {
            Ok((_off, obj)) => {
                scanned += 1;
                let Object::Frame(f) = obj else { continue };
                if disasm::frame_is_codeblock(&heap, &f) {
                    continue;
                }
                let mut p = Printer {
                    heap,
                    depth_left: depth,
                    max_bytes,
                    seen: HashSet::new(),
                    out: String::new(),
                };
                p.dump_object(obj, 0, "");
                println!("{}", p.out);
                printed += 1;
            }
            Err(e) => {
                last_err = Some(e);
                break;
            }
        }
    }
    eprintln!(
        "// scanned {} object(s) from 0x{:08x} (align={}); printed {} non-CodeBlock frame(s)",
        scanned, offset, align, printed
    );
    if let Some(e) = last_err {
        eprintln!("// stopped on parse error: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn dump_flat(heap: Heap<'_>, offset: u32, align: u32, max_bytes: usize) -> ExitCode {
    let mut count: u32 = 0;
    let mut last_err: Option<newton_objects::ParseError> = None;
    for item in heap.iter_from(offset, align) {
        match item {
            Ok((off, obj)) => {
                count += 1;
                println!("{}", flat_summary(&heap, off, obj, max_bytes));
            }
            Err(e) => {
                last_err = Some(e);
                break;
            }
        }
    }
    eprintln!("// {} object(s) iterated from 0x{:08x} (align={})", count, offset, align);
    if let Some(e) = last_err {
        eprintln!("// stopped on parse error: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn flat_summary(heap: &Heap<'_>, offset: u32, obj: Object<'_>, max_bytes: usize) -> String {
    let kind_letter = match obj {
        Object::Binary(_) => 'B',
        Object::Array(_) => 'A',
        Object::Frame(_) => 'F',
    };
    let size = obj.size();
    let flags = obj.flags();
    let mut s = format!("0x{offset:08x} [{kind_letter} size={size:5} flags=0x{flags:02x}]");
    match obj {
        Object::Binary(b) => {
            let class = b.class();
            if let Some(sym) = b.as_symbol() {
                let name = sym.name().unwrap_or("<bad-utf8>");
                s.push_str(&format!(" symbol '{name} hash=0x{:08x}", sym.hash()));
            } else if is_class_named_top(heap, class, "string") {
                let decoded = decode_string(b);
                let shown: String = decoded.chars().take(max_bytes).collect();
                s.push_str(&format!(" string \"{shown}\""));
                if decoded.chars().count() > max_bytes {
                    s.push_str(" ...");
                }
            } else {
                s.push_str(&format!(" class={}", short_ref(heap, class)));
                let data = b.data();
                let shown = data.len().min(max_bytes);
                s.push_str(&format!(" data[{}]:", data.len()));
                for byte in &data[..shown] {
                    s.push_str(&format!(" {byte:02x}"));
                }
                if shown < data.len() {
                    s.push_str(&format!(" ... ({} more)", data.len() - shown));
                }
            }
        }
        Object::Array(a) => {
            let class = a.class();
            let len = a.len();
            s.push_str(&format!(
                " class={} slots[{}]:",
                short_ref(heap, class),
                len
            ));
            let shown = len.min(max_bytes);
            for slot in a.iter().take(shown) {
                s.push(' ');
                s.push_str(&short_ref(heap, slot));
            }
            if shown < len {
                s.push_str(&format!(" ... ({} more)", len - shown));
            }
        }
        Object::Frame(f) => {
            let len = f.len();
            s.push_str(&format!(
                " map={} slots[{}]:",
                short_ref(heap, f.map()),
                len
            ));
            let shown = len.min(max_bytes);
            for slot in f.iter_slots().take(shown) {
                s.push(' ');
                s.push_str(&short_ref(heap, slot));
            }
            if shown < len {
                s.push_str(&format!(" ... ({} more)", len - shown));
            }
        }
    }
    s
}

/// True if `class` is a pointer Ref to a symbol named `expected`
/// (case-insensitive — Newton symbols are equal under ASCII case-fold,
/// matching the spec's hash function). Used by the flat printer (the
/// tree printer has its own copy on `Printer`).
fn is_class_named_top(heap: &Heap<'_>, class: Ref, expected: &str) -> bool {
    let off = match class.pointer_offset() {
        Some(o) => o,
        None => return false,
    };
    match heap.object_at(off) {
        Ok(Object::Binary(b)) => match b.as_symbol().and_then(|s| s.name().ok()) {
            Some(name) => name.eq_ignore_ascii_case(expected),
            None => false,
        },
        _ => false,
    }
}

/// Decode a `'string` binary's UTF-16BE data to a UTF-8 string. Lone
/// surrogates (and any other invalid sequences) are rendered as
/// `\uXXXX` escapes; valid characters are escaped via
/// `char::escape_debug` so control codes / quotes stay readable.
fn decode_string(b: Binary<'_>) -> String {
    let units: Vec<u16> = b.as_string_chars().collect();
    let mut out = String::new();
    for r in char::decode_utf16(units.into_iter()) {
        match r {
            Ok(ch) => {
                for esc in ch.escape_debug() {
                    out.push(esc);
                }
            }
            Err(e) => {
                let unpaired = e.unpaired_surrogate();
                out.push_str(&format!("\\u{:04X}", unpaired));
            }
        }
    }
    out
}

/// Compact one-token rendering of a Ref for the flat summary.
pub(crate) fn short_ref(heap: &Heap<'_>, r: Ref) -> String {
    match r.kind() {
        RefKind::Integer(i) => format!("{i}"),
        RefKind::Character(c) => format!("$\\u{:04X}", c),
        RefKind::Special(_) if r.is_nil() => "NIL".to_string(),
        RefKind::Special(_) if r == Ref::TRUE => "TRUE".to_string(),
        RefKind::Special(s) => format!("special(0x{s:x})"),
        RefKind::MagicPointer { table, index } => format!("@{table}.{index}"),
        RefKind::Pointer(off) => match heap.object_at(off) {
            Ok(Object::Binary(b)) => match b.as_symbol().and_then(|s| s.name().ok()) {
                Some(name) => format!("'{name}"),
                None => format!("ptr(0x{off:08x})"),
            },
            Ok(_) => format!("ptr(0x{off:08x})"),
            Err(_) => format!("bad-ptr(0x{off:08x})"),
        },
    }
}

// ---------------------------------------------------------------- Printer

struct Printer<'a> {
    heap: Heap<'a>,
    depth_left: u32,
    max_bytes: usize,
    seen: HashSet<u32>,
    out: String,
}

impl<'a> Printer<'a> {
    /// `suffix` is text the caller wants appended in slot-trailer position
    /// (typically "," for non-last slots, "" otherwise). Most paths emit it
    /// at the very end, but the binary hex-dump fallback inserts it between
    /// the closing `]` and the trailing `// ascii` comment.
    fn dump_ref(&mut self, r: Ref, indent: usize, suffix: &str) {
        match r.kind() {
            RefKind::Integer(i) => self.out.push_str(&format!("{i}{suffix}")),
            RefKind::Character(c) => match char::from_u32(c as u32) {
                Some(ch) if !ch.is_control() => {
                    self.out.push_str(&format!("$\\u{:04X} ('{ch}'){suffix}", c))
                }
                _ => self.out.push_str(&format!("$\\u{:04X}{suffix}", c)),
            },
            RefKind::Special(_) if r.is_nil() => {
                self.out.push_str("NIL");
                self.out.push_str(suffix);
            }
            RefKind::Special(s) if r == Ref::TRUE => self
                .out
                .push_str(&format!("TRUE /* 0x{s:x} */{suffix}")),
            RefKind::Special(s) => self
                .out
                .push_str(&format!("<special 0x{s:x}>{suffix}")),
            RefKind::MagicPointer { table, index } => {
                self.out.push_str(&format!("@{table}.{index}{suffix}"));
            }
            RefKind::Pointer(off) => {
                // Symbols always inline regardless of depth.
                if let Ok(Object::Binary(b)) = self.heap.object_at(off) {
                    if let Some(sym) = b.as_symbol() {
                        self.dump_symbol_inline(sym);
                        self.out.push_str(suffix);
                        return;
                    }
                }
                if self.depth_left == 0 {
                    self.out
                        .push_str(&format!("<...at 0x{:08x}>{suffix}", off));
                    return;
                }
                self.depth_left -= 1;
                match self.heap.object_at(off) {
                    Ok(obj) => self.dump_object(obj, indent, suffix),
                    Err(e) => self
                        .out
                        .push_str(&format!("<bad-ref 0x{:08x}: {e}>{suffix}", off)),
                }
                self.depth_left += 1;
            }
        }
    }

    fn dump_object(&mut self, obj: Object<'a>, indent: usize, suffix: &str) {
        // Symbol roots inline; otherwise dispatch by kind. Depth control
        // lives in dump_ref so that the root passed in here always renders
        // in full — descents from a Ref consume the depth budget.
        if let Object::Binary(b) = obj {
            if let Some(sym) = b.as_symbol() {
                self.dump_symbol_inline(sym);
                self.out.push_str(suffix);
                return;
            }
        }

        if !self.seen.insert(obj.offset()) {
            self.out
                .push_str(&format!("<cycle to 0x{:08x}>{suffix}", obj.offset()));
            return;
        }

        match obj {
            Object::Binary(b) => self.dump_binary(b, indent, suffix),
            Object::Array(a) => {
                self.dump_array(a, indent);
                self.out.push_str(suffix);
            }
            Object::Frame(f) => {
                self.dump_frame(f, indent);
                self.out.push_str(suffix);
            }
        }
    }

    fn dump_symbol_inline(&mut self, sym: Symbol<'a>) {
        match sym.name() {
            Ok(n) => self.out.push_str(&format!("'{n}")),
            Err(_) => self
                .out
                .push_str(&format!("'<bad-utf8 @ 0x{:08x}>", sym.offset())),
        }
    }

    fn dump_binary(&mut self, b: Binary<'a>, _indent: usize, suffix: &str) {
        let class = b.class();
        // Recognise a few well-known classes for friendlier output.
        if self.is_class_named(class, "string") {
            self.out.push('"');
            self.out.push_str(&decode_string(b));
            self.out
                .push_str(&format!("\" // string at 0x{:08x}", b.offset()));
            self.out.push_str(suffix);
            return;
        }
        if class == SYMBOL_CLASS {
            // Already handled above, but keep this branch for completeness.
            if let Some(sym) = b.as_symbol() {
                self.dump_symbol_inline(sym);
                self.out.push_str(suffix);
                return;
            }
        }
        if self.is_class_named(class, "real") {
            if let Some(v) = b.as_real() {
                self.out
                    .push_str(&format!("{v} // real at 0x{:08x}", b.offset()));
                self.out.push_str(suffix);
                return;
            }
        }

        self.dump_binary_fallback(b, suffix);
    }

    /// Hex+ASCII layout for the fallback (non-string/real/symbol) binary case.
    /// Bytes are shown in 4-byte groups, two groups (8 bytes) per line.
    /// `suffix` (typically "," for slot context) is inserted between the
    /// closing `]` and the trailing `// ascii` comment so slot punctuation
    /// reads as `[hex hex], // ascii` rather than after the comment.
    /// On multi-line output, continuation lines are aligned under the
    /// first line's `[` and padded so all `//` columns line up.
    fn dump_binary_fallback(&mut self, b: Binary<'a>, suffix: &str) {
        let data = b.data();
        let total = data.len();
        let cap = self.max_bytes;
        let shown = total.min(cap);
        let truncated = shown < total;
        let bytes = &data[..shown];

        if shown == 0 {
            self.out.push_str(&format!("[]{suffix}"));
            if truncated {
                self.out.push_str(&format!(" // ({} more)", total - shown));
            }
            return;
        }

        const PER_LINE: usize = 8;
        let n_lines = shown.div_ceil(PER_LINE);
        let bracket_col = self.current_column();

        for line_idx in 0..n_lines {
            let off = line_idx * PER_LINE;
            let chunk = &bytes[off..(off + PER_LINE).min(shown)];
            let is_last = line_idx + 1 == n_lines;

            if line_idx == 0 {
                self.out.push('[');
            } else {
                for _ in 0..bracket_col {
                    self.out.push(' ');
                }
                self.out.push(' '); // align under `[`
            }

            // Hex: two 4-byte groups, padding missing bytes with two spaces
            // so widths match across lines.
            for grp in 0..2 {
                if grp > 0 {
                    self.out.push(' ');
                }
                for byte_idx in 0..4 {
                    let abs_idx = grp * 4 + byte_idx;
                    if abs_idx < chunk.len() {
                        self.out.push_str(&format!("{:02x}", chunk[abs_idx]));
                    } else {
                        self.out.push_str("  ");
                    }
                }
            }

            // Last line gets `]` + suffix + ` ` before the comment;
            // continuation lines pad to the same width so all `//` align.
            if is_last {
                self.out.push(']');
                self.out.push_str(suffix);
                self.out.push(' ');
            } else {
                for _ in 0..(2 + suffix.len()) {
                    self.out.push(' ');
                }
            }

            self.out.push_str("// ");
            for grp in 0..2 {
                if grp > 0 {
                    self.out.push(' ');
                }
                for byte_idx in 0..4 {
                    let abs_idx = grp * 4 + byte_idx;
                    if abs_idx < chunk.len() {
                        let byte = chunk[abs_idx];
                        if (0x20..=0x7e).contains(&byte) {
                            self.out.push(byte as char);
                        } else {
                            self.out.push('.');
                        }
                    } else {
                        self.out.push(' ');
                    }
                }
            }

            if is_last && truncated {
                self.out
                    .push_str(&format!(" ... ({} more)", total - shown));
            }

            if !is_last {
                self.out.push('\n');
            }
        }
    }

    fn dump_array(&mut self, a: Array<'a>, indent: usize) {
        let len = a.len();
        if len == 0 {
            self.out
                .push_str(&format!("[] // array at 0x{:08x}", a.offset()));
            return;
        }
        self.out.push_str(&format!(
            "[ // array({}) at 0x{:08x}",
            len,
            a.offset()
        ));
        let class = a.class();
        if !class.is_nil() {
            self.out.push_str(", class=");
            self.dump_ref(class, indent, "");
        }
        self.out.push('\n');
        for (i, slot) in a.iter().enumerate() {
            self.indent(indent + 2);
            let suffix = if i + 1 < len { "," } else { "" };
            self.dump_ref(slot, indent + 2, suffix);
            self.out.push('\n');
        }
        self.indent(indent);
        self.out.push(']');
    }

    fn dump_frame(&mut self, f: Frame<'a>, indent: usize) {
        let len = f.len();
        if len == 0 {
            self.out
                .push_str(&format!("{{}} // frame at 0x{:08x}", f.offset()));
            return;
        }
        self.out.push_str(&format!(
            "{{ // frame({}) at 0x{:08x}\n",
            len, f.offset()
        ));
        let codeblock_ctx = if disasm::frame_is_codeblock(&self.heap, &f) {
            Some(disasm::CodeBlockCtx::from_codeblock(&self.heap, &f))
        } else {
            None
        };
        for (i, (name_opt, slot)) in f.iter().enumerate() {
            self.indent(indent + 2);
            let slot_name = name_opt.and_then(|s| s.name().ok());
            match slot_name {
                Some(n) => self.out.push_str(&format!("{n}: ")),
                None => match name_opt {
                    Some(_) => self.out.push_str("<bad-name>: "),
                    None => self.out.push_str(&format!("<slot{i}>: ")),
                },
            }
            let suffix = if i + 1 < len { "," } else { "" };
            if codeblock_ctx.is_some()
                && slot_name == Some("instructions")
                && self.try_disasm_instructions_slot(slot, codeblock_ctx.as_ref(), indent + 2, suffix)
            {
                self.out.push('\n');
                continue;
            }
            self.dump_ref(slot, indent + 2, suffix);
            self.out.push('\n');
        }
        self.indent(indent);
        self.out.push('}');
    }

    /// If `slot` is a Ref to an `'instructions` binary, render it as a
    /// disassembly listing (consuming one depth like `dump_ref` would
    /// for any pointer Ref). Returns `true` if disassembly was emitted;
    /// `false` if the caller should fall back to the standard `dump_ref`
    /// path.
    fn try_disasm_instructions_slot(
        &mut self,
        slot: Ref,
        ctx: Option<&disasm::CodeBlockCtx<'a>>,
        indent: usize,
        suffix: &str,
    ) -> bool {
        let bin = match disasm::slot_is_instructions_binary(&self.heap, slot) {
            Some(b) => b,
            None => return false,
        };
        if self.depth_left == 0 {
            // Match dump_ref's depth-0 behavior for pointer Refs.
            let off = slot.pointer_offset().unwrap_or(0);
            self.out.push_str(&format!("<...at 0x{:08x}>{suffix}", off));
            return true;
        }
        if !self.seen.insert(bin.offset()) {
            self.out.push_str(&format!(
                "<cycle to 0x{:08x}>{suffix}",
                bin.offset()
            ));
            return true;
        }
        self.depth_left -= 1;
        disasm::format_disasm(&mut self.out, bin, ctx, &self.heap, indent);
        self.depth_left += 1;
        self.out.push_str(suffix);
        true
    }

    fn indent(&mut self, n: usize) {
        for _ in 0..n {
            self.out.push(' ');
        }
    }

    fn current_column(&self) -> usize {
        let after_newline = self.out.rfind('\n').map_or(0, |p| p + 1);
        self.out.len() - after_newline
    }

    /// Returns true if `class` is a pointer Ref to a symbol whose name
    /// equals `expected` under ASCII case-fold (Newton symbols are
    /// case-insensitive per the spec hash). Used for recognising
    /// 'string, 'real, etc.
    fn is_class_named(&self, class: Ref, expected: &str) -> bool {
        let off = match class.pointer_offset() {
            Some(o) => o,
            None => return false,
        };
        let obj = match self.heap.object_at(off) {
            Ok(o) => o,
            Err(_) => return false,
        };
        match obj {
            Object::Binary(b) => match b.as_symbol().and_then(|s| s.name().ok()) {
                Some(name) => name.eq_ignore_ascii_case(expected),
                None => false,
            },
            _ => false,
        }
    }
}

