# Plan — current state and remaining work

## State

The 717006 ROM boots through kernel, scheduler and NewtonScript
interpreter to the Welcome UI, and the builtin apps work
interactively — on QEMU `raspi3b`, on ARM FVP, and on a real
Pi Zero 2 W with HDMI display, USB touch, HDMI audio and SD-backed
flash persistence. All 39 guest tests are green on QEMU; on FVP 38 of
39 (`test_swp_rom_aperture` gives NO-VERDICT — item 8 below); all
build combinations in `scripts/check-matrix.sh` pass.

## Standing rules

- Run the *original ROM code*. No workarounds, no shortcuts. ROM
  patches are the last resort, only when no other layer can host the
  fix.
- No shadow page tables and no per-access AP emulation: guest stage-1
  incompatibilities are resolved by normalising the guest's own
  descriptors in place (`HIGHLEVEL.md` §4.3).
- Every commit that touches hypervisor functionality (not merely
  diagnostics) must pass `guest-tests/scripts/run-all.sh`, all 39
  tests. Fix warnings before committing.
- Unknown inputs on emulation paths halt loudly with a context dump.
  Never add a silent default to quiet a halt — the halt is the
  trip-wire that says which table entry to extend.

## Remaining work

1. **Add-on app packages.** Install now works through the store path.
   Root cause of the `-10606` / `-48402` / `-48421` / `-48200` install
   failures was the package pager, `TROMDomainManager1K`: it demand-
   pages store-backed large objects at 1 KiB granularity using ARMv4
   subpage AP (absent subpages = AP 00, so the first touch faults and
   `Fault -> DecompressAndMap` fills that 1 KiB). ARMv7 has no subpage
   AP, so an absent subpage read as stale RAM instead of faulting and
   `IsPackageHeader` saw zeros in the freshly installed package. Fixed
   like the stack/heap allocators, with kernel ROM patches
   (`rom_ver/r717006/patches.rs` "Package pager", `rom_patches.rs`
   `apply_package_pager_patch`): GetSubPage claims whole physical
   pages, Fault fills all four subpages, AllocatePackageEntry places
   objects on 4 KiB-aligned VAs. Decoded structures in
   `docs/STRUCTURES.md` "TROMDomainManager1K".

   Verified on QEMU via `scripts/pkg-repl-install.py` (uploads a .pkg
   through the NewtonScript REPL and calls the store's own
   `SuckPackageFromBinary`, the call the ROM's restore path makes):
   five packages back to back (NTK-built ROMDumper.pkg, 8 KiB, with
   an installScript and bytecode functions; plus four newt64-built
   test packages from `tools/test-packages/`), package count 26 -> 31, entries persist across a
   reboot with their titles and show up in Extras with their icons.
   Also verified through the real path: UnixNPI (built from
   github.com/chuma/unixnpi) -> `scripts/serial-pty-bridge.py` ->
   Dock "Connect via Serial" installs a package that then appears in
   Extras. Not yet verified on real hardware. Native code inside packages remains untested
   ([`docs/PACKAGE_NATIVE_CODE.md`](docs/PACKAGE_NATIVE_CODE.md)).

   Open follow-ups from the same investigation:
   - An exception escaping `SuckPackageFromBinary` (e.g. a bad
     package) leaves the source binary's `TObjectPtr` lock leaked;
     the next heap growth then parks the newt task forever inside
     `LockHeapRange -> MonitorDispatchSWI` (UI and REP dead, system
     idle). Repro: install a package that throws, then `MakeBinary`
     a few KiB. Real-hardware NewtonOS would only have one 1 KiB
     subpage locked; our 4 KiB chunking widens the lock. Triage
     recipe in `docs/DEBUGGING.md` "Parked newt task".
   - The working set now spends 4 KiB per cached VA page instead of
     1 KiB; no eviction-path testing yet (`FreeAnySubPages`,
     `ShuffleSubPages`, writable large objects / `ResizeObject`).
   - `GetPackages()` returns info frames (`title`, `pssid`, `store`),
     not package refs; `GetPkgRefInfo` wants the ref.

2. **Snapshot resume — fix or remove.** The ring is now behind the
   default-off `snapshot` cargo feature (`resolve_snapshot` in
   build.rs → `nh_snapshot`), so a normal build cold-boots and never
   writes a slot — the interim mitigation for the fact that resume is
   broken. Saving works, and the two-run `test_snapshot_resume` guest
   test resumes correctly (guest-test builds force the ring on);
   resuming the *Newton ROM* does not. The resumed guest ERETs to the
   saved PC and immediately wedges in a prefetch-abort loop at the
   vector page (`ELR = IFAR = 0xc`, ABT mode), after which the 2 s
   autosave overwrites all four slots with the wedged state within
   ~8 s — so with `--features snapshot` on, still cold-boot each run
   and never use resume as a verification signal. The remaining
   decision is fix vs. delete: fix the restore path
   (`src/hv/snapshot.rs`, and the state it deliberately does not
   restore — see
   [`docs/SNAPSHOT_RESUME_CONTRACT.md`](docs/SNAPSHOT_RESUME_CONTRACT.md)),
   or remove the ring entirely (the feature gate makes deletion a
   contained change now).

3. **Guest serial port on real hardware.** On the emulated hosts the
   extr port flows through the `host-io-semihost` file pair and
   `scripts/serial-pty-bridge.py` (README "External serial port";
   verified end-to-end with UnixNPI). On the Pi the `serial-mux`
   feature shares the console PL011 with the kernel log by framing
   the guest bytes (`src/host/serial_mux.rs`, PL011 RX interrupt
   fed; `scripts/pi-upload.py --extr-pty / --ctl-fifo` on the host —
   `docs/REAL_HW_BRINGUP.md` "Guest serial over the console wire").
   Bench-verified 2026-09-01: UnixNPI installed `tdock.pkg` on the
   Pi Zero 2 W over the shared wire, pen taps injected through the
   control channel. Opt-in, not yet in the `pi-bare-metal-*`
   aggregates. A second
   *physical* port is still wanted for tools that must own a tty
   without the log; it cannot be the mini-UART (both on-chip UARTs
   reach the Zero 2 W header only on GPIO 14/15), so it means a USB
   CDC-ACM/FTDI adapter on the DWC2 host stack or an SPI/I²C UART
   bridge, installed on the same `peripherals::console` seam.

4. **PCMCIA card images.** Newton flash-card images map naturally onto
   files on the SD card through the existing `flash_persist` backend;
   not wired. The PCMCIA model currently reports empty slots.

5. **Targeted guest-TLB maintenance.** The hypervisor rewrites guest
   stage-1 PTEs behind the guest's back (`fix_stage1_xn_bits`, the
   scratch-pool L1 section install) with no TLBI at the rewrite sites;
   a blanket `tlbi vmalle1` on the 16 ms heartbeat
   (`hv::timer::on_irq`) currently bounds how long a stale entry can
   live. Replace it with targeted TLBIs at each rewrite site, then
   drop the blanket flush.

6. **Other ROM versions.** Only 717006 boots. `rom-710031` is a
   compiling skeleton (Tier-1 constants only, no ROM image checked in)
   that proves the `rom_ver` seam; filling it in — and re-verifying
   the stage-1 descriptor-format findings against 737041, localised
   variants and eMate ROMs — is open work.

7. **Performance and polish.** No measurement against the real
   162 MHz StrongARM has been done. Display-scaling quality on real
   hardware is the other polish item.

8. **FVP SWP divergence.** `test_swp_rom_aperture` gives NO-VERDICT
   on FVP: SWP takes the UND route there, and `hv/trap/und.rs`'s SWP
   emulation halts on a ROM target ("address not writable"), while
   the ROM-aperture absorb for SWP lives only in `hv/trap/dabt.rs`
   (the stage-2 route QEMU takes). The UND-path emulation needs the
   same mask-ROM aperture behaviour.

9. **Emulate the kernel's MMU-off access routines in EL2.** The three
   ROM routines behind every page-table access (`LoadFromPhysAddress`
   `0x18CA4`, `StoreToPhysAddress` `0x18CE0`, `Load`/
   `StorePhysicalByte` `0x18D1C`/`0x18D58`) could be emulated at
   their entry probes: perform the access through EL2's coherent
   mapping and return to `lr`. Per window that removes the two
   `SCTLR.M` traps, the `HCR_EL2.DC` toggle with its two
   `TLBI VMALLE1`s, and the rising-edge `fix_stage1_xn_bits` L1/L2
   walk (`HIGHLEVEL.md` §4.4) — a large saving at thousands of
   windows per second. Until then, §4.4's TLBI rule stands: the DC
   toggle without TLB maintenance corrupts guest memory.

10. **Video path — follow-ups.** The paint layers are cheap
    (`screen::blit` per-page walks + bulk copies, ~0.1 ms avg;
    `push_blit` 1:1 onto the VC-scaled surface, ~0.1-0.4 ms avg,
    coalesced at ~60 Hz) and the animation stall is fixed (the
    alignment-fault install path's rejected-PC bitmap; NewtTest
    open 0.87 s, Extras 0.37 s — the hunt is in
    `docs/project-history.md` §10). Attribution tooling: the
    `blit_timing` counters, the digitizer/serial-tap harness
    (`docs/REAL_HW_BRINGUP.md` "Serial pen injection"), and the
    trap-hist per-window masked-EL2-time line. What remains:
    - Alignment-fault storms are gone: spill-based inline stubs
      (scratches pushed/popped on the guest stack) cover the
      no-dead-scratch sites, so every site faults once, ever
      (hardware: 0-6 Align faults per 2 s window after warmup,
      ~655 stubs / 272 spill). The only stub-less form left is
      Rm == SP.
    - The remaining steady trap load is the domain-fault machinery
      (~7-10k faults/s: DACR write pairs at 0x3ad6f0/0x3adb08 + the
      0x800a08 native call + IntCtrl polling) — item 9 territory if
      it ever matters.
    - Portrait rotation is verified on hardware (`pi-fb-rot90` +
      `display_hdmi_rotate=1` + full `start.elf`/`gpu_mem=64`):
      direction is 90° CW as the touch map assumed, taps land,
      Newton spans all 1080 panel rows (rot90 drops the top-bar
      allowance — it lands at the panel bottom and dodges nothing),
      and the firmware's transposed physical-size readback both
      fixes the geometry and detects the mismatched-pair case.
      Details in `docs/REAL_HW_BRINGUP.md` "Portrait rotation".
    - Hires Newton geometry: implemented behind `pi-fb-hires`
      (540×960 on the rotated bench panel, exact ×2 HVS scale) and
      hardware-tested — the OS reflows fully, touch and store are
      fine — but DEFERRED over three ROM native-size quirks (boot
      logo off-center, trash-crumple erase bounded to y<480, Dates
      opens 480 tall). Findings + resume plan (Einstein oracle at
      540×960, then hunt the constants in rom.dis) in
      `docs/REAL_HW_BRINGUP.md` "Hires Newton geometry".
    - `SetFeature`(orientation) is real: the Extras Rotate button
      cycles all four `EOrientation` values (stored orientation,
      GetScreenInfo swap, rotated blits into the portrait
      GUEST_FB, inverse pen transform), hardware-verified through
      two full rotations. Note the MP2x00's native UI is
      landscape — the ROM asserts `SetFeature(4,1)` at UI start,
      so a fresh store now first boots landscape until rotated
      (the old accept-and-discard stub was silently vetoing it).
    - The 8 bpp paletted surface is in: guest scan-out surfaces
      allocate at 8 bpp with a shared palette (`SET_PALETTE` in
      mailbox.rs, gray ramp + color cube in `display/fb.rs`), loud
      32 bpp fallback. Hardware-verified; Extras-animation
      `push_blit` avg 679 → 312 µs.
    - Tear-free display: investigated and CLOSED as unreachable
      from this layer. Firmware `SET_VIRTUAL_OFFSET` pans latch
      mid-scan (every pan produces exactly one seamed frame at any
      rate, hardware-measured) and block 21–42 ms; `SET_VSYNC`
      blocks ~50 ms. Double-buffered flips would guarantee a seam
      per presentation — worse than the occasional paint-race
      tear. The workable mechanism is KMS-scale HVS ownership.
      Evidence + reproduction protocol in
      `docs/REAL_HW_BRINGUP.md` "Tearing".
    - Deferred, in likely-value order: DMA offload (host_dma.rs
      lacks `TI_DEST_INC`/`TI_TDMODE`; low value while the CPU
      format-converts), Normal-NC framebuffer remap (only if cache
      maintenance ever dominates again).

11. **HDMI audio CTS mis-derived on high-clock sinks.**
    `cts_pixel_clock_hz` treats any >=80 MHz pixel-clock readback as
    the known-bad PLL artifact and substitutes the 51.2 MHz panel
    constant — but a genuine 1080p sink (the capture digitizer)
    really runs 148.5 MHz, so audio CTS is computed from the wrong
    clock there. Needs a discriminator better than a threshold
    (e.g. compare against the mode geometry the firmware reports).

12. **Store ROM-identity check — done.** NewtonOS erases the internal
    store at boot ("a different ROM has been installed") when the
    ROM/REx checksums stored in the flash's reserved block differ
    from freshly computed ones, and the computation reads the
    patched ROM, so every build with a different in-ROM patch
    population wiped the store. The check is `TReservedBlockAccessor::
    CheckIfRecoveryIsNeeded` comparing `TROMREXCheckSums`
    (`docs/STRUCTURES.md` "Reserved-block calibration parameters");
    `rom_patches::apply_rom_rex_checksums_patch` now replaces
    `CalculateROMREXCheckSums(TROMREXCheckSums&)` with stores of the
    constant `rom_patches::STORE_ROM_IDENTITY`. The first boot after
    this change wipes once (stored sums → constant); bump the
    constant only when a wipe is genuinely wanted.

Real-hardware specifics (cores 1–3 left parked, snapshot ring deferred
on hardware, thermal re-verification) are tracked in
[`docs/REAL_HW_BRINGUP.md`](docs/REAL_HW_BRINGUP.md).

## Workflow per stop

1. Reproduce the stall on QEMU and capture the loud-halt context dump.
2. **Bitmap-first triage** when the wedge names a guest PC in ROM:
   check whether that address is marked as code in the classifier
   bitmap before digging into trap state (see `docs/DEBUGGING.md`). If it
   isn't, the fix is a classifier seeder, not a runtime handler.
3. Identify the kernel-side code at the wedge PC from
   `scripts/disasm-out/rom.dis`, and instrument the entry point with
   an HVC probe if more detail is needed.
4. Cross-reference Einstein as the oracle
   (`build/NewtonProbe baremetal/roms/newton.rom _Data_/Einstein.rex 30`).
5. Decide where the fix belongs — a hypervisor handler gap
   (`src/peripherals/*`, `src/hv/trap/`), an Einstein behavioural
   quirk to port, or, only when no other layer can host it, a ROM patch
   in `src/newton/rom_patches.rs`.
6. Re-run, confirm the wedge is gone, repeat.

## Tools

### Hosts

- **QEMU raspi3b** (default; `cargo run --release`) — fast; banking
  quirks in `docs/QEMU_BUGS.md`.
- **ARM FVP `FVP_Base_RevC-2xAEMvA`** — `scripts/fvp <elf>`. GICv3,
  accurate timer + cache model. Build with `--no-default-features
  --features "platform-fvp-base rom-717006 quiet diag"`.
- **Pi Zero 2 W** — first card: `PI_CARGO_FEATURES=pi-bare-metal-input
  scripts/build-sd.sh <dest>`; every rebuild after that:
  `scripts/pi-upload.py --kernel <elf> --until 'Welcome to
  NewtonScript' --timeout 120` (power-cycles the board through the
  `Pi Off`/`Pi On` Shortcuts, sends a delta of the image over the
  USB-TTL cable to the nhboot bootloader, captures the console);
  `--no-upload` is power-cycle + capture. See `docs/REAL_HW_BRINGUP.md`,
  "Serial image upload".

### Trace and observation

- **Function tracer** — `--features trace[_once],quiet`: an HVC
  trampoline on every entry in `scripts/classify-out/code-symbols.txt`.
- **`scripts/trace-diff.sh`** — diff Einstein vs hypervisor traces.
- **`build/NewtonProbe`** — Einstein as oracle;
  `probe/FINDINGS.md` is the captured golden record.
- **Tarmac on FVP** — `scripts/fvp --tarmac=<file>`.
- **Trap histograms / task dumps** — `src/diag/`, feature `diag`.

### Debugging

- **gdb on QEMU** — `DEBUG=1 cargo run --release` (term 1) +
  `aarch64-elf-gdb -x scripts/gdb-init <elf>` (term 2). Helpers
  `bg <addr>`, `bp <addr>`, `tt N`, `guest-state`.
- **DABT/PABT DIAG HVCs** at ROM offsets `0x10` / `0x0C`.
- **Loud-halt canaries** on `BootOS` / `PowerOffAndReboot` / `Reboot`
  and the bus-error throw, gated on `cfg(nh_loud_halt_canaries)` so a
  user reset on real hardware doesn't halt the hypervisor.

### Live display and pen input

`src/host/host_io/` forwards each `screen::blit` to
`tools/host-viewer/` through `/tmp/newton-host-io/` (semihosting
files); the viewer posts mouse events back as pen samples. Enable with
`--features host-io-semihost`.

## Critical files

- `src/newton/loader.rs` — ROM load, selective byteswap, CP15-encoding
  rewrite; `src/newton/os.rs` — `fix_stage1_xn_bits` and the MMU-enable
  ritual.
- `src/newton/rom_patches.rs` / `probes.rs` — the patch table, the
  unified installer, probe handler bodies.
- `src/newton/guest_trampolines.rs` — UND/DABT vector trampolines.
- `src/newton/inline_patch.rs` + `unaligned[_inline].rs` — stub pool,
  liveness walker, SA-1100 rotate-LDR emulation.
- `src/hv/layout.rs` — the region + MMIO-window manifest.
- `src/hv/trap/` — `mod.rs` (dispatch + IRQ), `dabt.rs`, `und.rs`,
  `cp15.rs`, `hvc.rs`.
- `src/hv/stage2.rs` — stage-2 tables and the RW+XN ↔ RO+X page state
  machine; `src/hv/guest.rs` — `HCR_EL2` / `CPTR_EL2` setup.
- `src/arch/banked.rs` — AArch32 banked registers from EL2
  (ARM ARM Table D1-79).
- `src/peripherals/*` — Newton device models.
- `src/host/{sd,usb,input,audio,display,host_dma}` — real-hardware
  stacks; `src/host/flash_persist/` — SD-backed flash with DMA
  autosave.
- `guest-tests/tests/` — 39 tests; `guest-tests/scripts/run-all.sh`.

## Verification

```
guest-tests/scripts/run-all.sh                 # 39 tests, QEMU
guest-tests/scripts/run-all.sh --platform fvp  # same on FVP
CHECK_MATRIX=1 guest-tests/scripts/run-all.sh  # + the 18-combo build matrix
scripts/boot-check.sh --cold                   # ROM boot to the Welcome UI
```

## Non-goals

Multi-ROM switching at runtime, JIT or software CPU emulation, Pi 4/5
support, Einstein's UI layer, running under Linux.
