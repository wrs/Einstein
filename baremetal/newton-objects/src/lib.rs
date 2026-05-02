//! Parser for the NewtonScript object encoding.
//!
//! Targets the *interior* of an Object Part: a flat sequence of packed
//! objects (binary, array, frame). Pointer Refs are absolute file offsets
//! into the backing buffer. There is no part directory, no locator
//! array, and no relocation step here.
//!
//! Format reference: NewtonFormats.pdf chapter 1, "Newton Package
//! Specification", section "NewtonScript Object Parts".

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

// -------------------------------------------------------------------- Ref

/// A 32-bit NewtonScript Ref. Either an immediate (integer, character,
/// special, magic pointer) or a pointer into the heap.
///
/// Big-endian on disk; the value held here is already a host `u32`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ref(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefKind {
    /// 30-bit two's-complement integer.
    Integer(i32),
    /// Byte offset into the heap. Already masked of tag bits.
    Pointer(u32),
    /// 16-bit Unicode character. Note that the canonical TRUE constant
    /// is encoded as character code 1 (`Ref(0x1A)`).
    Character(u16),
    /// Non-character special immediate (e.g. NIL, function-type tags).
    /// The value is the 30-bit immediate code (already shifted right
    /// by 2 to strip the tag).
    Special(u32),
    /// Indirect reference resolved via an externally-supplied table.
    /// Bit split: tag = bits 1:0 (`11`), index = bits 15:2 (14 bits),
    /// table = bits 31:16 (16 bits).
    MagicPointer { table: u16, index: u16 },
}

impl Ref {
    pub const NIL: Ref = Ref(0x02);
    /// The canonical TRUE constant (encoded as character code 1).
    pub const TRUE: Ref = Ref(0x1A);

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub fn kind(self) -> RefKind {
        let r = self.0;
        match r & 0b11 {
            0b00 => RefKind::Integer((r as i32) >> 2),
            0b01 => RefKind::Pointer(r & !0b11),
            0b11 => RefKind::MagicPointer {
                table: (r >> 16) as u16,
                index: ((r >> 2) & 0x3FFF) as u16,
            },
            // Tag 0b10 — non-integer immediate. The 4-bit subtag 0b1010
            // distinguishes characters from other specials.
            _ => {
                if r & 0xF == 0xA {
                    RefKind::Character((r >> 4) as u16)
                } else {
                    RefKind::Special(r >> 2)
                }
            }
        }
    }

    pub fn is_nil(self) -> bool {
        self.0 == Self::NIL.0
    }

    pub fn is_pointer(self) -> bool {
        self.0 & 0b11 == 0b01
    }

    /// Heap offset for a pointer Ref; `None` for any other tag.
    pub fn pointer_offset(self) -> Option<u32> {
        if self.is_pointer() {
            Some(self.0 & !0b11)
        } else {
            None
        }
    }
}

impl fmt::Debug for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            RefKind::Integer(i) => write!(f, "Ref::Integer({i})"),
            RefKind::Pointer(p) => write!(f, "Ref::Pointer(0x{p:08x})"),
            RefKind::Character(c) => write!(f, "Ref::Character({c:#06x})"),
            RefKind::Special(_) if self.is_nil() => write!(f, "Ref::NIL"),
            RefKind::Special(s) => write!(f, "Ref::Special({s:#x})"),
            RefKind::MagicPointer { table, index } => {
                write!(f, "Ref::Magic({table}:{index})")
            }
        }
    }
}

// ------------------------------------------------------------------ Errors

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseError {
    OutOfBounds { offset: u32, len: u32 },
    NotAPointer(Ref),
    NotABinary,
    NotAnArray,
    NotAFrame,
    BadHeader { offset: u32, size: u32, flags: u8 },
    BadUtf8,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::OutOfBounds { offset, len } => {
                write!(f, "read of {len} bytes at 0x{offset:08x} is out of bounds")
            }
            Self::NotAPointer(r) => write!(f, "{r:?} is not a pointer Ref"),
            Self::NotABinary => f.write_str("object is not a binary"),
            Self::NotAnArray => f.write_str("object is not an array"),
            Self::NotAFrame => f.write_str("object is not a frame"),
            Self::BadHeader { offset, size, flags } => write!(
                f,
                "bad object header at 0x{offset:08x}: size={size} flags=0x{flags:02x}"
            ),
            Self::BadUtf8 => f.write_str("symbol name is not valid UTF-8"),
        }
    }
}

// ----------------------------------------------------------------- Endian

/// Byte order of the on-buffer encoding.
///
/// Newton package parts use big-endian (the default — preserves the
/// original behavior). The runtime heap on a little-endian CPU
/// (e.g. Cortex-A53 running an unmodified Newton ROM) uses
/// little-endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Big,
    Little,
}

// ------------------------------------------------------------------- Heap

/// A read-only view of a packed object region.
///
/// All offsets that flow through the Heap API — pointer Refs, the entry
/// offset to [`Heap::object_at`], the cursor for [`Heap::iter_from`], and
/// the `offset` returned by [`Object::offset`] — live in the same
/// "load-address" space. Internally each access translates to a file
/// offset by subtracting [`Heap::load_addr`].
///
/// A heap built with [`Heap::new`] has `load_addr = 0`, so load-address
/// offsets equal file offsets — the original behavior. Use
/// [`Heap::with_load_addr`] when the buffer represents a region that
/// was originally loaded at a non-zero address (e.g. a Newton heap dump
/// whose pointer Refs encode `loadaddr + file_offset`).
///
/// Default endianness is big-endian (Newton package format). Use
/// [`Heap::with_endian`] to parse a runtime little-endian heap.
#[derive(Clone, Copy, Debug)]
pub struct Heap<'a> {
    bytes: &'a [u8],
    load_addr: u32,
    endian: Endian,
}

impl<'a> Heap<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, load_addr: 0, endian: Endian::Big }
    }

    /// Construct a heap whose offsets are interpreted as `load_addr + X`,
    /// where `X` is the file index into `bytes`. A pointer Ref to
    /// `load_addr + X` resolves to the object whose 8-byte header begins
    /// at `bytes[X..]`.
    pub const fn with_load_addr(bytes: &'a [u8], load_addr: u32) -> Self {
        Self { bytes, load_addr, endian: Endian::Big }
    }

    /// Override the byte order for word reads. Builder-style; combine
    /// with [`Heap::new`] / [`Heap::with_load_addr`].
    pub const fn with_endian(self, endian: Endian) -> Self {
        Self { bytes: self.bytes, load_addr: self.load_addr, endian }
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn load_addr(&self) -> u32 {
        self.load_addr
    }

    pub fn endian(&self) -> Endian {
        self.endian
    }

    /// Translate a load-address-space offset to a file index, or `None`
    /// if it falls below `load_addr`.
    fn file_off(&self, abs: u32) -> Option<u32> {
        abs.checked_sub(self.load_addr)
    }

    /// Parse the object whose 8-byte header begins at `offset` (in
    /// load-address space).
    pub fn object_at(&self, offset: u32) -> Result<Object<'a>, ParseError> {
        Object::parse(*self, offset)
    }

    /// Resolve a pointer Ref to the object it points at.
    pub fn deref(&self, r: Ref) -> Result<Object<'a>, ParseError> {
        let off = r.pointer_offset().ok_or(ParseError::NotAPointer(r))?;
        self.object_at(off)
    }

    /// Iterate over packed objects starting at `offset` (load-address
    /// space). Each successive object's header is taken to begin
    /// immediately after the previous object's logical size, rounded up
    /// to `align` bytes.
    ///
    /// Newton parts are documented as using either 4- or 8-byte alignment;
    /// pass the value that matches the source data. Iteration ends at the
    /// first parse error (returned as the final `Err` item) or when the
    /// next read would exceed the buffer.
    pub fn iter_from(&self, offset: u32, align: u32) -> ObjectIter<'a> {
        ObjectIter {
            heap: *self,
            cursor: offset,
            align: align.max(4),
            done: false,
        }
    }
}

/// Iterator returned by [`Heap::iter_from`]. Yields `Result<Object, ParseError>`
/// so callers can decide whether to bail or skip past malformed regions.
pub struct ObjectIter<'a> {
    heap: Heap<'a>,
    cursor: u32,
    align: u32,
    done: bool,
}

impl<'a> Iterator for ObjectIter<'a> {
    type Item = Result<(u32, Object<'a>), ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let cursor = self.cursor;
        let cursor_file = match self.heap.file_off(cursor) {
            Some(f) => f as usize,
            None => {
                self.done = true;
                return None;
            }
        };
        if cursor_file >= self.heap.bytes.len() {
            self.done = true;
            return None;
        }
        match self.heap.object_at(cursor) {
            Ok(obj) => {
                let size = obj.size();
                let mask = self.align - 1;
                let advance = (size + mask) & !mask;
                self.cursor = cursor.saturating_add(advance);
                Some(Ok((cursor, obj)))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

fn read_word_u32(heap: Heap<'_>, abs: u32) -> Result<u32, ParseError> {
    let off = heap
        .file_off(abs)
        .ok_or(ParseError::OutOfBounds { offset: abs, len: 4 })? as usize;
    let end = off.checked_add(4).ok_or(ParseError::OutOfBounds { offset: abs, len: 4 })?;
    let s = heap
        .bytes
        .get(off..end)
        .ok_or(ParseError::OutOfBounds { offset: abs, len: 4 })?;
    let bytes = [s[0], s[1], s[2], s[3]];
    Ok(match heap.endian {
        Endian::Big => u32::from_be_bytes(bytes),
        Endian::Little => u32::from_le_bytes(bytes),
    })
}

// --------------------------------------------------------------- Header

/// Object-header flag bits (low byte of word 0).
pub mod flags {
    pub const KOBJ_SLOTTED: u8 = 0x01;
    pub const KOBJ_FRAME: u8 = 0x02;
    /// The spec mandates this bit; everything else outside KOBJ_* must be 0.
    pub const HEADER_BASE: u8 = 0x40;
}

#[derive(Clone, Copy, Debug)]
struct Header {
    /// Logical size in bytes (header + class/map + data/slots, no padding).
    size: u32,
    flags: u8,
}

impl Header {
    fn parse(heap: Heap<'_>, offset: u32) -> Result<Self, ParseError> {
        let w0 = read_word_u32(heap, offset)?;
        // word 1 is documented as zero, except the locator-array's
        // alignment-flag bit. We tolerate any value here.
        let _w1 = read_word_u32(heap, offset.wrapping_add(4))?;
        let size = w0 >> 8;
        let flags = (w0 & 0xFF) as u8;
        // read_word_u32 succeeded so file_off(offset) is in-bounds.
        let file_off = offset.wrapping_sub(heap.load_addr) as usize;
        if size < 8 || file_off.saturating_add(size as usize) > heap.bytes.len() {
            return Err(ParseError::BadHeader { offset, size, flags });
        }
        Ok(Self { size, flags })
    }

    fn is_slotted(&self) -> bool {
        self.flags & flags::KOBJ_SLOTTED != 0
    }
    fn is_frame(&self) -> bool {
        self.flags & flags::KOBJ_FRAME != 0
    }
}

// ----------------------------------------------------------------- Object

#[derive(Clone, Copy, Debug)]
pub enum Object<'a> {
    Binary(Binary<'a>),
    Array(Array<'a>),
    Frame(Frame<'a>),
}

impl<'a> Object<'a> {
    fn parse(heap: Heap<'a>, offset: u32) -> Result<Self, ParseError> {
        let header = Header::parse(heap, offset)?;
        let common = ObjBase { heap, offset, header };
        Ok(if !header.is_slotted() {
            Object::Binary(Binary(common))
        } else if header.is_frame() {
            Object::Frame(Frame(common))
        } else {
            Object::Array(Array(common))
        })
    }

    pub fn offset(&self) -> u32 {
        self.base().offset
    }

    pub fn size(&self) -> u32 {
        self.base().header.size
    }

    pub fn flags(&self) -> u8 {
        self.base().header.flags
    }

    pub fn as_binary(self) -> Result<Binary<'a>, ParseError> {
        match self {
            Object::Binary(b) => Ok(b),
            _ => Err(ParseError::NotABinary),
        }
    }
    pub fn as_array(self) -> Result<Array<'a>, ParseError> {
        match self {
            Object::Array(a) => Ok(a),
            _ => Err(ParseError::NotAnArray),
        }
    }
    pub fn as_frame(self) -> Result<Frame<'a>, ParseError> {
        match self {
            Object::Frame(f) => Ok(f),
            _ => Err(ParseError::NotAFrame),
        }
    }

    fn base(&self) -> &ObjBase<'a> {
        match self {
            Object::Binary(b) => &b.0,
            Object::Array(a) => &a.0,
            Object::Frame(f) => &f.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ObjBase<'a> {
    heap: Heap<'a>,
    offset: u32,
    header: Header,
}

impl<'a> ObjBase<'a> {
    /// Offset of the first 4-byte word after the 8-byte header.
    /// For binary/array this holds the class Ref; for frame, the map Ref.
    fn class_or_map_off(&self) -> u32 {
        self.offset + 8
    }

    /// Offset of the data area / first slot (after class-or-map word).
    fn body_off(&self) -> u32 {
        self.offset + 12
    }

    /// Logical body length (excludes 8-byte header and 4-byte class/map).
    fn body_len(&self) -> u32 {
        self.header.size.saturating_sub(12)
    }

    fn read_ref(&self, off: u32) -> Result<Ref, ParseError> {
        read_word_u32(self.heap, off).map(Ref)
    }
}

// -------------------------------------------------------- Binary / Symbol

/// `kSymbolClass` — the class Ref shared by every symbol object.
pub const SYMBOL_CLASS: Ref = Ref(0x55552);

/// Multiplier from the spec's symbol hash function (Listing 0-5).
const HASH_MUL: u32 = 2_654_435_769;

#[derive(Clone, Copy, Debug)]
pub struct Binary<'a>(ObjBase<'a>);

impl<'a> Binary<'a> {
    pub fn heap(&self) -> Heap<'a> {
        self.0.heap
    }
    pub fn offset(&self) -> u32 {
        self.0.offset
    }
    pub fn size(&self) -> u32 {
        self.0.header.size
    }
    pub fn flags(&self) -> u8 {
        self.0.header.flags
    }

    pub fn class(&self) -> Ref {
        self.0.read_ref(self.0.class_or_map_off()).unwrap_or(Ref::NIL)
    }

    /// Logical bytes of the binary, excluding the 8-byte header and
    /// 4-byte class Ref. Trailing alignment padding is *not* included
    /// (the size in the header is the logical length).
    pub fn data(&self) -> &'a [u8] {
        let bytes = self.0.heap.bytes;
        let start = self
            .0
            .heap
            .file_off(self.0.body_off())
            .map(|f| f as usize)
            .unwrap_or(bytes.len());
        let len = self.0.body_len() as usize;
        let end = start.saturating_add(len);
        &bytes[start.min(bytes.len())..end.min(bytes.len())]
    }

    pub fn as_symbol(&self) -> Option<Symbol<'a>> {
        if self.class() == SYMBOL_CLASS {
            Some(Symbol(*self))
        } else {
            None
        }
    }

    /// Returns the Unicode characters of a `'string` object. The data
    /// is null-terminated UCS-2 in the heap's endianness; the
    /// terminator is stripped.
    pub fn as_string_chars(&self) -> impl Iterator<Item = u16> + 'a {
        let data = self.data();
        StringChars { data, i: 0, endian: self.0.heap.endian }
    }

    /// IEEE-754 double-precision float for a `'real` object.
    /// The first 8 bytes of `data` are interpreted in the heap's
    /// endianness.
    pub fn as_real(&self) -> Option<f64> {
        let d = self.data();
        if d.len() < 8 {
            return None;
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&d[..8]);
        Some(match self.0.heap.endian {
            Endian::Big => f64::from_be_bytes(buf),
            Endian::Little => f64::from_le_bytes(buf),
        })
    }
}

pub struct StringChars<'a> {
    data: &'a [u8],
    i: usize,
    endian: Endian,
}

impl<'a> Iterator for StringChars<'a> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        if self.i + 2 > self.data.len() {
            return None;
        }
        let bytes = [self.data[self.i], self.data[self.i + 1]];
        let c = match self.endian {
            Endian::Big => u16::from_be_bytes(bytes),
            Endian::Little => u16::from_le_bytes(bytes),
        };
        self.i += 2;
        if c == 0 {
            None
        } else {
            Some(c)
        }
    }
}

#[derive(Clone, Copy)]
pub struct Symbol<'a>(Binary<'a>);

impl<'a> Symbol<'a> {
    pub fn binary(&self) -> Binary<'a> {
        self.0
    }
    pub fn offset(&self) -> u32 {
        self.0.offset()
    }

    /// 4-byte hash stored as the first word of the symbol's data.
    pub fn hash(&self) -> u32 {
        let d = self.0.data();
        if d.len() < 4 {
            return 0;
        }
        let bytes = [d[0], d[1], d[2], d[3]];
        match self.0.heap().endian() {
            Endian::Big => u32::from_be_bytes(bytes),
            Endian::Little => u32::from_le_bytes(bytes),
        }
    }

    /// Symbol name as raw bytes, with the null terminator stripped.
    pub fn name_bytes(&self) -> &'a [u8] {
        let d = self.0.data();
        let tail = if d.len() > 4 { &d[4..] } else { &[][..] };
        match tail.iter().position(|&b| b == 0) {
            Some(i) => &tail[..i],
            None => tail,
        }
    }

    pub fn name(&self) -> Result<&'a str, ParseError> {
        core::str::from_utf8(self.name_bytes()).map_err(|_| ParseError::BadUtf8)
    }

    /// Reference implementation of the symbol-hash function from the
    /// spec (Listing 0-5). Useful for verifying on-disk hashes.
    pub fn hash_of(name: &[u8]) -> u32 {
        let mut result: u32 = 0;
        for &b in name {
            let folded = if (b'a'..=b'z').contains(&b) {
                b - (b'a' - b'A')
            } else {
                b
            };
            result = result.wrapping_add(folded as u32);
        }
        result.wrapping_mul(HASH_MUL)
    }
}

impl<'a> fmt::Debug for Symbol<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Ok(n) => write!(f, "'{n}"),
            Err(_) => write!(f, "'<bad-utf8 @ 0x{:08x}>", self.offset()),
        }
    }
}

// ------------------------------------------------------------------ Array

#[derive(Clone, Copy, Debug)]
pub struct Array<'a>(ObjBase<'a>);

impl<'a> Array<'a> {
    pub fn heap(&self) -> Heap<'a> {
        self.0.heap
    }
    pub fn offset(&self) -> u32 {
        self.0.offset
    }
    pub fn size(&self) -> u32 {
        self.0.header.size
    }
    pub fn flags(&self) -> u8 {
        self.0.header.flags
    }

    pub fn class(&self) -> Ref {
        self.0.read_ref(self.0.class_or_map_off()).unwrap_or(Ref::NIL)
    }

    pub fn len(&self) -> usize {
        (self.0.body_len() / 4) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn slot(&self, i: usize) -> Option<Ref> {
        if i >= self.len() {
            return None;
        }
        let off = self.0.body_off() + (i as u32) * 4;
        self.0.read_ref(off).ok()
    }

    pub fn iter(&self) -> ArrayIter<'a> {
        ArrayIter {
            array: *self,
            i: 0,
        }
    }
}

pub struct ArrayIter<'a> {
    array: Array<'a>,
    i: usize,
}

impl<'a> Iterator for ArrayIter<'a> {
    type Item = Ref;
    fn next(&mut self) -> Option<Ref> {
        let r = self.array.slot(self.i)?;
        self.i += 1;
        Some(r)
    }
}

// ------------------------------------------------------------------ Frame

#[derive(Clone, Copy, Debug)]
pub struct Frame<'a>(ObjBase<'a>);

impl<'a> Frame<'a> {
    pub fn heap(&self) -> Heap<'a> {
        self.0.heap
    }
    pub fn offset(&self) -> u32 {
        self.0.offset
    }
    pub fn size(&self) -> u32 {
        self.0.header.size
    }
    pub fn flags(&self) -> u8 {
        self.0.header.flags
    }

    /// Frame map Ref (an array; slot 0 is the supermap, slots 1.. are
    /// symbol Refs naming this map's local slots).
    pub fn map(&self) -> Ref {
        self.0.read_ref(self.0.class_or_map_off()).unwrap_or(Ref::NIL)
    }

    pub fn len(&self) -> usize {
        (self.0.body_len() / 4) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Value Ref at slot `i` (positional).
    pub fn slot(&self, i: usize) -> Option<Ref> {
        if i >= self.len() {
            return None;
        }
        let off = self.0.body_off() + (i as u32) * 4;
        self.0.read_ref(off).ok()
    }

    /// Symbol naming slot `i` after walking the supermap chain.
    pub fn name(&self, i: usize) -> Option<Symbol<'a>> {
        let heap = self.0.heap;
        map_name_at(heap, self.map(), i)
    }

    /// Look up a slot value by symbol name.
    pub fn lookup(&self, name: &str) -> Option<Ref> {
        for (i, value) in self.iter_slots().enumerate() {
            if let Some(sym) = self.name(i) {
                if sym.name().ok() == Some(name) {
                    return Some(value);
                }
            }
        }
        None
    }

    pub fn iter_slots(&self) -> ArrayIter<'a> {
        // Slots are laid out the same as an array's; reuse the iterator.
        ArrayIter {
            array: Array(self.0),
            i: 0,
        }
    }

    pub fn iter(&self) -> FrameIter<'a> {
        FrameIter {
            frame: *self,
            i: 0,
        }
    }
}

pub struct FrameIter<'a> {
    frame: Frame<'a>,
    i: usize,
}

impl<'a> Iterator for FrameIter<'a> {
    type Item = (Option<Symbol<'a>>, Ref);
    fn next(&mut self) -> Option<Self::Item> {
        let value = self.frame.slot(self.i)?;
        let name = self.frame.name(self.i);
        self.i += 1;
        Some((name, value))
    }
}

/// Walks the supermap chain looking for the symbol that names slot `i`.
/// Convention: a map array's slot 0 is the supermap (NIL terminator),
/// and slots 1..N name the *last* N positional slots of the frame. So
/// we descend into the supermap first to consume the leading slots,
/// then check this map's local names.
fn map_name_at<'a>(heap: Heap<'a>, map_ref: Ref, i: usize) -> Option<Symbol<'a>> {
    if map_ref.is_nil() {
        return None;
    }
    let arr = heap.deref(map_ref).ok()?.as_array().ok()?;
    let supermap = arr.slot(0)?;
    let super_count = count_map_names(heap, supermap);
    if i < super_count {
        return map_name_at(heap, supermap, i);
    }
    let local_index = i - super_count;
    let local_count = arr.len().saturating_sub(1);
    if local_index >= local_count {
        return None;
    }
    let name_ref = arr.slot(1 + local_index)?;
    let bin = heap.deref(name_ref).ok()?.as_binary().ok()?;
    bin.as_symbol()
}

fn count_map_names(heap: Heap<'_>, map_ref: Ref) -> usize {
    if map_ref.is_nil() {
        return 0;
    }
    let arr = match heap.deref(map_ref).ok().and_then(|o| o.as_array().ok()) {
        Some(a) => a,
        None => return 0,
    };
    let local = arr.len().saturating_sub(1);
    let supermap = arr.slot(0).unwrap_or(Ref::NIL);
    local + count_map_names(heap, supermap)
}

// ------------------------------------------------------------------ Tests

#[cfg(test)]
mod tests;
