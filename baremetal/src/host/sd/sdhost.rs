//! BCM2835 SDHOST controller driver.
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
//! - DMA block I/O — DREQ-paced writes through DMA channel 6:
//!   `write_block_dma` / `write_sectors_dma` (polled), and the
//!   `start_sectors_dma` / `finish_sectors_dma` async pair that
//!   drives the background flash autosave
//!   (see `docs/SD_DMA_AUTOSAVE.md`).
//!
//! Ported from Circle's
//! [`addon/SDCard/sdhost.cpp`](https://github.com/rsta2/circle/blob/master/addon/SDCard/sdhost.cpp)
//! (P. Elwell @ RPi Trading, Rust port-by-hand). Constants live in
//! [`super::regs`].

#![allow(dead_code)] // Parts go unused in probe-only / non-DMA feature combinations.

use core::ptr::{read_volatile, write_volatile};

use super::regs::*;

/// SDHOST base on the BCM2710 peripheral window.
const SDHOST_BASE: usize = 0x3F20_2000;

/// SDHCFG configuration for the bus while no data transfer is
/// active. Matches Linux's bcm2835-sdhost base config:
///
/// - `WIDE_INT_BUS`: enable 4-bit internal data path. Always on per
///   Linux (independent of external bus width).
/// - `SLOW_CARD`: timing margin for slower cards — Linux sets this
///   unconditionally.
/// - `BUSY_IRPT_EN`: although we poll, this gates the controller's
///   busy-wait FSM in a way that's needed for correctness on R1b
///   commands. Same observation as DATA_IRPT_EN below — name is
///   misleading.
///
/// `SDHCFG_WIDE_EXT_BUS` is OR'd in dynamically when ACMD6 has
/// switched the card to 4-bit external; see `SdHost::hcfg_base`.
const SDHCFG_BASE_NARROW: u32 = SDHCFG_WIDE_INT_BUS | SDHCFG_SLOW_CARD | SDHCFG_BUSY_IRPT_EN;

/// Bit to OR into SDHCFG for data-bearing commands. Despite the
/// name, `SDHCFG_DATA_IRPT_EN` is required even when polling — on
/// this controller it gates the FSM's data-movement path, not just
/// interrupt generation. Without it, the FSM walks the read/write
/// states but doesn't actually move bytes through the FIFO.
const SDHCFG_DATA_BIT: u32 = SDHCFG_DATA_IRPT_EN;

/// Result of a single command execution.
#[derive(Debug, Clone, Copy)]
pub enum CmdError {
    /// `SDHSTS_CMD_TIME_OUT` — card didn't respond in the 1.6 ms window
    /// programmed via SDTOUT.
    Timeout,
    /// CRC7 mismatch on the command **response** token. Usually
    /// driven by bus signal integrity (clock too high, weak pulls).
    Crc7Error,
    /// CRC16 mismatch on a **data** block. Same root causes as
    /// Crc7Error but only during data-bearing transfers.
    Crc16Error,
    /// FIFO over- or under-run during a data-bearing command.
    FifoError,
    /// `SDHSTS_REW_TIME_OUT` during data phase.
    DataTimeout,
    /// SW timeout polling `SDCMD_NEW_FLAG` — we never observed the
    /// hardware accept the command.
    HardwareWedge,
    /// DMA-path failure: the SD-TX channel isn't firmware-enabled, or
    /// it latched CS.ERROR during the transfer.
    DmaError,
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
    /// SDHCFG value to install when no data transfer is active.
    /// Always includes `SDHCFG_BASE_NARROW`; additionally OR's in
    /// `SDHCFG_WIDE_EXT_BUS` after ACMD6 has switched the card to
    /// 4-bit. We never re-narrow once widened, so the field is set
    /// in `init` and read-only afterwards.
    hcfg_base: u32,
    /// Total addressable 512-byte sectors, decoded from the CSD at
    /// `init`. `u32::MAX` if the CSD structure version was unknown
    /// (decode declined to guess) — see `decode_csd_num_blocks`.
    num_blocks: u32,
}

impl SdHost {
    /// Bring up the controller and enumerate the card. Returns a
    /// driver instance ready for `read_block` / `write_block`. This is
    /// the production SD path on the Pi Zero 2 W (flash persistence).
    pub fn init() -> Result<Self, CmdError> {
        gpio_setup();

        let core_clock = match clock_setup() {
            Ok(r) => r,
            Err(_) => return Err(CmdError::HardwareWedge),
        };

        reset_controller();
        delay_us(10_000);

        // Card power-up via SDVDD (1 = on).
        write_reg(SDVDD, 1);
        delay_us(10_000);

        // Host config for non-data commands. See `SDHCFG_BASE_NARROW`
        // doc for what each bit does — the takeaway is that on this
        // controller the *_IRPT_EN bits gate FSM functionality, not
        // just interrupt generation. `WIDE_EXT_BUS` is added later
        // (if ACMD6 succeeds).
        write_reg(SDHCFG, SDHCFG_BASE_NARROW);
        // Identification-phase clock: ≤400 kHz on the SD bus per the
        // SD spec. The SDHOST divides core_clock by (cdiv + 2).
        program_sdcdiv(core_clock, 400_000);
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
        let mut acmd41_iter: u32 = 0;
        let ocr = loop {
            send_cmd(CMD_APP_CMD, 0, ResponseKind::Short)?;
            let resp = send_cmd(ACMD_SD_SEND_OP_COND, arg, ResponseKind::Short)?;
            acmd41_iter += 1;
            if resp & OCR_BUSY != 0 {
                break resp;
            }
            if acmd41_iter >= 1000 {
                return Err(CmdError::HardwareWedge);
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
        // CMD9 — fetch the CSD (136-bit R2). The controller leaves the
        // response in SDRSP0..3; read all four to decode the card
        // capacity. SDRSP3 = bits[127:96] (top), SDRSP0 = bits[31:0]
        // (bottom), matching the standard CRC-stripped R2 layout
        // (verified against Linux bcm2835-sdhost: it copies SDRSP0..3
        // straight into resp[3..0] with no cross-register shift).
        send_cmd(CMD_SEND_CSD, rca, ResponseKind::Long)?;
        let csd = [
            read_reg(SDRSP0),
            read_reg(SDRSP1),
            read_reg(SDRSP2),
            read_reg(SDRSP3),
        ];
        let num_blocks = decode_csd_num_blocks(&csd);
        // CMD7 — select the card, putting it in transfer state.
        send_cmd(CMD_SELECT_CARD, rca, ResponseKind::Short)?;

        // Set 512-byte block length for byte-addressed cards. SDHC
        // ignores CMD16 (always 512); send it anyway for uniformity.
        send_cmd(CMD_SET_BLOCKLEN, 512, ResponseKind::Short)?;

        // ACMD6: switch the card to 4-bit external bus. 4-bit is
        // mandatory in the SD spec so this should always succeed,
        // but treat failure as soft — stay at 1-bit; the always-on
        // "bus ready" summary below reports the resulting width.
        // Order matters: switch the *card* first, then the
        // *controller* (writing SDHCFG_WIDE_EXT_BUS). Reversed, we'd
        // drive 4-bit signals to a card still expecting 1-bit and
        // mismatch on every transfer.
        let mut hcfg_base = SDHCFG_BASE_NARROW;
        if (|| -> Result<u32, CmdError> {
            send_cmd(CMD_APP_CMD, rca, ResponseKind::Short)?;
            send_cmd(ACMD_SET_BUS_WIDTH, 2, ResponseKind::Short)
        })()
        .is_ok()
        {
            hcfg_base |= SDHCFG_WIDE_EXT_BUS;
            write_reg(SDHCFG, hcfg_base);
        }

        // Bump the SD bus clock to default-speed 25 MHz now that the
        // card is in transfer state. The SD spec allows this
        // immediately after CMD7 (no SD switch command required for
        // DS mode). 400 kHz / 1-bit is fine for identification but
        // way too slow for a 64 KiB / 8 MiB flash save: at 400 kHz a
        // full save is ~3 minutes, which blocks EL2 long enough to
        // perturb the guest's timing assumptions during early boot.
        //
        // The early CrcError we hit at 25 MHz turned out to be a
        // FIFO-drain bug (DATA_FLAG vs FIFO_FILL) and a SDHCFG bit
        // (DATA_IRPT_EN gating the FSM data path), both since
        // fixed. Reads have been stable at 400 kHz across full and
        // incremental saves; 25 MHz is the next variable to flip.
        //
        // ACMD6 above handled the 4-bit switch.
        program_sdcdiv(core_clock, 25_000_000);
        // Boot-time summary of what bus we ended up on.
        // core_clock / (cdiv+2) = bus clock in Hz; the bus-width
        // comes from hcfg_base.
        let cdiv = read_reg(SDCDIV);
        let bus_hz = core_clock / (cdiv + 2);
        let width = if hcfg_base & SDHCFG_WIDE_EXT_BUS != 0 {
            4
        } else {
            1
        };
        crate::kprintln!(
            "sd: bus ready ({}.{} MHz, {}-bit)",
            bus_hz / 1_000_000,
            (bus_hz / 100_000) % 10,
            width
        );
        if num_blocks == u32::MAX {
            crate::kprintln!("sd: capacity unknown (CSD undecoded)");
        } else {
            // num_blocks * 512 / 1 MiB, computed without overflowing u32.
            crate::kprintln!("sd: capacity {} MiB ({} sectors)", num_blocks / 2048, num_blocks);
        }

        Ok(SdHost {
            rca,
            capacity,
            hcfg_base,
            num_blocks,
        })
    }

    /// Total addressable 512-byte sectors on the card, decoded from
    /// the CSD at init. `u32::MAX` if the CSD structure version was
    /// unknown.
    pub fn num_blocks(&self) -> u32 {
        self.num_blocks
    }

    /// Read one 512-byte sector. `lba` is a sector index regardless
    /// of card capacity — we translate to a byte offset for SDSC
    /// cards internally.
    ///
    /// Takes `&self`: the underlying state lives in MMIO registers,
    /// not in this struct, and the embedded-sdmmc `BlockDevice` trait
    /// uses `&self` (interior-mutability model). Concurrent access
    /// to the controller is not safe but we're single-core EL2;
    /// nothing else touches SDHOST while this runs.
    pub fn read_block(&self, lba: u32, buf: &mut [u8; 512]) -> Result<(), CmdError> {
        let arg = match self.capacity {
            CardCapacity::HighCapacity => lba,
            CardCapacity::StandardCapacity => lba.wrapping_mul(512),
        };
        prepare_data(self.hcfg_base, 512, 1);
        // Single exit through the SDHCFG restore below: `prepare_data`
        // sets `DATA_IRPT_EN`, which gates the FSM's data path, and it
        // must be cleared back to `hcfg_base` on *every* path — an
        // early CMD17 failure included — so a stale value can't leak
        // into the next non-data command. (The DMA variants restore on
        // their error paths for the same reason.)
        let resp = send_cmd_kind(CMD_READ_SINGLE_BLOCK, arg, ResponseKind::Short, CmdDir::Read);
        let r = resp.and_then(|_| drain_fifo_to(buf).and_then(|()| finish_data_phase(true)));
        write_reg(SDHCFG, self.hcfg_base);
        r
    }

    /// Write one 512-byte sector. See [`SdHost::read_block`] for
    /// argument semantics and the `&self` rationale.
    pub fn write_block(&self, lba: u32, buf: &[u8; 512]) -> Result<(), CmdError> {
        let arg = match self.capacity {
            CardCapacity::HighCapacity => lba,
            CardCapacity::StandardCapacity => lba.wrapping_mul(512),
        };
        prepare_data(self.hcfg_base, 512, 1);
        // Single exit through the SDHCFG restore below — see the note
        // in `read_block`: `DATA_IRPT_EN` must be cleared on the early
        // CMD24-failure path too, not just the success path.
        let r = send_cmd_kind(CMD_WRITE_SINGLE_BLOCK, arg, ResponseKind::Short, CmdDir::Write)
            .and_then(|_| fill_fifo_from(buf).and_then(|()| finish_data_phase(false)));
        write_reg(SDHCFG, self.hcfg_base);
        r
    }

    /// Translate a sector index to the SDCMD argument for this card:
    /// block index on SDHC (block-addressed) or byte offset on SDSC.
    /// Only the DMA write paths use this; gated with them on the
    /// real-hardware config where `host_dma` exists.
    #[cfg(nh_real_hw)]
    #[inline]
    fn cmd_arg(&self, lba: u32) -> u32 {
        match self.capacity {
            CardCapacity::HighCapacity => lba,
            CardCapacity::StandardCapacity => lba.wrapping_mul(512),
        }
    }

    /// Like [`write_block`], but the 512-byte data phase is fed by DMA
    /// (a DREQ-paced channel into the `SDDATA` FIFO) instead of PIO.
    ///
    /// This is the *isolated bring-up* form: it still **polls** the
    /// channel to completion, so the CPU/guest isn't freed yet — that
    /// arrives when the autosave is restructured around the channel's
    /// completion IRQ. Its job here is to prove the DMA→SDHOST path in
    /// isolation (milestone 2): the DREQ number, the FIFO addressing,
    /// and the command/data sequencing.
    #[cfg(nh_real_hw)]
    pub fn write_block_dma(&self, lba: u32, buf: &[u8; 512]) -> Result<(), CmdError> {
        use crate::host::host_dma as dma;
        if !dma::init_sd_tx() {
            // Channel not firmware-enabled; caller may fall back to PIO.
            return Err(CmdError::DmaError);
        }
        let arg = self.cmd_arg(lba);
        prepare_data(self.hcfg_base, 512, 1);
        // Arm first; the channel idles on DREQ until the command opens
        // the data phase and SDHOST starts asserting FIFO-space DREQs.
        // Polled form → no completion IRQ (inten=false).
        arm_sd_dma(buf.as_ptr() as u64, 512, false);
        if let Err(e) =
            send_cmd_kind(CMD_WRITE_SINGLE_BLOCK, arg, ResponseKind::Short, CmdDir::Write)
        {
            dma::sd_tx_abort();
            write_reg(SDHCFG, self.hcfg_base);
            return Err(e);
        }
        let r = poll_sd_dma_done().and_then(|()| finish_data_phase(false));
        write_reg(SDHCFG, self.hcfg_base);
        r
    }

    /// Multi-block DMA write of `buf` (length a multiple of 512) to
    /// the sectors starting at `lba`, polled to completion. The
    /// isolated bring-up / validation form (milestone 4a); the
    /// background save (milestone 4b) uses [`start_sectors_dma`] /
    /// [`finish_sectors_dma`] to take the completion IRQ instead.
    ///
    /// Sequence mirrors Linux's `bcm2835-sdhost` multi-block write:
    /// `prepare_data(blocks=n)` → `CMD25` (WRITE_MULTIPLE_BLOCK) → DMA
    /// the whole buffer (DREQ-paced) → settle the write FSM → `CMD12`
    /// (STOP_TRANSMISSION, R1b busy-wait, applied automatically by
    /// `send_cmd_kind`).
    #[cfg(nh_real_hw)]
    pub fn write_sectors_dma(&self, lba: u32, buf: &[u8]) -> Result<(), CmdError> {
        use crate::host::host_dma as dma;
        if buf.is_empty() || buf.len() % 512 != 0 {
            return Err(CmdError::DmaError);
        }
        if !dma::init_sd_tx() {
            return Err(CmdError::DmaError);
        }
        let n_blocks = (buf.len() / 512) as u32;
        let arg = self.cmd_arg(lba);
        prepare_data(self.hcfg_base, 512, n_blocks);
        arm_sd_dma(buf.as_ptr() as u64, buf.len() as u32, false);
        if let Err(e) =
            send_cmd_kind(CMD_WRITE_MULTIPLE_BLOCK, arg, ResponseKind::Short, CmdDir::Write)
        {
            dma::sd_tx_abort();
            write_reg(SDHCFG, self.hcfg_base);
            return Err(e);
        }
        let r = poll_sd_dma_done()
            .and_then(|()| finish_data_phase(false))
            .and_then(|()| send_cmd(CMD_STOP_TRANSMISSION, 0, ResponseKind::Short).map(|_| ()));
        write_reg(SDHCFG, self.hcfg_base);
        r
    }

    /// Begin a background multi-block DMA write: program the block
    /// count, arm the SD-TX channel with completion-IRQ enabled, and
    /// issue `CMD25`, then return immediately. The data phase runs in
    /// the background; the caller must call [`finish_sectors_dma`] from
    /// the SD-TX completion IRQ. `buf` must stay stable until then and
    /// be a non-empty multiple of 512 bytes.
    ///
    /// On a setup / `CMD25` failure the channel is aborted and the idle
    /// `SDHCFG` restored before returning Err.
    #[cfg(nh_real_hw)]
    pub fn start_sectors_dma(&self, lba: u32, buf: &[u8]) -> Result<(), CmdError> {
        use crate::host::host_dma as dma;
        if buf.is_empty() || buf.len() % 512 != 0 {
            return Err(CmdError::DmaError);
        }
        if !dma::init_sd_tx() {
            return Err(CmdError::DmaError);
        }
        let n_blocks = (buf.len() / 512) as u32;
        let arg = self.cmd_arg(lba);
        prepare_data(self.hcfg_base, 512, n_blocks);
        // inten=true → the channel raises GPU IRQ source 16+SD_TX_CHANNEL
        // on completion; `host_dma::on_completion` dispatches it.
        arm_sd_dma(buf.as_ptr() as u64, buf.len() as u32, true);
        if let Err(e) =
            send_cmd_kind(CMD_WRITE_MULTIPLE_BLOCK, arg, ResponseKind::Short, CmdDir::Write)
        {
            dma::sd_tx_abort();
            write_reg(SDHCFG, self.hcfg_base);
            return Err(e);
        }
        Ok(())
    }

    /// Complete a write begun by [`start_sectors_dma`], after the SD-TX
    /// channel has signalled completion. Checks for a latched DMA error,
    /// settles the write FSM, issues `CMD12` (STOP_TRANSMISSION, R1b
    /// busy-wait), and restores the idle `SDHCFG`.
    ///
    /// The caller runs this from the completion IRQ inside an
    /// IRQ-unmasked window so the `CMD12` busy-wait (card program time)
    /// doesn't starve the audio MAI feed / CNTHP rearm while it waits.
    #[cfg(nh_real_hw)]
    pub fn finish_sectors_dma(&self) -> Result<(), CmdError> {
        use crate::host::host_dma as dma;
        let r = if dma::sd_tx_error() {
            dma::sd_tx_abort();
            Err(CmdError::DmaError)
        } else {
            finish_data_phase(false)
                .and_then(|()| send_cmd(CMD_STOP_TRANSMISSION, 0, ResponseKind::Short).map(|_| ()))
        };
        write_reg(SDHCFG, self.hcfg_base);
        r
    }

    pub fn capacity(&self) -> CardCapacity {
        self.capacity
    }

    pub fn rca(&self) -> u32 {
        self.rca
    }
}

/// Static control block for the SD-TX DMA channel. `UnsafeCell` (not
/// `static mut`) so taking `&DmaCb` for `arm_sd_tx` doesn't trip the
/// `static_mut_refs` lint; single-core EL2 makes the aliasing sound.
#[cfg(nh_real_hw)]
struct SdTxCbCell(core::cell::UnsafeCell<crate::host::host_dma::DmaCb>);
// SAFETY: single-core EL2; only the SD DMA write paths touch it, serialised.
#[cfg(nh_real_hw)]
unsafe impl Sync for SdTxCbCell {}
#[cfg(nh_real_hw)]
static SD_TX_CB: SdTxCbCell =
    SdTxCbCell(core::cell::UnsafeCell::new(crate::host::host_dma::DmaCb::zero()));

/// Build the SD-TX control block for a RAM→`SDDATA` transfer of `len`
/// bytes from `buf_pa` (RAM, incrementing) into the DREQ-paced FIFO
/// (peripheral, no increment), flush the source range to RAM, and arm
/// the channel. `inten` requests a completion IRQ — set for the
/// background async save, clear for the polled bring-up paths that spin
/// on `sd_tx_active`.
#[cfg(nh_real_hw)]
fn arm_sd_dma(buf_pa: u64, len: u32, inten: bool) {
    use crate::host::host_dma as dma;
    let sddata_pa = (SDHOST_BASE + SDDATA) as u32;
    let mut ti = (dma::DREQ_SDHOST << dma::TI_PERMAP_SHIFT)
        | dma::TI_SRC_INC
        | dma::TI_DEST_DREQ
        | dma::TI_WAIT_RESP;
    if inten {
        ti |= dma::TI_INTEN;
    }
    // SAFETY: single-core EL2; the SD-TX channel and this CB are owned
    // by the serialised SD write paths. The CB stays stable until the
    // transfer completes — the polled paths spin on `sd_tx_active`, the
    // async path holds it until the completion IRQ runs `finish`.
    let cb = SD_TX_CB.0.get();
    unsafe {
        (*cb).ti = ti;
        (*cb).source_ad = dma::bus_addr_ram(buf_pa);
        (*cb).dest_ad = dma::bus_addr_periph(sddata_pa);
        (*cb).txfr_len = len;
        (*cb).stride = 0;
        (*cb).nextconbk = 0;
    }
    // The DMA master reads the source via the uncached bus alias, so
    // cacheable writes must be flushed to RAM first.
    crate::arch::cpu::dc_civac_range(buf_pa, len as usize);
    // SAFETY: `SD_TX_CB` is 'static and stable for the transfer.
    unsafe {
        dma::arm_sd_tx(&*cb);
    }
}

/// Spin until the SD-TX DMA channel finishes, watching SDHOST status
/// for errors meanwhile. Returns Ok when the channel goes idle cleanly,
/// Err on a latched CS.ERROR, an SDHOST error flag, or a SW timeout.
#[cfg(nh_real_hw)]
fn poll_sd_dma_done() -> Result<(), CmdError> {
    use crate::host::host_dma as dma;
    let mut spins = 0u32;
    loop {
        if !dma::sd_tx_active() {
            return if dma::sd_tx_error() {
                Err(CmdError::DmaError)
            } else {
                Ok(())
            };
        }
        let h = read_reg(SDHSTS);
        if h & SDHSTS_ERROR_MASK != 0 {
            dma::sd_tx_abort();
            return Err(map_hsts_error(h));
        }
        spins += 1;
        if spins > 5_000_000 {
            dma::sd_tx_abort();
            return Err(CmdError::HardwareWedge);
        }
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
    // Power off, zero command/argument/timeout/divider/host config,
    // clear status, then RMW the FIFO thresholds into SDEDM. Mirrors
    // Linux's bcm2835_sdhost_reset_internal — in particular SDEDM is
    // a read-modify-write so FORCE_DATA_MODE / CLOCK_PULSE / BYPASS
    // bits are preserved, and SDHCFG / SDHBCT / SDHBLC get explicitly
    // cleared (per "silicon bug" comment in the Linux source).
    write_reg(SDVDD, 0);
    write_reg(SDCMD, 0);
    write_reg(SDARG, 0);
    // 1.6 ms timeout at the core clock the firmware leaves us with;
    // refined later when SDCDIV is programmed.
    write_reg(SDTOUT, 0xF00000);
    write_reg(SDCDIV, 0);
    write_reg(SDHSTS, SDHSTS_CLEAR_MASK);
    write_reg(SDHCFG, 0);
    write_reg(SDHBCT, 0);
    write_reg(SDHBLC, 0);

    let mut edm = read_reg(SDEDM);
    edm &= !((SDEDM_THRESHOLD_MASK << SDEDM_READ_THRESHOLD_SHIFT)
        | (SDEDM_THRESHOLD_MASK << SDEDM_WRITE_THRESHOLD_SHIFT));
    edm |= (FIFO_READ_THRESHOLD << SDEDM_READ_THRESHOLD_SHIFT)
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
    // Set BUSYWAIT for the R1b commands we issue: CMD7 (SELECT_CARD)
    // during init and CMD12 (STOP_TRANSMISSION) on the multi-block DMA
    // write paths (`write_sectors_dma` / `finish_sectors_dma`). The
    // controller waits out the card's BUSY assertion internally, so we
    // never poll the DAT0 BUSY line ourselves.
    if matches!(cmd, CMD_SELECT_CARD | CMD_STOP_TRANSMISSION) {
        cmd_word |= SDCMD_BUSYWAIT;
    }
    write_reg(SDCMD, cmd_word);

    // Poll NEW_FLAG to drop. Bounded; if we sit here too long the
    // controller is wedged (clock not running, card not present,
    // bus floating). An SD write can stall here for >100 ms of
    // card-internal program time; the EL2 IRQ path keeps the HDMI
    // MAI ring fed and CNTHP rearmed in the background.
    for _ in 0..1_000_000u32 {
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

// CRC7-error handling vs. Linux's bcm2835-sdhost: Linux
// (`drivers/mmc/host/bcm2835.c`, `bcm2835_finish_command`) swallows a
// hardware CRC7 error for exactly one opcode — `MMC_SEND_OP_COND`
// (CMD1, the *MMC* op-cond), which carries an all-ones (0xFF) CRC
// field. It does NOT exempt SD's ACMD41 (`SD_SEND_OP_COND`), nor the
// R2 responses (CMD2/CMD9/CMD10). Our init path is SD-only: it uses
// ACMD41, never CMD1. So there is no command in our path that the
// Linux precedent would exempt, and we deliberately keep CRC7 a hard
// error here. (If a future MMC-card path issues CMD1, add the
// single-opcode exemption matching Linux — not a blanket R3 exemption.)
fn map_hsts_error(hsts: u32) -> CmdError {
    if hsts & SDHSTS_CMD_TIME_OUT != 0 {
        CmdError::Timeout
    } else if hsts & SDHSTS_CRC7_ERROR != 0 {
        CmdError::Crc7Error
    } else if hsts & SDHSTS_CRC16_ERROR != 0 {
        CmdError::Crc16Error
    } else if hsts & SDHSTS_FIFO_ERROR != 0 {
        CmdError::FifoError
    } else if hsts & SDHSTS_REW_TIME_OUT != 0 {
        CmdError::DataTimeout
    } else {
        CmdError::HardwareWedge
    }
}

// ---- FIFO drain / fill ----------------------------------------------
//
// Driven by SDEDM.FIFO_FILL — number of 32-bit words currently in the
// FIFO. The same approach Linux's bcm2835-sdhost and Circle take.
// SDHSTS_DATA_FLAG is threshold-driven (fires at FIFO >= read
// threshold, clears below) and doesn't behave well for word-at-a-time
// PIO: once we read enough words to fall below the threshold,
// DATA_FLAG clears and our wait loop can hang while data is still
// streaming in. FIFO_FILL is the raw count and has no such hysteresis.

const FIFO_DEPTH: u32 = 16;

#[inline]
fn fifo_fill() -> u32 {
    (read_reg(SDEDM) >> SDEDM_FIFO_FILL_SHIFT) & SDEDM_FIFO_FILL_MASK
}

/// Read 512 bytes (128 32-bit words) from the FIFO into `buf`. The
/// caller (`read_block`) is responsible for the post-drain FSM wait
/// via `finish_data_phase(true)`.
fn drain_fifo_to(buf: &mut [u8; 512]) -> Result<(), CmdError> {
    let mut written: usize = 0;
    while written < 128 {
        let avail = wait_for_fifo_avail()?;
        let take = (avail as usize).min(128 - written);
        for _ in 0..take {
            let w = read_reg(SDDATA);
            let off = written * 4;
            buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
            written += 1;
        }
    }
    Ok(())
}

/// Write 512 bytes (128 32-bit words) from `buf` to the FIFO. The
/// caller (`write_block`) is responsible for the post-fill FSM wait
/// via `finish_data_phase(false)`.
fn fill_fifo_from(buf: &[u8; 512]) -> Result<(), CmdError> {
    let mut written: usize = 0;
    while written < 128 {
        let space = wait_for_fifo_space()?;
        let put = (space as usize).min(128 - written);
        for _ in 0..put {
            let off = written * 4;
            let w =
                u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            write_reg(SDDATA, w);
            written += 1;
        }
    }
    Ok(())
}

/// Wait until SDEDM.FIFO_FILL > 0 (at least one word available to
/// read). Returns the observed fill count so the caller can burst-
/// drain up to that many words before re-polling.
fn wait_for_fifo_avail() -> Result<u32, CmdError> {
    for _ in 0..2_000_000u32 {
        let h = read_reg(SDHSTS);
        if h & SDHSTS_ERROR_MASK != 0 {
            return Err(map_hsts_error(h));
        }
        let fill = fifo_fill();
        if fill > 0 {
            return Ok(fill);
        }
    }
    Err(CmdError::HardwareWedge)
}

/// Wait until SDEDM.FIFO_FILL < FIFO_DEPTH (at least one word of
/// space). Returns the available space so the caller can burst-fill.
fn wait_for_fifo_space() -> Result<u32, CmdError> {
    for _ in 0..2_000_000u32 {
        let h = read_reg(SDHSTS);
        if h & SDHSTS_ERROR_MASK != 0 {
            return Err(map_hsts_error(h));
        }
        let fill = fifo_fill();
        if fill < FIFO_DEPTH {
            return Ok(FIFO_DEPTH - fill);
        }
    }
    Err(CmdError::HardwareWedge)
}

/// Set up the controller for a data-bearing transfer. Mirrors
/// Linux's `bcm2835_sdhost_prepare_data`:
///
/// 1. Flip SDHCFG to `hcfg_base | SDHCFG_DATA_BIT` (adds
///    `DATA_IRPT_EN`). On this controller that bit is functionally
///    required, not just interrupt-related — see `SDHCFG_DATA_BIT`
///    doc.
/// 2. Program block byte count and block count.
///
/// Must be called *before* writing SDCMD with the data-bearing
/// opcode. `read_block` / `write_block` restore `hcfg_base` once
/// the transfer is complete. `hcfg_base` carries any
/// `SDHCFG_WIDE_EXT_BUS` bit set by ACMD6 during init.
fn prepare_data(hcfg_base: u32, blksz: u32, blocks: u32) {
    write_reg(SDHCFG, hcfg_base | SDHCFG_DATA_BIT);
    write_reg(SDHBCT, blksz);
    write_reg(SDHBLC, blocks);
}

/// Wait for the controller's data-transfer FSM to settle after the
/// FIFO drain (read) or fill (write) completes. Mirrors Linux's
/// post-transfer wait:
///
/// - FSM in `IDENTMODE` or `DATAMODE` → transfer complete, return Ok.
/// - FSM in the alternate-idle state (`READWAIT` for reads,
///   `WRITESTART1` for writes) → controller is waiting for an event
///   we won't deliver in polling mode. Kick it out by writing
///   `SDEDM | FORCE_DATA_MODE`, then return Ok.
/// - Anything else → keep polling.
///
/// SDHSTS_BLOCK_IRPT, which the previous implementation polled, is
/// only meaningful when SDHCFG.BLOCK_IRPT_EN is set; we don't set it.
fn finish_data_phase(is_read: bool) -> Result<(), CmdError> {
    let alternate_idle = if is_read {
        SDEDM_FSM_READWAIT
    } else {
        SDEDM_FSM_WRITESTART1
    };
    for _ in 0..2_000_000 {
        let h = read_reg(SDHSTS);
        if h & SDHSTS_ERROR_MASK != 0 {
            return Err(map_hsts_error(h));
        }
        let edm = read_reg(SDEDM);
        let fsm = edm & SDEDM_FSM_MASK;
        if fsm == SDEDM_FSM_IDENTMODE || fsm == SDEDM_FSM_DATAMODE {
            return Ok(());
        }
        if fsm == alternate_idle {
            write_reg(SDEDM, edm | SDEDM_FORCE_DATA_MODE);
            return Ok(());
        }
    }
    Err(CmdError::HardwareWedge)
}

// ---- GPIO pinmux -----------------------------------------------------

const GPIO_BASE: usize = 0x3F20_0000;
const GPFSEL5: *mut u32 = (GPIO_BASE + 0x14) as *mut u32; // pins 50–53 here, plus 54–57.
const GPFSEL4: *mut u32 = (GPIO_BASE + 0x10) as *mut u32; // pins 40–49 here (includes 48, 49).
const GPPUD: *mut u32 = (GPIO_BASE + 0x94) as *mut u32;
const GPPUDCLK1: *mut u32 = (GPIO_BASE + 0x9C) as *mut u32;

const GPIO_PULL_OFF: u32 = 0;
const GPIO_PULL_UP: u32 = 2;
const GPIO_ALT0: u32 = 0b100;

/// Route GPIO 48..53 to the SDHOST controller (ALT0) with
/// appropriate pulls.
///
/// On BCM2835/2710 the alternate-function table puts SDHOST signals
/// on GPIO 48..53 ALT0:
/// - GPIO 48: `SD_CLK_N`  — no pull (clock-only line).
/// - GPIO 49: `SD_CMD_N`  — pull-up.
/// - GPIO 50..53: `SD_DAT0..3` — pull-up.
///
/// We deliberately do **not** touch GPIO 34..39 here: those go to
/// the on-package WLAN/BT chip via the Arasan EMMC controller, and
/// firmware has already configured them. Re-driving them risks
/// dropping the Bluetooth wakeup path during a future
/// `dtoverlay=disable-bt`-less boot. Circle's driver does touch
/// them, but Circle is single-OS — it owns everything. We're
/// targeted at a hypervisor that should leave anything not directly
/// used alone.
fn gpio_setup() {
    // Function-select for GPIO 48 and 49 lives in GPFSEL4 at bit
    // offsets (48-40)*3 = 24 and (49-40)*3 = 27.
    // SAFETY: GPIO MMIO at fixed BCM2710 base.
    unsafe {
        let mut fsel4 = read_volatile(GPFSEL4);
        fsel4 &= !(0b111 << 24);
        fsel4 &= !(0b111 << 27);
        fsel4 |= GPIO_ALT0 << 24;
        fsel4 |= GPIO_ALT0 << 27;
        write_volatile(GPFSEL4, fsel4);

        // GPIO 50..53 in GPFSEL5 at offsets (50-50)*3, ..., (53-50)*3.
        let mut fsel5 = read_volatile(GPFSEL5);
        for pin in 50..=53 {
            let shift = ((pin - 50) * 3) as u32;
            fsel5 &= !(0b111 << shift);
            fsel5 |= GPIO_ALT0 << shift;
        }
        write_volatile(GPFSEL5, fsel5);
    }

    // Pull config: BCM2835 mechanism (still works on BCM2710; Pi 4's
    // BCM2711 changed this — irrelevant for the Zero 2 W).
    //
    // Sequence per the ARM Peripherals manual:
    //   1. Write to GPPUD to set the required control signal.
    //   2. Wait 150 cycles — for the control signal to settle.
    //   3. Write to GPPUDCLK0/1 to clock the control signal into the
    //      GPIO pads we care about.
    //   4. Wait another 150 cycles.
    //   5. Write 0 to GPPUD to remove the control signal.
    //   6. Write 0 to GPPUDCLK0/1 to remove the clock.
    gpio_set_pull(1 << (48 - 32), GPIO_PULL_OFF);
    gpio_set_pull(
        (1 << (49 - 32)) | (1 << (50 - 32)) | (1 << (51 - 32)) | (1 << (52 - 32)) | (1 << (53 - 32)),
        GPIO_PULL_UP,
    );
}

fn gpio_set_pull(pin_mask_in_high_bank: u32, mode: u32) {
    // SAFETY: GPIO MMIO; `delay_cycles` provides the mandated 150-
    // cycle settle windows.
    unsafe {
        write_volatile(GPPUD, mode);
        delay_cycles(200);
        write_volatile(GPPUDCLK1, pin_mask_in_high_bank);
        delay_cycles(200);
        write_volatile(GPPUD, 0);
        write_volatile(GPPUDCLK1, 0);
    }
}

#[inline]
fn delay_cycles(n: u32) {
    for _ in 0..n {
        unsafe { core::arch::asm!("nop") }
    }
}

// ---- Clock setup via VC mailbox -------------------------------------

/// Query (and pin) the SoC core clock the SDHOST is hung off of.
/// Returns the rate in Hz.
///
/// We don't change the core clock rate — that would knock several
/// other peripherals around. We just read what firmware has set and
/// derive SDCDIV from it.
fn clock_setup() -> Result<u32, crate::host::mailbox::MailboxError> {
    let rate = crate::host::mailbox::get_clock_rate(crate::host::mailbox::CLOCK_ID_CORE)?;
    // The cut-down GPU firmware on the Zero 2 W typically reports
    // 250 MHz here. If we see something outside a sane window the
    // mailbox response is likely garbled — fail loudly rather than
    // dial in a wrong divider.
    if !(50_000_000..=600_000_000).contains(&rate) {
        return Err(crate::host::mailbox::MailboxError::FirmwareError);
    }
    Ok(rate)
}

/// Decode the addressable 512-byte sector count from a 136-bit CSD.
///
/// `csd[i]` holds CSD bits `[32*i + 31 : 32*i]` — i.e. `csd[0]` is
/// SDRSP0 (bits[31:0]) and `csd[3]` is SDRSP3 (bits[127:96]), the
/// standard CRC-stripped R2 word order. We extract the standard SD
/// fields and return the device size in 512-byte sectors.
///
/// Both CSD v1 (SDSC) and v2 (SDHC/SDXC) are decoded. The cards in
/// the field are SDHC (v2), but v1 is handled defensively. An unknown
/// `CSD_STRUCTURE` value is NOT guessed: we return `u32::MAX` (the
/// "bounds-check disabled" sentinel) and log loudly, so a future card
/// reporting a v3+ structure never produces a *wrong* size that could
/// misdirect a raw block write.
fn decode_csd_num_blocks(csd: &[u32; 4]) -> u32 {
    // Extract `width` bits starting at bit `lo` of the 128-bit CSD.
    // All fields we read fit within a single 32-bit word boundary
    // except none span words here, so the simple form suffices: but
    // guard against spanning by reading up to 64 bits around `lo`.
    let bits = |lo: u32, width: u32| -> u64 {
        debug_assert!(width >= 1 && width <= 32);
        let word = |i: usize| -> u64 { csd[i] as u64 };
        // Bit `b` lives in word `b/32` at offset `b%32`. A field of
        // width ≤ 32 spans at most two adjacent words.
        let lo_word = (lo / 32) as usize;
        let lo_off = lo % 32;
        let low = word(lo_word) >> lo_off;
        let window = if lo_off == 0 || lo_word + 1 >= 4 {
            low
        } else {
            // High word contributes the bits above the (32 - lo_off)
            // bits the low word already supplied.
            low | (word(lo_word + 1) << (32 - lo_off))
        };
        window & ((1u64 << width) - 1)
    };

    // CSD_STRUCTURE: bits [127:126].
    let structure = bits(126, 2);
    match structure {
        0 => {
            // CSD v1 (SDSC).
            //   READ_BL_LEN  [83:80]  (4 bits) — log2(max read block len)
            //   C_SIZE       [73:62]  (12 bits)
            //   C_SIZE_MULT  [49:47]  (3 bits)
            // capacity_bytes = (C_SIZE+1) * 2^(C_SIZE_MULT+2) * 2^READ_BL_LEN
            let read_bl_len = bits(80, 4) as u32;
            let c_size = bits(62, 12) as u64;
            let c_size_mult = bits(47, 3) as u32;
            // mult = 2^(C_SIZE_MULT+2); block_len = 2^READ_BL_LEN.
            let blocknr = (c_size + 1) << (c_size_mult + 2);
            let block_len: u64 = 1u64 << read_bl_len;
            let capacity_bytes = blocknr * block_len;
            // Whole 512-byte sectors. Saturate to u32 (SDSC ≤ 2 GiB so
            // this fits, but be defensive).
            (capacity_bytes / 512).min(u32::MAX as u64) as u32
        }
        1 => {
            // CSD v2 (SDHC/SDXC). C_SIZE [69:48] (22 bits); device size
            // in 512-byte sectors = (C_SIZE + 1) * 1024.
            let c_size = bits(48, 22);
            ((c_size + 1) * 1024).min(u32::MAX as u64) as u32
        }
        other => {
            crate::kprintln!(
                "sd: WARN unknown CSD_STRUCTURE={} (csd=[{:08x} {:08x} {:08x} {:08x}]); \
                 leaving num_blocks=u32::MAX (bounds checks disabled)",
                other,
                csd[0],
                csd[1],
                csd[2],
                csd[3],
            );
            u32::MAX
        }
    }
}

/// Program SDCDIV so that `core_clock / (cdiv + 2)` is at most
/// `target_hz`. SDHOST's effective SD-bus clock is
/// `core_clock / (SDCDIV + 2)`; rounding up keeps us under the
/// target (the SD spec ceilings are inclusive).
fn program_sdcdiv(core_clock: u32, target_hz: u32) {
    let div = ((core_clock + target_hz - 1) / target_hz).saturating_sub(2);
    let cdiv = core::cmp::min(div, SDCDIV_MAX_CDIV);
    write_reg(SDCDIV, cdiv);
}

/// Spin until at least `us` microseconds have elapsed by CNTPCT_EL0.
/// The generic timer is running out of reset (`boot.s` programs
/// `CNTFRQ_EL0`), so the named SD power-up/reset settles are now
/// real microseconds rather than the ~20×-fast nop loop they were.
/// Same CNTPCT pattern as `cpu::delay_ms`.
fn delay_us(us: u32) {
    let freq: u64;
    let start: u64;
    // SAFETY: sysreg reads, side-effect free.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq,
            options(nomem, nostack, preserves_flags));
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) start,
            options(nomem, nostack, preserves_flags));
    }
    // ticks = freq[Hz] * us / 1_000_000. Compute in u64 to avoid
    // overflow at the largest call site (10_000 us).
    let target = start.wrapping_add((freq.saturating_mul(us as u64)) / 1_000_000);
    loop {
        let now: u64;
        // SAFETY: sysreg read.
        unsafe {
            core::arch::asm!("mrs {}, cntpct_el0", out(reg) now,
                options(nomem, nostack, preserves_flags));
        }
        if now.wrapping_sub(target) as i64 >= 0 {
            return;
        }
    }
}
