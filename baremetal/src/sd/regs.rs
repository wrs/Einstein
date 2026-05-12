//! BCM2835 SDHOST register layout.
//!
//! Numbers transcribed verbatim from Circle's
//! `addon/SDCard/sdhost.cpp` / `sdhost.h`
//! (<https://github.com/rsta2/circle/blob/master/addon/SDCard/sdhost.cpp>),
//! which in turn derives from the Linux `bcm2835-sdhost` driver.
//! Cross-checked against the BCM2835 ARM Peripherals manual where the
//! peripheral is undocumented; the Linux + Circle code is the de-facto
//! spec.
//!
//! All offsets are from the SDHOST base. On the Pi Zero 2 W
//! (BCM2710, peripheral window at 0x3F00_0000) the base is
//! 0x3F20_2000; defined in `super::sdhost` rather than here so we
//! never inadvertently depend on it from a platform-agnostic context.

#![allow(dead_code)]

// ---- Register offsets -------------------------------------------------

/// Command to SD card. 16-bit R/W.
pub const SDCMD: usize = 0x00;
/// Argument to SD card. 32-bit R/W.
pub const SDARG: usize = 0x04;
/// Start value for the timeout counter. 32-bit R/W.
pub const SDTOUT: usize = 0x08;
/// Start value for the clock divider. 11-bit R/W.
pub const SDCDIV: usize = 0x0C;
/// SD card response, bits [31:0]. 32-bit R.
pub const SDRSP0: usize = 0x10;
/// SD card response, bits [63:32]. 32-bit R.
pub const SDRSP1: usize = 0x14;
/// SD card response, bits [95:64]. 32-bit R.
pub const SDRSP2: usize = 0x18;
/// SD card response, bits [127:96]. 32-bit R.
pub const SDRSP3: usize = 0x1C;
/// SD host status. 11-bit R/W (writes clear sticky bits).
pub const SDHSTS: usize = 0x20;
/// SD card power control. 1-bit R/W.
pub const SDVDD: usize = 0x30;
/// Emergency Debug Mode. 13-bit R/W.
pub const SDEDM: usize = 0x34;
/// Host configuration. R/W.
pub const SDHCFG: usize = 0x38;
/// Host byte count (debug). 32-bit R/W.
pub const SDHBCT: usize = 0x3C;
/// Data to / from SD card. 32-bit R/W (FIFO port).
pub const SDDATA: usize = 0x40;
/// Host block count (SDIO/SDHC). 9-bit R/W.
pub const SDHBLC: usize = 0x50;

// ---- SDCMD bit fields -------------------------------------------------

/// Set by SW to start a command; cleared by HW when the command is
/// done.
pub const SDCMD_NEW_FLAG: u32 = 0x8000;
/// Set by HW if the command failed (CMD_TIME_OUT / CRC7 / FIFO).
pub const SDCMD_FAIL_FLAG: u32 = 0x4000;
/// Wait for `BUSY` to deassert after R1b commands.
pub const SDCMD_BUSYWAIT: u32 = 0x800;
/// This command has no response.
pub const SDCMD_NO_RESPONSE: u32 = 0x400;
/// This command has a 136-bit response (R2).
pub const SDCMD_LONG_RESPONSE: u32 = 0x200;
/// Write data follows the command on the data lines.
pub const SDCMD_WRITE_CMD: u32 = 0x80;
/// Read data follows the command on the data lines.
pub const SDCMD_READ_CMD: u32 = 0x40;
/// 6-bit command index mask.
pub const SDCMD_CMD_MASK: u32 = 0x3F;

// ---- SDCDIV --------------------------------------------------------

/// Max value of the 11-bit clock divider (slowest clock).
pub const SDCDIV_MAX_CDIV: u32 = 0x7FF;

// ---- SDHSTS bit fields ----------------------------------------------

/// Card BUSY line went high while we were waiting for R1b.
pub const SDHSTS_BUSY_IRPT: u32 = 0x400;
/// One block transfer completed.
pub const SDHSTS_BLOCK_IRPT: u32 = 0x200;
/// SDIO interrupt (we don't use this path).
pub const SDHSTS_SDIO_IRPT: u32 = 0x100;
/// Read / Write data timeout.
pub const SDHSTS_REW_TIME_OUT: u32 = 0x80;
/// Command timeout (no response from card).
pub const SDHSTS_CMD_TIME_OUT: u32 = 0x40;
/// Block CRC16 mismatch on data transfer.
pub const SDHSTS_CRC16_ERROR: u32 = 0x20;
/// Command response CRC7 mismatch.
pub const SDHSTS_CRC7_ERROR: u32 = 0x10;
/// FIFO over/underrun.
pub const SDHSTS_FIFO_ERROR: u32 = 0x08;
/// Data ready in the FIFO (read direction).
pub const SDHSTS_DATA_FLAG: u32 = 0x01;

pub const SDHSTS_TRANSFER_ERROR_MASK: u32 =
    SDHSTS_CRC7_ERROR | SDHSTS_CRC16_ERROR | SDHSTS_REW_TIME_OUT | SDHSTS_FIFO_ERROR;
pub const SDHSTS_ERROR_MASK: u32 = SDHSTS_CMD_TIME_OUT | SDHSTS_TRANSFER_ERROR_MASK;

/// Bits we write back to SDHSTS to clear all sticky status flags.
/// (Writing 1 clears in this peripheral.)
pub const SDHSTS_CLEAR_MASK: u32 = SDHSTS_BUSY_IRPT
    | SDHSTS_BLOCK_IRPT
    | SDHSTS_SDIO_IRPT
    | SDHSTS_REW_TIME_OUT
    | SDHSTS_CMD_TIME_OUT
    | SDHSTS_CRC16_ERROR
    | SDHSTS_CRC7_ERROR
    | SDHSTS_FIFO_ERROR;

// ---- SDHCFG bit fields ----------------------------------------------

pub const SDHCFG_BUSY_IRPT_EN: u32 = 1 << 10;
pub const SDHCFG_BLOCK_IRPT_EN: u32 = 1 << 8;
pub const SDHCFG_SDIO_IRPT_EN: u32 = 1 << 5;
pub const SDHCFG_DATA_IRPT_EN: u32 = 1 << 4;
pub const SDHCFG_SLOW_CARD: u32 = 1 << 3;
pub const SDHCFG_WIDE_EXT_BUS: u32 = 1 << 2;
pub const SDHCFG_WIDE_INT_BUS: u32 = 1 << 1;
pub const SDHCFG_REL_CMD_LINE: u32 = 1 << 0;

// ---- SDEDM bit fields -----------------------------------------------

pub const SDEDM_FORCE_DATA_MODE: u32 = 1 << 19;
pub const SDEDM_CLOCK_PULSE: u32 = 1 << 20;
pub const SDEDM_BYPASS: u32 = 1 << 21;

pub const SDEDM_WRITE_THRESHOLD_SHIFT: u32 = 9;
pub const SDEDM_READ_THRESHOLD_SHIFT: u32 = 14;
pub const SDEDM_THRESHOLD_MASK: u32 = 0x1F;

/// FSM state field — bottom 4 bits of SDEDM.
pub const SDEDM_FSM_MASK: u32 = 0xF;
pub const SDEDM_FSM_IDENTMODE: u32 = 0x0;
pub const SDEDM_FSM_DATAMODE: u32 = 0x1;
pub const SDEDM_FSM_READDATA: u32 = 0x2;
pub const SDEDM_FSM_WRITEDATA: u32 = 0x3;
pub const SDEDM_FSM_READWAIT: u32 = 0x4;
pub const SDEDM_FSM_READCRC: u32 = 0x5;
pub const SDEDM_FSM_WRITECRC: u32 = 0x6;
pub const SDEDM_FSM_WRITEWAIT1: u32 = 0x7;
pub const SDEDM_FSM_POWERDOWN: u32 = 0x8;
pub const SDEDM_FSM_POWERUP: u32 = 0x9;
pub const SDEDM_FSM_WRITESTART1: u32 = 0xA;
pub const SDEDM_FSM_WRITESTART2: u32 = 0xB;
pub const SDEDM_FSM_GENPULSES: u32 = 0xC;
pub const SDEDM_FSM_WRITEWAIT2: u32 = 0xD;
pub const SDEDM_FSM_STARTPOWDOWN: u32 = 0xF;

// ---- FIFO geometry --------------------------------------------------

/// FIFO depth in 32-bit words.
pub const SDDATA_FIFO_WORDS: u32 = 16;
/// PIO burst threshold the Circle driver uses (words).
pub const FIFO_READ_THRESHOLD: u32 = 4;
pub const FIFO_WRITE_THRESHOLD: u32 = 4;

// ---- SD card commands ------------------------------------------------
//
// Subset we drive from the bare-metal layer to bring a card from
// post-reset state up to ready-to-read-blocks. CMD numbers from
// JESD84 / SD Physical Layer Specification §4.7.4.
//
// Application-Specific Commands (ACMDxx) are sent as CMD55 followed by
// the ACMD number; the SDHOST controller doesn't distinguish at the
// register level.

pub const CMD_GO_IDLE_STATE: u8 = 0;        // CMD0   no response
pub const CMD_ALL_SEND_CID: u8 = 2;         // CMD2   R2 (long)
pub const CMD_SEND_RELATIVE_ADDR: u8 = 3;   // CMD3   R6
pub const CMD_SELECT_CARD: u8 = 7;          // CMD7   R1b
pub const CMD_SEND_IF_COND: u8 = 8;         // CMD8   R7
pub const CMD_SEND_CSD: u8 = 9;             // CMD9   R2 (long)
pub const CMD_STOP_TRANSMISSION: u8 = 12;   // CMD12  R1b
pub const CMD_SET_BLOCKLEN: u8 = 16;        // CMD16  R1
pub const CMD_READ_SINGLE_BLOCK: u8 = 17;   // CMD17  R1
pub const CMD_READ_MULTIPLE_BLOCK: u8 = 18; // CMD18  R1
pub const CMD_WRITE_SINGLE_BLOCK: u8 = 24;  // CMD24  R1
pub const CMD_WRITE_MULTIPLE_BLOCK: u8 = 25;// CMD25  R1
pub const CMD_APP_CMD: u8 = 55;             // CMD55  R1

pub const ACMD_SD_SEND_OP_COND: u8 = 41;    // ACMD41 R3
pub const ACMD_SET_BUS_WIDTH: u8 = 6;       // ACMD6  R1

/// CMD8 check pattern (low 8 bits) + voltage supplied (bits 11:8 = 1).
pub const CMD8_VHS_27_36_PATTERN: u32 = (1 << 8) | 0xAA;

/// CMD8 echo expected back from a 2.7-3.6 V card.
pub const CMD8_R7_PATTERN_MASK: u32 = 0xFFF;
pub const CMD8_R7_PATTERN_VALUE: u32 = (1 << 8) | 0xAA;

/// ACMD41 OCR argument bits.
/// HCS bit — "host supports high-capacity" (SDHC/SDXC).
pub const OCR_HCS: u32 = 1 << 30;
/// Card-busy bit in OCR response — clear while card is initialising.
pub const OCR_BUSY: u32 = 1 << 31;
/// CCS bit — set in OCR when the card is SDHC/SDXC (block-addressed).
pub const OCR_CCS: u32 = 1 << 30;
/// Voltage window 3.2–3.4 V (typical of an SDHC card).
pub const OCR_VOLT_3V2_3V4: u32 = (1 << 20) | (1 << 21);
