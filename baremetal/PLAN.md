# Plan — current state and remaining work

## State

The 717006 ROM boots through kernel, scheduler and NewtonScript
interpreter to the Welcome UI, and the builtin apps work
interactively — on QEMU `raspi3b`, on ARM FVP, and on a real
Pi Zero 2 W with HDMI display, USB touch, HDMI audio and SD-backed
flash persistence. All 38 guest tests are green on both emulated
hosts; all 18 build combinations in `scripts/check-matrix.sh` pass.

## Standing rules

- Run the *original ROM code*. No workarounds, no shortcuts. ROM
  patches are the last resort, only when no other layer can host the
  fix.
- No shadow page tables and no per-access AP emulation: guest stage-1
  incompatibilities are resolved by normalising the guest's own
  descriptors in place (`HIGHLEVEL.md` §4.3).
- Every commit that touches hypervisor functionality (not merely
  diagnostics) must pass `guest-tests/scripts/run-all.sh`, all 38
  tests. Fix warnings before committing.
- Unknown inputs on emulation paths halt loudly with a context dump.
  Never add a silent default to quiet a halt — the halt is the
  trip-wire that says which table entry to extend.

## Remaining work

1. **Add-on app packages.** The main functional gap. The `.pkg`
   installation flow — soup storage, package loader, and native code
   inside packages — is unexercised. Needs an investigation pass:
   install a known-simple package, see where it stops, fix, repeat.
   The design note for the native-code half (which `inline_patch`
   "real code" invariants extend above the ROM aperture, what the
   stage-2 RW+XN ↔ RO+X rescan guarantees, how to triage a wedge PC in
   RAM) is [`docs/PACKAGE_NATIVE_CODE.md`](docs/PACKAGE_NATIVE_CODE.md).

2. **Snapshot resume — fix or remove.** Saving works, and the two-run
   `test_snapshot_resume` guest test resumes correctly; resuming the
   *Newton ROM* does not. The resumed guest ERETs to the saved PC and
   immediately wedges in a prefetch-abort loop at the vector page
   (`ELR = IFAR = 0xc`, ABT mode), after which the 2 s autosave
   overwrites all four slots with the wedged state within ~8 s.
   Until this is resolved, cold-boot for every run and do not use
   resume as a verification signal. Decide between fixing the restore
   path (`src/hv/snapshot.rs`, and the state it deliberately does not
   restore — see
   [`docs/SNAPSHOT_RESUME_CONTRACT.md`](docs/SNAPSHOT_RESUME_CONTRACT.md))
   and deleting the mechanism.

3. **Guest serial port on real hardware.** PL011 carries the kernel
   log; the guest's own serial port needs a separate host-side
   sink/source. Channels 0/1 already route through the guest DMA model
   on the emulated hosts.

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

8. **ROM-blob alignment for the serial loader.** The delta upload
   sends only changed bytes, but a rebuild that grows the code before
   the embedded ROM + REx blob shifts the blob inside `HYPERV.IMG`,
   so nhboot rewrites most of the file's sectors (~15 s of PIO). A
   link-time alignment of the blob (own section, 64 KiB alignment in
   `linker.ld.in`) would keep it at a stable file offset and make the
   persist step as small as the delta.

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
- `guest-tests/tests/` — 38 tests; `guest-tests/scripts/run-all.sh`.

## Verification

```
guest-tests/scripts/run-all.sh                 # 38 tests, QEMU
guest-tests/scripts/run-all.sh --platform fvp  # same on FVP
CHECK_MATRIX=1 guest-tests/scripts/run-all.sh  # + the 18-combo build matrix
scripts/boot-check.sh --cold                   # ROM boot to the Welcome UI
```

## Non-goals

Multi-ROM switching at runtime, JIT or software CPU emulation, Pi 4/5
support, Einstein's UI layer, running under Linux.
