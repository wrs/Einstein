# Newton Hypervisor — Diagnostic Subsystem Review (stabilization pass)

> Review agent report, 2026-06-11, at working copy `somv 8b564c93`.
> Scope: `src/tracer.rs`, `src/task_dump.rs`, `src/heap_check.rs`,
> `src/rep_print.rs`, `src/tarmac.rs`, `src/symbols.rs`, `src/pi_probe.rs`,
> `src/usb_probe.rs`, `src/panic.rs`, `src/sd/probe.rs`, plus their call sites
> in `src/trap.rs`, `guest-tests/`, and `scripts/`.

## Findings

### High

**H1. The "wedge probe" sound-DMA IRQ injection is load-bearing, not a diagnostic — and it's a heuristic.**
`src/trap.rs:385-422` (`trap_irq`) + `src/peripherals/vic.rs:277` (`inject_sound_dma_irq`). The block is commented as a Phase-B *hypothesis test* ("if the kernel resumes forward progress after injection, we know the gating factor; we can then move the injection to a more targeted path"). That move never happened. With the default `audio-null` backend (QEMU/FVP and any Pi build without `audio-pi-hdmi`), `audio::schedule_output` is a no-op (`src/audio/mod.rs:90-93`, `src/audio/null.rs`) and **nothing else ever raises the sound-DMA completion IRQ** — the only completion path is this wedge detector firing after the guest PC parks for 64 heartbeats (~1 s) with sound IRQs armed. So sound playback on null-audio builds completes only via a parked-PC heuristic with ~1 s latency, and the injection can also mis-fire when the guest legitimately idles at a stable PC with sound armed. Fix: implement buffer completion in the null backend itself (e.g. `schedule_output` in `null.rs` records the output mask from subfn 0x1F and raises it immediately or on the next timer tick, mirroring Einstein's `TNullSoundManager`), then delete the wedge detector and `inject_sound_dma_irq` entirely.

**H2. Concluded-investigation tripwires still run unconditionally in production paths.**
Three copies of the "newt"/pckm stack-corruption investigation survive, all referencing `INVESTIGATION.md` — **which no longer exists in the repo**:
- `src/trap.rs:353-374` — every timer IRQ on every build (including `pi-bare-metal-*`) polls PA `0x0402a250` for the byte pattern `"newt"`.
- `src/tracer.rs:479-507` — the same tripwire again, polled on **every traced function call**, plus stage-1 walks of hardcoded VAs `0x0cc82250`/`0x0cc7a250`.
- `src/trap.rs:3279-3290` (`handle_dabt_dispatch`) — one-shot on `FAR == 0x6e657774` dumping save areas of `"cdsv"`-named tasks; `src/task_dump.rs:1208-1212` (`dump_full`) — `dump_phys_for_pa(0x0402_a000)` "needed by the Phase B newt-DABT investigation"; `src/task_dump.rs:713-720` — the per-blocked-task `newt` user-stack window; `src/task_dump.rs:938-945` (`dump_save_area`) — unconditional stage-1 walk of "suspected alias" VA `0x0c602e2c` "(per trace 183155)".
These are address-literal probes for a specific, closed bug hunt. They cost cycles on hot paths, add copy-paste duplication (the tripwire exists verbatim twice), and will produce baffling output if those addresses ever recur in an unrelated context. Fix: delete all of them; the snapshot/gdb/`bp` workflow re-creates this kind of probe in minutes when next needed.

### Medium

**M1. ~40 % of `src/tracer.rs` is dead Phase-B probe payload baked into the trace hot path.**
`src/tracer.rs:457-652` and `668-849`: the alloc-sequence watch (five hardcoded PCs, lines 459-477), KSRVTask stack dumps (511-518), empty `SVC_WATCH` (525-533), `SMemCopyToSharedSWI` one-shot with hand-rolled L2 walk (539-584), `PrimGetEnvDomainName` / USR `GetEnvDomainName` / `RegisterEnvironmentId` one-shots (585-652), `dump_env_config_table` / `dump_param_buffer` (668-730), `dump_movefreeblock_entry` keyed on the exact literal args `r0=0x0c2041e0 r1=0x20` (616-624, 779-849). Each was a single-stall investigation. The durable core of the module — trampoline install, `rewrite_first_insn`, `log_trace_at`'s main line, and the `putc` line-buffering (732-777) — is genuinely good and worth keeping. Fix: strip the module down to that core; keep `SVC_WATCH` as the one documented extension point if you want a template for future hunts.

**M2. Cargo.toml `trace`/`trace_once` feature docs describe the previous implementation.**
`Cargo.toml` (`trace` feature comment): "build.rs parses `../_Data_/unified_symbols.tsv` … overwritten with `UDF #index` … 'first-touch' trace". The actual mechanism (per `src/tracer.rs` and `build.rs:361-409`) is trampoline-based, every-call, sourced from `scripts/classify-out/code-symbols.txt`. The `trace_once` comment also advertises "newt-tripwire poll, mode-13 SP_svc tracking" as live instrumentation — the SP_svc watchlist is empty and the tripwire is slated for deletion (H2). Misleading docs at the feature-selection layer are the kind that cost a future session an hour. Fix: rewrite both comments to match `tracer.rs`'s module doc (which is accurate).

**M3. `src/heap_check.rs` is half orphaned, hidden by a module-wide `#![allow(dead_code)]`.**
`src/heap_check.rs:6`. No caller outside the module exists for `log_ref`, `classify_ptr`, `dump_object`, `print_object`, `pretty_print_ref`, or `force_kernel_diag_on` (the latter is documented at lines 149-163 as actively crash-inducing — it triggers the `WriteDebugByte` NULL-ring-buffer halt). The live surface is: `log_heap_bounds_once` (called from `trap_irq`, `src/trap.rs:351`), `pretty_print_ref_inline` (the `log_store` probes, `src/trap.rs:3150,3169`), and `force_interpreter_trace_on` (under `ns_trace`). The module header still says "iter-78" and the `dump_object` section comment contradicts itself ("feed newton-objects a Heap configured with `Endian::Little`" at line ~305 vs. "Parsing as big-endian gives correct u32 values" at line 339-342). Fix: delete `force_kernel_diag_on` and the unused classifier/dump_object half, remove the `#![allow(dead_code)]`, and let the compiler police it again; fix or drop the endianness comment.

**M4. `src/tarmac.rs` is wired to nothing that fires, with a stale "Active investigation (2026-04-24)" header.**
The trap-count trigger is permanently disabled (`START_AT_TRAP = 0`, line 33); `emit_start` is `#[allow(dead_code)]` (line 75) because both former call sites are suppressed stubs — `src/trap.rs:4250` (`let _ = (); // tarmac::emit_start() suppressed for iter-85`) and `src/unaligned.rs:59`. Only `emit_stop` runs, from two halt paths (`src/trap.rs:2022, 2808`), emitting `<<TRM_STOP>>` markers into QEMU and real-hardware logs where no TarmacTrace exists. The module header describes an investigation wiring that no longer exists. Fix: gate the module (and its call sites) behind `platform-fvp-base` — it's FVP-plugin-specific by construction — and replace the iter-85 suppression comments with nothing (delete them; the module doc can note where to hook start/stop).

**M5. `task_dump.rs` doc comments were severed from their items by later insertions.**
`src/task_dump.rs:384-392`: the doc line `/// For each task whose flags[+0x6c] has bit … (q.prev or wq1/wq2 non-zero` sits attached to `jt_target`, and its continuation `/// (i.e. it's blocked somewhere), print its saved PC…` reappears 260 lines later at 645-648 attached to `dump_blocked_pcs`. Similarly `dump_oplist`'s citation block is fine but `walk_apcs_frames`'s doc (621) is interleaved with the `SEMAPHORE_OP_GLUE` constants (505-509). Harmless to rustc, actively misleading to readers. Fix: reunite the doc fragments with `dump_blocked_pcs` and give `jt_target` its own (currently missing) first doc line.

**M6. ~700 KiB symbol blob is embedded in every image, including `pi-bare-metal-*`.**
`src/symbols.rs:19-21` includes `fn_addrs.bin`/`fn_name_offs.bin`/`fn_names.bin` unconditionally (18,925 entries; ≈ 700 KiB of names+addresses). It powers `fmt_pc_name` stack traces in halt paths and `task_dump` — valuable on a wedge even in production — so this is a deliberate trade, but it should be a *recorded* one. Fix: either add a brief note in `symbols.rs` that the size cost is accepted on real hardware for halt-path backtraces, or add a `symbols` feature (on by default, off in the `pi-bare-metal*` aggregates) with a `fn_name_for_pc → None` stub.

### Low

**L1. `rep_print.rs` `%.*` swallows the precision argument without consuming it.**
`src/rep_print.rs:121-135`: in the precision branch, a `*` is skipped by the scanner but `args.next()` is never called, so for `%.*s` (a real printf idiom) every subsequent argument shifts by one. Width `%*d` (line 114-120) handles this correctly. Fix: in the `b'.'` loop, call `args.next()` when `p == b'*'`.

**L2. `tracer.rs` lint-suppression hacks.**
`src/tracer.rs:653` `let _ = cpu::halt;` to silence an unused import — remove the `use crate::cpu;` instead (it becomes genuinely unused once H2/M1 strip the probes). `src/tracer.rs:340` `let _ = sets_pc;` — the `sets_pc` distinction returned by `rewrite_first_insn` is never used; either drop the tuple field or use it to skip writing slots 2/4.

**L3. `heap_check::read_word` silently falls back from VA to PA interpretation.**
`src/heap_check.rs:51-54` (and `rep_print.rs:49-50, 421-424` use the same pattern): if the stage-1 walk fails, the same number is re-tried as a PA. For kernel-VA globals like `0x0c105548` that fallback reads unrelated physical memory and can populate the *permanent* `CACHED_LO/HI` bounds with garbage. Acceptable for a one-shot diagnostic, but the cache makes it sticky. Fix: drop the PA fallback for the bounds read, or don't cache on the fallback path.

**L4. `usb_probe.rs` is superseded.**
`src/usb_probe.rs:9-13` says "Once the DWC2 driver in the main crate is brought up, this binary will fold in the enumeration walk." Per `docs/REAL_HW_BRINGUP.md` and the working MTouch stack, the DWC2 driver *is* up; the probe still only reads `GSNPSID`. It also duplicates the entire PL011 driver from `pi_probe.rs` verbatim (lines 32-78). Delete it (and the `usb-probe` feature + `[[bin]]` stanza), or at minimum update the stale "will fold in" promise.

**L5. `scripts/build-sd.sh` defaults `PI_KERNEL_BIN` to `pi-probe`.**
Header comment: "default: pi-probe; Phase 1+ will swap to newton-hypervisor". Phase 1+ happened — the hypervisor boots to the Welcome UI on the Pi. A fresh `scripts/build-sd.sh <dir>` builds an SD card that boots the probe, not Newton. Fix: flip the default to `newton-hypervisor` (with `pi-bare-metal-input`?) and keep `pi-probe` as the documented override.

**L6. Logging-discipline stragglers.**
The `log_*` macro migration (`src/uart.rs:444-516`) is mostly complete, but `src/peripherals/sound.rs:58-73` prints its subfn trace via raw `kprintln!` (first 32 + 1-in-64 — permanent noise on real-hardware sound playback), and the H2 tripwires print via raw `kprintln!`. Anything periodic should go through `log_irqs!`/`log_traps!`/`dprintln!` per the project's own convention. Note also the default feature set still ships *all* `log_*` gates ON ("to preserve Phase-B trace-debug behaviour" — Cargo.toml); now that boot works, consider trimming the QEMU default to `log_traps` + `log_tasks` or fewer.

**L7. `static mut` density.**
`tracer.rs` (6), `trap.rs` (29) use `static mut` with `// SAFETY: single-threaded` comments; `rep_print.rs:169-177` already shows the better pattern (`addr_of_mut!` + struct). Single-core EL2 makes these sound in practice, but the wedge-probe counters and `TRACE_SEQ` would be one-line conversions to `AtomicU32/U64` (the codebase already uses atomics elsewhere in the same functions). Opportunistic cleanup only.

## Diagnostic-module disposition table

| Module | Wired up today? | Current gate | Recommendation |
|---|---|---|---|
| `src/tracer.rs` | Yes (trap.rs HVC + USR-UND fallback, guest_mem init) | `trace` / `trace_once` features | **Keep, gated as-is — but strip the stale per-investigation probes (M1, H2)**. The trampoline tracer is the project's best bisection tool and the gating is already correct. |
| `src/symbols.rs` | Yes — always compiled; used by halt-path backtraces | none | **Keep ungated** (halt-path stack traces earn the bytes), but record/decide the ~700 KiB cost for Pi images (M6). |
| `src/task_dump.rs` | Yes — `periodic` from `trap_irq` (cfg `log_tasks`), `dump_full` via HVC + halt paths, `dump_current_chain`/`dump_chain_at` pinned for gdb | `log_tasks` for the periodic path; rest always-on | **Keep** — this is earned kernel-introspection capital (and `docs/STRUCTURES.md`'s executable counterpart). Delete the newt/cdsv-specific bits (H2, M5); consider whether `dump_save_area_for_named` retains any caller after that (if not, keep it — it's generic — but the `0x0c602e2c` walk inside `dump_save_area` goes). |
| `src/heap_check.rs` | Partially — `log_heap_bounds_once` (always), pretty-printer (under `log_store`/`ns_trace`) | none / `log_store` / `ns_trace` | **Keep the Ref pretty-printer + heap-bounds core; delete the dead half** (`force_kernel_diag_on`, `log_ref`, `dump_object` family) and the `#![allow(dead_code)]` (M3). |
| `src/rep_print.rs` | Yes — Hammer Print/Putc/Flush body patches are always installed; REP output is the only window into guest printf | none | **Keep ungated.** This is no longer scaffolding; it's the guest console. Fix L1. |
| `src/tarmac.rs` | Effectively orphaned (only `emit_stop` from halt paths; both `emit_start` sites suppressed) | none | **Gate behind `platform-fvp-base`** (or delete — 97 lines, trivially recreated). Refresh the stale module header either way (M4). |
| `src/trap_hist.rs` | Yes — recorded on every trap; printed every 2 s under `log_traps`; also feeds splash progress | printing under `log_traps` | **Keep.** Recording cost is a few relaxed atomics; consider cfg-ing the `record_*` bodies too if Pi trap-path cycles ever matter. |
| `src/pi_probe.rs` | Standalone `[[bin]]`, `required-features = platform-raspi3b` | bin-level | **Keep** — first-light serial triage for new boards is cheap insurance; documented in REAL_HW_BRINGUP. Fix the build-sd.sh default (L5). |
| `src/usb_probe.rs` | Standalone `[[bin]]`, gated `usb-probe` | feature + bin | **Delete** — superseded by the working DWC2/MTouch stack; never grew past GSNPSID (L4). |
| `src/panic.rs` | Always (panic handler) | n/a | **Keep** — not scaffolding; correct and minimal. |
| `src/sd/probe.rs` | `kmain` under `sd-probe` feature | `sd-probe` / `sd-probe-trace` | **Keep gated** — destructive-by-design halt probe, but it's the documented first-light test for the SD stack on new cards/boards, and the gate already keeps it out of every normal build. |

## Guest-test coverage assessment

`guest-tests/tests/MANIFEST` lists all 35 test sources (none orphaned; `build-tests.sh`/`run-all.sh`/`run-test.sh` are consistent with each other and with `hvc_abi.S` ↔ `src/hvc_imm.rs`). Coverage against `src/peripherals/*` is genuinely broad: vic/gpio/alarm, flash + flash_driver, dma + dma_irq, pcmcia, serial + serial_driver, sound, tablet, battery, printer, network, platform_driver, native_primitives, screen blit, in/out translators, host_call, RTC calendar, plus trap-machinery tests (cp15 fault regs, StrongARM clock, UND handler, SPSR/ERET ×2, rotate-LDR unaligned, SWP ROM aperture, finetable rewrite, ROM patches, snapshot, MIDR, MMIO regs, bio_bank).

Handlers with **no** test exercising them:
- `peripherals/host_dma.rs` (host log-DMA path) — no test.
- `rep_print.rs` + the Hammer body patches and `UnhandledException` tripwires — probe-only paths exercised solely by the Newton ROM; a small `test_rep_print.S` issuing the Hammer HVCs would pin the VaArgs/format-rendering ABI.
- The snapshot **resume** path — `test_snapshot.S` exercises the save HVC; resume correctness is only validated by the manual workflow.
- `guest_bp.rs` (one-shot SW breakpoints) — debug-only, exercised via gdb workflow; acceptable.
- Hardware-only stacks (`sd/*`, `usb/*`, `display/*`, `audio/pi_hdmi`, `mailbox`) — inherently untestable under the QEMU guest-test harness; covered by the gated probes instead.
- The diagnostic modules themselves (tracer/task_dump/heap_check) — reasonable to leave untested given their disposition above; deleting the stale halves shrinks the untested surface more cheaply than testing it.

`scripts/` is in better shape than expected: `classify-symbols.py` → `classify-out/{code-symbols,uncertain}.txt` → `build.rs`/`show-first-word.py` references all resolve; `regen-classify.sh`, `dump-data-regions.py`, `build-rom-disasm.sh`, `trace-diff.sh` (uses the still-real `trace,quiet` features) all reference existing artifacts. The only stale spots found are the `build-sd.sh` default (L5) and the deleted-`INVESTIGATION.md` references in source comments (H2; also `src/tracer.rs:483`).

## Shape of this subsystem

The diagnostic stack has a clean three-layer shape that's worth preserving: always-on capital (symbols + task_dump kernel introspection + rep_print guest console + panic/halt dumps), feature-gated heavy machinery (trace/trace_once, log_* categories, ns_trace, the hardware probes), and per-investigation probes that were supposed to be temporary. The first two layers are in good condition — gating is consistent, the `log_*` macro migration mostly landed, and the gdb/snapshot/HVC-dump integration is genuinely well-engineered. The rot is concentrated entirely in the third layer: roughly a dozen address-literal tripwires from the concluded "newt" corruption hunt and the iter-7x/8x era are still compiled into hot paths on every build, one of them (sound-IRQ injection) has quietly become load-bearing, and the feature docs in Cargo.toml describe a tracer that no longer exists.

**Top cleanup actions:**
1. **Promote the sound-completion IRQ into the `audio-null` backend and delete the wedge probe** (H1) — this converts an accidental behavioral dependency into an explicit, Einstein-mirroring implementation.
2. **Sweep all concluded-investigation tripwires** — the duplicated `0x0402a250` newt-tripwires, the `cdsv`/`0x6e657774` one-shots, `dump_phys_for_pa(0x0402a000)` in `dump_full`, the `0x0c602e2c` alias walk, and tracer.rs's hardcoded function probes (H2 + M1). One commit, purely deletions.
3. **Fix the stale gates and docs**: rewrite the Cargo.toml `trace`/`trace_once` comments, gate `tarmac.rs` to FVP, delete `usb_probe`, de-`allow(dead_code)` heap_check, and flip `build-sd.sh`'s default kernel to the hypervisor (M2-M4, L4-L5).
