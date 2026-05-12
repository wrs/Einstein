//! BCM2835 SDHOST controller driver (polled mode, no IRQ, no DMA).
//!
//! Targets the Raspberry Pi Zero 2 W's micro-SD slot, which on
//! BCM2710 routes to the **SDHOST** controller (NOT the SDHCI-style
//! Arasan EMMC block — that one is wired to the on-package
//! BCM43436B0 Wi-Fi/BT chip via SDIO on this SoC).
//!
//! Layering:
//! - Register access (`read`/`write`) — trivial volatile MMIO.
//! - Command execution (`send_cmd`) — bit-pack SDCMD, poll SDHSTS,
//!   read SDRSPx. Returns the response or an error code.
//! - Init / probe (`init`) — CMD0, CMD8, ACMD41, CMD2, CMD3, CMD9,
//!   CMD7. Determines whether the card is SDHC (block-addressed) or
//!   SDSC (byte-addressed) and stashes the RCA.
//! - Block I/O (`read_block` / `write_block`) — single-block PIO
//!   transfer via SDDATA FIFO.
//!
//! Ported from Circle's
//! [`addon/SDCard/sdhost.cpp`](https://github.com/rsta2/circle/blob/master/addon/SDCard/sdhost.cpp)
//! (P. Elwell @ RPi Trading, Rust port-by-hand). Constants live in
//! [`super::regs`].
//!
//! ## What's stubbed
//!
//! Two pieces are intentionally not implemented yet and will panic
//! loudly if reached. Both need real-hardware bring-up to validate:
//!
//! - [`gpio_setup`] — pinmux of GPIO 48–53 to the SDHOST ALT function
//!   (and which ALT it is on the Zero 2 W vs. the Pi 3B — they
//!   differ). The wrong ALT will leave the bus floating; the wrong
//!   pull configuration on D0–D3 will produce CRC errors.
//! - [`clock_setup`] — the Pi firmware controls the SDHOST clock via
//!   a mailbox property tag (`RPI_FIRMWARE_SET_CLOCK_RATE` for
//!   `CLOCK_ID_CORE`), and the driver computes the SDCDIV divider
//!   from the resulting core clock. We don't have a mailbox driver
//!   yet (Phase 4 will need one anyway); writing a poll-mode mailbox
//!   client is small but I'd rather land that as its own change.
//!
//! Until both stubs are filled in, [`SdHost::init`] is gated by a
//! `cfg!` and panics with a pointer to this comment. The rest of the
//! driver (command pack, FIFO drain, MBR decode, FAT shim) is
//! independently reviewable and compile-tested.

#![allow(dead_code)] // Reachable once the SDHOST bring-up is wired in.

use core::ptr::{read_volatile, write_volatile};

use super::regs::*;

/// SDHOST base on the BCM2710 peripheral window.
const SDHOST_BASE: usize = 0x3F20_2000;

/// Result of a single command execution.
#[derive(Debug, Clone, Copy)]
pub enum CmdError {
    /// `SDHSTS_CMD_TIME_OUT` — card didn't respond in the 1.6 ms window
    /// programmed via SDTOUT.
    Timeout,
    /// CRC7 mismatch on the response. Usually a bus / pull issue.
    CrcError,
    /// FIFO over- or under-run during a data-bearing command.
    FifoError,
    /// `SDHSTS_REW_TIME_OUT` during data phase.
    DataTimeout,
    /// SW timeout polling `SDCMD_NEW_FLAG` — we never observed the
    /// hardware accept the command.
    HardwareWedge,
}

/// Card-type signal we get from the OCR response to ACMD41.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardCapacity {
    /// SDSC — byte-addressed. CMD17 argument is a byte offset.
    StandardCapacity,
    /// SDHC / SDXC — block-addressed (512 B/block). CMD17 argument is
    /// a sector index.
    HighCapacity,
}

/// State the driver needs to keep between calls.
pub struct SdHost {
    rca: u32,
    capacity: CardCapacity,
}

impl SdHost {
    /// Bring up the controller and enumerate the card. Returns a
    /// driver instance ready for `read_block` / `write_block`.
    ///
    /// **Not yet runnable on real hardware**: see the module-level
    /// "What's stubbed" note.
    #[allow(unreachable_code, clippy::diverging_sub_expression)]
    pub fn init() -> Result<Self, CmdError> {
        gpio_setup();
        clock_setup();

        reset_controller();
        delay_us(10_000);

        // Card power-up via SDVDD (1 = on).
        write_reg(SDVDD, 1);
        delay_us(10_000);

        // Default host config: relax CMD line, enable wide internal
        // bus (4-bit data path inside the controller; outside-bus
        // width is negotiated separately via ACMD6).
        write_reg(SDHCFG, SDHCFG_WIDE_INT_BUS | SDHCFG_REL_CMD_LINE);
        // Max divider until the card is up; we'll speed up later.
        write_reg(SDCDIV, SDCDIV_MAX_CDIV);
        write_reg(SDHSTS, SDHSTS_CLEAR_MASK);

        // Identification phase. Per SD Physical Layer Spec §4.2.
        send_cmd(CMD_GO_IDLE_STATE, 0, ResponseKind::None)?;
        delay_us(1_000);

        // CMD8: probe for SDv2 / supply-voltage match. A v1.x card
        // returns CMD_TIME_OUT here; that's not a fatal error.
        let v2 = match send_cmd(CMD_SEND_IF_COND, CMD8_VHS_27_36_PATTERN, ResponseKind::Short) {
            Ok(resp) => (resp & CMD8_R7_PATTERN_MASK) == CMD8_R7_PATTERN_VALUE,
            Err(CmdError::Timeout) => false,
            Err(e) => return Err(e),
        };

        // ACMD41 loop until OCR_BUSY clears. Argument carries our
        // voltage window and (for v2 cards) the HCS bit.
        let arg = OCR_VOLT_3V2_3V4 | if v2 { OCR_HCS } else { 0 };
        let ocr = loop {
            send_cmd(CMD_APP_CMD, 0, ResponseKind::Short)?;
            let resp = send_cmd(ACMD_SD_SEND_OP_COND, arg, ResponseKind::Short)?;
            if resp & OCR_BUSY != 0 {
                break resp;
            }
            delay_us(10_000);
        };
        let capacity = if v2 && (ocr & OCR_CCS) != 0 {
            CardCapacity::HighCapacity
        } else {
            CardCapacity::StandardCapacity
        };

        // CMD2 — fetch CID (we don't decode it; just complete the
        // protocol step).
        send_cmd(CMD_ALL_SEND_CID, 0, ResponseKind::Long)?;
        // CMD3 — card returns its RCA in bits [31:16].
        let rca = send_cmd(CMD_SEND_RELATIVE_ADDR, 0, ResponseKind::Short)? & 0xFFFF_0000;
        // CMD9 — CSD; again we don't decode it yet.
        send_cmd(CMD_SEND_CSD, rca, ResponseKind::Long)?;
        // CMD7 — select the card, putting it in transfer state.
        send_cmd(CMD_SELECT_CARD, rca, ResponseKind::Short)?;

        // Set 512-byte block length for byte-addressed cards. SDHC
        // ignores CMD16 (always 512); send it anyway for uniformity.
        send_cmd(CMD_SET_BLOCKLEN, 512, ResponseKind::Short)?;

        // TODO: ACMD6 to switch to 4-bit bus once we trust the
        // single-line path. For first bring-up keep 1-bit; CRC errors
        // are easier to diagnose without bus-width complications.

        Ok(SdHost { rca, capacity })
    }

    /// Read one 512-byte sector. `lba` is a sector index regardless
    /// of card capacity — we translate to a byte offset for SDSC
    /// cards internally.
    pub fn read_block(&self, lba: u32, buf: &mut [u8; 512]) -> Result<(), CmdError> {
        let arg = match self.capacity {
            CardCapacity::HighCapacity => lba,
            CardCapacity::StandardCapacity => lba.wrapping_mul(512),
        };
        write_reg(SDHBCT, 512);
        write_reg(SDHBLC, 1);
        send_cmd_kind(CMD_READ_SINGLE_BLOCK, arg, ResponseKind::Short, CmdDir::Read)?;
        drain_fifo_to(buf)
    }

    /// Write one 512-byte sector. See [`read_block`] for argument
    /// semantics.
    pub fn write_block(&self, lba: u32, buf: &[u8; 512]) -> Result<(), CmdError> {
        let arg = match self.capacity {
            CardCapacity::HighCapacity => lba,
            CardCapacity::StandardCapacity => lba.wrapping_mul(512),
        };
        write_reg(SDHBCT, 512);
        write_reg(SDHBLC, 1);
        send_cmd_kind(CMD_WRITE_SINGLE_BLOCK, arg, ResponseKind::Short, CmdDir::Write)?;
        fill_fifo_from(buf)
    }

    pub fn capacity(&self) -> CardCapacity {
        self.capacity
    }

    pub fn rca(&self) -> u32 {
        self.rca
    }
}

// ---- Register helpers ------------------------------------------------

#[inline]
fn read_reg(off: usize) -> u32 {
    // SAFETY: SDHOST MMIO at fixed BCM2710 base; identity-mapped
    // Device-nGnRE by mmu::init via DEVICE_MMIO_START..DEVICE_MMIO_END.
    unsafe { read_volatile((SDHOST_BASE + off) as *const u32) }
}

#[inline]
fn write_reg(off: usize, val: u32) {
    // SAFETY: see read_reg.
    unsafe { write_volatile((SDHOST_BASE + off) as *mut u32, val) }
}

fn reset_controller() {
    // Power off, zero command/argument/timeout/divider, clear status,
    // program FIFO thresholds. Sequence taken from Circle's
    // `reset_internal()`.
    write_reg(SDVDD, 0);
    write_reg(SDCMD, 0);
    write_reg(SDARG, 0);
    // 1.6 ms timeout at the core clock the firmware leaves us with;
    // overwritten once `clock_setup` runs.
    write_reg(SDTOUT, 0xF00000);
    write_reg(SDCDIV, 0);
    write_reg(SDHSTS, SDHSTS_CLEAR_MASK);
    let edm = (FIFO_READ_THRESHOLD << SDEDM_READ_THRESHOLD_SHIFT)
        | (FIFO_WRITE_THRESHOLD << SDEDM_WRITE_THRESHOLD_SHIFT);
    write_reg(SDEDM, edm);
}

// ---- Command path ----------------------------------------------------

#[derive(Clone, Copy)]
enum ResponseKind {
    None,
    Short, // 48-bit; SDRSP0 holds bits [39:8] of the response token.
    Long,  // 136-bit; SDRSP0..3 hold the CID/CSD contents.
}

#[derive(Clone, Copy)]
enum CmdDir {
    NoData,
    Read,
    Write,
}

fn send_cmd(cmd: u8, arg: u32, kind: ResponseKind) -> Result<u32, CmdError> {
    send_cmd_kind(cmd, arg, kind, CmdDir::NoData)
}

fn send_cmd_kind(cmd: u8, arg: u32, kind: ResponseKind, dir: CmdDir) -> Result<u32, CmdError> {
    // Clear stale status from the previous transfer.
    write_reg(SDHSTS, SDHSTS_CLEAR_MASK);

    write_reg(SDARG, arg);
    let mut cmd_word: u32 = SDCMD_NEW_FLAG | (cmd as u32 & SDCMD_CMD_MASK);
    cmd_word |= match kind {
        ResponseKind::None => SDCMD_NO_RESPONSE,
        ResponseKind::Short => 0,
        ResponseKind::Long => SDCMD_LONG_RESPONSE,
    };
    cmd_word |= match dir {
        CmdDir::NoData => 0,
        CmdDir::Read => SDCMD_READ_CMD,
        CmdDir::Write => SDCMD_WRITE_CMD,
    };
    // Treat all R1b commands as busy-wait. We don't currently issue
    // any (CMD7/CMD12 are R1b; the only one we send is CMD7 during
    // init, which the hardware can wait for without us looking at the
    // BUSY line). Left as a TODO to revisit if a future caller needs
    // CMD12 mid-transfer.
    if matches!(cmd, CMD_SELECT_CARD | CMD_STOP_TRANSMISSION) {
        cmd_word |= SDCMD_BUSYWAIT;
    }
    write_reg(SDCMD, cmd_word);

    // Poll NEW_FLAG to drop. Bounded; if we sit here too long the
    // controller is wedged (clock not running, card not present,
    // bus floating).
    for _ in 0..1_000_000 {
        let c = read_reg(SDCMD);
        if (c & SDCMD_NEW_FLAG) == 0 {
            if (c & SDCMD_FAIL_FLAG) != 0 {
                let hsts = read_reg(SDHSTS);
                return Err(map_hsts_error(hsts));
            }
            return Ok(read_reg(SDRSP0));
        }
    }
    Err(CmdError::HardwareWedge)
}

fn map_hsts_error(hsts: u32) -> CmdError {
    if hsts & SDHSTS_CMD_TIME_OUT != 0 {
        CmdError::Timeout
    } else if hsts & SDHSTS_CRC7_ERROR != 0 || hsts & SDHSTS_CRC16_ERROR != 0 {
        CmdError::CrcError
    } else if hsts & SDHSTS_FIFO_ERROR != 0 {
        CmdError::FifoError
    } else if hsts & SDHSTS_REW_TIME_OUT != 0 {
        CmdError::DataTimeout
    } else {
        CmdError::HardwareWedge
    }
}

// ---- FIFO drain / fill ----------------------------------------------

/// Read 512 bytes (128 32-bit words) from the FIFO into `buf`.
///
/// We poll `SDHSTS_DATA_FLAG` per word rather than burst-checking
/// against the FIFO threshold; this is simpler and the loss vs. the
/// burst path is irrelevant for a once-per-snapshot flash write.
fn drain_fifo_to(buf: &mut [u8; 512]) -> Result<(), CmdError> {
    for word_ix in 0..128 {
        wait_for_data()?;
        let w = read_reg(SDDATA);
        let off = word_ix * 4;
        buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
    }
    finish_data_phase()
}

fn fill_fifo_from(buf: &[u8; 512]) -> Result<(), CmdError> {
    for word_ix in 0..128 {
        wait_for_fifo_space()?;
        let off = word_ix * 4;
        let w = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        write_reg(SDDATA, w);
    }
    finish_data_phase()
}

fn wait_for_data() -> Result<(), CmdError> {
    for _ in 0..2_000_000 {
        let h = read_reg(SDHSTS);
        if h & SDHSTS_ERROR_MASK != 0 {
            return Err(map_hsts_error(h));
        }
        if h & SDHSTS_DATA_FLAG != 0 {
            return Ok(());
        }
    }
    Err(CmdError::HardwareWedge)
}

fn wait_for_fifo_space() -> Result<(), CmdError> {
    // The SDHOST exposes "FIFO has space" via the same DATA_FLAG bit,
    // direction-dependent. For writes Circle treats it as a single
    // ready/not-ready signal; we follow that lead. If we observe
    // FIFO_ERROR in practice the threshold-aware path will replace
    // this.
    wait_for_data()
}

fn finish_data_phase() -> Result<(), CmdError> {
    // Wait for the block to land (BLOCK_IRPT) and clear the sticky
    // status flag so the next command starts clean.
    for _ in 0..2_000_000 {
        let h = read_reg(SDHSTS);
        if h & SDHSTS_ERROR_MASK != 0 {
            return Err(map_hsts_error(h));
        }
        if h & SDHSTS_BLOCK_IRPT != 0 {
            write_reg(SDHSTS, SDHSTS_BLOCK_IRPT);
            return Ok(());
        }
    }
    Err(CmdError::HardwareWedge)
}

// ---- Stubs (see module-level "What's stubbed") -----------------------

/// Pinmux GPIO 48–53 onto the SDHOST controller. Needs careful
/// per-board verification (Pi 3B and Pi Zero 2 W are not identical at
/// the GPIO-routing level) before this can run on hardware.
fn gpio_setup() {
    unimplemented!(
        "src/sd/sdhost.rs::gpio_setup not yet implemented — see module \
         doc 'What's stubbed'. Needs ALT function + pull config for \
         GPIO 48..53 verified against the BCM2835 ARM Peripherals manual \
         and the Pi Zero 2 W datasheet."
    );
}

/// Program the SDHOST clock via the firmware mailbox. Requires a
/// mailbox driver we don't have yet.
fn clock_setup() {
    unimplemented!(
        "src/sd/sdhost.rs::clock_setup not yet implemented — see module \
         doc 'What's stubbed'. Needs a poll-mode VC mailbox client to \
         issue RPI_FIRMWARE_SET_CLOCK_RATE on CLOCK_ID_CORE, then derive \
         the SDCDIV divider from the resulting rate."
    );
}

fn delay_us(us: u32) {
    // Coarse busy loop. Refined once we have a real-silicon timer
    // reference; CNTPCT runs at 19.2 MHz on the Zero 2 W so a "rough"
    // loop count of ~50 per microsecond is a placeholder, not a
    // calibrated delay.
    let iters = us.saturating_mul(50);
    for _ in 0..iters {
        unsafe { core::arch::asm!("nop") }
    }
}
