extern crate alloc;
extern crate std;

use alloc::vec::Vec;
use std::vec;

use super::*;

// ----------------------------------------------------------- Ref decoding

#[test]
fn ref_integers() {
    assert_eq!(Ref(0x14).kind(), RefKind::Integer(5));
    assert_eq!(Ref(0xFFFFFFFC).kind(), RefKind::Integer(-1));
    assert_eq!(Ref(0x00000000).kind(), RefKind::Integer(0));
}

#[test]
fn ref_pointer() {
    let r = Ref(0x0001_2345);
    // Tag is bottom 2 bits = 01; offset = 0x12344.
    assert_eq!(r.kind(), RefKind::Pointer(0x0001_2344));
    assert_eq!(r.pointer_offset(), Some(0x0001_2344));
    assert!(r.is_pointer());
}

#[test]
fn ref_nil_and_special() {
    assert_eq!(Ref::NIL.kind(), RefKind::Special(0));
    assert!(Ref::NIL.is_nil());
    // 0x06 = 0b0110, low 2 bits = 10, low 4 bits = 0110 (not character).
    assert_eq!(Ref(0x06).kind(), RefKind::Special(1));
}

#[test]
fn ref_character() {
    // 'A' (0x41) → ref = 0x41A.
    let r = Ref(0x41A);
    assert_eq!(r.kind(), RefKind::Character(0x41));
    // TRUE = character code 1.
    assert_eq!(Ref::TRUE.kind(), RefKind::Character(1));
}

#[test]
fn ref_magic_pointer() {
    // table=2, index=42 → (2 << 16) | (42 << 2) | 0b11.
    let raw = (2u32 << 16) | (42u32 << 2) | 0b11;
    let r = Ref(raw);
    assert_eq!(
        r.kind(),
        RefKind::MagicPointer { table: 2, index: 42 }
    );
}

// ----------------------------------------------------------- Symbol hash

#[test]
fn symbol_hash_matches_spec() {
    // Spec listing 0-5: case-folds [a-z] up to [A-Z], sums, multiplies
    // by 2654435769.
    let h_foo = Symbol::hash_of(b"foo");
    let folded: u32 = (b'F' as u32) + (b'O' as u32) + (b'O' as u32);
    assert_eq!(h_foo, folded.wrapping_mul(2_654_435_769));

    // Mixed case must collapse to the same hash as upper.
    assert_eq!(Symbol::hash_of(b"Foo"), Symbol::hash_of(b"FOO"));
    assert_eq!(Symbol::hash_of(b"Foo"), Symbol::hash_of(b"foo"));

    // Non-letters pass through unchanged.
    let expected: u32 = (b'_' as u32)
        + (b'P' as u32)
        + (b'R' as u32)
        + (b'O' as u32)
        + (b'T' as u32)
        + (b'O' as u32);
    assert_eq!(
        Symbol::hash_of(b"_proto"),
        expected.wrapping_mul(2_654_435_769)
    );
}

// ------------------------------------------------------ Hand-built heap

/// Helper for assembling a packed heap of NewtonScript objects.
struct Builder {
    bytes: Vec<u8>,
}

impl Builder {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn align4(&mut self) {
        while self.bytes.len() % 4 != 0 {
            self.bytes.push(0);
        }
    }

    fn ptr(off: u32) -> Ref {
        assert!(off & 0b11 == 0, "object offsets must be 4-byte aligned");
        Ref(off | 0b01)
    }

    fn header(&mut self, size: u32, flags: u8) {
        let w0 = (size << 8) | flags as u32;
        self.bytes.extend_from_slice(&w0.to_be_bytes());
        self.bytes.extend_from_slice(&[0u8; 4]); // word 1
    }

    /// Emit a binary object; returns the object's offset.
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

    /// Emit a symbol; returns its offset.
    fn symbol(&mut self, name: &str) -> u32 {
        let mut data = Vec::new();
        let hash = Symbol::hash_of(name.as_bytes());
        data.extend_from_slice(&hash.to_be_bytes());
        data.extend_from_slice(name.as_bytes());
        data.push(0);
        self.binary(SYMBOL_CLASS, &data)
    }

    /// Emit an array; returns its offset.
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

    /// Emit a frame; returns its offset.
    fn frame(&mut self, map: Ref, slots: &[Ref]) -> u32 {
        self.align4();
        let off = self.bytes.len() as u32;
        let size = 8 + 4 + (slots.len() as u32) * 4;
        let flags_byte = flags::HEADER_BASE | flags::KOBJ_SLOTTED | flags::KOBJ_FRAME;
        self.header(size, flags_byte);
        self.bytes.extend_from_slice(&map.0.to_be_bytes());
        for s in slots {
            self.bytes.extend_from_slice(&s.0.to_be_bytes());
        }
        off
    }
}

#[test]
fn walk_simple_frame() {
    // Build a frame { name: 'world, count: 42 }.
    let mut b = Builder::new();
    let world_off = b.symbol("world");
    let name_sym_off = b.symbol("name");
    let count_sym_off = b.symbol("count");

    // Map array: [supermap=NIL, 'name, 'count]. Class is integer 0 (no flags).
    let map_off = b.array(
        Ref(0), // class: integer 0
        &[Ref::NIL, Builder::ptr(name_sym_off), Builder::ptr(count_sym_off)],
    );

    // Frame slots: 'world, 42 (integer ref = 42 << 2 = 168).
    let frame_off = b.frame(
        Builder::ptr(map_off),
        &[Builder::ptr(world_off), Ref(42 << 2)],
    );

    let heap = Heap::new(&b.bytes);
    let frame = heap.object_at(frame_off).unwrap().as_frame().unwrap();

    assert_eq!(frame.len(), 2);

    // Positional access.
    let n0 = frame.name(0).unwrap();
    assert_eq!(n0.name().unwrap(), "name");
    let n1 = frame.name(1).unwrap();
    assert_eq!(n1.name().unwrap(), "count");

    // Slot values.
    let v0 = frame.slot(0).unwrap();
    let world_obj = heap.deref(v0).unwrap().as_binary().unwrap();
    let world_sym = world_obj.as_symbol().unwrap();
    assert_eq!(world_sym.name().unwrap(), "world");

    let v1 = frame.slot(1).unwrap();
    assert_eq!(v1.kind(), RefKind::Integer(42));

    // Lookup by name.
    let count = frame.lookup("count").unwrap();
    assert_eq!(count.kind(), RefKind::Integer(42));
    let missing = frame.lookup("nope");
    assert!(missing.is_none());

    // FrameIter pairs name + value.
    let pairs: Vec<_> = frame
        .iter()
        .map(|(s, r)| (s.unwrap().name().unwrap(), r))
        .collect();
    assert_eq!(pairs.len(), 2);
    assert_eq!(pairs[0].0, "name");
    assert_eq!(pairs[1].0, "count");
}

#[test]
fn walk_supermap_frame() {
    // Build:
    //   supermap = [NIL, 'a]
    //   localmap = [supermap, 'b]
    //   frame    = { a: 1, b: 2 }   (slots in supermap-order: 'a first)
    let mut b = Builder::new();
    let sym_a = b.symbol("a");
    let sym_b = b.symbol("b");
    let supermap_off = b.array(Ref(0), &[Ref::NIL, Builder::ptr(sym_a)]);
    let localmap_off = b.array(
        Ref(0),
        &[Builder::ptr(supermap_off), Builder::ptr(sym_b)],
    );
    let frame_off = b.frame(
        Builder::ptr(localmap_off),
        &[Ref(1 << 2), Ref(2 << 2)],
    );

    let heap = Heap::new(&b.bytes);
    let frame = heap.object_at(frame_off).unwrap().as_frame().unwrap();

    // Supermap names slot 0 ('a); local map names slot 1 ('b).
    assert_eq!(frame.name(0).unwrap().name().unwrap(), "a");
    assert_eq!(frame.name(1).unwrap().name().unwrap(), "b");

    assert_eq!(frame.lookup("a").unwrap().kind(), RefKind::Integer(1));
    assert_eq!(frame.lookup("b").unwrap().kind(), RefKind::Integer(2));
}

#[test]
fn array_iter_yields_all_slots() {
    let mut b = Builder::new();
    let arr_off = b.array(
        Ref::NIL,
        &[Ref(1 << 2), Ref(2 << 2), Ref(3 << 2)],
    );
    let heap = Heap::new(&b.bytes);
    let arr = heap.object_at(arr_off).unwrap().as_array().unwrap();
    assert_eq!(arr.len(), 3);
    let v: Vec<i32> = arr
        .iter()
        .map(|r| match r.kind() {
            RefKind::Integer(i) => i,
            other => panic!("expected integer, got {other:?}"),
        })
        .collect();
    assert_eq!(v, vec![1, 2, 3]);
}

#[test]
fn binary_data_roundtrip() {
    let mut b = Builder::new();
    let off = b.binary(Ref::NIL, b"hello, newton");
    let heap = Heap::new(&b.bytes);
    let bin = heap.object_at(off).unwrap().as_binary().unwrap();
    assert_eq!(bin.class(), Ref::NIL);
    assert_eq!(bin.data(), b"hello, newton");
}

#[test]
fn bad_header_rejected() {
    // size = 4 (less than the 8-byte header itself) → BadHeader.
    let bytes: [u8; 8] = [0x00, 0x00, 0x04, 0x40, 0, 0, 0, 0];
    let heap = Heap::new(&bytes);
    let err = heap.object_at(0).unwrap_err();
    assert!(matches!(err, ParseError::BadHeader { .. }));
}

#[test]
fn iter_from_walks_sequential_objects() {
    let mut b = Builder::new();
    let s1 = b.symbol("alpha");
    let s2 = b.symbol("beta");
    let arr = b.array(Ref::NIL, &[Builder::ptr(s1), Builder::ptr(s2)]);
    let heap = Heap::new(&b.bytes);

    let collected: Vec<u32> = heap
        .iter_from(0, 4)
        .map(|r| r.expect("packed objects should all parse").0)
        .collect();
    assert_eq!(collected, vec![s1, s2, arr]);
}

#[test]
fn out_of_bounds_offset_rejected() {
    let bytes: [u8; 4] = [0, 0, 0, 0];
    let heap = Heap::new(&bytes);
    // Reading 8 bytes at offset 0 of a 4-byte buffer: OOB.
    let err = heap.object_at(0).unwrap_err();
    assert!(matches!(err, ParseError::OutOfBounds { .. }));
}

#[test]
fn load_addr_translates_pointer_refs() {
    // Build a frame { name: 'world } and view the buffer as if it were
    // loaded at LOAD = 0x0100_0000. Pointer Refs use file offsets when
    // we build them, then we shift each one up by LOAD to mimic what an
    // on-target heap dump would actually contain. The heap, configured
    // with `with_load_addr(LOAD)`, must resolve those load-address-space
    // pointers back to the right file offsets.
    const LOAD: u32 = 0x0100_0000;

    let mut b = Builder::new();
    let world_off = b.symbol("world");
    let name_sym_off = b.symbol("name");
    let map_off = b.array(Ref(0), &[Ref::NIL, Builder::ptr(name_sym_off + LOAD)]);
    let frame_off = b.frame(
        Builder::ptr(map_off + LOAD),
        &[Builder::ptr(world_off + LOAD)],
    );

    let heap = Heap::with_load_addr(&b.bytes, LOAD);

    // Entry offset is also load-address-space.
    let frame = heap
        .object_at(frame_off + LOAD)
        .unwrap()
        .as_frame()
        .unwrap();
    assert_eq!(frame.offset(), frame_off + LOAD);
    assert_eq!(frame.name(0).unwrap().name().unwrap(), "name");
    let v0 = frame.slot(0).unwrap();
    assert_eq!(v0.pointer_offset(), Some(world_off + LOAD));
    let world = heap.deref(v0).unwrap().as_binary().unwrap();
    assert_eq!(world.as_symbol().unwrap().name().unwrap(), "world");

    // iter_from cursors are load-address-space too. Walking from the
    // first object reaches the frame; offsets reported are absolute.
    let offsets: Vec<u32> = heap
        .iter_from(LOAD, 4)
        .map(|r| r.expect("packed objects parse").0)
        .collect();
    assert_eq!(offsets.first().copied(), Some(world_off + LOAD));
    assert_eq!(offsets.last().copied(), Some(frame_off + LOAD));

    // An offset below load_addr is OOB rather than wrapping into the buffer.
    assert!(matches!(
        heap.object_at(LOAD - 4).unwrap_err(),
        ParseError::OutOfBounds { .. }
    ));
}
