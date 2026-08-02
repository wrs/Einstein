//! Newton DMA manager — Rust port of Einstein's `TDMAManager` plus the
//! per-channel serial DMA modeled by `TBasicSerialPortManager`.
//!
//! Einstein's TDMAManager is almost an API stub: `mAssignmentReg` is
//! the only piece of real chip-wide state (`Emulator/TDMAManager.cpp:
//! 69-95`). The chip-wide enable / disable / status registers are
//! observable as zero on read and acknowledged on write — except that
//! channels 0 and 1 belong to the external serial port and delegate
//! per-channel-register reads/writes to the serial driver
//! (`TDMAManager::Read/WriteChannel{1,2}Register` at
//! `Emulator/TDMAManager.cpp:172-277`).
//!
//! For phase-B we wire those two channels through the guest-console
//! seam (`peripherals::console`, installed by `main.rs` with the host
//! PL011 endpoints) so the guest's external-serial port (`extr`)
//! actually moves bytes. The register-level semantics here
//! mirror Einstein's `TBasicSerialPortManager::{Read,Write}{Rx,Tx}DMARegister`
//! (`Emulator/Serial/TBasicSerialPortManager.cpp:642-891`):
//!
//!   bank1 reg 0 — mRx/TxDMAPhysicalBufferStart (buffer base PA)
//!   bank1 reg 1 — mRx/TxDMAPhysicalData (current PA)
//!   bank1 reg 3 — written 0x80 (RX) / 0xC0 (TX) — ignored (purpose unknown)
//!   bank1 reg 4 — mRx/TxDMADataCountdown (bytes left)
//!   bank1 reg 5 — mRx/TxDMABufferSize (ring size, wraps at end)
//!   bank1 reg 6 — RX writes 0xFF, TX writes 0 — ignored
//!   bank2 reg 0 — mRx/TxDMAControl (bit 0x02 = "DMA enabled")
//!   bank2 reg 1 — mRx/TxDMAEvent (RX completion = 0x40, TX completion = 0x80)
//!   bank2 reg 2 — `event &= ~inValue` (write to clear)
//!   bank2 reg 3 — interrupt-select / direction (ignored)
//!
//! Completion-IRQ semantics match Einstein's `TPtySerialPortManager`
//! (`Emulator/Serial/TPtySerialPortManager.cpp:175-300`):
//!
//!   TX: when control bit 0x02 is set and countdown > 0, drain bytes
//!       from `[mTxDMAPhysicalData ..]` to the host UART, decrement
//!       countdown / buffer size (with wrap), and on countdown=0 set
//!       `mTxDMAEvent = 0x80` and raise `INT_DMA_CH1` (mask 0x100).
//!   RX: when host UART has bytes and control bit 0x02 is set, deposit
//!       each byte at `mRxDMAPhysicalData`, advance with wrap; when at
//!       least one byte was deposited, set `mRxDMAEvent = 0x40` and
//!       raise `INT_DMA_CH0` (mask 0x80). Polled from `trap::trap_irq`
//!       (see `poll_rx`).
//!
//! Channels 2-7 keep the historic "log on first use, return 0, drop"
//! behaviour; we don't have a modeled backend for IR / modem / sound
//! DMA yet. Crucially we *don't* synthesise a completion IRQ on the
//! enable-register write: doing so caused a FIQ runaway when the
//! kernel re-armed channel 0 from inside the FIQ handler (the IRQ
//! raised on the re-arm immediately re-took the FIQ before it could
//! exit).

use core::cell::UnsafeCell;

use crate::{hv::guest_endian, kprintln, peripherals::vic};
use crate::peripherals::console;

/// Per-channel completion-IRQ mask. From `TBasicSerialPortManager.cpp:
/// 295-296` (`kDMAChannel0IntMask = 0x80`, `kDMAChannel1IntMask = 0x100`).
const INT_DMA_CH0: u32 = 0x0000_0080; // serial port 0 receive
const INT_DMA_CH1: u32 = 0x0000_0100; // serial port 0 transmit

/// Bank 1 channel-register window (8 channels × 8 regs × 4 B, channel
/// stride 0x2000, reg stride 0x400). Einstein
/// `Emulator/TDMAManager.cpp:172-222`, also `docs/peripherals.md`
/// §"DMA manager".
const BANK1_BASE: u64 = 0x0F08_0000;
const BANK1_END: u64 = 0x0F08_FC00;

/// Chip-wide channel-assignment register. R/W; writes latch, reads
/// return the last write. Einstein `TDMAManager.cpp:69-95`.
const K_HDWR_ASSIGN: u64 = 0x0F08_FC00;

/// Bank 2 channel-register window (same layout as bank 1).
const BANK2_BASE: u64 = 0x0F09_0000;
const BANK2_END: u64 = 0x0F09_8000;

/// Chip-wide enable register: writes a bitmask of channels to start.
/// Einstein's `WriteEnableRegister` (`TDMAManager.cpp:101-114`) just
/// logs; the real hardware semantic is "kick the channel state
/// machine". Our model uses it as the trigger to drain TX / arm RX.
const K_HDWR_ENABLE_STATUS: u64 = 0x0F09_8000;

/// Chip-wide disable register. Write-only; Einstein logs and drops
/// (`TDMAManager.cpp:136-149`). We treat it as "stop the per-channel
/// state machine" (clears the channel's local `armed` flag).
const K_HDWR_DISABLE: u64 = 0x0F09_8400;

/// Chip-wide word-status register. Read-only; Einstein always reads 0
/// (`TDMAManager.cpp:152-166`).
const K_HDWR_WORD_STATUS: u64 = 0x0F09_8800;

/// Per-channel state. Matches the public fields of Einstein's
/// `TBasicSerialPortManager` for the RX/TX side of port 0; for
/// channels 2-7 only `armed` is consulted (and we never deposit/drain
/// bytes for them).
#[derive(Default, Clone, Copy)]
struct ChannelState {
    /// Bank 1 register 0 — buffer base PA. Set by
    /// `TSerialDMAEngine::BindToBuffer`.
    buf_start: u32,
    /// Bank 1 register 1 — current data PA. Advanced by every byte
    /// moved; wraps around back to `buf_start` when the cursor reaches
    /// `buf_start + buf_size` (Einstein's `if (mRxDMABufferSize == 0)`
    /// pattern).
    data_ptr: u32,
    /// Bank 1 register 4 — bytes remaining in this DMA request. Hits 0
    /// → completion IRQ fires.
    countdown: u32,
    /// Bank 1 register 5 — ring buffer size in bytes (used to wrap
    /// `data_ptr`).
    buf_size: u32,
    /// Bank 2 register 0 — control register. Bit `0x02` = "DMA
    /// enabled" (Einstein `TPtySerialPortManager.cpp:194` checks
    /// `mTxDMAControl & 0x00000002`).
    control: u32,
    /// Bank 2 register 1 — event/interrupt-reason register.
    /// `DMAInterrupt` (Newton ROM `0x001D9550`) reads this and ORs
    /// into its accumulated status. Cleared by writing the same value
    /// to bank2 reg 2.
    event: u32,
    /// `true` after a chip-wide enable for this channel until a
    /// chip-wide disable clears it. Decouples the per-channel control
    /// register from the "transfer in flight" predicate.
    armed: bool,
}

struct DmaState {
    assign: u32,
    channels: [ChannelState; 8],
}

struct DmaCell(UnsafeCell<DmaState>);
// SAFETY: accessed only from the single EL2 trap handler on core 0.
//
// Borrow invariant: no `&mut DmaState` borrow (via `DMA.0.get()`) may be
// live across any point where EL2 IRQs are unmasked. The only such point
// today is `platform::pause_system`'s WFI loop, which unmasks IRQs at EL2
// so a nested `trap_irq` can run; that nested handler re-borrows DMA state
// (`poll_rx`, `poll_tx`). A `&mut` held across the unmask window would
// alias the nested borrow — undefined behavior. Each `read`/`write`/
// `poll_*` derives one `&mut` and threads it through its helpers (e.g.
// `write_enable` → `drain_tx_channel`); never re-derive a second `&mut`
// while one is live, and never hold one across a WFI/unmask.
unsafe impl Sync for DmaCell {}

static DMA: DmaCell = DmaCell(UnsafeCell::new(DmaState {
    assign: 0,
    channels: [ChannelState {
        buf_start: 0,
        data_ptr: 0,
        countdown: 0,
        buf_size: 0,
        control: 0,
        event: 0,
        armed: false,
    }; 8],
}));

/// Split log budgets: "expected stub" traffic
/// (unmodeled channels 2-7, the chip-wide enable on those channels)
/// burns a tight budget so a spinning kernel driver can't drown the
/// console, while genuinely-unknown register offsets get their own
/// generous budget so discovery never goes fully silent on the back of
/// routine traffic.
static LOG: crate::diag::diag_util::TwoTierLog = crate::diag::diag_util::TwoTierLog::new(8, 64);

/// Marker for the [`crate::hv::mmio::MmioPeripheral`] router. The DMA
/// register state lives in the module-level `DMA` cell; this zero-sized
/// type only names the model for static dispatch.
pub struct Dma;

impl crate::hv::mmio::MmioPeripheral for Dma {
    fn read(ipa: u64) -> u32 {
        read(ipa)
    }
    fn write(ipa: u64, value: u32) {
        write(ipa, value)
    }
}

/// True iff `ipa` is inside the per-channel register banks.
fn is_channel_reg(ipa: u64) -> bool {
    (BANK1_BASE..BANK1_END).contains(&ipa) || (BANK2_BASE..BANK2_END).contains(&ipa)
}

/// Decode a per-channel-register IPA into `(bank, channel, register)`.
/// Bank is 1 or 2; channel is 0..7; register is 0..7. Caller must have
/// already verified `is_channel_reg(ipa)`.
fn split_channel_reg(ipa: u64) -> (u32, u32, u32) {
    let (bank, base) = if (BANK1_BASE..BANK1_END).contains(&ipa) {
        (1u32, BANK1_BASE)
    } else {
        (2u32, BANK2_BASE)
    };
    let rel = ipa - base;
    let channel = (rel / 0x2000) as u32;
    let register = ((rel % 0x2000) / 0x400) as u32;
    (bank, channel, register)
}

fn read(ipa: u64) -> u32 {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *DMA.0.get() };
    match ipa {
        K_HDWR_ASSIGN => s.assign,
        K_HDWR_ENABLE_STATUS | K_HDWR_WORD_STATUS => 0,
        _ if is_channel_reg(ipa) => {
            let (bank, channel, register) = split_channel_reg(ipa);
            read_channel_reg(s, bank, channel, register)
        }
        _ => halt_unknown_dma("read", ipa, 0),
    }
}

fn write(ipa: u64, value: u32) {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *DMA.0.get() };
    match ipa {
        K_HDWR_ASSIGN => s.assign = value,
        K_HDWR_ENABLE_STATUS => write_enable(s, value),
        K_HDWR_DISABLE => write_disable(s, value),
        _ if is_channel_reg(ipa) => {
            let (bank, channel, register) = split_channel_reg(ipa);
            write_channel_reg(s, bank, channel, register, value);
        }
        _ => halt_unknown_dma("write", ipa, value),
    }
}

/// Per-channel read. Matches `TBasicSerialPortManager::ReadRxDMARegister`
/// and `ReadTxDMARegister` for channels 0/1; channels 2-7 fall through
/// to the "Einstein reads 0" stub.
fn read_channel_reg(s: &mut DmaState, bank: u32, channel: u32, register: u32) -> u32 {
    let stateful = channel < 2;
    if !stateful {
        log_expected_chan("dma channel read (unmodeled, returning 0)", bank, channel, register, 0);
        return 0;
    }
    let ch = &s.channels[channel as usize];
    match (bank, register) {
        (1, 0) => ch.buf_start,
        (1, 1) => ch.data_ptr,
        (1, 3) => 0,
        (1, 4) => ch.countdown,
        (1, 5) => ch.buf_size,
        (1, 6) => 0,
        (2, 0) => ch.control,
        (2, 1) => ch.event,
        (2, 2) | (2, 3) => 0,
        _ => {
            // Einstein logs unknown reads via KPrintf but returns 0.
            log_unknown_chan("dma channel read (unknown reg, returning 0)", bank, channel, register, 0);
            0
        }
    }
}

/// Per-channel write. Updates the modeled state for channels 0/1 per
/// Einstein's WriteRx/TxDMARegister; channels 2-7 are logged-and-dropped
/// (no observable backend).
fn write_channel_reg(s: &mut DmaState, bank: u32, channel: u32, register: u32, value: u32) {
    let stateful = channel < 2;
    if !stateful {
        log_expected_chan("dma channel write (unmodeled, dropped)", bank, channel, register, value);
        return;
    }
    let ch = &mut s.channels[channel as usize];
    match (bank, register) {
        (1, 0) => ch.buf_start = value,
        (1, 1) => ch.data_ptr = value,
        (1, 3) | (1, 6) => { /* purpose unknown per Einstein; ignored */ }
        (1, 4) => ch.countdown = value,
        (1, 5) => ch.buf_size = value,
        (2, 0) => ch.control = value,
        (2, 1) => ch.event = value,
        (2, 2) => ch.event &= !value, // write-to-clear (Einstein TBSPM.cpp:751,881)
        (2, 3) => { /* interrupt-select / direction; ignored, no FIQ routing decision */ }
        _ => {
            log_unknown_chan("dma channel write (unknown reg, dropped)", bank, channel, register, value);
        }
    }
}

/// Chip-wide enable register write. For channels 0/1 we kick the
/// transfer immediately: TX drains the buffer to the host console wire, RX
/// just marks the channel armed (poll picks up bytes later). Other
/// channels are logged-and-dropped.
fn write_enable(s: &mut DmaState, value: u32) {
    for ch_idx in 0..8u32 {
        if (value >> ch_idx) & 1 == 0 {
            continue;
        }
        let ch = &mut s.channels[ch_idx as usize];
        ch.armed = true;
        match ch_idx {
            0 => {
                // RX: nothing to do here. `poll_rx` will pick up host
                // bytes on the next trap_irq tick. Matches
                // Einstein's PTY behaviour: the receiver thread sees
                // mRxDMAControl & 2 set when DMA is enabled.
            }
            1 => {
                // TX: drain any bytes in the buffer right away. This
                // matches the PtySerialPortManager loop body
                // (`Emulator/Serial/TPtySerialPortManager.cpp:215-240`)
                // which fires once per byte at 38400 bps; we coalesce
                // all bytes into one synchronous drain since the host
                // wire already has its own FIFO. If the request exceeds
                // the per-call 4 KiB cap, `poll_tx` continues the drain
                // on subsequent trap_irq ticks.
                drain_tx_channel(s, ch_idx);
            }
            _ => {
                log_expected("dma enable (unmodeled channel, no IRQ)", K_HDWR_ENABLE_STATUS, value);
            }
        }
    }
}

/// Chip-wide disable register write. Clears the `armed` flag on each
/// listed channel; matches Einstein's "abort pending transfers"
/// semantic without actually unwinding mid-byte (the kernel always
/// re-arms via the per-channel registers anyway).
fn write_disable(s: &mut DmaState, value: u32) {
    for ch_idx in 0..8u32 {
        if (value >> ch_idx) & 1 == 0 {
            continue;
        }
        s.channels[ch_idx as usize].armed = false;
    }
}

/// Drain channel 1 (serial 0 TX) to the host console wire. Decrements
/// `countdown`/`buf_size`, wraps `data_ptr` at the end of the ring,
/// and on countdown=0 raises `INT_DMA_CH1` with `event=0x80` —
/// mirroring `TPtySerialPortManager::HandleDMA` TX branch.
fn drain_tx_channel(s: &mut DmaState, ch_idx: u32) {
    let ch = &mut s.channels[ch_idx as usize];
    // Einstein's PTY loop only does work when control bit 0x02 is set.
    // The Newton kernel writes that bit in `StartTxDMA` just before
    // the enable register; missing the bit means "kernel still
    // configuring", so we no-op rather than draining garbage.
    if ch.control & 0x0000_0002 == 0 {
        return;
    }
    if ch.countdown == 0 || ch.buf_size == 0 {
        return;
    }
    let mut drained = 0u32;
    while ch.countdown > 0 {
        // Read one byte at the current PA, push to host UART. The
        // serial DMA buffer is in guest RAM and BE-8 host bytes match
        // logical byte addresses (see src/guest_endian.rs). A data_ptr
        // outside guest memory means the kernel armed the channel with
        // a wild buffer pointer — halt loudly rather than draining
        // fabricated bytes.
        let byte = match guest_endian::guest_read_u8_pa(ch.data_ptr) {
            Some(b) => b,
            None => {
                crate::kprintln!(
                    "*** dma: TX drain ch{} data_ptr={:#010x} outside guest memory \
                     (buf_start={:#010x} buf_size={:#x} countdown={:#x}) ***",
                    ch_idx, ch.data_ptr, ch.buf_start, ch.buf_size, ch.countdown,
                );
                crate::arch::cpu::halt();
            }
        };
        console::tx(byte);
        ch.data_ptr = ch.data_ptr.wrapping_add(1);
        ch.buf_size = ch.buf_size.wrapping_sub(1);
        if ch.buf_size == 0 {
            // Wrap back to the buffer's start PA, matching Einstein's
            // `mTxDMAPhysicalData = mTxDMAPhysicalBufferStart`.
            ch.data_ptr = ch.buf_start;
        }
        ch.countdown = ch.countdown.wrapping_sub(1);
        drained += 1;
        // Cap a single drain at 4 KiB to keep the trap handler bounded.
        // The kernel will re-arm if there's more.
        if drained >= 4096 {
            break;
        }
    }
    if ch.countdown == 0 {
        // Buffer empty — fire TX completion. mTxDMAEvent=0x80,
        // INT_DMA_CH1 raised.
        ch.event |= 0x0000_0080;
        vic::raise(INT_DMA_CH1);
    }
}

/// Pump channel 0 (serial 0 RX) from the host console wire into guest RAM.
/// Called from `trap_irq` on each timer-IRQ tick. Raises
/// `INT_DMA_CH0` (event=0x40) whenever at least one byte was
/// deposited, matching `TPtySerialPortManager::HandleDMA` RX branch
/// at `Emulator/Serial/TPtySerialPortManager.cpp:250-298`.
pub fn poll_rx() {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *DMA.0.get() };
    let ch = &mut s.channels[0];
    if !ch.armed || (ch.control & 0x0000_0002) == 0 {
        return;
    }
    let mut deposited = 0u32;
    while ch.countdown > 0 {
        let Some(byte) = console::rx() else { break };
        guest_endian::guest_write_u8_pa(ch.data_ptr, byte);
        ch.data_ptr = ch.data_ptr.wrapping_add(1);
        ch.buf_size = ch.buf_size.wrapping_sub(1);
        if ch.buf_size == 0 {
            ch.data_ptr = ch.buf_start;
        }
        ch.countdown = ch.countdown.wrapping_sub(1);
        deposited += 1;
        if deposited >= 256 {
            // Cap per-tick to keep IRQ handler bounded; remaining
            // bytes drain on the next tick.
            break;
        }
    }
    if deposited > 0 {
        ch.event |= 0x0000_0040;
        vic::raise(INT_DMA_CH0);
    }
}

/// Continue draining channel 1 (serial 0 TX) to the host console wire.
/// Called from `trap_irq` on each timer-IRQ tick alongside `poll_rx`.
/// `write_enable` drains the initial burst synchronously but caps a
/// single drain at 4 KiB; for a TX request larger than that the cap
/// breaks the loop with `countdown > 0` and no completion IRQ. This
/// resumes the drain each tick so the transfer eventually reaches
/// `countdown == 0` and raises `INT_DMA_CH1` (mTxDMAEvent=0x80) — the
/// same terminal condition Einstein's `TPtySerialPortManager::HandleDMA`
/// TX branch reaches one byte at a time
/// (`Emulator/Serial/TPtySerialPortManager.cpp:215-238`).
pub fn poll_tx() {
    // SAFETY: single-threaded.
    let s = unsafe { &mut *DMA.0.get() };
    if !s.channels[1].armed {
        return;
    }
    drain_tx_channel(s, 1);
}

/// Expected-stub traffic (unmodeled channels 2-7): tight budget.
fn log_expected(what: &str, ipa: u64, value: u32) {
    if LOG.expected() {
        kprintln!("{} IPA={:#010x} val={:#010x}", what, ipa, value);
    }
}

fn log_expected_chan(what: &str, bank: u32, channel: u32, register: u32, value: u32) {
    if LOG.expected() {
        kprintln!(
            "{} bank={} ch={} reg={} val={:#010x}",
            what, bank, channel, register, value
        );
    }
}

/// Genuinely-unknown register offsets (discovery): own generous budget
/// so routine stub traffic can't silence it.
fn log_unknown_chan(what: &str, bank: u32, channel: u32, register: u32, value: u32) {
    if LOG.unknown() {
        kprintln!(
            "{} bank={} ch={} reg={} val={:#010x}",
            what, bank, channel, register, value
        );
    }
}

fn halt_unknown_dma(op: &'static str, ipa: u64, value: u32) -> ! {
    kprintln!();
    kprintln!(
        "*** dma::{} IPA={:#010x} val={:#010x} — inside the DMA window but not a modelled register ***",
        op, ipa, value
    );
    kprintln!(
        "  (IPA is inside DMA register window but outside any modeled register."
    );
    kprintln!(
        "   Extend peripherals/dma.rs — see Emulator/TDMAManager.cpp and"
    );
    kprintln!(
        "   Emulator/Serial/TBasicSerialPortManager.cpp.)"
    );
    crate::arch::cpu::halt();
}
