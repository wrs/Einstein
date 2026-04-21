// build.rs
//
// - Selects between booting the real Newton ROM (default) and a small ARM
//   guest test image (if $NH_GUEST_TEST is set to a .bin file). Sets the
//   `nh_guest_test` cfg so `guest_mem.rs` takes the test-mode branch.
// - When the `trace` cargo feature is on, parses
//   ../_Data_/demangled_symbols.txt and emits three compact binary blobs
//   into OUT_DIR for src/tracer.rs to `include_bytes!`:
//     fn_addrs.bin       — packed u32 LE, sorted ROM-range function entry PAs
//     fn_name_offs.bin   — parallel u32 LE offsets into fn_names.bin
//     fn_names.bin       — NUL-separated demangled names (name pool)

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=NH_GUEST_TEST");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=roms/newton.rom");

    // Tell rustc that `nh_guest_test` is a known cfg so it doesn't warn.
    println!("cargo:rustc-check-cfg=cfg(nh_guest_test)");

    if let Ok(path) = env::var("NH_GUEST_TEST") {
        let p = PathBuf::from(&path);
        if !p.is_file() {
            panic!("NH_GUEST_TEST={} is not a file", path);
        }
        let abs = p.canonicalize().expect("canonicalize NH_GUEST_TEST");
        println!(
            "cargo:warning=nh-baremetal: guest-test mode, embedding {}",
            abs.display()
        );
        println!("cargo:rustc-env=NH_GUEST_TEST_PATH={}", abs.display());
        println!("cargo:rustc-cfg=nh_guest_test");
        println!("cargo:rerun-if-changed={}", abs.display());
    }

    if env::var("CARGO_FEATURE_TRACE").is_ok() {
        build_trace_tables();
    }
}

fn build_trace_tables() {
    // The symbol file lives one directory up from baremetal/ in the
    // Einstein checkout. Fall back to an absolute path via CARGO_MANIFEST_DIR.
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let sym_path = Path::new(&manifest).join("../_Data_/demangled_symbols.txt");
    let sym_path = sym_path
        .canonicalize()
        .unwrap_or_else(|_| panic!("trace: cannot locate {:?}", sym_path));
    println!("cargo:rerun-if-changed={}", sym_path.display());

    let text = fs::read_to_string(&sym_path)
        .unwrap_or_else(|e| panic!("trace: read {:?}: {e}", sym_path));

    // Collect (addr, name) pairs that look like ARM functions:
    //   - addr is word-aligned
    //   - addr is inside the 16 MiB ROM region (< 0x0100_0000)
    //   - name looks function-ish (C++ `::`/`(`, or starts with an
    //     ASCII uppercase letter — catches `Reset`, `BootOS`, etc.)
    // Dedupe on addr (first name wins).
    let mut entries: Vec<(u32, String)> = Vec::new();
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();

    for line in text.lines() {
        let mut it = line.splitn(2, '\t');
        let addr_s = match it.next() { Some(s) => s.trim(), None => continue };
        let name = match it.next() { Some(s) => s.trim(), None => continue };
        if name.is_empty() { continue; }

        let hex = match addr_s.strip_prefix("0x").or_else(|| addr_s.strip_prefix("0X")) {
            Some(h) => h,
            None => continue,
        };
        let addr = match u32::from_str_radix(hex, 16) {
            Ok(a) => a,
            Err(_) => continue,
        };

        if addr & 3 != 0 { continue; }
        if addr >= 0x0100_0000 { continue; }

        // Linker-generated section markers point at data, not code.
        // Drop them so we don't waste table slots on symbols we'll
        // always reject at patch time anyway.
        if name.contains("$$")
            || name.starts_with("Image$")
            || name.ends_with("$Size")
            || name.ends_with("$Length")
            || name.ends_with("$Base")
            || name.ends_with("$Limit")
            || name.ends_with("$End")
            || name.ends_with("$ZI")
        {
            continue;
        }

        // Newton convention: functions start with an uppercase letter
        // (`Reset`, `BootOS`, `TInterpreter::TInterpreter`, …) or are
        // demangled C++ signatures containing `::` / `(`. A leading
        // lowercase letter — especially `g` — marks a global, not a
        // function.
        let looks_func = name.contains("::")
            || name.contains('(')
            || name.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false);
        if !looks_func { continue; }

        if seen.insert(addr) {
            entries.push((addr, name.to_string()));
        }
    }

    entries.sort_by_key(|(a, _)| *a);

    // Emit three blobs into OUT_DIR.
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let out = Path::new(&out_dir);

    let mut addrs = fs::File::create(out.join("fn_addrs.bin")).expect("fn_addrs.bin");
    let mut offs = fs::File::create(out.join("fn_name_offs.bin")).expect("fn_name_offs.bin");
    let mut pool = fs::File::create(out.join("fn_names.bin")).expect("fn_names.bin");

    let mut pool_off: u32 = 0;
    for (addr, name) in &entries {
        addrs.write_all(&addr.to_le_bytes()).unwrap();
        offs.write_all(&pool_off.to_le_bytes()).unwrap();
        pool.write_all(name.as_bytes()).unwrap();
        pool.write_all(&[0u8]).unwrap();
        pool_off = pool_off
            .checked_add(name.len() as u32 + 1)
            .expect("trace name pool overflows u32");
    }

    println!(
        "cargo:warning=nh-baremetal: trace feature — {} function entries, {} bytes of names",
        entries.len(),
        pool_off
    );
}
