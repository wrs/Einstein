# Code review: MMIO dispatch + peripherals (`src/mmio.rs`, `src/peripherals/*`)

> Review agent report, 2026-06-11, at working copy `somv 8b564c93`.
> Scope: stabilization review — correctness, loud-vs-silent stubs, structure,
> consistency, Rust practices. Einstein cross-checks were done against
> `Emulator/TNativePrimitives.cpp`, `Emulator/TDMAManager.cpp`,
> `Emulator/TMemoryConsts.h`, and `Emulator/Serial/TPtySerialPortManager.cpp`.

## High

### H1. `screen.rs:473-482` — blit mode read uses `ctx.x[13]` (SP_usr) instead of the source mode's banked SP
`ctx_blit_mode` reads the blit-mode word from `[SP+4]` via `ctx.x[13]`, with a comment claiming "r13 is at ctx.x[13] in AArch32 user/svc context". That contradicts the project's own hard-won convention: `flash_driver.rs:110-137` documents at length that R13_svc lives in **X19** per ARM ARM Table D1-79 ("Reading the wrong slot was the historical bug here"), `trap.rs:709-711` uses `crate::banked::sp_for_mode(ctx, spsr)` for exactly this reason, and `docs/QEMU_BUGS.md` (per CLAUDE.md) flags this as a repeatedly-misdiagnosed trap. Einstein reads `GetRegister(13)` — the *current-mode* banked R13 (TNativePrimitives.cpp, screen case 0x07). If the kernel issues the Blit from SVC mode, this reads a junk word off the user stack. The failure is **silent**: `unwrap_or(0)` plus the "unrecognised mode → srcCopy" fallback means a wrong read degrades mode-1 ink overlays into rect-clearing srcCopy blits instead of halting.
**Fix:** read `SPSR_EL2`, use `crate::banked::sp_for_mode(ctx, spsr)`; on translate/read failure, halt loudly (matching every other guest-memory access in this file) rather than defaulting to 0.

### H2. `mmio.rs:279, 520-526` — sub-word MMIO **reads** return the wrong byte lane (asymmetric with the write splice)
The write path (lines 336-349) carefully models BE-8: a byte write at lane 0 is spliced into bits[31:24] of the register's numeric value (`splice_byte`, line 478-483). The read path does nothing of the sort: `mask_for_size` (line 520) returns `value & 0xFF` / `& 0xFFFF` regardless of `ipa & 3`, and `trap.rs:768-771` stores that straight into the destination register. Under BE-8 an `LDRB` at the word-aligned register address must observe bits[31:24] — the same lane `splice_byte` writes — but gets bits[7:0]. So a guest byte read-back of any modelled register whose interesting bits live in the high lanes silently returns 0. (Byte reads at +1..+3 of peripheral registers mostly miss `owns()` and halt loudly, which masks the issue today; lane-0 byte reads are the silent case.)
**Fix:** mirror the splice: for `sas < 2`, read the aligned word, then extract `(value >> (24 - 8*lane)) & 0xFF` (resp. halfword). Or, if sub-word MMIO reads are believed never to occur, halt loudly on `sas < 2` reads — anything but the current silent wrong-lane return.

## Medium

### M1. `mmio.rs:336-349` — the RMW splice calls `read()`, which (a) halts misleadingly on write-only registers and (b) triggers read side effects
Any sub-word write to a register that exists only in the write whitelist (`0x0F00_2000`, `0x0F04_3000/3800`, `0x0F24_1800/7000`, the `0x0F28_xxxx` bus-strap set, `serial::TX_BYTE`…) first issues `read(ctx, aligned, 2, elr)` — and the read path has no arm for those, so the run halts with "*** unknown MMIO **read**" at an address the guest only ever *wrote*. That's loud (good) but misattributed (bad): the diagnostic will send the next debugging session hunting a phantom load. Worse, `read()` has side effects: a stray sub-word write into `0x0F24_3000..3` advances the ROM-serial-chip bit index (`ROM_SERIAL_IX`, line 222-233), silently corrupting the 65-bit serial-number stream the store-signature path depends on (the file's own header explains how load-bearing that is).
**Fix:** give peripherals a side-effect-free `peek_word` used only by the splice (defaulting to 0 for write-only registers), or at minimum tag the halt message as "read-for-splice during sub-word write of {orig ipa}".

### M2. `dma.rs:273-301 + 320-323` — aliasing `&mut DmaState` (UB by Rust rules)
`write` creates `s = &mut *DMA.0.get()` and passes it to `write_enable(s, value)`; inside the loop, `drain_tx_channel(ch_idx)` re-derives a *second* `&mut *DMA.0.get()` while the caller's `&mut DmaState` is still live (it's used by the next loop iteration). Two simultaneous `&mut` to the same data is undefined behavior regardless of single-threadedness — this is exactly the pattern Miri flags, and the optimizer is entitled to assume the two don't alias.
**Fix:** change `drain_tx_channel(s: &mut DmaState, ch_idx: u32)` and pass the existing borrow down (the function already takes `ch_idx` only so the call-site change is mechanical).

### M3. `dma.rs:362-368` — the 4 KiB TX-drain cap can strand a transfer with no completion IRQ
Einstein's PTY thread keeps draining one byte per 260 µs until `mTxDMADataCountdown == 0` and only then raises `0x100` (TPtySerialPortManager.cpp, TX branch — verified). Our model drains only inside `write_enable`, capped at 4096 bytes "to keep the trap handler bounded", and if the cap breaks the loop, no event is set, no IRQ is raised, and nothing ever resumes the drain — the comment "The kernel will re-arm if there's more" is wishful: the kernel is more likely waiting on the TX-complete IRQ for the transfer it already armed. For countdown > 4096 this is a silent serial-port hang.
**Fix:** add a `poll_tx()` sibling to `poll_rx()` driven from `trap_irq` that continues draining armed TX channels (the `armed` flag already exists and is otherwise unused on channel 1), or remove the cap (host UART writes are cheap on all three platforms).

### M4. `pcmcia.rs:158-159, 248-253` (also `dma.rs:152-154`) — one shared log budget burns out on routine traffic, making unknown-offset stubs fully silent
In pcmcia, *every* access — including known-register reads/writes that are working as intended — consumes the single `LOG_BUDGET` (16). The boot-time chip-detect probes exhaust it within the first dozen accesses, after which "pcmcia read unknown reg (returning 0)" and "write unknown reg (dropped)" produce **nothing**. That defeats the discovery purpose the file's own header claims ("we'd rather discover them lazily"); you can't discover what's never printed. `dma.rs` has the same shape: `LOG_MAX = 32` shared between expected channel-2-7 stub traffic and genuinely-unknown register offsets.
**Fix:** separate budgets — routine/expected stub traffic on a tight budget (or `dprintln!`), unknown offsets on their own generous budget (or, for the controller-register window where the register set is closed, halt loudly like vic/dma do).

### M5. `vic.rs:778-789`, `sound.rs:31-56` — `static mut` counters where every other peripheral uses atomics; the "single-threaded" SAFETY story has an undocumented hole
`vic::write` uses `static mut LOG_N`; `sound::handle` uses `static mut SEEN` and `static mut SUBFN_COUNT`. Everything else in the subsystem (dma, pcmcia, screen, serial, tablet) uses `AtomicU32`/`AtomicUsize` for the identical pattern. Beyond inconsistency, the blanket "SAFETY: single-threaded EL2" claim on `VicCell`/`DmaCell` is no longer literally true: `platform::pause_system` unmasks IRQs at EL2 (platform.rs:180-195) and the nested `trap_irq` mutates VIC and DMA state (`poll_timer_matches`, `dma::poll_rx`, `vic::raise`) through the same `UnsafeCell`s. It happens to be sound today only because no caller holds a borrow across the unmask window — an invariant nowhere written down.
**Fix:** convert the `static mut` counters to atomics (mechanical), and document the real invariant on `VicCell`/`DmaCell`: "no `&mut` borrow may be live across any point where EL2 IRQs are unmasked (currently only `pause_system`'s WFI loop)".

### M6. `serial.rs:116-152` vs `dma.rs:320-375` — two TX paths for the same port; PIO bytes never reach the host UART and are silently dropped after 64
DMA channel 1 (serial port 0 TX) drains real bytes to `uart::write_byte`. The PIO register path for the same port (`serial::TX_BYTE`) only logs the byte — budget 64 — then drops further bytes with no trace at all. A guest driver that mixes PIO and DMA (or falls back to PIO) gets its output truncated invisibly, and PIO bytes never interleave into the actual host serial stream the DMA path feeds.
**Fix:** forward `TX_BYTE` writes for port 0 to `uart::write_byte` (matching the DMA path), keep the budgeted log as a diagnostic; for ports 1-3 at least keep a "dropped N bytes" running counter visible in some diagnostic dump.

### M7. `mmio.rs:117-125, 266-268, 394-397` — TEST_SCRATCH window is live in production builds
The 16-byte R/W scratch at `0x1200_0000` exists solely for `test_shadow_stub.S` subtest 11, but it's not gated on `cfg(nh_guest_test)`. In production a read/write there round-trips real storage instead of Einstein's "unknown bank #5" zero/absorb semantics — a (small) divergence from the oracle inside an address window the comment itself says "Real Newton hardware doesn't expose anything in". Cheap to gate; gating also removes a `static mut` from production builds.

### M8. `pcmcia.rs:181-182, 221-222` — `owns()`-then-`None` arms silently return instead of halting as unreachable
`read`/`write` handle `ipa_to_slot(ipa) == None` by returning 0 / dropping — but `mmio.rs` only routes here when `owns()` (== `ipa_to_slot().is_some()`) already said yes, so the arm is dead and, if ever reached, indicates the same owns/dispatch desync that `vic::halt_vic_unreachable` and `dma::halt_unknown_dma` halt loudly on. Use the same unreachable-halt pattern for consistency.

## Low

### L1. `vic.rs:440-451` — `raised()` and `int_present_raw()` are byte-identical
Both are used (trap.rs uses each name in different places). Keep one, alias or migrate call sites.

### L2. `vic.rs:190-192` — contradictory comments on the baked RTC seed
The comment above says "midnight 2026-05-11 UTC, the first known Pi-Zero-2-W boot"; the inline comment says `2026-05-15 00:00:00 UTC`; the value `1_778_889_600` is actually 2026-05-16T00:00:00Z. Pick one and make it true.

### L3. `mmio.rs:470` — `let _ = value;` is dead
`value` is consumed by the match arms above (`BANK_CTRL_REG.store`, `halt_on_unknown`). Leftover suppress-unused from an earlier shape; remove.

### L4. `flash.rs:252-259` — `is_flash_pa` re-implements `pa_to_offset`'s range logic
`pa <= u32::MAX` guard + `pa_to_offset(pa as u32).is_some()` says the same thing without a second copy of the bank bounds to keep in sync.

### L5. `flash_driver.rs:267-272` — `do_write` doc comment is wrong about who programs the word
"`BeginWrite` already programmed the masked word into flash" — `begin_write` (0x0D) only range-checks; the programming happens in `write` (0x08). The conclusion (DoWrite is bookkeeping, r0=0) is right; the justification will mislead the next reader.

### L6. `platform.rs:276-283` — `log_message` read failure returns without halting
Every other failed guest-memory access in the native-prim handlers halts (`fill_status`, `get_subsystem_power`, tablet, battery, screen…); `platform.Log` and `network.log_string` (network.rs:123-130) just print and return. Defensible for a log primitive, but it leaves `r0` untouched on a path Einstein would have completed — worth either a comment stating it's deliberate or aligning with the halt convention.

### L7. `vic.rs:600-605` — `tick_advance()` back-compat alias has exactly one caller
`stage2.rs:544` is the only user; renaming that call to `tick_advance_sync_trap()` lets the alias (and its "older callers" comment) go.

## Verified non-findings (so they don't get "fixed" later)

- `dma.rs` bank-2 window ending at `0x0F09_8000` (4 channels' worth) is Einstein-verbatim (`kHdWr_DMAChan2End = 0x0F098000`, TMemoryConsts.h:75), not a typo.
- The odd `buf_size` underflow-after-wrap in `drain_tx_channel`/`poll_rx` (decrement past 0 without reload) mirrors Einstein's `TPtySerialPortManager::HandleDMA` byte-for-byte, including the quirk.
- `tablet.rs`'s double-write/read of offset `+0x10` and never `+0x0C` is a deliberately-mirrored Einstein bug, documented as such.
- `vic.rs`'s "stateless registers must read 0 even under guest RMW" stance is correct oracle-matching, well documented.

## Shape of this subsystem

Overall this is a healthy subsystem: the loud-halt trip-wire convention is genuinely enforced almost everywhere, every stub carries an Einstein file:line citation, and the MMIO-window peripherals (vic, dma, pcmcia, serial) share a recognizable `owns()/read()/write()` contract while the native-primitive peripherals share a `DRIVER_ID/handle(ctx, subfn, pc)` contract. The two real soft spots are (1) the BE-8 sub-word handling, which is modelled carefully on the write side and not at all on the read side (H2/M1) — that asymmetry should be closed in one place in `mmio.rs` rather than per-peripheral; and (2) log-budget hygiene, where "expected stub" and "unknown input" traffic share counters and the discovery property quietly dies (M4, M6).

Top refactors by payoff:

1. **Extract the guest-memory access helpers into one module.** `write_guest_word`/`read_guest_word`/`read_guest_byte` with VA-first/PA-fallback semantics are copy-pasted into flash_driver, platform, battery, tablet (and screen/network have private variants). A `peripherals::guest_access` (or additions to `guest_endian`) with `read_word_or_halt(addr, what, pc)`-style loud variants would delete ~6 copies and make the halt-on-failure convention impossible to forget (it was forgotten in `platform.Log`, `network.log_string`, and critically in `screen::ctx_blit_mode`'s `unwrap_or(0)`).
2. **Formalize the two dispatch contracts as traits** (`MmioPeripheral { owns, read, write }`, `NativeDriver { DRIVER_ID, handle }`) with a shared `halt_unreachable`/`halt_unknown_subfn` helper. The per-file halt blocks are near-identical five-line copies whose wording has already drifted; a shared helper also guarantees every new peripheral gets the context dump and the "extend file X" hint for free.
3. **Centralize budgeted logging** into one small utility (`LogBudget::new(N)` with `.log(args…)`), with the expected-stub vs unknown-input split from M4 built in, and route the routine tier through `dprintln!` so `quiet` builds keep their trace budget. That collapses ~8 hand-rolled budget patterns (including the two `static mut` ones from M5) into one audited implementation.
