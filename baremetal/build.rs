// build.rs
//
// - Selects between booting the real Newton ROM (default) and a small ARM
//   guest test image (if $NH_GUEST_TEST is set to a .bin file). Sets the
//   `nh_guest_test` cfg so `guest_mem.rs` takes the test-mode branch.
// - When the `diag` feature is on, parses
//   scripts/classify-out/code-symbols.txt for the vetted code-only
//   address list and ../_Data_/symbols.txt for the matching mangled
//   names, then emits three compact binary blobs into OUT_DIR.
//   `src/diag/symbols.rs` includes them (PC→name lookup in halt-path
//   stack traces); `src/diag/tracer.rs` additionally consults them
//   for its trampoline pool when the `trace` feature is on. With
//   `diag` off the staging is skipped entirely — `symbols.rs` is
//   cfg-gated out and the ~743 KiB of rodata disappears from the
//   image. The blobs are:
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
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=linker-fvp.ld");

    // Tell rustc that `nh_guest_test` is a known cfg so it doesn't warn.
    println!("cargo:rustc-check-cfg=cfg(nh_guest_test)");
    println!("cargo:rustc-check-cfg=cfg(nh_guest_test_embed)");
    println!("cargo:rustc-check-cfg=cfg(nh_guest_test_semihost)");
    // Cfg flags driven by backend selection (see resolve_*_backend).
    println!("cargo:rustc-check-cfg=cfg(nh_host_io_null)");
    println!("cargo:rustc-check-cfg=cfg(nh_host_io_semihost)");
    println!("cargo:rustc-check-cfg=cfg(nh_host_io_pi_fb)");
    println!("cargo:rustc-check-cfg=cfg(nh_flash_persist_null)");
    println!("cargo:rustc-check-cfg=cfg(nh_flash_persist_semihost)");
    println!("cargo:rustc-check-cfg=cfg(nh_flash_persist_sd)");
    println!("cargo:rustc-check-cfg=cfg(nh_input_null)");
    println!("cargo:rustc-check-cfg=cfg(nh_input_mtouch)");
    println!("cargo:rustc-check-cfg=cfg(nh_audio_null)");
    println!("cargo:rustc-check-cfg=cfg(nh_audio_pi_hdmi)");
    println!("cargo:rustc-check-cfg=cfg(nh_loud_halt_canaries)");
    println!("cargo:rustc-check-cfg=cfg(nh_real_hw)");
    println!("cargo:rustc-check-cfg=cfg(nh_diag)");

    resolve_loud_halt_canaries();
    resolve_real_hw();
    resolve_diag();

    let rom_ver = resolve_rom_version();

    select_platform_linker_script();
    resolve_host_io_backend();
    resolve_flash_persist_backend();
    resolve_input_backend();
    resolve_audio_backend();
    emit_flash_path(&rom_ver);

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

    // Build the symbol tables when the diag layer is in the image.
    // `trace` consumes them for its trampoline pool; `task_dump`
    // consumes them for PC → name lookup in stack traces. With `diag`
    // off, `src/diag/symbols.rs` (the sole includer) is compiled out,
    // so the staging is skipped too.
    if env::var("CARGO_FEATURE_DIAG").is_ok() {
        build_trace_tables(&rom_ver);
    }

    // Stage the classify bitmap for include_bytes! in shadow_stub.
    // In guest-test mode the bitmap is embedded but never consulted —
    // patch_rom_from_bitmap is skipped because the ROM slot holds the
    // test binary, not Newton 2.x. Requiring the bitmap unconditionally
    // keeps the layout simple and catches a stale file earlier.
    let _ = guest_test;
    build_classify_bitmap(&rom_ver);

    // Stage the splash logo for include_bytes! in display::splash.
    // Missing file = zero-size placeholder (splash skips the logo render).
    build_splash_logo();
}

/// The selected guest-ROM version and its resolved build inputs. The
/// `rom-*` cargo feature picks the arm; exactly one must be enabled
/// (mirrors `select_platform_linker_script`). Each version supplies:
///
///   * the ROM image + REx paths, exposed to `newton::loader` as the
///     `NH_ROM_PATH` / `NH_REX_PATH` envs (consumed via
///     `include_bytes!(env!(..))`);
///   * the symbol-table inputs for the diag layer / tracer;
///   * the flash-persist filename (`$HOME/.newton/<flash_file>`).
///     717006 keeps the grandfathered legacy name `flash.bin` so
///     existing flash content loads with zero migration; other
///     versions use `flash-<ver>.bin`;
///   * `rom_code_end`, the symbol-address filter bound (mirrors
///     `rom_ver::ROM_CODE_END`).
///
/// When a version's ROM image is absent and `allow_missing_rom` is
/// set (skeleton versions with no image checked in), the resolver
/// stages zero-length ROM/REx placeholders plus an all-zero classify
/// bitmap into OUT_DIR with a cargo:warning — `cargo check`/`build`
/// stay green, and `loader::load_newton_rom` halts loudly at boot on
/// the zero-length ROM. The fully-supported 717006 keeps the hard
/// error instead.
struct RomVersion {
    tag: &'static str,
    /// Resolved ROM image path (may be the OUT_DIR placeholder).
    rom_path: PathBuf,
    /// Resolved REx path (may be the OUT_DIR placeholder).
    rex_path: PathBuf,
    /// `true` when the ROM image was absent and placeholders were
    /// staged — the classify-bitmap step then stages a placeholder
    /// bitmap instead of demanding a regenerated one.
    placeholder: bool,
    /// Curated code-only symbol list (classify-symbols.py output).
    code_symbols_path: PathBuf,
    /// Mangled-name symbol table (optional refinement).
    symbols_path: PathBuf,
    /// Flash-persist filename under `$HOME/.newton/`.
    flash_file: String,
    /// Upper bound for symbol addresses fed to the tracer tables.
    rom_code_end: u32,
}

fn resolve_rom_version() -> RomVersion {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let manifest = Path::new(&manifest);

    let v717006 = env::var("CARGO_FEATURE_ROM_717006").is_ok();
    let v710031 = env::var("CARGO_FEATURE_ROM_710031").is_ok();

    let (tag, allow_missing_rom) = match (v717006, v710031) {
        (true, false) => ("717006", false),
        (false, true) => ("710031", true),
        (false, false) => panic!(
            "no ROM version selected: enable exactly one of \
             --features rom-717006 or --features rom-710031"
        ),
        (true, true) => panic!("rom-717006 and rom-710031 are mutually exclusive"),
    };

    // Per-version input directory, with 717006 grandfathered onto the
    // historical locations (shared with the Einstein C++ project —
    // ../_Data_/Einstein.rex and ../_Data_/symbols.txt must not move):
    // roms/717006/* is honoured when present, else the legacy paths.
    let ver_dir = manifest.join("roms").join(tag);
    let pick = |candidates: &[PathBuf]| -> Option<PathBuf> {
        candidates.iter().find(|p| p.is_file()).cloned()
    };

    let (rom_candidates, rex_candidates, code_sym_candidates, sym_candidates) = if tag == "717006" {
        (
            vec![ver_dir.join("newton.rom"), manifest.join("roms/newton.rom")],
            vec![ver_dir.join("Einstein.rex"), manifest.join("../_Data_/Einstein.rex")],
            vec![
                ver_dir.join("code-symbols.txt"),
                manifest.join("scripts/classify-out/code-symbols.txt"),
            ],
            vec![ver_dir.join("symbols.txt"), manifest.join("../_Data_/symbols.txt")],
        )
    } else {
        (
            vec![ver_dir.join("newton.rom")],
            vec![ver_dir.join("Einstein.rex")],
            vec![ver_dir.join("code-symbols.txt")],
            vec![ver_dir.join("symbols.txt")],
        )
    };
    // Track only candidates that exist — cargo re-runs the build script
    // unconditionally when a rerun-if-changed path is missing, which
    // would defeat incremental builds for every version whose optional
    // override paths are absent. (Cost: creating a previously-absent
    // override needs one `touch build.rs` / clean check to be noticed.)
    for c in rom_candidates.iter().chain(rex_candidates.iter()) {
        if c.exists() {
            println!("cargo:rerun-if-changed={}", c.display());
        }
    }

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let out = Path::new(&out_dir);

    let rom_found = pick(&rom_candidates);
    let (rom_path, rex_path, placeholder) = match rom_found {
        Some(rom) => {
            let rex = pick(&rex_candidates).unwrap_or_else(|| {
                panic!(
                    "rom-{tag}: ROM image found but no REx at any of {:?}",
                    rex_candidates
                )
            });
            (rom, rex, false)
        }
        None if allow_missing_rom => {
            // Skeleton version with no image checked in: stage
            // zero-length placeholders so the include_bytes! sites
            // compile. The loader halts at boot on the empty ROM.
            let rom = out.join("placeholder-newton.rom");
            let rex = out.join("placeholder-Einstein.rex");
            fs::write(&rom, []).expect("write placeholder rom");
            fs::write(&rex, []).expect("write placeholder rex");
            println!(
                "cargo:warning=nh-baremetal: rom-{tag}: no ROM image at {:?} — \
                 staging zero-length placeholders (build checks; boot halts)",
                rom_candidates
            );
            (rom, rex, true)
        }
        None => panic!(
            "rom-{tag}: ROM image missing — expected one of {:?}",
            rom_candidates
        ),
    };

    let rom_path = rom_path.canonicalize().expect("canonicalize rom path");
    let rex_path = rex_path.canonicalize().expect("canonicalize rex path");
    println!("cargo:rustc-env=NH_ROM_PATH={}", rom_path.display());
    println!("cargo:rustc-env=NH_REX_PATH={}", rex_path.display());

    // Symbol tables degrade gracefully: a missing code-symbols.txt
    // yields empty fn tables (hex-only backtraces, tracer inert) with
    // a warning, handled in build_trace_tables.
    let code_symbols_path =
        pick(&code_sym_candidates).unwrap_or_else(|| code_sym_candidates[0].clone());
    let symbols_path = pick(&sym_candidates).unwrap_or_else(|| sym_candidates[0].clone());

    // Flash filename: 717006 keeps the legacy `flash.bin` name
    // (grandfathered — existing flash content must keep loading with
    // zero migration); every other version gets a version suffix.
    let flash_file = if tag == "717006" {
        "flash.bin".to_string()
    } else {
        format!("flash-{tag}.bin")
    };

    RomVersion {
        tag,
        rom_path,
        rex_path,
        placeholder,
        code_symbols_path,
        symbols_path,
        flash_file,
        rom_code_end: 0x0100_0000,
    }
}

/// Read `assets/splash_logo.ppm` (P6 binary; 8-bit RGB) and write the
/// raw RGB byte stream to `OUT_DIR/splash_logo.bin`, exposing the
/// dimensions as `NH_SPLASH_LOGO_W` / `NH_SPLASH_LOGO_H`. If the file
/// doesn't exist, emit an empty blob and W=H=0 so the splash renders
/// without a logo.
fn build_splash_logo() {
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let bin_path = Path::new(&out_dir).join("splash_logo.bin");
    let src = Path::new("assets/splash_logo.ppm");
    println!("cargo:rerun-if-changed=assets/splash_logo.ppm");

    let Ok(bytes) = fs::read(src) else {
        fs::write(&bin_path, []).expect("write empty splash_logo.bin");
        println!("cargo:rustc-env=NH_SPLASH_LOGO_W=0");
        println!("cargo:rustc-env=NH_SPLASH_LOGO_H=0");
        return;
    };

    let (w, h, pixels) = parse_ppm_p6(&bytes)
        .unwrap_or_else(|e| panic!("assets/splash_logo.ppm: {e}"));
    fs::write(&bin_path, &pixels).expect("write splash_logo.bin");
    println!("cargo:rustc-env=NH_SPLASH_LOGO_W={w}");
    println!("cargo:rustc-env=NH_SPLASH_LOGO_H={h}");
}

/// Minimal PPM parser. Accepts both P6 (binary RGB body) and P3 (ASCII
/// decimal RGB triples) — `magick` produces P6 by default but switches
/// to P3 with `-compress none`, so absorbing both removes a footgun.
/// Header is ASCII: `<magic>\n<W> <H>\n<MAXVAL>\n` with optional
/// `#`-comment lines anywhere in the header. MAXVAL must be 255.
fn parse_ppm_p6(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut p = 0usize;
    let next_token = |p: &mut usize| -> Result<&str, String> {
        loop {
            while *p < bytes.len() && bytes[*p].is_ascii_whitespace() {
                *p += 1;
            }
            if *p < bytes.len() && bytes[*p] == b'#' {
                while *p < bytes.len() && bytes[*p] != b'\n' {
                    *p += 1;
                }
                continue;
            }
            break;
        }
        let start = *p;
        while *p < bytes.len() && !bytes[*p].is_ascii_whitespace() {
            *p += 1;
        }
        if start == *p {
            return Err("unexpected end of header".into());
        }
        std::str::from_utf8(&bytes[start..*p])
            .map_err(|_| "non-ascii header token".to_string())
    };

    let magic = next_token(&mut p)?;
    let is_ascii = match magic {
        "P6" => false,
        "P3" => true,
        other => return Err(format!("expected P6 or P3 magic, got {other:?}")),
    };
    let w: u32 = next_token(&mut p)?.parse().map_err(|_| "bad width")?;
    let h: u32 = next_token(&mut p)?.parse().map_err(|_| "bad height")?;
    let maxval: u32 = next_token(&mut p)?
        .parse()
        .map_err(|_| "bad maxval")?;
    if maxval != 255 {
        return Err(format!(
            "maxval must be 255 (8 bpc); got {maxval}. Re-export with 8-bit depth."
        ));
    }

    let want = (w as usize) * (h as usize) * 3;

    if is_ascii {
        // P3: whitespace-separated decimal samples (and possibly more
        // `#` comments) for the rest of the file. Reuse `next_token`.
        let mut pixels = Vec::with_capacity(want);
        for _ in 0..want {
            let tok = next_token(&mut p)?;
            let v: u32 = tok.parse().map_err(|_| format!("bad sample {tok:?}"))?;
            if v > 255 {
                return Err(format!("sample {v} > maxval"));
            }
            pixels.push(v as u8);
        }
        Ok((w, h, pixels))
    } else {
        // P6: exactly one whitespace byte follows MAXVAL before pixels.
        if p >= bytes.len() {
            return Err("no pixel data".into());
        }
        p += 1;
        let body = &bytes[p..];
        if body.len() < want {
            return Err(format!(
                "short pixel data: have {} bytes, need {} ({w}x{h}*3)",
                body.len(),
                want
            ));
        }
        Ok((w, h, body[..want].to_vec()))
    }
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

// Cross-axis feature constraints are expressed as Cargo feature
// dependencies in Cargo.toml (hardware-implying backends pull in
// `platform-raspi3b`; `sd-probe` pulls in `no-semihost` too), so a
// hardware backend on the wrong platform can no longer be selected —
// the dependency drags in the second platform and
// `select_platform_linker_script` rejects the contradiction with a
// named message. That platform mutual-exclusion check is the only
// imperative gate left.

/// Emit `cfg(nh_real_hw)` when the build targets a real Pi Zero 2 W:
/// `no-semihost` + `platform-raspi3b`, i.e. the BCM2835 DMA engine and
/// real peripherals exist. This is the one cfg combination that was
/// otherwise spelled out as `all(feature = "no-semihost",
/// feature = "platform-raspi3b")` across sdhost/uart/trap/input/audio/
/// flash_persist/host_dma; naming it once means a future board adds one
/// definition here instead of editing every gate.
fn resolve_real_hw() {
    let raspi3b = env::var("CARGO_FEATURE_PLATFORM_RASPI3B").is_ok();
    let no_semihost = env::var("CARGO_FEATURE_NO_SEMIHOST").is_ok();
    if raspi3b && no_semihost {
        println!("cargo:rustc-cfg=nh_real_hw");
    }
}

/// Pick the active host-io backend and emit a `cfg(nh_host_io_*)`.
///
/// Cargo features are additive — `default = [..., "host-io-null"]`
/// can't be overridden by `cargo run --features host-io-semihost`
/// without `--no-default-features`. To make backend selection
/// composable, the `host-io-*` features are opt-in markers (not in
/// `default`); this function picks the active backend (with "null" as
/// the no-features fallback) and emits a single `cfg(nh_host_io_<x>)`
/// the source consumes. Multiple opt-ins are still MUEX.
fn resolve_host_io_backend() {
    let null = env::var("CARGO_FEATURE_HOST_IO_NULL").is_ok();
    let semihost = env::var("CARGO_FEATURE_HOST_IO_SEMIHOST").is_ok();
    let pi_fb = env::var("CARGO_FEATURE_HOST_IO_PI_FB").is_ok();
    let chosen = match (null, semihost, pi_fb) {
        (false, false, false) => "null",
        (true, false, false) => "null",
        (false, true, false) => "semihost",
        (false, false, true) => "pi_fb",
        _ => panic!(
            "multiple host-io backends selected \
             (null={null} semihost={semihost} pi_fb={pi_fb}); \
             they are mutually exclusive"
        ),
    };
    println!("cargo:rustc-cfg=nh_host_io_{chosen}");
}

/// Pick the active flash-persist backend and emit a
/// `cfg(nh_flash_persist_*)`. Same opt-in-with-fallback pattern as
/// `resolve_host_io_backend`. Default is "semihost" (a bare
/// `cargo run` persists flash); `nh_guest_test` always overrides to
/// "null" for hermetic tests regardless of features.
fn resolve_flash_persist_backend() {
    let null = env::var("CARGO_FEATURE_FLASH_PERSIST_NULL").is_ok();
    let semihost = env::var("CARGO_FEATURE_FLASH_PERSIST_SEMIHOST").is_ok();
    let sd = env::var("CARGO_FEATURE_FLASH_PERSIST_SD").is_ok();
    let guest_test = env::var("NH_GUEST_TEST").is_ok();
    let chosen = if guest_test {
        "null"
    } else {
        match (null, semihost, sd) {
            (false, false, false) => "semihost",
            (true, false, false) => "null",
            (false, true, false) => "semihost",
            (false, false, true) => "sd",
            _ => panic!(
                "multiple flash-persist backends selected \
                 (null={null} semihost={semihost} sd={sd}); \
                 they are mutually exclusive"
            ),
        }
    };
    println!("cargo:rustc-cfg=nh_flash_persist_{chosen}");
}

/// Pick the active input backend and emit `cfg(nh_input_*)`. Same
/// opt-in-with-fallback pattern as the host-io and flash-persist
/// axes. Default ("null") = no pen source; QEMU/FVP routes pen
/// events through `host_io-semihost`, which is independent of this
/// axis. `input-mtouch` lights up `src/host/input/mtouch.rs` plus the
/// USB host stack under `src/host/usb/`.
fn resolve_input_backend() {
    let null = env::var("CARGO_FEATURE_INPUT_NULL").is_ok();
    let mtouch = env::var("CARGO_FEATURE_INPUT_MTOUCH").is_ok();
    let chosen = match (null, mtouch) {
        (false, false) => "null",
        (true, false) => "null",
        (false, true) => "mtouch",
        (true, true) => panic!(
            "input-null and input-mtouch are mutually exclusive"
        ),
    };
    println!("cargo:rustc-cfg=nh_input_{chosen}");
}

/// Pick the active host-audio backend and emit `cfg(nh_audio_*)`.
/// Same opt-in-with-fallback pattern as the other axes. Default
/// ("null") means no host audio output; `audio-pi-hdmi` lights up
/// `src/host/audio/pi_hdmi.rs` against the VC4 HDMI MAI block.
fn resolve_audio_backend() {
    let null = env::var("CARGO_FEATURE_AUDIO_NULL").is_ok();
    let pi_hdmi = env::var("CARGO_FEATURE_AUDIO_PI_HDMI").is_ok();
    let chosen = match (null, pi_hdmi) {
        (false, false) => "null",
        (true, false) => "null",
        (false, true) => "pi_hdmi",
        (true, true) => panic!(
            "audio-null and audio-pi-hdmi are mutually exclusive"
        ),
    };
    println!("cargo:rustc-cfg=nh_audio_{chosen}");
}

/// Emit `cfg(nh_loud_halt_canaries)` for dev (semihost / QEMU / FVP)
/// builds, where halting on StopImage/Reboot/PowerOffAndReboot/busError
/// is a useful debugging tripwire. Real-hardware builds (the
/// `no-semihost` feature) must NOT have these canaries: a user reset or
/// idle/sleep entry would halt the hypervisor. The canary install
/// (`rom_patches::apply_loud_halt_traps`) is gated on this cfg.
fn resolve_loud_halt_canaries() {
    let no_semihost = env::var("CARGO_FEATURE_NO_SEMIHOST").is_ok();
    if !no_semihost {
        println!("cargo:rustc-cfg=nh_loud_halt_canaries");
    }
}

/// Emit `cfg(nh_diag)` when the `diag` feature is enabled. Source code
/// reads the cfg, never the raw feature — same rule as the backend
/// axes — so the "which builds carry diagnostics" policy lives in
/// Cargo.toml (`default` + the `pi-bare-metal*` aggregates) and here,
/// not scattered across `#[cfg(feature = …)]` sites.
fn resolve_diag() {
    if env::var("CARGO_FEATURE_DIAG").is_ok() {
        println!("cargo:rustc-cfg=nh_diag");
    }
}

/// Resolve `$HOME/.newton/<flash_file>` at build time and expose it as
/// `NEWTON_FLASH_PATH` + `NEWTON_FLASH_DIR`. The filename is part of
/// the per-version resolver output: `flash.bin` for 717006
/// (grandfathered legacy name), `flash-<ver>.bin` otherwise. Callers
/// append a literal `"\0"` via `concat!` to get the NUL-terminated
/// form SYS_OPEN / SYS_SYSTEM need — rustc rejects NUL bytes in
/// `cargo:rustc-env` values so we can't do it here.
fn emit_flash_path(ver: &RomVersion) {
    println!("cargo:rerun-if-env-changed=HOME");
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{home}/.newton");
    let path = format!("{dir}/{}", ver.flash_file);
    println!("cargo:rustc-env=NEWTON_FLASH_PATH={path}");
    println!("cargo:rustc-env=NEWTON_FLASH_DIR={dir}");
}

fn build_trace_tables(ver: &RomVersion) {
    // Source of truth: the version's `code-symbols.txt`, the curated
    // code-only symbol list produced by classify-symbols.py. It's the
    // same list that seeds the shadow-stub classifier's walker, so
    // anything in it has already been vetted as a real function entry
    // (not a global, string-table, or mislabelled data symbol). This
    // lets the tracer trust the address list without applying first-word
    // prologue heuristics at patch time.
    //
    // Graceful degradation: a version without a symbol table gets
    // empty blobs — hex-only backtraces, tracer logs "0 functions" —
    // plus a cargo:warning, instead of a failed build.
    let sym_path = &ver.code_symbols_path;
    println!("cargo:rerun-if-changed={}", sym_path.display());
    if !sym_path.is_file() {
        println!(
            "cargo:warning=nh-baremetal: rom-{}: no code-symbols.txt at {} — \
             emitting empty symbol tables (hex-only backtraces; tracer inert). \
             Run baremetal/scripts/regen-classify.sh to generate it.",
            ver.tag,
            sym_path.display()
        );
        let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
        let out = Path::new(&out_dir);
        for f in ["fn_addrs.bin", "fn_name_offs.bin", "fn_names.bin"] {
            fs::write(out.join(f), []).unwrap_or_else(|e| panic!("write empty {f}: {e}"));
        }
        return;
    }

    let text = fs::read_to_string(sym_path)
        .unwrap_or_else(|e| panic!("trace: read {:?}: {e}", sym_path));

    // Mangled-name source. classify-symbols.py reads demangled_symbols.txt
    // (because the classifier rules match on demangled patterns), so
    // code-symbols.txt carries demangled names. We override with the
    // mangled equivalent at the same address — falling back to the
    // demangled string only if the addr isn't in symbols.txt (rare; mostly
    // applies to a handful of tool-emitted entries that aren't part of
    // the original symbol table).
    let mangled_path = &ver.symbols_path;
    let mut mangled_map: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    if mangled_path.is_file() {
        println!("cargo:rerun-if-changed={}", mangled_path.display());
        let mtext = fs::read_to_string(mangled_path)
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
        if addr >= ver.rom_code_end { continue; }

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
fn build_classify_bitmap(ver: &RomVersion) {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let manifest = Path::new(&manifest);

    // One bit per 32-bit word across 16 MiB of guest ROM space.
    const EXPECTED_BITMAP_BYTES: u64 = (16 * 1024 * 1024 / 4) / 8;

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let out = Path::new(&out_dir);

    if ver.placeholder {
        // Skeleton version with no ROM image: an all-zero bitmap (every
        // word "data") keeps the include_bytes! site compiling; the
        // loader halts before the bitmap is ever consulted.
        println!(
            "cargo:warning=nh-baremetal: rom-{}: staging all-zero reach.bitmap \
             placeholder (no ROM image to classify)",
            ver.tag
        );
        fs::write(out.join("reach.bitmap"), vec![0u8; EXPECTED_BITMAP_BYTES as usize])
            .unwrap_or_else(|e| panic!("classify: write placeholder bitmap: {e}"));
        return;
    }

    let rom_bytes = fs::read(&ver.rom_path)
        .unwrap_or_else(|e| panic!("classify: read {:?}: {e}", ver.rom_path));
    let rex_bytes = fs::read(&ver.rex_path)
        .unwrap_or_else(|e| panic!("classify: read {:?}: {e}", ver.rex_path));

    let hash = fnv1a_32(&rex_bytes, fnv1a_32(&rom_bytes, 0x811C_9DC5));
    let hash_hex = format!("{:08x}", hash);

    let reach_bitmap_path = manifest
        .join("classify")
        .join(&hash_hex)
        .join("reach.bitmap");

    if !reach_bitmap_path.is_file() {
        panic!(
            "classify: reach.bitmap for rom-{} ROM+REX hash {hash_hex} not found at {}. \
             Run baremetal/scripts/regen-classify.sh to generate it.",
            ver.tag,
            reach_bitmap_path.display()
        );
    }

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

    fs::copy(&reach_bitmap_path, out.join("reach.bitmap"))
        .unwrap_or_else(|e| panic!("classify: copy reach.bitmap: {e}"));
}
