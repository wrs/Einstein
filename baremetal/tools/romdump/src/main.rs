//! romdump — pretty-print a NewtonScript object graph rooted at a file
//! offset, using the `newton-objects` library.
//!
//!     romdump <file> <hex-offset> [--depth N] [--max-bytes N]

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use newton_objects::{Array, Binary, Frame, Heap, Object, Ref, RefKind, Symbol, SYMBOL_CLASS};

const DEFAULT_DEPTH: u32 = 6;
const DEFAULT_MAX_BYTES: usize = 64;

struct Args {
    file: PathBuf,
    offset: u32,
    depth: u32,
    max_bytes: usize,
    flat: bool,
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
        align,
        load_addr,
    })
}

fn print_usage() {
    eprintln!(
        "romdump <file> <hex-addr> [--depth N] [--max-bytes N] [--flat] [--align 4|8] [--loadaddr HEX]\n\
         \n\
         Default: walks the NewtonScript object graph rooted at <hex-addr>\n\
         within <file> and pretty-prints it as a tree.\n\
         \n\
         --flat: instead of a tree walk, iterate packed objects starting at\n\
         <hex-addr>, advancing by each object's size rounded up to --align,\n\
         and print one summary line per object. Stops at the first parse error.\n\
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
    p.dump_object(root, 0);
    print!("{}", p.out);
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
            s.push_str(&format!(
                " class={} slots={}",
                short_ref(heap, class),
                a.len()
            ));
        }
        Object::Frame(f) => {
            s.push_str(&format!(
                " map={} slots={}",
                short_ref(heap, f.map()),
                f.len()
            ));
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
fn short_ref(heap: &Heap<'_>, r: Ref) -> String {
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
    fn dump_ref(&mut self, r: Ref, indent: usize) {
        match r.kind() {
            RefKind::Integer(i) => self.out.push_str(&format!("{i}")),
            RefKind::Character(c) => match char::from_u32(c as u32) {
                Some(ch) if !ch.is_control() => self.out.push_str(&format!("$\\u{:04X} ('{ch}')", c)),
                _ => self.out.push_str(&format!("$\\u{:04X}", c)),
            },
            RefKind::Special(_) if r.is_nil() => self.out.push_str("NIL"),
            RefKind::Special(s) if r == Ref::TRUE => self.out.push_str(&format!("TRUE /* 0x{s:x} */")),
            RefKind::Special(s) => self.out.push_str(&format!("<special 0x{s:x}>")),
            RefKind::MagicPointer { table, index } => {
                self.out.push_str(&format!("@{table}.{index}"));
            }
            RefKind::Pointer(off) => match self.heap.object_at(off) {
                Ok(obj) => self.dump_object(obj, indent),
                Err(e) => self.out.push_str(&format!("<bad-ref 0x{:08x}: {e}>", off)),
            },
        }
    }

    fn dump_object(&mut self, obj: Object<'a>, indent: usize) {
        // Symbols and "leaf" binaries get inlined; arrays/frames open a block.
        if let Object::Binary(b) = obj {
            if let Some(sym) = b.as_symbol() {
                self.dump_symbol_inline(sym);
                return;
            }
        }

        if !self.seen.insert(obj.offset()) {
            self.out
                .push_str(&format!("<cycle to 0x{:08x}>", obj.offset()));
            return;
        }

        if self.depth_left == 0 {
            self.out
                .push_str(&format!("<...elided at 0x{:08x}>", obj.offset()));
            return;
        }
        self.depth_left -= 1;

        match obj {
            Object::Binary(b) => self.dump_binary(b, indent),
            Object::Array(a) => self.dump_array(a, indent),
            Object::Frame(f) => self.dump_frame(f, indent),
        }

        self.depth_left += 1;
    }

    fn dump_symbol_inline(&mut self, sym: Symbol<'a>) {
        match sym.name() {
            Ok(n) => self.out.push_str(&format!("'{n}")),
            Err(_) => self
                .out
                .push_str(&format!("'<bad-utf8 @ 0x{:08x}>", sym.offset())),
        }
    }

    fn dump_binary(&mut self, b: Binary<'a>, indent: usize) {
        let class = b.class();
        // Recognise a few well-known classes for friendlier output.
        if self.is_class_named(class, "string") {
            self.out.push('"');
            self.out.push_str(&decode_string(b));
            self.out
                .push_str(&format!("\" // string at 0x{:08x}", b.offset()));
            return;
        }
        if class == SYMBOL_CLASS {
            // Already handled above, but keep this branch for completeness.
            if let Some(sym) = b.as_symbol() {
                self.dump_symbol_inline(sym);
                return;
            }
        }
        if self.is_class_named(class, "real") {
            if let Some(v) = b.as_real() {
                self.out
                    .push_str(&format!("{v} // real at 0x{:08x}", b.offset()));
                return;
            }
        }

        // Fallback: hex-dump the data.
        let data = b.data();
        let shown = data.len().min(self.max_bytes);
        self.out.push_str(&format!(
            "<binary class="
        ));
        self.dump_ref(class, indent);
        self.out.push_str(&format!(
            " size={} bytes at 0x{:08x}> [",
            data.len(),
            b.offset()
        ));
        for (i, byte) in data[..shown].iter().enumerate() {
            if i > 0 {
                self.out.push(' ');
            }
            self.out.push_str(&format!("{byte:02x}"));
        }
        if shown < data.len() {
            self.out
                .push_str(&format!(" ... ({} more)", data.len() - shown));
        }
        self.out.push(']');
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
            self.dump_ref(class, indent);
        }
        self.out.push('\n');
        for (i, slot) in a.iter().enumerate() {
            self.indent(indent + 2);
            self.dump_ref(slot, indent + 2);
            if i + 1 < len {
                self.out.push(',');
            }
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
        for (i, (name_opt, slot)) in f.iter().enumerate() {
            self.indent(indent + 2);
            match name_opt {
                Some(sym) => match sym.name() {
                    Ok(n) => self.out.push_str(&format!("{n}: ")),
                    Err(_) => self.out.push_str("<bad-name>: "),
                },
                None => self.out.push_str(&format!("<slot{i}>: ")),
            }
            self.dump_ref(slot, indent + 2);
            if i + 1 < len {
                self.out.push(',');
            }
            self.out.push('\n');
        }
        self.indent(indent);
        self.out.push('}');
    }

    fn indent(&mut self, n: usize) {
        for _ in 0..n {
            self.out.push(' ');
        }
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

