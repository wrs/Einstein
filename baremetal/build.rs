// build.rs
//
// - Selects between booting the real Newton ROM (default) and a small ARM
//   guest test image (if $NH_GUEST_TEST is set to a .bin file). Sets the
//   `nh_guest_test` cfg so `guest_mem.rs` takes the test-mode branch.
// - When the `trace` cargo feature is on, parses
//   scripts/classify-out/code-symbols.txt for the vetted code-only address
//   list and ../_Data_/symbols.txt for the matching mangled names, then
//   emits three compact binary blobs into OUT_DIR for src/tracer.rs and
//   src/task_dump.rs to `include_bytes!`:
//     fn_addrs.bin       — packed u32 LE, sorted ROM-range function entry PAs
//     fn_name_offs.bin   — parallel u32 LE offsets into fn_names.bin
//     fn_names.bin       — NUL-separated mangled names (name pool). Mangled
//                          rather than demangled because demangled C++ names
//                          can be 100+ chars (full arg type list); the
//                          mangled form is typically <40 chars and prints
//                          legibly in stack traces.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=NH_GUEST_TEST");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=roms/newton.rom");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=linker-fvp.ld");

    // Tell rustc that `nh_guest_test` is a known cfg so it doesn't warn.
    println!("cargo:rustc-check-cfg=cfg(nh_guest_test)");
    println!("cargo:rustc-check-cfg=cfg(nh_guest_test_embed)");
    println!("cargo:rustc-check-cfg=cfg(nh_guest_test_semihost)");

    select_platform_linker_script();
    check_host_io_features();
    check_flash_persist_features();
    emit_flash_path();

    let guest_test = env::var("NH_GUEST_TEST").ok();
    if let Some(val) = &guest_test {
        if val == "1" {
            // Semihost-load mode: build the hypervisor as a generic
            // test image; the actual test bin is loaded at boot via
            // semihosting from the path passed in QEMU's
            // `-semihosting-config arg=<path>`. iter-86 added this so
            // `run-all.sh` can build once and run N tests without the
            // per-test relink that dominated wall time.
            println!("cargo:rustc-cfg=nh_guest_test");
            println!("cargo:rustc-cfg=nh_guest_test_semihost");
            println!(
                "cargo:warning=nh-baremetal: guest-test mode (semihost-load)"
            );
        } else {
            // Embed-from-path mode. Single-test loop hitting one fixed
            // .bin — the path resolves to `include_bytes!` at compile
            // time, so cargo rebuilds when the path changes (slow for
            // run-all but fast when iterating on hypervisor changes
            // against one fixed test).
            let p = PathBuf::from(val);
            if !p.is_file() {
                panic!("NH_GUEST_TEST={} is not a file or '1'", val);
            }
            let abs = p.canonicalize().expect("canonicalize NH_GUEST_TEST");
            println!(
                "cargo:warning=nh-baremetal: guest-test mode (embed) — {}",
                abs.display()
            );
            println!("cargo:rustc-env=NH_GUEST_TEST_PATH={}", abs.display());
            println!("cargo:rustc-cfg=nh_guest_test");
            println!("cargo:rustc-cfg=nh_guest_test_embed");
            println!("cargo:rerun-if-changed={}", abs.display());
        }
    }

    // Build the symbol tables unconditionally. `trace` consumes them
    // for its trampoline pool; `task_dump` consumes them (always) for
    // PC → name lookup in stack traces. Cost is just a few hundred KB
    // of `include_bytes!` data in the image.
    build_trace_tables();

    // Stage the classify bitmap for include_bytes! in shadow_stub.
    // In guest-test mode the bitmap is embedded but never consulted —
    // patch_rom_from_bitmap is skipped because the ROM slot holds the
    // test binary, not Newton 2.x. Requiring the bitmap unconditionally
    // keeps the layout simple and catches a stale file earlier.
    let _ = guest_test;
    build_classify_bitmap();
}

/// Select the linker script based on the platform-* feature. Panics if
/// zero or multiple platform features are enabled (they are mutually
/// exclusive — they fix the load address, MMIO addresses, and UART base
/// into the image). The `.cargo/config.toml` deliberately doesn't set
/// `-Tlinker.ld` so this is the single source of truth.
fn select_platform_linker_script() {
    // Skip linker-script selection when building for a host target
    // (e.g. `cargo test --target aarch64-apple-darwin`) — the host
    // linker doesn't accept GNU-style `-T script.ld` and the bare-
    // metal memory layout is irrelevant under `cfg(test)`.
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("none") {
        return;
    }

    let raspi3b = env::var("CARGO_FEATURE_PLATFORM_RASPI3B").is_ok();
    let fvp_base = env::var("CARGO_FEATURE_PLATFORM_FVP_BASE").is_ok();

    let script = match (raspi3b, fvp_base) {
        (true, false) => "linker.ld",
        (false, true) => "linker-fvp.ld",
        (false, false) => panic!(
            "no platform selected: enable exactly one of \
             --features platform-raspi3b or --features platform-fvp-base"
        ),
        (true, true) => panic!(
            "platform-raspi3b and platform-fvp-base are mutually exclusive"
        ),
    };
    println!("cargo:rustc-link-arg=-T{script}");
}

/// Ensure exactly one `host-io-*` feature is selected. Same pattern
/// as `select_platform_linker_script`. The host-IO backend choice is
/// load-bearing (it controls whether `src/host_io/semihost.rs` or
/// `null.rs` is compiled in), so we don't want a default fallback.
fn check_host_io_features() {
    let null = env::var("CARGO_FEATURE_HOST_IO_NULL").is_ok();
    let semihost = env::var("CARGO_FEATURE_HOST_IO_SEMIHOST").is_ok();
    let pico = env::var("CARGO_FEATURE_HOST_IO_PICO").is_ok();
    let n = (null as u8) + (semihost as u8) + (pico as u8);
    match n {
        0 => panic!(
            "no host-io backend selected: enable exactly one of \
             host-io-null, host-io-semihost, or host-io-pico"
        ),
        1 => {}
        _ => panic!(
            "multiple host-io backends selected (null={null} semihost={semihost} pico={pico}); \
             they are mutually exclusive"
        ),
    }
}

/// Ensure exactly one `flash-persist-*` feature is selected. Same
/// pattern as `check_host_io_features`. The persistent-flash backend
/// is compile-time-selected so the no-op null backend has zero cost
/// in guest-test / hardware builds.
fn check_flash_persist_features() {
    let null = env::var("CARGO_FEATURE_FLASH_PERSIST_NULL").is_ok();
    let semihost = env::var("CARGO_FEATURE_FLASH_PERSIST_SEMIHOST").is_ok();
    let pico = env::var("CARGO_FEATURE_FLASH_PERSIST_PICO").is_ok();
    let n = (null as u8) + (semihost as u8) + (pico as u8);
    match n {
        0 => panic!(
            "no flash-persist backend selected: enable exactly one of \
             flash-persist-null, flash-persist-semihost, or flash-persist-pico"
        ),
        1 => {}
        _ => panic!(
            "multiple flash-persist backends selected (null={null} semihost={semihost} pico={pico}); \
             they are mutually exclusive"
        ),
    }
}

/// Resolve `$HOME/.newton/flash.bin` at build time and expose it as
/// `NEWTON_FLASH_PATH` + `NEWTON_FLASH_DIR`. Callers append a literal
/// `"\0"` via `concat!` to get the NUL-terminated form SYS_OPEN /
/// SYS_SYSTEM need — rustc rejects NUL bytes in `cargo:rustc-env`
/// values so we can't do it here.
fn emit_flash_path() {
    println!("cargo:rerun-if-env-changed=HOME");
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{home}/.newton");
    let path = format!("{dir}/flash.bin");
    println!("cargo:rustc-env=NEWTON_FLASH_PATH={path}");
    println!("cargo:rustc-env=NEWTON_FLASH_DIR={dir}");
}

fn build_trace_tables() {
    // Source of truth: `scripts/classify-out/code-symbols.txt`, the
    // curated code-only symbol list produced by classify-symbols.py.
    // It's the same list that seeds the shadow-stub classifier's walker,
    // so anything in it has already been vetted as a real function entry
    // (not a global, string-table, or mislabelled data symbol). This
    // lets the tracer trust the address list without applying first-word
    // prologue heuristics at patch time.
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let sym_path = Path::new(&manifest).join("scripts/classify-out/code-symbols.txt");
    if !sym_path.is_file() {
        panic!(
            "trace: {} missing. Run baremetal/scripts/regen-classify.sh (or \
             baremetal/scripts/classify-symbols.py) to generate it.",
            sym_path.display()
        );
    }
    println!("cargo:rerun-if-changed={}", sym_path.display());

    let text = fs::read_to_string(&sym_path)
        .unwrap_or_else(|e| panic!("trace: read {:?}: {e}", sym_path));

    // Mangled-name source. classify-symbols.py reads demangled_symbols.txt
    // (because the classifier rules match on demangled patterns), so
    // code-symbols.txt carries demangled names. We override with the
    // mangled equivalent at the same address — falling back to the
    // demangled string only if the addr isn't in symbols.txt (rare; mostly
    // applies to a handful of tool-emitted entries that aren't part of
    // the original symbol table).
    let mangled_path = Path::new(&manifest).join("../_Data_/symbols.txt");
    let mut mangled_map: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    if mangled_path.is_file() {
        println!("cargo:rerun-if-changed={}", mangled_path.display());
        let mtext = fs::read_to_string(&mangled_path)
            .unwrap_or_else(|e| panic!("trace: read {:?}: {e}", mangled_path));
        for line in mtext.lines() {
            let mut it = line.splitn(3, '\t');
            let addr_s = match it.next() { Some(s) => s.trim(), None => continue };
            let name = match it.next() { Some(s) => s.trim(), None => continue };
            if name.is_empty() { continue; }
            let hex = match addr_s.strip_prefix("0x").or_else(|| addr_s.strip_prefix("0X")) {
                Some(h) => h, None => continue,
            };
            if let Ok(addr) = u32::from_str_radix(hex, 16) {
                mangled_map.entry(addr).or_insert_with(|| name.to_string());
            }
        }
    }

    // Strict tab-separated parse of code-symbols.txt: `<hex addr>\t<name>`.
    // Classify-symbols.py has already applied the address/name filters;
    // we only defend against blatantly wrong entries (unaligned, out of
    // ROM range).
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

        if seen.insert(addr) {
            let final_name = mangled_map.get(&addr).cloned().unwrap_or_else(|| name.to_string());
            entries.push((addr, final_name));
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
        "cargo:warning=nh-baremetal: symbol table — {} function entries, {} bytes of names",
        entries.len(),
        pool_off
    );
}

/// FNV-1a-32 matching baremetal/tools/classify-rom/src/main.rs:70-94.
/// Seed chained across two byte slices = `fnv1a32(rom || rex)`.
fn fnv1a_32(bytes: &[u8], seed: u32) -> u32 {
    let mut h = seed;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Stage the per-hash `reach.bitmap` (one bit per 32-bit word across the
/// 16 MiB ROM+REX aperture; set bit = code, clear bit = data) into
/// OUT_DIR so `guest_mem` can `include_bytes!` it, and emit the
/// ROM+REX FNV-1a hash as a Rust const. Panics with an actionable
/// message if the bitmap for the current ROM+REX has not been
/// regenerated.
fn build_classify_bitmap() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let manifest = Path::new(&manifest);

    let rom_path = manifest.join("roms/newton.rom");
    let rex_path = manifest.join("../_Data_/Einstein.rex");
    let rex_path = rex_path
        .canonicalize()
        .unwrap_or_else(|_| panic!("classify: cannot locate {:?}", rex_path));

    println!("cargo:rerun-if-changed={}", rom_path.display());
    println!("cargo:rerun-if-changed={}", rex_path.display());

    let rom_bytes = fs::read(&rom_path)
        .unwrap_or_else(|e| panic!("classify: read {:?}: {e}", rom_path));
    let rex_bytes = fs::read(&rex_path)
        .unwrap_or_else(|e| panic!("classify: read {:?}: {e}", rex_path));

    let hash = fnv1a_32(&rex_bytes, fnv1a_32(&rom_bytes, 0x811C_9DC5));
    let hash_hex = format!("{:08x}", hash);

    let reach_bitmap_path = manifest
        .join("classify")
        .join(&hash_hex)
        .join("reach.bitmap");

    if !reach_bitmap_path.is_file() {
        panic!(
            "classify: reach.bitmap for ROM+REX hash {hash_hex} not found at {}. \
             Run baremetal/scripts/regen-classify.sh to generate it.",
            reach_bitmap_path.display()
        );
    }

    // One bit per 32-bit word across 16 MiB of guest ROM space.
    const EXPECTED_BITMAP_BYTES: u64 = (16 * 1024 * 1024 / 4) / 8;
    let reach_meta = fs::metadata(&reach_bitmap_path)
        .unwrap_or_else(|e| panic!("classify: stat {:?}: {e}", reach_bitmap_path));
    if reach_meta.len() != EXPECTED_BITMAP_BYTES {
        panic!(
            "classify: {} is {} bytes, expected {}",
            reach_bitmap_path.display(),
            reach_meta.len(),
            EXPECTED_BITMAP_BYTES
        );
    }

    println!("cargo:rerun-if-changed={}", reach_bitmap_path.display());

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let out = Path::new(&out_dir);

    fs::copy(&reach_bitmap_path, out.join("reach.bitmap"))
        .unwrap_or_else(|e| panic!("classify: copy reach.bitmap: {e}"));
}
