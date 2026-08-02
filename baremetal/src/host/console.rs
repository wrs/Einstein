//! Console output paths.
//!
//! `kprintln!` / `dprintln!` route through the semihosting host stdout
//! (Arm Semihosting `SYS_OPEN(":tt")` + `SYS_WRITE`, HLT `#0xF000`).
//! This frees the PL011 MMIO for the guest's external serial port
//! ("extr") wireup — see `peripherals/serial.rs`.
//!
//! The PL011 itself is still brought up and exposed through
//! `write_byte` for callers that must hit the real wire:
//!
//!   * `diag::tarmac::emit_marker` — the FVP TarmacTrace plugin's UART-token
//!     window-gating watches PL011 byte stream for `<<TRM_START>>` /
//!     `<<TRM_STOP>>`. Semihosting bytes aren't visible to the
//!     plugin.
//!   * `GuestTestPrintByte` HVC (guest-test self-checks).
//!
//! PL011 address/clock come from `crate::host::platform`: raspi3b uses
//! 0x3F20_1000 @ 48 MHz, FVP_Base_RevC uses 0x1C09_0000 @ 14.7456 MHz.
//! 8N1 @ 115200, TX+RX, FIFO on. Both `cargo run` (QEMU
//! `-serial mon:stdio`) and `scripts/fvp` (`bp.pl011_uart0.out_file=-`)
//! deliver PL011 bytes to the host process stdio, the same destination
//! semihosting writes land on, so a single `> /tmp/run` capture sees
//! both streams interleaved.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};
#[cfg(nh_semihost)]
use core::sync::atomic::Ordering;

use crate::host::platform::{UART_BASE, UART_CLOCK_HZ};

// ---- PL011 (raw wire) --------------------------------------------------

const UART_DR: *mut u32 = (UART_BASE + 0x00) as *mut u32;
const UART_FR: *mut u32 = (UART_BASE + 0x18) as *mut u32;
const UART_IBRD: *mut u32 = (UART_BASE + 0x24) as *mut u32;
const UART_FBRD: *mut u32 = (UART_BASE + 0x28) as *mut u32;
const UART_LCRH: *mut u32 = (UART_BASE + 0x2C) as *mut u32;
const UART_CR: *mut u32 = (UART_BASE + 0x30) as *mut u32;
const UART_IMSC: *mut u32 = (UART_BASE + 0x38) as *mut u32;
const UART_ICR: *mut u32 = (UART_BASE + 0x44) as *mut u32;
#[cfg(nh_real_hw)]
const UART_DMACR: *mut u32 = (UART_BASE + 0x48) as *mut u32;

const FR_RXFE: u32 = 1 << 4; // Receive FIFO empty.
const FR_TXFF: u32 = 1 << 5; // Transmit FIFO full.
const LCRH_FEN: u32 = 1 << 4; // Enable TX/RX FIFOs.
const LCRH_WLEN_8: u32 = 0b11 << 5; // 8-bit word length.
const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

const BAUD: u32 = 115_200;
const IBRD_VAL: u32 = UART_CLOCK_HZ / (16 * BAUD);
// Fractional part of clock / (16 * baud) in 1/64-ths.
const FBRD_VAL: u32 = {
    let scaled = (UART_CLOCK_HZ as u64 * 64) / (16 * BAUD as u64);
    (scaled - (IBRD_VAL as u64) * 64) as u32
};

/// Bring up PL011 for 115200 8N1, TX+RX, FIFO on, and open the
/// semihosting host stdout handle. Called exactly once from
/// [`crate::kmain`] on core 0 before any other code that produces
/// console output.
///
/// Without `nh_semihost` (no host is listening) the stdout open is
/// skipped and `kprintln!` goes to the wire instead — through the
/// BCM2835 DMA ring on `nh_real_hw`, polled `write_byte` otherwise.
/// See `write_str` and `tx_dma` below.
pub fn init() {
    // SAFETY: MMIO at fixed, documented addresses; called once at startup
    // before other cores are running any hypervisor code.
    unsafe {
        write_volatile(UART_CR, 0); // Disable entirely while we reconfigure.
        write_volatile(UART_ICR, 0x7FF); // Clear all pending interrupts.
        write_volatile(UART_IBRD, IBRD_VAL);
        write_volatile(UART_FBRD, FBRD_VAL);
        write_volatile(UART_LCRH, LCRH_FEN | LCRH_WLEN_8);
        write_volatile(UART_IMSC, 0); // Mask all interrupts for now.
        write_volatile(UART_CR, CR_UARTEN | CR_TXE | CR_RXE);
    }
    #[cfg(nh_semihost)]
    sh::open_stdout();
    // DMA-driven TX is brought up separately via `init_dma_tx`, which
    // MUST run after `mmu::init` enables Normal-WB cacheable RAM.
    // Before the MMU is on, RAM is treated as Normal Non-cacheable
    // memory, and exclusive accesses (LDXR/STXR, which Rust uses for
    // atomic RMW on a v8.0 core like Cortex-A53) are CONSTRAINED
    // UNPREDICTABLE on Non-cacheable memory — in practice the A53
    // raises a synchronous abort. The ring's bookkeeping uses
    // AtomicU32 RMW ops, so we cannot start the DMA path before the
    // MMU is up. Output before `init_dma_tx` keeps going through the
    // polled `write_byte` fallback inside `write_str`.
}

/// Bring up the DMA-driven TX path. Must be called AFTER `mmu::init`
/// — see comment in `init` above. Idempotent. No-op outside
/// `nh_real_hw`, so callers don't need to mirror the cfg.
pub fn init_dma_tx() {
    #[cfg(nh_real_hw)]
    tx_dma::init();
}

/// Write a single byte to the PL011, busy-waiting until the TX FIFO has
/// room. Reserved for callers that must produce bytes on the actual
/// wire — `diag/tarmac.rs::emit_marker` and the `GuestTestPrintByte` HVC.
/// Console output (`kprintln!`/`dprintln!`) goes through semihosting
/// via `Writer` instead; routing it here would defeat the purpose of
/// freeing PL011 for the guest's serial chip.
pub fn write_byte(b: u8) {
    // SAFETY: MMIO at a fixed, documented address. Volatile access, no aliasing.
    unsafe {
        while read_volatile(UART_FR) & FR_TXFF != 0 {}
        write_volatile(UART_DR, b as u32);
    }
}

/// Non-blocking host-PL011 RX. Returns `Some(byte)` if the receive
/// FIFO has data, `None` otherwise. Used by `peripherals::dma` to
/// stream incoming bytes into the guest's external-serial DMA buffer.
///
/// FR.RXFE bit position confirmed against Linux's
/// `include/linux/amba/serial.h` (UART01x_FR_RXFE = `BIT(4)`),
/// matching the PrimeCell PL011 TRM (ARM DDI 0183G §3.3.3).
pub fn read_byte_nonblock() -> Option<u8> {
    // SAFETY: MMIO at a fixed, documented address. Volatile access, no aliasing.
    unsafe {
        if read_volatile(UART_FR) & FR_RXFE != 0 {
            None
        } else {
            // DR low 8 bits = data; upper bits are error flags we ignore
            // for the host-console use case.
            Some((read_volatile(UART_DR) & 0xFF) as u8)
        }
    }
}

// ---- semihosting host stdout ------------------------------------------
//
// This whole block is compiled out without `nh_semihost` (no host is
// listening). `write_str` then routes to the PL011 wire instead.

#[cfg(nh_semihost)]
mod sh {
    use core::sync::atomic::{AtomicI64, Ordering};

    const SYS_OPEN: u64 = 0x01;
    const SYS_WRITE: u64 = 0x05;
    const SYS_WRITEC: u64 = 0x03;

    /// Arm Semihosting SYS_OPEN mode 4 = "w" (write, text).
    const MODE_WRITE_TEXT: u64 = 0x04;

    /// `:tt` opens the host's stdout (per Arm Semihosting §5.3.1.2).
    static STDOUT_PATH: &[u8] = b":tt\0";

    /// File handle returned by `SYS_OPEN(":tt", "w")`. `-1` sentinel
    /// means "not opened yet" — `write_str` falls back to per-byte
    /// SYS_WRITEC in that case (covers any kprintln issued before
    /// `console::init()`).
    pub(super) static STDOUT_FH: AtomicI64 = AtomicI64::new(-1);

    /// Open `:tt` once, stash the handle for the rest of the run.
    pub(super) fn open_stdout() {
        let args: [u64; 3] = [
            STDOUT_PATH.as_ptr() as u64,
            MODE_WRITE_TEXT,
            (STDOUT_PATH.len() - 1) as u64,
        ];
        let h = unsafe { semihost(SYS_OPEN, args.as_ptr()) };
        if h >= 0 {
            STDOUT_FH.store(h, Ordering::Release);
        }
    }

    /// Execute one semihosting call. `op` is the SYS_* subfunction ID;
    /// `arg` points at the argument block (layout per op, see Arm
    /// Semihosting §5.3). Returns the value placed in x0 by the host.
    ///
    /// SAFETY: HLT #0xF000 is the AArch64 semihosting trap. QEMU's and
    /// the FVP AEM model's handlers intercept it and return to EL2
    /// without disturbing register state beyond x0.
    unsafe fn semihost(op: u64, arg: *const u64) -> i64 {
        let result: u64;
        unsafe {
            core::arch::asm!(
                "hlt #0xF000",
                inout("x0") op => result,
                in("x1") arg as u64,
                options(nostack, preserves_flags),
            );
        }
        result as i64
    }

    /// Push a byte buffer to the host stdout via SYS_WRITE. Short
    /// writes are silently ignored — we'd have nowhere to surface the
    /// error anyway, and dropping a few bytes of console output is a
    /// better failure mode than recursing back into the same write
    /// path.
    pub(super) fn write_bytes(fh: i64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let args: [u64; 3] = [fh as u64, data.as_ptr() as u64, data.len() as u64];
        let _ = unsafe { semihost(SYS_WRITE, args.as_ptr()) };
    }

    /// Per-byte SYS_WRITEC fallback for kprintlns issued before
    /// `init()` completes (i.e., before STDOUT_FH is set). One HLT per
    /// character — slow but bounded to the handful of `print_banner`
    /// / `print_caps` lines that run before `console::init`.
    pub(super) fn writec(b: u8) {
        let byte: u8 = b;
        let ptr = &byte as *const u8 as u64;
        let args = [ptr];
        let _ = unsafe { semihost(SYS_WRITEC, args.as_ptr()) };
    }
}

/// Wall-clock since EL2 reset, in microseconds. Reads CNTPCT_EL0 and
/// scales by CNTFRQ_EL0. Both are AArch64 generic-timer sysregs;
/// CNTPCT_EL0 is monotonic, free-running since power-on. Use as a
/// log prefix via `kprintln!` (which is already wired to call this)
/// to disambiguate the order of fast-firing events that share a
/// single output line in the UART ring.
#[inline(always)]
pub fn now_us() -> u64 {
    // SAFETY: sysreg reads, no side effects.
    let (pct, freq): (u64, u64);
    unsafe {
        core::arch::asm!(
            "mrs {0}, cntpct_el0",
            "mrs {1}, cntfrq_el0",
            out(reg) pct, out(reg) freq,
            options(nomem, nostack, preserves_flags),
        );
    }
    // freq is typically 19_200_000 Hz on BCM2710. Scale to microseconds:
    // us = pct * 1_000_000 / freq.
    if freq == 0 { 0 } else { pct.wrapping_mul(1_000_000) / freq }
}

/// Write a string to the console.
///
/// Default build: routes through Arm Semihosting `SYS_WRITE` to `:tt`,
/// keeping PL011 free for the guest's external-serial chip emulation.
///
/// `nh_real_hw` (real Pi silicon): routes through the DMA-fed TX ring
/// (`tx_dma`). Pre-init bytes (before `init()` has brought up the ring)
/// fall back to the polled `write_byte` path so the kmain banner
/// doesn't disappear.
///
/// No host and no BCM2835 DMA (FVP with `no-semihost`): polled
/// `write_byte` throughout.
pub fn write_str(s: &str) {
    #[cfg(nh_real_hw)]
    {
        if tx_dma::enqueue(s.as_bytes()) {
            return;
        }
        // Pre-init or ring-init failure: fall back to per-byte busy
        // wait so early boot banners still appear.
        for &b in s.as_bytes() {
            write_byte(b);
        }
        return;
    }
    #[cfg(all(not(nh_semihost), not(nh_real_hw)))]
    {
        for &b in s.as_bytes() {
            write_byte(b);
        }
        return;
    }
    #[cfg(nh_semihost)]
    {
        let fh = sh::STDOUT_FH.load(Ordering::Acquire);
        if fh >= 0 {
            sh::write_bytes(fh, s.as_bytes());
        } else {
            for &b in s.as_bytes() {
                sh::writec(b);
            }
        }
    }
}

/// DMA-completion hook called from `host_dma::on_completion`
/// when channel `host_dma::UART_TX_CHANNEL` reports a finished transfer.
/// Always defined so callers don't need to mirror the cfg (dead off
/// real hardware, where the completion IRQ never fires).
#[allow(dead_code)]
#[inline]
pub fn on_tx_done() {
    #[cfg(nh_real_hw)]
    tx_dma::on_done();
}

/// Drain the DMA TX ring to the wire by polling, for halt/panic paths
/// where the completion IRQ will never fire again (IRQs masked, CPU
/// about to park). Without this, a "loud halt" leaves its entire
/// context dump undelivered in the ring and presents as a silent
/// freeze. Always defined; no-op on non-DMA console builds.
pub fn flush_tx_dma_polled() {
    #[cfg(nh_real_hw)]
    tx_dma::flush_polled();
}

/// Writer implementing [`core::fmt::Write`] so callers can `write!` formatted
/// output. Routes through semihosting (`SYS_WRITE` to `:tt`).
pub struct Writer;

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}

/// Polled escape hatch: write directly to the PL011 wire (or, on the
/// semihost build, to the host stdout) without going through the
/// DMA ring. Use this when the fancy printer might be broken — DMA
/// not draining, channel wedged, etc. — and you still need bytes to
/// reach the wire.
///
/// Slow by design: at 115200 baud each byte busy-waits up to 87 µs
/// once the FIFO is full. Don't use on the hot path. The
/// `raw_println!` / `raw_print!` macros below are the ergonomic
/// front door.
pub fn write_str_polled(s: &str) {
    #[cfg(not(nh_semihost))]
    {
        for &b in s.as_bytes() {
            write_byte(b);
        }
    }
    #[cfg(nh_semihost)]
    {
        let fh = sh::STDOUT_FH.load(Ordering::Acquire);
        if fh >= 0 {
            sh::write_bytes(fh, s.as_bytes());
        } else {
            for &b in s.as_bytes() {
                sh::writec(b);
            }
        }
    }
}

/// `core::fmt::Write` adapter that routes through `write_str_polled`.
/// Lets `raw_print!` / `raw_println!` format through `write!` without
/// allocating.
pub struct RawWriter;

impl fmt::Write for RawWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str_polled(s);
        Ok(())
    }
}

// ---- DMA-fed TX ring (real Pi only) ---------------------------------
//
// Without this layer, every kprintln/dprintln byte busy-waits the PL011
// TX FIFO at 115200 baud (~87 µs/byte once full), so a 100-byte log
// line burns ~6 ms of EL2 CPU. That's enough to break the audio pump
// (44.1 kHz frames = 22 µs) and anything else gated on the trap-tail
// cadence.
//
// The ring approach: writers enqueue bytes into a fixed RAM ring with
// IRQs masked; a BCM2835 DMA channel paced by PL011 TX DREQ drains the
// ring at wire rate without further CPU involvement. The completion
// IRQ kicks the next chunk if more bytes have arrived in the meantime.
// Wrap-around is handled by issuing one CB per contiguous tail→end or
// start→head segment.
#[cfg(nh_real_hw)]
mod tx_dma {
    use core::cell::UnsafeCell;
    use core::ptr::addr_of_mut;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use crate::host::host_dma::{
        self, bus_addr_periph, bus_addr_ram, DmaCb, DREQ_UART_TX, TI_DEST_DREQ, TI_INTEN,
        TI_PERMAP_SHIFT, TI_SRC_INC, TI_WAIT_RESP,
    };

    /// 16384 characters. Sustained wire ceiling at 115200 baud is
    /// ~11.5 KB/s, so this absorbs ~1.4 s of bursty logging before
    /// the drop-newest policy kicks in. 8192 was tight enough on
    /// real-hw boot bursts to trip the drop counter occasionally;
    /// doubling costs another 32 KiB of BSS and clears it.
    ///
    /// Storage is one u32 per character, not one byte. The BCM2835
    /// DMA controller has no 8-bit transfer width; the narrowest it
    /// can do is 32-bit reads + 32-bit writes (TI.SRC_WIDTH=0,
    /// TI.DEST_WIDTH=0). Each 32-bit write to PL011 DR transmits
    /// only the low 8 bits, discarding the other 24, so we need the
    /// source to be one byte per 32-bit slot with the character in
    /// the low octet. Per-beat: DMA reads one u32 = one char, writes
    /// it to DR, PL011 transmits it, src pointer advances by 4 bytes
    /// to the next slot. Total ring size in RAM is RING_LEN × 4 = 64
    /// KiB, the price for the controller having no narrower mode.
    const RING_LEN: usize = 16384;

    /// Cache-line-aligned ring storage. Cortex-A53 line size is 64 B
    /// (see `cpu::dc_civac_range`), and the producer cleans whatever
    /// span it's about to DMA, so aligning to 64 B avoids cleaning
    /// adjacent unrelated data.
    #[repr(C, align(64))]
    struct Ring(UnsafeCell<[u32; RING_LEN]>);

    // SAFETY: single-CPU hypervisor; concurrency is between mainline
    // and the EL2 IRQ handler, mediated by IRQ-masked critical
    // sections in `enqueue`.
    unsafe impl Sync for Ring {}

    static RING: Ring = Ring(UnsafeCell::new([0u32; RING_LEN]));

    /// Producer cursor (next write index, mod RING_LEN). Advanced by
    /// `enqueue` only.
    static HEAD: AtomicU32 = AtomicU32::new(0);
    /// Consumer cursor (next byte the DMA will read, mod RING_LEN).
    /// Advanced by `kick` and `on_done`.
    static TAIL: AtomicU32 = AtomicU32::new(0);
    /// Length of the currently-in-flight transfer, in bytes. Zero
    /// when the channel is idle.
    static IN_FLIGHT_LEN: AtomicU32 = AtomicU32::new(0);
    /// True once `init()` succeeds and writes should route through
    /// the ring.
    static READY: AtomicBool = AtomicBool::new(false);
    /// Bytes the drop-newest policy refused since the last successful
    /// enqueue with room. Flushed into the next "<<N bytes dropped>>"
    /// marker injected ahead of normal traffic.
    static DROPPED: AtomicU32 = AtomicU32::new(0);

    /// Control block. Repacked on every `kick`.
    static mut CB: DmaCb = DmaCb::zero();

    /// Bring up the DMA controller's UART-TX channel and mark the
    /// ring ready. Idempotent. Must run after `console::init` has
    /// configured PL011 (the channel destination).
    pub fn init() {
        if !host_dma::init() {
            // Firmware hasn't powered this channel on. Leave READY
            // false and PL011 DMACR at 0; writers fall back to the
            // polled `write_byte` path.
            return;
        }
        // The DMA controller paces destination writes on the
        // peripheral's TX DREQ signal, but PL011 doesn't assert TX
        // DREQ until DMACR.TXDMAE is set. Without this, a DMA
        // configured with `DEST_DREQ` would sit waiting for a DREQ
        // that never comes and no bytes would reach the wire (PL011
        // TRM §3.3.8). Order: set this only AFTER host_dma::init
        // succeeds, so a failed bring-up doesn't change PL011 state
        // out from under the polled fallback path.
        // SAFETY: MMIO write at a fixed peripheral address, identity-
        // mapped at boot and Device-nGnRE after mmu::init.
        unsafe { core::ptr::write_volatile(super::UART_DMACR, 1 << 1) };
        READY.store(true, Ordering::Release);
    }

    /// Append `s` to the ring. Returns `true` if the ring is in use
    /// (callers should not fall back to polled writes), `false` if
    /// the DMA path isn't ready yet.
    ///
    /// Drop-newest on overflow: if the ring is full, the trailing
    /// bytes are dropped and counted; the next successful enqueue
    /// injects a `<<N bytes dropped>>` marker.
    pub fn enqueue(s: &[u8]) -> bool {
        if !READY.load(Ordering::Acquire) {
            return false;
        }
        if s.is_empty() {
            return true;
        }
        // Mask EL2 IRQ/FIQ for the head-advance + cache-clean +
        // kick window. The on_done path runs in IRQ context and
        // touches TAIL/IN_FLIGHT/CB; the producer must not race
        // with it. Single-CPU, so masking is sufficient.
        let daif = mask_irqs();
        // Opportunistic completion check. The real consumer is the
        // BCM2835 IRQ controller dispatch in `trap_irq`, but until
        // EL2 IRQs are unmasked (post-kmain vector install) no
        // completion IRQ ever fires, so the channel would otherwise
        // stop after the very first CB. We're already IRQ-masked,
        // so checking the status register and re-kicking is
        // race-free.
        if IN_FLIGHT_LEN.load(Ordering::Acquire) != 0
            && host_dma::uart_tx_pending()
        {
            host_dma::on_completion(host_dma::UART_TX_CHANNEL);
        }
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            // Best-effort: a fresh ring with no in-flight transfer
            // has full RING_LEN-1 bytes free, plenty for this notice.
            let mut buf = [0u8; 32];
            let n = fmt_dropped(&mut buf, dropped);
            write_unmasked(&buf[..n]);
        }
        write_unmasked(s);
        maybe_kick();
        unmask_irqs(daif);
        true
    }

    /// Called by the DMA-completion IRQ hook in
    /// `host_dma::on_completion`. Advances `TAIL` by
    /// the just-finished length, and kicks the next contiguous
    /// segment if more bytes are queued.
    pub fn on_done() {
        let len = IN_FLIGHT_LEN.swap(0, Ordering::AcqRel);
        if len == 0 {
            return;
        }
        let tail = TAIL.load(Ordering::Relaxed);
        let new_tail = (tail + len) % (RING_LEN as u32);
        TAIL.store(new_tail, Ordering::Release);
        maybe_kick();
    }

    /// Polled drain for halt/panic paths — see `console::flush_tx_dma_polled`.
    ///
    /// Pumps the completion status register and re-kicks segments until
    /// the ring is empty, all without needing the completion IRQ. If the
    /// channel stops making progress (wedged DMA — the very failure some
    /// halts are reporting), falls back after a wall-clock budget: takes
    /// PL011 out of DMA mode and pushes the remaining bytes through the
    /// polled FIFO so the dump reaches the wire regardless.
    pub fn flush_polled() {
        if !READY.load(Ordering::Acquire) {
            return;
        }
        let daif = mask_irqs();
        let (mut now, freq): (u64, u64);
        // SAFETY: counter reads, side-effect free.
        unsafe {
            core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
                options(nomem, nostack, preserves_flags));
            core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq,
                options(nomem, nostack, preserves_flags));
        }
        // 64 KiB of ring at 115200 baud ≈ 6 s; give it 8 s.
        let deadline = now + freq.saturating_mul(8);
        loop {
            let in_flight = IN_FLIGHT_LEN.load(Ordering::Acquire);
            let head = HEAD.load(Ordering::Acquire);
            let tail = TAIL.load(Ordering::Acquire);
            if in_flight == 0 && head == tail {
                unmask_irqs(daif);
                return;
            }
            if in_flight != 0 && host_dma::uart_tx_pending() {
                host_dma::on_completion(host_dma::UART_TX_CHANNEL);
            } else if in_flight == 0 {
                maybe_kick();
            }
            // SAFETY: counter read, side-effect free.
            unsafe {
                core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
                    options(nomem, nostack, preserves_flags));
            }
            if now >= deadline {
                break;
            }
        }
        // DMA stopped making progress. Disable PL011's DMA handshake and
        // hand-feed the remaining slots through the polled FIFO.
        // SAFETY: MMIO write; channel is being abandoned on a halt path.
        unsafe { core::ptr::write_volatile(super::UART_DMACR, 0) };
        let head = HEAD.load(Ordering::Acquire);
        let mut tail = TAIL.load(Ordering::Acquire) as usize;
        // SAFETY: halt path, single core, IRQs masked — exclusive ring
        // access; the DMA consumer is disabled above.
        let buf = unsafe { &*RING.0.get() };
        while tail != head as usize {
            super::write_byte(buf[tail] as u8);
            tail = (tail + 1) % RING_LEN;
        }
        TAIL.store(head, Ordering::Release);
        IN_FLIGHT_LEN.store(0, Ordering::Release);
        unmask_irqs(daif);
    }

    // ---- internals -------------------------------------------------

    /// Push `s` into the ring without touching IRQ state — caller has
    /// already masked. Drops bytes that overflow.
    ///
    /// Each source byte goes into the low octet of one u32 slot.
    /// See the `RING_LEN` doc-comment for why.
    fn write_unmasked(s: &[u8]) {
        let head = HEAD.load(Ordering::Relaxed) as usize;
        let tail = TAIL.load(Ordering::Acquire) as usize;
        // Slot indices, not byte indices. `used` is the number of
        // character slots physically in the ring not yet consumed
        // by DMA. `free` is the slot capacity minus used minus the
        // one empty slot the head==tail convention reserves for
        // "empty".
        let used = (head + RING_LEN - tail) % RING_LEN;
        let free = RING_LEN - used - 1;
        let take = core::cmp::min(s.len(), free);
        let dropped = s.len() - take;
        if dropped > 0 {
            DROPPED.fetch_add(dropped as u32, Ordering::Relaxed);
        }
        // SAFETY: We hold the IRQ-masked critical section and are
        // the sole writer of slots [head..head+take). The reads of
        // the same slots by the DMA controller, if any, are
        // governed by IN_FLIGHT_LEN — and IN_FLIGHT_LEN only covers
        // slots strictly before HEAD at the moment kick() built
        // the CB.
        unsafe {
            let buf = &mut *RING.0.get();
            for i in 0..take {
                buf[(head + i) % RING_LEN] = s[i] as u32;
            }
        }
        let new_head = ((head + take) % RING_LEN) as u32;
        HEAD.store(new_head, Ordering::Release);
    }

    /// If the channel is idle and bytes are queued, build a CB for
    /// the next contiguous segment and arm the DMA. Caller is in
    /// IRQ-masked context.
    fn maybe_kick() {
        if IN_FLIGHT_LEN.load(Ordering::Acquire) != 0 {
            return;
        }
        let head = HEAD.load(Ordering::Acquire) as usize;
        let tail = TAIL.load(Ordering::Acquire) as usize;
        if head == tail {
            return;
        }
        // Contiguous slot segment: tail .. min(head, RING_LEN). A
        // wrap is handled by the *next* on_done firing maybe_kick
        // again.
        let end = if head > tail { head } else { RING_LEN };
        let len = end - tail; // count of u32 slots = chars
        let byte_len = len * core::mem::size_of::<u32>();
        // SAFETY: RING.0 is a `'static` UnsafeCell; we're the sole
        // owner of slots [tail..tail+len) until on_done.
        let ring_ptr = unsafe { (*RING.0.get()).as_ptr() } as u64;
        let src_arm_phys = ring_ptr + (tail * core::mem::size_of::<u32>()) as u64;
        // Flush ARM L1/L2 of the segment so the DMA reading via the
        // uncached 0xC000_0000 bus alias sees what we wrote
        // (BCM2835 §1.2.3: bus alias bypasses ARM caches).
        crate::arch::cpu::dc_civac_range(src_arm_phys, byte_len);
        // Build the single-shot CB. SRC_INC + 32-bit beats from
        // RAM; DEST_DREQ paced by PL011 TX DREQ writes one beat
        // per char into PL011 DR (low octet of the 32-bit slot is
        // transmitted, upper 24 are discarded by the chip);
        // WAIT_RESP prevents AXI pipelining writes ahead of FIFO
        // drains; INTEN raises the channel's completion IRQ.
        let ti = (DREQ_UART_TX << TI_PERMAP_SHIFT)
            | TI_SRC_INC
            | TI_DEST_DREQ
            | TI_WAIT_RESP
            | TI_INTEN;
        // SAFETY: single-writer (this function under IRQ mask);
        // the DMA controller will only read CB after we write
        // CONBLK_AD inside arm_uart_tx.
        unsafe {
            let cb = &mut *addr_of_mut!(CB);
            cb.ti = ti;
            cb.source_ad = bus_addr_ram(src_arm_phys);
            cb.dest_ad = bus_addr_periph(super::UART_DR as u32);
            // TXFR_LEN is in bytes; one beat = 4 bytes of source =
            // one transmitted char.
            cb.txfr_len = byte_len as u32;
            cb.stride = 0;
            cb.nextconbk = 0;
        }
        IN_FLIGHT_LEN.store(len as u32, Ordering::Release);
        // SAFETY: CB lives in a static; contents are stable until
        // the channel raises completion (which sets IN_FLIGHT_LEN
        // back to 0 in on_done before maybe_kick rewrites it).
        unsafe {
            host_dma::arm_uart_tx(&*addr_of_mut!(CB));
        }
    }

    /// Format "<<N bytes dropped>>" into `buf` and return the length.
    /// Bounded to ~32 chars even for a u32-max counter.
    fn fmt_dropped(buf: &mut [u8; 32], n: u32) -> usize {
        let prefix = b"<<";
        let mid = b" bytes dropped>>";
        // Decimal-encode n. u32::MAX is 10 digits; buf is sized for
        // prefix+10+mid+spare = 28 bytes plus padding.
        let mut digits = [0u8; 10];
        let mut d = 0usize;
        let mut v = n;
        if v == 0 {
            digits[0] = b'0';
            d = 1;
        } else {
            while v > 0 {
                digits[d] = b'0' + (v % 10) as u8;
                v /= 10;
                d += 1;
            }
        }
        let mut p = 0usize;
        for &b in prefix {
            buf[p] = b;
            p += 1;
        }
        for i in 0..d {
            buf[p] = digits[d - 1 - i];
            p += 1;
        }
        for &b in mid {
            buf[p] = b;
            p += 1;
        }
        p
    }

    /// Mask EL2 IRQ + FIQ, return previous DAIF state.
    #[inline]
    fn mask_irqs() -> u64 {
        let daif: u64;
        // SAFETY: sysreg read + write to DAIF, side-effect on IRQ mask.
        unsafe {
            core::arch::asm!(
                "mrs {}, daif",
                "msr daifset, #3",
                out(reg) daif,
                options(nostack, preserves_flags),
            );
        }
        daif
    }

    /// Restore DAIF to its previous value.
    #[inline]
    fn unmask_irqs(daif: u64) {
        // SAFETY: sysreg write to DAIF, restoring caller-saved state.
        unsafe {
            core::arch::asm!(
                "msr daif, {}",
                in(reg) daif,
                options(nostack, preserves_flags),
            );
        }
    }
}
