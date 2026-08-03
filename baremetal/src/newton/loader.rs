//! ROM loader: copies the Newton ROM + Einstein REx (or a guest-test
//! flat binary) into the `hv::guest_mem` ROM backing store, applying
//! the classifier-driven BE-8 code-vs-data byte layout, the load-time
//! ROM patches, the UND/DABT vector trampolines, and the CP15 /
//! NATIVE_PRIM encoding rewrites — then publishes the result to the
//! Point of Unification for the guest's instruction fetcher.
//!
//! Distinct from `hv::guest_mem` (which keeps the backing stores and
//! the IPA/VA access layer): everything here is Newton- or test-image-
//! specific load orchestration.

#[cfg(nh_guest_test_semihost)]
use core::ptr::addr_of_mut;

use crate::hv::guest_mem::{
    ram_host_pa, rom_host_pa, rom_word_is_code, write_rom_code_word, write_rom_word_by_kind,
    ROM_SIZE,
};
use crate::kprintln;

// Big-endian ROM dump captured from hardware. Each 32-bit word is stored
// with the MSB first in memory. The guest runs BE-8, so the load layout
// is per-word and depends on the classifier: code words are byte-reversed
// (instruction fetch is always LE on A53), data words go down verbatim
// (the CPU's BE-8 reversal recovers the value). See docs/ENDIAN_FIXES.md.
// The path is resolved per ROM version by
// `resolve_rom_version()` in build.rs; when the selected version's ROM
// image isn't present on disk, build.rs stages a zero-length placeholder
// (so `cargo check` of a skeleton version stays green) and
// `load_newton_rom` halts loudly at boot instead.
#[cfg(not(nh_guest_test))]
static ROM_BE: &[u8] = include_bytes!(env!("NH_ROM_PATH"));

// Einstein's REx goes into the second 8 MB of the 16 MB ROM region, at
// `rom_ver::REX.pa_offset`. Same per-word load layout as the main ROM.
// Maps the Newton kernel's high-half VA 0x01000000 onwards
// once the guest programs its stage-1 to point there.
// See Emulator/ROM/TFlatROMImageWithREX.cpp:139-178 for the layout.
// Path resolved by build.rs alongside the ROM image.
#[cfg(not(nh_guest_test))]
static REX_BE: &[u8] = include_bytes!(env!("NH_REX_PATH"));

// Guest-test mode: `build.rs` picked up $NH_GUEST_TEST and set this cfg.
// The test binary is an AArch32 flat binary with reset vector at offset
// 0, built by baremetal/guest-tests/scripts/build-tests.sh.
//
// Two delivery modes, selected by the value of `$NH_GUEST_TEST`:
//
// 1. **Path** (`NH_GUEST_TEST=path/to/test.bin`): embed the bytes into
//    the image at compile time via `include_bytes!`. The hypervisor
//    boots straight into the test with no runtime load step. Fast for
//    single-test iteration when cargo's incremental build only has to
//    re-emit one object + relink.
//
// 2. **Semihost** (`NH_GUEST_TEST=1`): build the hypervisor as a
//    test-mode image with no embedded test, and load the test binary at
//    boot time via Arm semihosting. The path is passed by the host as a
//    semihosting cmdline arg (`qemu-system-aarch64 ... -semihosting-config
//    arg=path/to/test.bin`). One hypervisor build serves N tests — used
//    by `run-all.sh` to skip the per-test relink that dominates the
//    36-test wall time.
//
// build.rs sets `nh_guest_test_embed` for mode 1 and `nh_guest_test_semihost`
// for mode 2; both also set `nh_guest_test`.
#[cfg(nh_guest_test_embed)]
static GUEST_TEST_BIN: &[u8] = include_bytes!(env!("NH_GUEST_TEST_PATH"));

// Semihost mode: a buffer the early-boot loader fills via SYS_READ.
// Sized at GUEST_ROM's full 16 MiB so any practical test binary fits.
#[cfg(nh_guest_test_semihost)]
static mut GUEST_TEST_BIN_BUF: [u8; ROM_SIZE] = [0u8; ROM_SIZE];
#[cfg(nh_guest_test_semihost)]
static mut GUEST_TEST_BIN_LEN: usize = 0;

#[cfg(nh_guest_test_semihost)]
fn guest_test_bin() -> &'static [u8] {
    // SAFETY: GUEST_TEST_BIN_LEN is only written by `load_test_bin_via_semihosting`
    // before any reader runs, and EL2 boot is single-threaded.
    unsafe {
        let len = GUEST_TEST_BIN_LEN;
        let ptr = addr_of_mut!(GUEST_TEST_BIN_BUF) as *const u8;
        core::slice::from_raw_parts(ptr, len)
    }
}

#[cfg(nh_guest_test_embed)]
fn guest_test_bin() -> &'static [u8] {
    GUEST_TEST_BIN
}

/// Copy the embedded ROM into the guest ROM backing, byteswapping each
/// 32-bit code word to produce the little-endian view the Newton CPU
/// expects. Any ROM bytes beyond the embedded file's length are left
/// zero (so the 8 MiB "Opt. ROM" half reads as zeros until we start
/// supplying a real REx).
pub unsafe fn load_rom() {
    #[cfg(nh_guest_test)]
    {
        return unsafe { load_guest_test() };
    }
    #[cfg(not(nh_guest_test))]
    {
        unsafe { load_newton_rom() }
    }
}

/// Load the test binary into `GUEST_TEST_BIN_BUF` via Arm semihosting.
///
/// The path is the first non-binary-name word of the cmdline, which QEMU
/// populates from `-semihosting-config arg=<path>`. iter-86 introduced
/// this to skip the per-test hypervisor rebuild that dominated
/// `run-all.sh`'s 5-minute wall time. With this loader the hypervisor
/// is built once with `NH_GUEST_TEST=1` and each test run only changes
/// the QEMU cmdline arg.
#[cfg(nh_guest_test_semihost)]
unsafe fn load_test_bin_via_semihosting() {
    use core::arch::asm;
    const SYS_OPEN: u64 = 0x01;
    const SYS_CLOSE: u64 = 0x02;
    const SYS_READ: u64 = 0x06;
    const SYS_FLEN: u64 = 0x0C;
    const SYS_GET_CMDLINE: u64 = 0x15;
    const MODE_READ_BINARY: u64 = 0x01;

    unsafe fn semihost(op: u64, arg: *const u64) -> i64 {
        let result: u64;
        // SAFETY: HLT #0xF000 is the AArch64 semihosting trap; QEMU
        // intercepts and returns to EL2 without touching state beyond x0.
        unsafe {
            asm!(
                "hlt #0xF000",
                inout("x0") op => result,
                in("x1") arg as u64,
                options(nostack, preserves_flags),
            );
        }
        result as i64
    }

    // Buffer for the cmdline. QEMU's cmdline format on raspi3b semihosting
    // is "<binary_name> <arg1> <arg2> ..." — for our use, arg1 is the
    // test bin path. 256 bytes is comfortably more than any /tmp path.
    const CMDLINE_CAP: usize = 256;
    static mut CMDLINE_BUF: [u8; CMDLINE_CAP] = [0; CMDLINE_CAP];
    // SYS_GET_CMDLINE: in: ptr, len; out: writes path to ptr, len-out at
    // arg[1]. Returns 0 on success, -1 on failure.
    let cmdline_args: [u64; 2] = [addr_of_mut!(CMDLINE_BUF) as u64, (CMDLINE_CAP as u64) - 1];
    let rc = unsafe { semihost(SYS_GET_CMDLINE, cmdline_args.as_ptr()) };
    if rc != 0 {
        kprintln!("loader: SYS_GET_CMDLINE failed (rc={}) — no test bin", rc);
        crate::arch::cpu::halt();
    }

    // Parse out the second whitespace-separated word from the cmdline.
    // The first word is the binary name (or "newton-hypervisor"), the
    // second is our test bin path.
    let cmdline = unsafe {
        let ptr = addr_of_mut!(CMDLINE_BUF) as *const u8;
        // Find NUL terminator or full buffer.
        let mut n = 0;
        while n < CMDLINE_CAP && core::ptr::read(ptr.add(n)) != 0 {
            n += 1;
        }
        core::slice::from_raw_parts(ptr, n)
    };
    // QEMU's semihosting cmdline is exactly the `arg=...` value (no
    // binary-name prefix as POSIX execve would have). Take the whole
    // string, trimmed of leading/trailing whitespace.
    let mut start = 0;
    let mut end = cmdline.len();
    while start < end && (cmdline[start] == b' ' || cmdline[start] == b'\t') {
        start += 1;
    }
    while end > start
        && (cmdline[end - 1] == b' ' || cmdline[end - 1] == b'\t' || cmdline[end - 1] == b'\n')
    {
        end -= 1;
    }
    let path_bytes = &cmdline[start..end];
    if path_bytes.is_empty() {
        kprintln!(
            "loader: cmdline empty — expected QEMU \
             `-semihosting-config arg=<test-bin-path>`"
        );
        crate::arch::cpu::halt();
    }

    // SYS_OPEN takes a NUL-terminated path; copy into a static buffer.
    const PATH_CAP: usize = 256;
    static mut PATH_BUF: [u8; PATH_CAP] = [0; PATH_CAP];
    if path_bytes.len() >= PATH_CAP - 1 {
        kprintln!("loader: test path too long ({} bytes)", path_bytes.len());
        crate::arch::cpu::halt();
    }
    // SAFETY: single-threaded EL2 init; bounded write under PATH_BUF.len().
    unsafe {
        let dst = addr_of_mut!(PATH_BUF) as *mut u8;
        for (i, &b) in path_bytes.iter().enumerate() {
            dst.add(i).write(b);
        }
        dst.add(path_bytes.len()).write(0);
    }

    let open_args: [u64; 3] = [
        addr_of_mut!(PATH_BUF) as u64,
        MODE_READ_BINARY,
        path_bytes.len() as u64,
    ];
    let fh = unsafe { semihost(SYS_OPEN, open_args.as_ptr()) };
    if fh < 0 {
        kprintln!(
            "loader: SYS_OPEN failed (rc={}) for path {:?} (len={})",
            fh,
            core::str::from_utf8(path_bytes).unwrap_or("<non-utf8>"),
            path_bytes.len(),
        );
        crate::arch::cpu::halt();
    }
    let fh = fh as u64;

    let flen_args: [u64; 1] = [fh];
    let flen = unsafe { semihost(SYS_FLEN, flen_args.as_ptr()) };
    let buf_cap = ROM_SIZE; // GUEST_TEST_BIN_BUF is sized at ROM_SIZE
    if flen < 0 || (flen as usize) > buf_cap {
        kprintln!("loader: SYS_FLEN={} (test bin too large or error)", flen);
        crate::arch::cpu::halt();
    }
    let flen = flen as usize;

    // SYS_READ: ptr, len. Returns bytes-NOT-read (0 on success).
    let read_args: [u64; 3] = [fh, addr_of_mut!(GUEST_TEST_BIN_BUF) as u64, flen as u64];
    let unread = unsafe { semihost(SYS_READ, read_args.as_ptr()) };
    if unread != 0 {
        kprintln!("loader: SYS_READ left {} bytes unread", unread);
        crate::arch::cpu::halt();
    }
    let close_args: [u64; 1] = [fh];
    let _ = unsafe { semihost(SYS_CLOSE, close_args.as_ptr()) };

    // SAFETY: single-threaded EL2 init.
    unsafe {
        GUEST_TEST_BIN_LEN = flen;
    }
}

#[cfg(nh_guest_test)]
pub unsafe fn load_guest_test() {
    #[cfg(nh_guest_test_semihost)]
    unsafe {
        load_test_bin_via_semihosting();
    }

    let rom_ptr = rom_host_pa() as *mut u8;
    let bin = guest_test_bin();
    let mode = if cfg!(nh_guest_test_semihost) {
        "semihost-loaded"
    } else {
        "embedded"
    };
    kprintln!(
        "loader: GUEST-TEST MODE ({}) — copying {} bytes into GUEST_ROM",
        mode,
        bin.len()
    );
    for (i, b) in bin.iter().enumerate() {
        // SAFETY: i < bin.len() <= ROM_SIZE.
        unsafe {
            rom_ptr.add(i).write(*b);
        }
    }
    // Make the freshly-written bytes visible to the guest's instruction
    // fetcher. Without this the I-cache misses, hits memory, and reads
    // pre-init zeros (the writes are still in the D-cache).
    crate::arch::cpu::icache_publish_range(rom_ptr as u64, bin.len());
    kprintln!(
        "loader: guest-test @ host PA {:#x}, RAM @ host PA {:#x}",
        rom_host_pa(),
        ram_host_pa()
    );
    // Install the UND trampoline so guest_bp UDFs and tracer
    // USR-fallback UDFs reach EL2. The ROM
    // patching that `load_newton_rom` does to rewrite CP15 encodings
    // is still skipped — guest-test binaries are already ARMv7-correct.
    unsafe {
        super::guest_trampolines::patch_und_vector(rom_host_pa() as *mut u32);
    }
    // Don't install the DABT trampoline here: test_cp15_fault_regs
    // installs its own VA 0x10 handler to probe the CP15 shim's DFAR /
    // DFSR pass-through, and unconditionally patching would break it.
    // Tests that want the hypervisor's alignment-fault emulator (e.g.
    // test_rotate_ldr) hand-roll the trampoline shape inline so the
    // DABT enters EL2 via HVC #ALIGN_TAG the same way the real ROM
    // path does.
}

#[cfg(not(nh_guest_test))]
pub unsafe fn load_newton_rom() {
    // A zero-length ROM_BE is build.rs's placeholder for a ROM version
    // whose image isn't checked in (see `resolve_rom_version`). The
    // build compiles — proving the version contract — but there is
    // nothing to boot.
    if ROM_BE.is_empty() {
        kprintln!(
            "*** loader: no ROM image staged for version {} — place the \
             image at roms/<ver>/newton.rom and regenerate the classifier \
             bitmap (scripts/regen-classify.sh <ver>); halting",
            super::rom_ver::NAME,
        );
        crate::arch::cpu::halt();
    }
    if ROM_BE.len() != super::rom_ver::ROM_IMAGE_SIZE {
        kprintln!(
            "*** loader: ROM image is {} bytes but rom_ver::ROM_IMAGE_SIZE \
             for {} is {:#x}; wrong image? halting",
            ROM_BE.len(),
            super::rom_ver::NAME,
            super::rom_ver::ROM_IMAGE_SIZE,
        );
        crate::arch::cpu::halt();
    }
    let rom_ptr = rom_host_pa() as *mut u32;
    let be_words = ROM_BE.len() / 4;

    kprintln!(
        "loader: loading {} bytes of ROM (BE-8: code words byteswapped, data verbatim)",
        ROM_BE.len()
    );

    for i in 0..be_words {
        let off = i * 4;
        let on_disk = [
            ROM_BE[off],
            ROM_BE[off + 1],
            ROM_BE[off + 2],
            ROM_BE[off + 3],
        ];
        // SAFETY: rom_ptr covers ROM_SIZE bytes; i*4 < ROM_BE.len() <= ROM_SIZE.
        if rom_word_is_code(i) {
            // Code: CPU LE fetch must decode the original BE numerical
            // instruction. The numerical value is from_be_bytes(on_disk);
            // a native LE write of that produces host bytes = LE encoding
            // of the instruction.
            let insn = u32::from_be_bytes(on_disk);
            unsafe {
                rom_ptr.add(i).write(insn);
            }
        } else {
            // Data: under BE-8 CPSR.E=1, LDR reads the original BE
            // numerical value when host bytes equal the on-disk (BE-
            // encoded) bytes. Write each byte verbatim.
            unsafe {
                let dst = rom_ptr.add(i) as *mut u8;
                dst.add(0).write(on_disk[0]);
                dst.add(1).write(on_disk[1]);
                dst.add(2).write(on_disk[2]);
                dst.add(3).write(on_disk[3]);
            }
        }
    }

    // Load Einstein's REx at `rom_ver::REX.pa_offset` (= the second
    // 8 MB of the 16 MB ROM region). The kernel's stage-1 MMU maps this
    // to VA 0x01000000 once it programs its page tables. Same BE->LE
    // byteswap as the main ROM, because the guest runs little-endian.
    let rex_pa_offset: usize = super::rom_ver::REX.pa_offset as usize;
    let rex_words = REX_BE.len() / 4;
    kprintln!(
        "loader: loading {} bytes of Einstein.rex at PA {:#x} (BE-8: code-vs-data per bitmap)",
        REX_BE.len(),
        rex_pa_offset,
    );
    assert!(REX_BE.len() <= ROM_SIZE - rex_pa_offset);
    let rex_base_word = rex_pa_offset / 4;
    for i in 0..rex_words {
        let off = i * 4;
        let on_disk = [
            REX_BE[off],
            REX_BE[off + 1],
            REX_BE[off + 2],
            REX_BE[off + 3],
        ];
        // SAFETY: rex_base_word + i stays below ROM_SIZE / 4 via the assert above.
        if rom_word_is_code(rex_base_word + i) {
            let insn = u32::from_be_bytes(on_disk);
            unsafe {
                rom_ptr.add(rex_base_word + i).write(insn);
            }
        } else {
            unsafe {
                let dst = rom_ptr.add(rex_base_word + i) as *mut u8;
                dst.add(0).write(on_disk[0]);
                dst.add(1).write(on_disk[1]);
                dst.add(2).write(on_disk[2]);
                dst.add(3).write(on_disk[3]);
            }
        }
    }

    // Patch the external REx's id field to one past the last embedded-REx
    // id (`rom_ver::REX.num_embedded_rexes`). Mirrors
    // Einstein/Emulator/ROM/TROMImage.cpp::LookForREXes (line 311-313):
    // "Patch the REx to have a sequential ID, or NewtonOS will be very
    // confused and erase the user's Flash image." The 717006 ROM has
    // exactly one embedded REx (id=0), so its first external REx claims
    // id=1. Without the patch, NewtonOS's PrimNextRExConfigEntry indexes
    // a per-id config table and never finds our REx —
    // SearchForFlashDrivers therefore never sees the 'fdrv' entry that
    // registers TEinsteinFlashDriver, and the kernel falls back to the
    // built-in T28F016_SA_SVDriver whose Identify fails against our
    // stub flash.
    //
    // REx header layout (offsets from block start):
    //   +0x00 "RExBlock" magic (8 bytes)
    //   +0x08 checksum
    //   +0x0C header version (=1)
    //   +0x10 manufacturer ('Eins')
    //   +0x14 version
    //   +0x18 size
    //   +0x1C id             <-- the field we patch
    //   +0x20 startAddr
    //   +0x24 numEntries
    let next_rex_id: u32 = super::rom_ver::REX.num_embedded_rexes;
    let rex_id_word_index = rex_base_word + (0x1C / 4);
    // SAFETY: rex_id_word_index < rex_base_word + 8 < ROM_SIZE / 4 (checked by assert above).
    // The REx id field is data — under BE-8 the kernel reads it via LDR
    // and must see the BE-encoded value, so dispatch through the
    // bitmap-aware helper. (The bitmap should mark this word as data,
    // but using `write_rom_word_by_kind` is robust either way.)
    unsafe {
        let old_id = rom_ptr.add(rex_id_word_index).read();
        write_rom_word_by_kind(rom_ptr, rex_id_word_index, next_rex_id);
        kprintln!(
            "loader: Einstein.rex id patch host_was={:#010x} -> id={} (first free slot after embedded REx)",
            old_id, next_rex_id,
        );
    }

    // Rewrite NATIVE_PRIM call sites in the REx from Rd=LR to Rd=R12.
    //
    // Einstein's Drivers/NativePrimitives.s macro emits:
    //     stmdb sp!, {lr}
    //     mov   lr, #id                ; or: ldr lr, [pc, #4]; .word native_insn
    //     [add  lr, lr, #impl*0x100]
    //     mcr   p10, 0, lr, c0, c0, 0  ; Rd = 14 (LR) — current-mode banked
    //     ldmia sp!, {pc}
    //
    // The Newton kernel makes these calls in SVC mode, so AArch32 R14
    // is R14_svc. Per ARM ARM DDI 0487 D1.21.1 Table D1-79 the AArch64
    // GPR file aliases AArch32 R14_svc as **X18**, not X14 — so an
    // EL2 trap handler that reads `ctx.x[14]` for the MCR's Rd value
    // would get LR_usr (whatever the user-mode return address was),
    // not the native-call ID the preceding MOV wrote into LR_svc.
    //
    // The native-call decode (`NewtonOs::handle_native_call`, reached
    // from `hv::trap::handle_fp_simd`) takes the MCR encoding's Rd
    // field (an AArch32 register number, 0-15) and reads `ctx.x[Rd]`
    // — which is the AArch64 view of R<Rd>_usr, never the source
    // mode's banked R14. So Rd=14 in SVC mode delivers LR_usr, not
    // LR_svc, and every native primitive would decode as garbage.
    //
    // Fix at load time: rewrite every MCR p10 Rd=LR in the REx to use
    // Rd=R12 (IP, non-banked: R12_usr lives in X12, and X12 ≡ AArch32
    // R12 across all non-FIQ modes per Table D1-79 — also AAPCS call-
    // clobbered, so no caller is disturbed). The 32-bit MCR encoding
    // only changes bits [15:12] (Rd); we also rewrite the matching
    // MOV / ADD / LDR that produced LR's value to target R12 instead
    // (the DP-immediate encodings additionally change Rn bits [19:16]
    // on the ADD form). LR is still pushed/popped by the outer
    // STMDB/LDMIA so control-flow return is unchanged.
    //
    // (A more general fix would be to teach the native-call decode to
    // map Rd → ctx slot via Table D1-79 using the source mode in
    // SPSR_EL2; the rewrite is preferred because it gives a smaller
    // and more localised hot path on every native-primitive call.)
    //
    // SAFETY: operates within the REx window we just loaded; bounds
    // checked against REX_BE.len().
    unsafe {
        let patched = patch_native_prim_mcr_lr_to_r12(
            rom_ptr,
            rex_pa_offset as u32,
            (rex_pa_offset + REX_BE.len()) as u32,
        );
        kprintln!(
            "loader: rewrote {} NATIVE_PRIM MCR/MOV/ADD/LDR sites in REx (Rd=lr → Rd=r12)",
            patched,
        );
    }

    kprintln!(
        "loader: ROM @ host PA {:#x}, RAM @ host PA {:#x}",
        rom_host_pa(),
        ram_host_pa()
    );

    // First few decoded words, for sanity-checking that we installed the
    // vector table correctly. The reset vector is at guest PA 0.
    let first: u32 = unsafe { rom_ptr.read() };
    let second: u32 = unsafe { rom_ptr.add(1).read() };
    kprintln!(
        "loader: ROM[0..2] (LE after swap) = {:#010x} {:#010x}",
        first,
        second
    );

    // Einstein's word-write ROM patches. Without them the kernel takes
    // the wrong boot path — see `rom_ver::PATCHES` for the list and
    // `rom_patches` for the installer.
    unsafe {
        super::rom_patches::apply_rom_patches(rom_ptr);
    }

    // UND vector (VA 0x04) + trampoline body: overwrite the ROM's
    // branch-to-REx-handler with a branch to the FPA-bypass stub and
    // UND trampoline that `guest_trampolines::patch_und_vector` installs
    // in the ROM-tail stub cluster (FPA bypass at
    // `FPA_BYPASS_STUB_OFFSET`, trampoline at `UND_TRAMP_OFFSET` =
    // 0x00FF_FF00). The trampoline saves R14_und/SPSR_und to the
    // SCRATCH_POOL save area, then issues HVC #UND_TAG so
    // `trap::und::handle_und` can decode and emulate the faulting
    // instruction; FPA-class UNDs are routed straight to the kernel's
    // FPE handler. Without this the A53-only CP15 UNDs (c15 c1 op2=2)
    // and the Einstein UND opcodes would take the REx handler's path,
    // which our hypervisor isn't set up to service.
    // SAFETY: rom_ptr covers ROM_SIZE bytes; patch_und_vector writes the
    // branch word at offset 0x04 and the stub bodies in the reserved
    // ROM-tail window (0x00FF_FEC0..0x00FF_FF60), all well under
    // ROM_SIZE. See `guest_trampolines` for the per-word layout.
    unsafe {
        super::guest_trampolines::patch_und_vector(rom_ptr);
    }

    // Install the DABT-vector intercept. See
    // `guest_trampolines::patch_dabt_vector`.
    unsafe {
        super::guest_trampolines::patch_dabt_vector(rom_ptr);
    }

    // Bring-up shim #2: the 717006 kernel uses StrongARM's lax CP15 encoding
    // where CRm == CRn for most system-control registers. On ARMv7+ those
    // encodings are undefined (c1 c1 0, c2 c2 0, c3 c3 0, c5 c5 0, c6 c6 0),
    // so MMU setup silently fails on A53. Rewrite CRm -> 0 wherever we see
    // these patterns so the writes/reads land on the standard ARMv7
    // encoding (c1 c0 0, c2 c0 0, ...), which TVM/TRVM then trap into the
    // CP15 shim, which in turn applies them to real SCTLR_EL1 / TTBR0_EL1 /
    // DACR32_EL2 and so on.
    let patched = unsafe { patch_cp15_encodings(rom_ptr, ROM_SIZE / 4) };
    kprintln!(
        "loader: rewrote {} CP15 c1/c2/c3/c5/c6 encodings (StrongARM CRm=n -> ARMv7 CRm=0)",
        patched
    );

    // Publish every byte of the patched ROM aperture to the Point of
    // Unification in one sweep. `write_rom_code_word` / the load loop
    // write instruction bytes through Normal-WB into EL2's D-cache; on
    // Cortex-A53 / AEMv8-A the I-cache is non-coherent, so a guest fetch
    // can cold-read stale memory bytes unless the dirty D-cache lines are
    // cleaned to PoU (DC CVAU) and the I-cache lines invalidated
    // (IC IVAU). The `ic iallu` in `eret_to_guest` invalidates the
    // I-cache but does NOT clean dirty D-cache lines, so it cannot
    // give that guarantee on its own. This sweep does: DC CVAU; DSB;
    // IC IVAU; DSB; ISB per line across the whole aperture, run
    // strictly after every patcher. Cost is measured below and printed
    // so a future change can re-check it.
    let (icache_t0, icache_freq): (u64, u64);
    // SAFETY: MRS of RO timer sysregs, no side effects.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) icache_t0,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) icache_freq,
            options(nomem, nostack, preserves_flags));
    }
    crate::arch::cpu::icache_publish_range(rom_ptr as u64, ROM_SIZE);
    let icache_t1: u64;
    // SAFETY: as above.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) icache_t1,
            options(nomem, nostack, preserves_flags));
    }
    let icache_dt = icache_t1.wrapping_sub(icache_t0);
    kprintln!(
        "loader: icache_publish_range over {} MiB ROM aperture: {} ticks (~{} us @ {} Hz)",
        ROM_SIZE / (1024 * 1024),
        icache_dt,
        if icache_freq != 0 {
            icache_dt * 1_000_000 / icache_freq
        } else {
            0
        },
        icache_freq,
    );

    // Register the tracer; actual ROM patching is deferred until the
    // guest turns on its stage-1 MMU (see src/diag/tracer.rs for why).
    #[cfg(feature = "trace")]
    crate::diag::tracer::init();
}

/// Scan the REx window (PA `start` .. `end`) for Einstein's
/// `NATIVE_PRIM` MCR p10 call sites (Rd = LR / R14) and rewrite each
/// triplet to use R12 (IP) instead. See the block comment at the call
/// site in `load_newton_rom` for why.
///
/// Three lead-in patterns are recognised, all targeting LR:
///   1. `MOV LR, #imm`                (`0xE3A0_EXXX`)
///   2. `MOV LR, #imm; ADD LR, LR, #imm` (`0xE3A0_EXXX; 0xE28E_EXXX`)
///   3. `LDR LR, [PC, #imm]`          (`0xE59F_EXXX`)
///
/// Each `MCR p10, 0, LR, ...` word (`0xEE00_EA10`) has its Rd field
/// rewritten to R12 (`0xEE00_CA10`); each identified lead-in word is
/// rewritten to write to R12 instead of LR.
///
/// Returns the number of MCR sites rewritten.
///
/// SAFETY: `rom` is the hypervisor-owned ROM backing and `start`/`end`
/// must bound the REx-loaded range. Reads and writes are word-aligned.
#[cfg(not(nh_guest_test))]
unsafe fn patch_native_prim_mcr_lr_to_r12(rom: *mut u32, start: u32, end: u32) -> usize {
    const MCR_P10_LR: u32 = 0xEE00_EA10;
    const MCR_P10_R12: u32 = 0xEE00_CA10;
    // DP-immediate: cond 001 opc S Rn Rd imm12. We identify MOV and ADD
    // by masking out the imm12 and S bit. Encoding for MOV (opcode 0xD):
    // bits [27:20] = 0b00111010, Rn ignored.
    // For ADD (opcode 0x4): bits [27:20] = 0b00101000.
    const MOV_LR_IMM_MASK: u32 = 0xFFFF_F000;
    const MOV_LR_IMM_BITS: u32 = 0xE3A0_E000; // mov lr, #imm
    const ADD_LR_IMM_MASK: u32 = 0xFFFF_F000;
    const ADD_LR_IMM_BITS: u32 = 0xE28E_E000; // add lr, lr, #imm
    const LDR_LR_PC_MASK: u32 = 0xFFFF_F000;
    const LDR_LR_PC_BITS: u32 = 0xE59F_E000; // ldr lr, [pc, #imm]

    let start_idx = (start / 4) as usize;
    let end_idx = (end / 4) as usize;
    let mut patched = 0usize;

    for j in (start_idx + 2)..end_idx {
        if !rom_word_is_code(j) {
            continue;
        }
        // SAFETY: j < end_idx, and end_idx is word-bounded.
        let mcr = unsafe { rom.add(j).read() };
        if mcr != MCR_P10_LR {
            continue;
        }

        // Look at the immediately preceding word(s).
        let prev = unsafe { rom.add(j - 1).read() };
        let (mov_idx, add_idx) = if (prev & MOV_LR_IMM_MASK) == MOV_LR_IMM_BITS {
            (j - 1, None)
        } else if (prev & ADD_LR_IMM_MASK) == ADD_LR_IMM_BITS {
            // Need `mov lr, #id` two words back.
            let prev2 = unsafe { rom.add(j - 2).read() };
            if (prev2 & MOV_LR_IMM_MASK) != MOV_LR_IMM_BITS {
                continue;
            }
            (j - 2, Some(j - 1))
        } else if (prev & LDR_LR_PC_MASK) == LDR_LR_PC_BITS {
            (j - 1, None)
        } else {
            continue;
        };

        // Rewrite Rd field (bits [15:12]) of the lead-in word from E to C.
        // For ADD we also rewrite Rn (bits [19:16]) from E to C so
        // `add lr, lr, #imm` becomes `add r12, r12, #imm`.
        // All these are instruction rewrites in REx code, so go
        // through write_rom_code_word so BE-8 sees the right encoding.
        let lead = unsafe { rom.add(mov_idx).read() };
        let new_lead = (lead & !0x0000_F000) | 0x0000_C000;
        unsafe {
            write_rom_code_word(rom, mov_idx, new_lead);
        }

        if let Some(ai) = add_idx {
            let add = unsafe { rom.add(ai).read() };
            let new_add = (add & !0x000F_F000) | 0x000C_C000;
            unsafe {
                write_rom_code_word(rom, ai, new_add);
            }
        }

        let new_mcr = MCR_P10_R12;
        unsafe {
            write_rom_code_word(rom, j, new_mcr);
        }
        patched += 1;
    }

    patched
}

/// Scan ROM words and rewrite MCR/MRC to CP15 c{1,2,3,5,6} with non-zero CRm
/// to the equivalent standard ARMv7 encoding with CRm=0. Returns the number
/// of patched words.
///
/// ARM data-processing-coprocessor encoding for MCR/MRC with opc2=0:
///   bits[31:28] = cond (any)
///   bits[27:24] = 0b1110
///   bit 20      = L (0 = MCR, 1 = MRC)
///   bits[23:21] = opc1 (we match 0)
///   bits[19:16] = CRn
///   bits[15:12] = Rt (any)
///   bits[11:8]  = 0b1111 (CP15)
///   bits[7:5]   = opc2 (we match 0)
///   bit 4       = 1
///   bits[3:0]   = CRm
#[cfg(not(nh_guest_test))]
unsafe fn patch_cp15_encodings(rom: *mut u32, word_count: usize) -> usize {
    let mut count = 0usize;
    let mut first_pcs: [u32; 4] = [0; 4];
    for i in 0..word_count {
        if !rom_word_is_code(i) {
            continue;
        }
        // SAFETY: i < word_count matches ROM_SIZE/4.
        let w = unsafe { rom.add(i).read() };

        // Quick filter: CP15 coprocessor, opc1=0, opc2=0.
        // mask keeps: [27:20], [11:8], [7:4]; ignore cond, Rt, CRn, CRm.
        // We're matching (w & 0x0F_F0_0F_F0) == 0x0E_00_0F_10 for MCR/MRC.
        if (w & 0x0FE0_0FF0) != 0x0E00_0F10 {
            continue;
        }

        let crn = (w >> 16) & 0xF;
        let crm = w & 0xF;

        let interesting = matches!(crn, 1 | 2 | 3 | 5 | 6);
        if !interesting || crm == 0 {
            continue;
        }

        let new = w & !0xF; // CRm <- 0
                            // SAFETY: same index, in-range. Code rewrite — under BE-8 we
                            // need the BE-numerical encoding stored as native u32.
        unsafe {
            write_rom_code_word(rom, i, new);
        }
        if count < first_pcs.len() {
            first_pcs[count] = (i * 4) as u32;
        }
        count += 1;
    }
    if count > 0 {
        let shown = count.min(first_pcs.len());
        kprintln!(
            "loader: patch_cp15_encodings: {} code words rewritten; first PCs: {:#x?}",
            count,
            &first_pcs[..shown],
        );
    }
    count
}
