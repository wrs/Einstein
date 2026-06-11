# Newton Hypervisor — Big-Picture Architecture Review

> Review agent report, 2026-06-11, at working copy `somv 8b564c93`.
> Scope: structure, boundaries, feature architecture, state, docs-vs-reality,
> follow-on readiness. Line-level findings are deliberately excluded (covered
> by reports 01–05).

## 1. Module boundaries and layering

**The intended layering is real and mostly holds.** The de facto layers, as built:

1. **Boot/bring-up** — `boot.s`, `vectors.s`, `cpu.rs`, `mmu.rs`, `main.rs`, `platform/` (raspi3b/fvp_base behind a `#[path]`-swapped `imp`).
2. **EL2 core** — `trap.rs`, `mmio.rs`, `stage2.rs`, `guest.rs`, `guest_mem.rs`, `guest_endian.rs`, `banked.rs`, `timer.rs`, `hvc_imm.rs`.
3. **Guest-ISA compensation** — `shadow_stub.rs`, `shadow_pool.rs`, `unaligned*.rs`, `rom_patches.rs`.
4. **Modelled Newton peripherals** — `src/peripherals/*` (Einstein ports).
5. **Host backends** — `host_io/`, `flash_persist/`, `input/`, `audio/`, `display/`, `sd/`, `usb/`, `mailbox.rs`.
6. **Debug tooling** — `snapshot.rs`, `tracer.rs`, `guest_bp.rs`, `task_dump.rs`, `trap_hist.rs`, `heap_check.rs`.

The discipline at the peripheral boundary is notably good: every `peripherals/*` module imports only `trap::TrapContext` (a plain register-frame type), `cpu`, `kprintln`, and sibling peripherals — none reach into dispatch internals. `mmio.rs` is a clean IPA router. `hvc_imm.rs` is exactly the right artifact: a single `#[repr(u32)]` enum making patcher/dispatcher HVC collisions a compile error.

**Concrete coupling problems, worst first:**

- **`trap.rs` as hub.** It imports `peripherals`, `mmio`, `guest_mem`, `guest_endian`, `stage2`, `timer`, `snapshot`, `host_io`, `input`, `display`, `task_dump`, `heap_check`, `trap_hist`, `tracer`, `guest_bp` — and is imported back (for `TrapContext`) by `peripherals/*`, `banked.rs`, `unaligned.rs`, `tracer.rs`, `guest_bp.rs`, `snapshot.rs`. The dependency graph is hub-and-spoke with a 4,760-line hub; the type-level cycles (`trap ↔ peripherals`, `trap ↔ snapshot`) exist only because `TrapContext` lives inside the dispatcher.
- **`uart.rs` ↔ `peripherals` triangle.** `uart.rs` (host console) depends on `peripherals::host_dma` for DMA TX; `peripherals/dma.rs` (the *guest* DMA model) depends on `uart` to feed PL011 RX into the guest's extr serial port. Host console, host DMA engine, and guest DMA model are three different layers sharing one corner.
- **`peripherals/host_dma.rs` is misfiled.** It is a host-side BCM2835 DMA driver (UART TX, MAI audio, SD channels) — a peer of `sd/`, `mailbox.rs`, `display/` — living in the directory whose contract is "modelled Newton hardware." Anyone reasoning "peripherals/ = guest-visible" gets burned here.
- **`cpu::with_irqs_unmasked` ↔ `trap::irq_from_el2`** — a real cross-module invariant ("f must not touch any state the slim ISR touches") enforced entirely by doc comments in two files. Excellent comments, zero mechanical enforcement.
- **`rom_patches.rs` ↔ `trap.rs` pairing.** Each probe is a patch site in one file plus a dispatch arm plus a handler body in the other. `hvc_imm.rs` solves the immediate-collision half; the handler bodies still accrete in `trap.rs` rather than co-locating with the patches they serve.

The graph is acyclic at the *function-call* level (peripherals never call back into dispatch); the cycles are all through the `TrapContext` type and `kprintln`. That's fixable cheaply (see §2).

## 2. The trap.rs problem

Yes — `trap.rs` (4,760 lines, 211 KB) is absorbing at least five distinct responsibilities:

1. **Dispatch** — `trap_sync_lower_aarch32`, `trap_irq`, `irq_from_guest`/`irq_from_el2`, `update_virq` (~600 lines).
2. **Instruction emulation** — SWP, FPA control-reg, ISV=0 DABT decode, `arm_shift`, condition-code evaluation, CP15 shim (~1,500 lines).
3. **Guest-VA plumbing** — `guest_translate_va`, `resolve_guest_pa`, `read_cstr_at`, `scan_to_null_word_aligned` — these are stage-1-walk utilities that belong with `guest_mem`/`guest_endian`, not the dispatcher.
4. **ROM-probe handlers** — `handle_hammer_*`, `handle_store_perm_obj_entry_probe`, `handle_bootos_canary`, `handle_dah_mrs_spsr_patch` — the receiving end of `rom_patches.rs` (~900 lines).
5. **Diagnostics with behavior** — UND history ring, heartbeat sampling, budgeted loggers, `dump_tstacks_and_check_invariants`, and the **wedge-probe** (see below) (~1,000+ lines).

**Concrete decomposition** (preserving the existing handler-table style, no traits needed):

| New module | Contents | Approx. |
|---|---|---|
| `trap/context.rs` | `TrapContext`, `advance_elr`, `read_sysreg!`, `describe_ec`, mode-label helpers | 150 |
| `trap/dispatch.rs` | sync entry, `trap_irq`, the two IRQ bodies, `update_virq`, an explicit ordered *trap-exit hook list* | 400 |
| `trap/dabt.rs` | `handle_data_abort`, `resolve_ipa`, ISV=0 emulation, ROM-write absorb, flash-write drop, `handle_dabt_dispatch` forwarding | 800 |
| `trap/cp15.rs` | `handle_cp15_trap` + SCTLR/TLBI/cache logging + `halt_unknown_cp15` | 600 |
| `trap/und.rs` (+ `fpa.rs`) | `handle_und`, UND history, FPA emulate/log, SWP emulate | 900 |
| `trap/hvc.rs` | `handle_hvc` tag match only | 300 |
| `probes.rs` (next to `rom_patches.rs`) | Hammer*/StorePermObj/canary/DAH handler bodies | 700 |
| `guest_mem` (merge) | `guest_translate_va`, `resolve_guest_pa`, string readers | 200 |
| `diag.rs` | heartbeat, beacon, tripwires, tstack invariant dump, loud-halt rendering | 700 |

The highest-leverage single move is `trap/context.rs`: it breaks every type-level cycle (`peripherals`, `banked`, `unaligned`, `tracer`, `guest_bp`, `snapshot` all stop importing the dispatcher) and makes the layering visible in the import graph. The second is `dispatch.rs` with an explicit exit-hook sequence — today the tail of `trap_sync_lower_aarch32` (pump input → update_virq → tick_page → beacon) and of `irq_from_guest` (DMA completions → heartbeat → task_dump → heap_check → tripwires → timer → pumps → virq → splash → autosave → histogram) are two hand-maintained, subtly different copies of "things that happen per trap exit."

**One probe is not a diagnostic.** The "wedge-probe" in `irq_from_guest` injects `INT_DMA_CH3|CH5` sound-completion IRQs whenever the guest PC parks for 64 heartbeats with sound IRQs armed. On `audio-null` builds — the QEMU/FVP default — nothing else ever raises the sound output-completion interrupt (`audio/mod.rs` forwards `schedule_output`/`set_interrupt_mask` to `pi_hdmi` only; `null.rs` is a stub). So a Phase-B hypothesis test is plausibly the de facto sound-completion model for the two primary dev platforms, dressed as a debug heuristic, firing on a heuristic schedule. This is the single clearest case of logic absorbed into `trap.rs` that belongs in a subsystem (the audio seam).

## 3. Feature-flag architecture

**The axis design is the strongest piece of architecture in the repo.** `build.rs` resolves each backend axis (`host-io-*`, `flash-persist-*`, `input-*`, `audio-*`) into exactly one `cfg(nh_<axis>_<choice>)`, with null fallbacks, mutual-exclusion panics, and a guest-test override forcing hermetic flash. Source never reads the raw features; each `mod.rs` concentrates the cfg switch and exposes plain functions (and `flash_persist` has a proper `FlashStore` trait). Aggregates (`pi-bare-metal-input` etc.) pin the real-world combinations. This is textbook.

**Two weaknesses:**

- **"Real hardware" is an emergent condition, not a named one.** `all(feature = "no-semihost", feature = "platform-raspi3b")` appears in `trap.rs`, `uart.rs`, `sd/sdhost.rs` (×10), `input/mod.rs`, `audio/mod.rs`, `flash_persist/mod.rs`, `peripherals/host_dma.rs` — the one cfg combination that *is* scattered through logic rather than behind a seam, including inside the hot IRQ path (`trap_irq`'s USB fast path, `irq_from_guest`'s DMA-completion dispatch). A build.rs-emitted `cfg(nh_real_hw)` would name the concept once; better still, the BCM2835-pending dispatch belongs in `platform::` (which already abstracts `irq_ack`/`irq_eoi`).
- **The matrix is broader than its test coverage.** `run-all.sh` exercises 2 platforms × default backends; the four `pi-bare-metal*` aggregates, `trace`, `ns_trace`, and the probe binaries are only validated when someone happens to build them. For a stabilization phase, an automated `cargo check` sweep over the supported combinations is cheap insurance against the classic cfg-rot failure (a refactor that compiles on default but breaks `pi-bare-metal-input`, discovered at the next SD-card build).

`no-semihost` itself is slightly overloaded — it simultaneously means "no host filesystem" (snapshot off), "no SYS_TIME" (calendar epoch), "PL011 console", and "real silicon." Snapshotting is effectively a fifth backend axis (semihost-only) that isn't expressed as one; that's tolerable, but it's why the flag has to appear in 11 files.

## 4. State management

The pattern is consistent and honest for a single-core no_std EL2: **per-module `static mut` (or `UnsafeCell` wrappers like `VIC`) + `// SAFETY: single-threaded` + atomics where the IRQ path genuinely races** (`WAKE_REQUEST`, autosave gates, log budgets). `trap.rs` alone holds ~47 statics; `peripherals/vic.rs`, `pcmcia.rs`, `tracer.rs` are the other concentrations. The one real concurrency boundary — the nested `irq_from_el2` slim ISR vs. everything else — is governed by an explicitly documented state-ownership contract (the numbered list in its doc comment, mirrored in `cpu::with_irqs_unmasked`). That contract is correct-by-discipline only; nothing stops a future handler called under `with_irqs_unmasked` from touching the UART ring or SDHOST state. Given stabilization is the goal, this is the place where one targeted abstraction (a marker type or a single module owning the "slim-ISR-touchable" state) would buy the most safety per line changed.

**Snapshot/global relationship: principled at the memory level, implicit at the peripheral level.** What's saved is crisply defined (`Header` in `snapshot.rs`: GPRs incl. banked aliases, EL1 sysregs, SPSRs, RAM, FB; flash by fingerprint, delegated to `flash_persist`; ROM by fingerprint). What's *not* saved is equally crisp in docs — "VIC state, timer deadlines fresh each boot" — but the consequence is an unstated per-module requirement: **every peripheral state machine must tolerate being reset to power-on defaults underneath a mid-flight guest** (VIC `int_ctrl`/`int_present` are guest-programmed state that silently vanishes on resume; tablet queue, DMA channel state, sound masks likewise). It works empirically, and `host_io::on_resume()` shows the right shape (an explicit resume hook), but it's the only such hook. As peripherals accumulate state (PCMCIA images, serial), each addition silently re-answers "is this resume-safe?" with nobody asking the question. A documented (or trait-encoded) resume contract per peripheral module would make the snapshot/global relationship principled end to end.

The flash-vs-snapshot split (`flash_persist/mod.rs`'s "Why a separate file from snapshots?") is a genuinely good piece of state architecture — user data outliving debug-state invalidation, with cross-fingerprints to detect divergence.

## 5. Docs vs. reality

Generally excellent — `README.md`, `CLAUDE.md`, `PLAN.md` and the build.rs/mod.rs comments are accurate and current. Specific staleness, mostly in `HIGHLEVEL.md`:

- **HIGHLEVEL §5.4 directly contradicts the build.** "AP bits, domains, and cacheability attributes are preserved unchanged. No software shadow table, no AP flattening" — versus `fix_stage1_xn_bits` flattening subpage-AP to AP=011, the verify-mmu alias detector, `shadow_pool.rs`, and `shadow_stub.rs`, all of which README/PLAN present as load-bearing. This is the most misleading stale claim because it describes the *hardest-won* subsystem as nonexistent.
- **HIGHLEVEL header + §3/§8 still say peripherals are "reused, reglued" Einstein C++ classes.** IMPLEMENTATION §1.2 documents the actual decision (pure-Rust ports; the `cxx-core` linking attempt abandoned). HIGHLEVEL should say "ported," not "reused."
- **HIGHLEVEL "Status: draft"** — it is the architecture record of a shipped system.
- **Cargo.toml's `trace` feature comment describes the previous mechanism** (UDF #index overwrite, first-touch-once-per-boot) — the implementation is the 5-word HVC trampoline firing on every call (`tracer.rs`, CLAUDE.md, README all agree). Anyone tuning trace from the manifest comment will mispredict behavior.
- **Dangling INVESTIGATION.md references.** PLAN says the Phase-B diary is archived and the file is gone, but `trap.rs` comments (newt-tripwire, tick-page rationale) and the auto-memory file still cite it.
- **PLAN's "Debug-scaffolding teardown — done" is overstated.** Its "Diagnostic scaffolding (active)" list names verify-mmu, DIAG vectors, and canaries — but the heartbeat still carries the newt-tripwire (one-shot watch of a hardcoded PA from a finished investigation) and the wedge-probe, which *changes guest behavior* (§2). Neither is on the declared survivors list.
- IMPLEMENTATION §2.2's crate table (`aarch64-cpu`, `tock-registers`, `bitflags`) was never adopted — everything is hand-rolled; the table is labeled "candidate set" so this is minor, but a one-line "we hand-rolled instead" would close it.

## 6. Readiness for follow-on work

**Sound (QEMU/FVP parity + remaining polish): structure supports it, one hack fights it.** The `audio-*` seam, the `host_dma` MAI ring, and the documented sound-driver contract in `audio/mod.rs` are the right shape; a `host-io`-style dev backend for QEMU/FVP would slot in without touching dispatch. The blocker is that null-build completion semantics currently live in the wedge-probe heuristic (§2) — any serious sound work on the emulators starts by moving "raise the output mask after a buffer's worth of virtual time" into the audio seam (a paced `audio::null` completer), then deleting the probe.

**Serial + PCMCIA images (Phase 6 remainder): supported.** `serial.rs`/`serial_driver.rs`/`dma.rs` already route PL011↔extr-port; PCMCIA images want exactly the `flash_persist` pattern (a `pcmcia-image-*` axis with an SD/FAT backend), and that pattern is proven three times over.

**App packages: the structure mostly supports it, with one architectural assumption to watch.** Package installation is guest-side (soups → flash store → package loader), and the flash + persistence stack is solid. The risk concentrates in *native code inside packages*: the BE-8 byteswap architecture classifies code-vs-data **at ROM load time** via the build-time `reach.bitmap`, and the tracer/classifier toolchain assumes all code lives below `0x0100_0000`. Package native code arrives at runtime into RAM/flash and is reachable only through the dynamic path (stage-2 RO+X ↔ RW+XN flipping with rescan-on-fetch in `stage2.rs`/`shadow_stub.rs`). That path exists — kernel demand-paged code already exercises it — but packages will stress it with code the classifier has never seen, and the "bitmap-first triage" debugging doctrine in CLAUDE.md silently stops applying to those PCs. Worth a short design note before starting: which invariants of `shadow_stub`'s "real code" definition extend to package code, and what the triage recipe is when a wedge PC is above the ROM aperture. Also: snapshot's ROM fingerprint covers only ROM; installed packages living in flash are covered by the flash fingerprint, so the snapshot debug loop should survive package work unchanged — good.

---

## Top 5 architectural improvements (prioritized)

1. **Decompose `trap.rs`, starting with `trap/context.rs`** (extract `TrapContext` + `advance_elr` + `read_sysreg!`), then split by EC class per the table in §2, moving probe handlers next to `rom_patches.rs` and VA-walk utilities into `guest_mem`. *Effort: large (mechanical, low-risk if done as moves; guest tests cover the handler surface).* Unblocks: parallel subsystem work without a 4,800-line merge hub, reviewable DABT/UND paths for package debugging, and an import graph that actually shows the layering.

2. **Move null-audio completion into the audio seam and retire the wedge-probe + newt-tripwire.** Give `audio::null` a paced "buffer drained → raise output mask" completer driven from the timer tick; delete the stuck-PC injection from `irq_from_guest`. *Effort: small.* Unblocks: principled sound behavior on the two primary dev platforms, removes the last behavior-mutating Phase-B relic, and is the prerequisite for any sound follow-on.

3. **Name the real-hardware configuration and re-home host drivers.** Emit `cfg(nh_real_hw)` from build.rs to replace the scattered `all(no-semihost, platform-raspi3b)`; move `peripherals/host_dma.rs` out of `peripherals/` (a `src/host/` or alongside `sd/`/`mailbox`); push the BCM2835 pending-register dispatch in `trap_irq`/`irq_from_guest` behind `platform::`. *Effort: medium.* Unblocks: a coherent host-backend layer, an IRQ path free of platform cfg blocks, and cheaper future platform/board work.

4. **Make the snapshot resume contract explicit per peripheral.** Document (or encode as an optional `on_resume()` hook, as `host_io` already has) which module statics are guest-visible-but-deliberately-not-saved and why reset-on-resume is safe for each. *Effort: small.* Unblocks: keeping the snapshot debug loop — the project's core iteration tool — trustworthy as PCMCIA/serial/package state is added.

5. **Automate the feature-matrix build check.** A script (wired into `run-all.sh` or pre-commit) that `cargo check`s the supported combinations: default, `platform-fvp-base`, all four `pi-bare-metal*` aggregates, `trace,quiet`, `host-io-semihost`. *Effort: small.* Unblocks: stabilization confidence that the excellent axis architecture (§3) stays green off the default path — today only the default and FVP builds are continuously proven.

(Honorable mention, near-zero effort: the doc corrections in §5 — HIGHLEVEL §5.4/§8, the Cargo.toml `trace` comment, and the dangling INVESTIGATION.md references — are worth batching into one cleanup commit, since HIGHLEVEL is the document new contributors are told to read first.)
