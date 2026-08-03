# Snapshot resume contract

What a snapshot save/load round-trip restores, and — more importantly
— which guest-visible state it deliberately does **not** restore, with
the reason reset-on-resume is safe for each. The field-level layout is
documented in `src/hv/snapshot.rs`.

**Caveat:** this describes the intended contract. Resuming a
*Newton-ROM* snapshot currently wedges the guest in a prefetch-abort
loop at the vector page; only the two-run `test_snapshot_resume` guest
test resumes correctly. Cold-boot every run whose result you intend to
trust (`rm -f /tmp/newton-snapshot-*.bin`). Fixing or removing the
resume path is tracked in [`../PLAN.md`](../PLAN.md).

## The ring

`src/hv/snapshot.rs` rolls four slots at
`/tmp/newton-snapshot-{0..3}.bin`. On startup the loader picks the file
with the highest `seq`; missing or mismatched files fall through to a
cold boot. Copying an older file over a newer slot carries its own seq
with it, so the older state wins — useful once resume works again.

Two save triggers:

- **Periodic (default):** every `AUTOSAVE_INTERVAL_MS = 2000` ms of wall
  time, hooked into `trap_irq` (timer IRQ) in `src/hv/trap/mod.rs`.
  Wall-clock pacing, not trap count, so a pathological abort loop won't
  thrash saves. A guest that never takes a timer IRQ produces no fresh
  snapshots; in practice the Newton kernel arms its match registers very
  early and CNTHP fires steadily.
- **Guest-triggered:** `HVC #0x18` (`HvcImm::Snapshot`) saves
  immediately — handy for a guest test that wants to snapshot at a
  specific PC.

Two fingerprints in the header reject a mismatched resume: an FNV-1a
hash of the first 1 KiB of `GUEST_ROM` after load-time patches (so
swapping a guest-test binary for the ROM, or shifting early ROM bytes
via an Einstein.rex change, cold-boots instead of ERET-ing into someone
else's code), and one of `GUEST_FLASH`. Features that mutate ROM words
— `trace`, `log_store`, `ns_trace` — change the ROM fingerprint, so
toggling them forces a cold boot.

Each save is ~6 MiB (4 MiB RAM + 2 MiB FB + 384 KiB SCRATCH_POOL +
header) through semihosting `SYS_WRITE`. Fast enough at a 2 s cadence,
but it would become painful if the cadence tightened.

## What is saved

`src/hv/snapshot.rs` serializes, per slot:

- **Three memory regions**, in the order the region manifest
  (`src/hv/layout.rs`) lists them as snapshotted: `GUEST_RAM`
  (4 MiB), `GUEST_FB` (2 MiB), and `inline_patch::SCRATCH_POOL`
  (384 KiB at IPA `0x0600_0000`).
- **Guest CPU state**: all 31 AArch64 GPRs (`x0..x30`, which alias every
  AArch32 banked `R0..R14` per ARM ARM Table D1-79), `ELR_EL2` /
  `SPSR_EL2` (the resume PC / CPSR), and the per-mode banked SPSRs.
- **EL1 / guest sysregs reachable from EL2**: `SCTLR_EL1`,
  `TTBR0/1_EL1`, `TCR_EL1`, `DACR32_EL2`, `VBAR_EL1`, `CPACR_EL1`,
  `MAIR_EL1`; the AArch32 fault-register homes (`FAR_EL1` = DFAR,
  `ESR_EL1` = DFSR, `IFSR32_EL2`); and the TLS scratch (`TPIDR_EL0` /
  `TPIDRRO_EL0`) the trampoline stubs stash through.

Persistent flash is **not** in the file — it lives in
`$HOME/.newton/flash.bin` (`src/host/flash_persist/`). The header carries an
FNV-1a fingerprint of `GUEST_FLASH` at save time; on resume a mismatch
forces a cold boot rather than risk resuming CPU state against diverged
flash.

## What is NOT saved (reset-on-resume), and why that is safe

The hypervisor's peripheral models hold guest-visible MMIO register
state in module statics. None of it is in the snapshot; on resume each
model starts from its `Default`/`new` value and the guest re-drives it.
The save path makes this safe by construction:

**The save is gated to a stable, between-transactions moment.**
`maybe_autosave` only writes a slot when:

1. the IRQ that woke EL2 came from the AArch32 guest (`SPSR_EL2.M[4]==1`),
   not from a nested EL2 timer IRQ; and
2. the guest `ELR_EL2` is **not** inside a hypervisor-owned trampoline /
   stub (`pc_in_hypervisor_transient_region`, delegating to
   `guest_trampolines::is_hypervisor_code_region`); and
3. no guest software breakpoint is installed (`guest_bp::any_installed`).

So a saved PC is always a guest instruction boundary reached via a timer
IRQ — never mid-MMIO-emulation, never mid-stub. The peripheral models
are only ever mutated synchronously inside a trap handler that runs to
completion before the guest is re-entered, so at the save point no model
is "half-updated". What remains is whether the *steady-state* register
contents matter across the gap. Per peripheral:

- **VIC** (`peripherals/vic.rs`, `VicState`): `int_present`, `int_ctrl`,
  `fiq_mask`, `int_ed_*`, `match_reg[]`, `match_fired`, `alarm_reg`,
  `alarm_fired`, `gpio_*`. Reset to zero on resume. **Safe** because the
  Newton kernel reinstalls the entire interrupt-controller configuration
  early and continuously: match registers are re-armed on every timer
  service, DACR/`int_ctrl` are rewritten at each context switch, and the
  edge-detect latches (`match_fired`, `alarm_fired`) exist only to
  suppress a re-raise within one already-serviced tick. A fresh latch at
  resume can at worst cause one extra timer IRQ on the first post-resume
  tick, which the kernel handles idempotently. `TICK_EPOCH` /
  `CALENDAR_*` re-seed from `CNTPCT` at init, so wall-clock continuity is
  re-established, not carried.

- **Serial DMA** (`peripherals/dma.rs`, `DmaState` / `ChannelState`):
  `assign`, per-channel `data_ptr`, `countdown`, `buf_size`, `control`,
  `event`, `armed`. Reset on resume. **Safe** for the boot/idle states
  the snapshot workflow targets: the snapshot is a developer tool for
  re-reaching a boot stall, and at the gated save points there is no
  in-flight host-fed serial DMA transfer whose mid-transfer cursor would
  need to survive. *Caveat (filed, not hand-waved):* if a snapshot were
  ever taken with a guest-armed TX/RX DMA mid-ring (non-zero `countdown`
  with `armed==true`), resuming would drop the remaining transfer and
  the guest would wait on a completion IRQ that never comes. This has not
  been observed because the workflow saves during boot, where serial DMA
  is quiescent — but a snapshot taken during active serial traffic is
  outside the contract.

- **Tablet** (`peripherals/tablet.rs`, `TabletState`): the pen-sample
  queue and digitizer register shadows. Reset on resume. **Safe**: the
  queue holds host-injected pen events that have no meaning across a
  rebuild (the host viewer / MTouch driver is re-initialized fresh), and
  the kernel re-reads the digitizer through `NativeGetSample` rather than
  caching it.

- **Sound model** (`peripherals/sound.rs`): the `SUBFN_COUNT` / `SEEN`
  diagnostic counters. Reset on resume. **Safe**: these are
  log-budget/observability counters, not guest-visible register state.

- **Null-audio completion** (`audio/null.rs`):
  `OUTPUT_INT_MASK`, `OUTPUT_RUNNING`, `PENDING_EDGES`, `NEXT_DEADLINE`,
  `LAST_DURATION_TICKS`. Reset on resume. **Safe with a one-tick
  caveat**: this is the state that arms a sound-DMA completion IRQ a
  buffer-duration after `schedule_output`. If a snapshot is taken in the
  window between `schedule_output` and the paced completion, the armed
  completion is lost on resume and the guest's sound code would wait for
  a buffer-done IRQ that was dropped. In practice the boot chime's
  buffers complete sub-second and the save cadence is ~2 s, so a resume
  almost never lands inside an armed-but-unfired window; and even when it
  does, the kernel's sound path tolerates a missed completion on the
  boot chime (it does not gate further boot progress on it). Re-arming
  on resume is **not** implemented — filed here as the one place where
  reset-on-resume is lossy rather than provably transparent.

- **PCMCIA** (`peripherals/pcmcia.rs`): the controller-register storage
  that backs chip-detect. Reset on resume. **Safe**: with no card
  present the kernel re-probes the controller (`TCardSocket::GetChipInfo`
  writes its own sentinels and reads them back); the storage exists only
  to make that round-trip succeed, and a freshly-zeroed store satisfies
  the very first write-then-read the probe performs.

## Summary

Everything the guest can observe *and depends on across the gap* is in
the snapshot: RAM, FB, the SCRATCH_POOL the trampolines stash through,
and the CPU/MMU/fault sysregs. The peripheral-model statics are reset
because the save is gated to a between-transactions guest IRQ boundary
where their steady-state contents are either (a) re-driven by the kernel
immediately (VIC, PCMCIA), (b) meaningless across a host re-init
(tablet, diagnostic counters), or (c) quiescent in the boot/idle states
the workflow targets (serial DMA). The two lossy edges — a snapshot
taken mid serial-DMA-ring, or inside an armed-but-unfired null-audio
completion window — are documented above as out-of-contract rather than
silently assumed away.
