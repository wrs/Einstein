//! DWC2 register map.
//!
//! Offsets and bit definitions from:
//! - Synopsys DWC OTG Programming Guide, §3 "Register descriptions".
//! - Circle's `dwhciregister.h` (cross-check):
//!   <https://github.com/rsta2/circle/blob/master/include/circle/usb/dwhciregister.h>
//! - Linux `drivers/usb/dwc2/hw.h`.
//!
//! Only the subset we read or write from `dwc2/mod.rs` is exposed.

// ---- Core global registers (GLOBAL block, offset 0x000..0x040) ----

/// AHB Configuration. Bit 0 = global interrupt mask (we leave 0 for
/// polling).
pub const GAHBCFG: usize = 0x008;
/// USB Configuration. Bits: 29=ForceHostMode, 30=ForceDeviceMode,
/// 6=PhyIf (0 = UTMI 8-bit), 10..14 = USBTrdTim.
pub const GUSBCFG: usize = 0x00C;
/// Core Reset. Bit 0 = CSftRst, bit 31 = AHBIdle.
pub const GRSTCTL: usize = 0x010;
/// Core Interrupt Mask. Polling — kept 0 except for the bits we sample.
pub const GINTMSK: usize = 0x018;
/// Receive FIFO Size (in dwords).
pub const GRXFSIZ: usize = 0x024;
/// Non-periodic Transmit FIFO Size: low half = StartAddr, high =
/// Depth.
pub const GNPTXFSIZ: usize = 0x028;
/// SNPS Identification — reads e.g. 0x4F54_280A on BCM2710.
pub const GSNPSID: usize = 0x040;
/// User HW Config 2 (channel count, FIFO depths, etc.): bits
/// 17:14 = num host channels - 1.
pub const GHWCFG2: usize = 0x048;
/// Host periodic Transmit FIFO Size.
pub const HPTXFSIZ: usize = 0x100;

// ---- Host-mode registers (HOST block, offset 0x400..0x800) ----

/// Host Configuration. Bits 1:0 = FSLSPClkSel (1 = 48 MHz), bit 2 =
/// FSLSSupp.
pub const HCFG: usize = 0x400;
/// Host Frame Interval (writeable on BCM2710).
pub const HFIR: usize = 0x404;
/// Host Frame Number / Frame Time Remaining.
pub const HFNUM: usize = 0x408;
/// Host All Channels Interrupt Mask.
pub const HAINTMSK: usize = 0x418;
/// Host Port Control and Status (HPRT). Important bits:
///   0  PrtConnSts   (read-only, 1 = device attached)
///   1  PrtConnDet   (W1C connect-change)
///   2  PrtEna       (1 = enabled; W0 to clear, W1 disables)
///   3  PrtEnChng    (W1C)
///   8  PrtRst       (1 = reset signalling)
/// 12  PrtPwr        (1 = port powered)
/// 17:18 PrtSpd      (0=HS, 1=FS, 2=LS)
pub const HPRT: usize = 0x440;

// ---- Per-channel block (HOST_CH(n) at 0x500 + n*0x20) ----

#[inline]
pub const fn hcchar(ch: usize) -> usize {
    0x500 + ch * 0x20
}
#[inline]
pub const fn hcsplt(ch: usize) -> usize {
    0x504 + ch * 0x20
}
#[inline]
pub const fn hcint(ch: usize) -> usize {
    0x508 + ch * 0x20
}
#[inline]
pub const fn hcintmsk(ch: usize) -> usize {
    0x50C + ch * 0x20
}
#[inline]
pub const fn hctsiz(ch: usize) -> usize {
    0x510 + ch * 0x20
}
#[inline]
pub const fn hcdma(ch: usize) -> usize {
    0x514 + ch * 0x20
}

// ---- Power and clock gating ----

/// PCGCCTL — Power & Clock Gating Control. Setting any bit gates
/// the core; init writes 0 to wake it.
pub const PCGCCTL: usize = 0xE00;

// ---- Bit shortcuts we actually use ----

pub const GRSTCTL_CSFTRST: u32 = 1 << 0;
pub const GRSTCTL_RXFFLSH: u32 = 1 << 4;
pub const GRSTCTL_TXFFLSH: u32 = 1 << 5;
pub const GRSTCTL_TXFNUM_ALL: u32 = 0x10 << 6; // flush all TX FIFOs
pub const GRSTCTL_AHBIDLE: u32 = 1 << 31;

// GAHBCFG bits — Circle `dwhci.h`.
pub const GAHBCFG_GLOBALINT_MASK: u32 = 1 << 0;
pub const GAHBCFG_MAX_AXI_BURST_SHIFT: u32 = 1; // BCM2835 only
pub const GAHBCFG_MAX_AXI_BURST_MASK: u32 = 0x3 << GAHBCFG_MAX_AXI_BURST_SHIFT;
pub const GAHBCFG_WAIT_AXI_WRITES: u32 = 1 << 4; // BCM2835 only
pub const GAHBCFG_DMA_ENABLE: u32 = 1 << 5;

// GUSBCFG bits — Circle `dwhci.h`. PHYIF is bit 3, ULPI_UTMI_SEL bit 4.
pub const GUSBCFG_PHYIF: u32 = 1 << 3;
pub const GUSBCFG_ULPI_UTMI_SEL: u32 = 1 << 4;
pub const GUSBCFG_SRP_CAPABLE: u32 = 1 << 8;
pub const GUSBCFG_HNP_CAPABLE: u32 = 1 << 9;
pub const GUSBCFG_ULPI_FSLS: u32 = 1 << 17;
pub const GUSBCFG_ULPI_CLK_SUS_M: u32 = 1 << 19;
pub const GUSBCFG_ULPI_EXT_VBUS_DRV: u32 = 1 << 20;
pub const GUSBCFG_TERM_SEL_DL_PULSE: u32 = 1 << 22;

// HCFG.FSLSPClkSel — bits[1:0]: 0=30/60 MHz (HS/FS, UTMI+),
// 1=48 MHz (FS-only ULPI), 2=6 MHz (low-speed ULPI).
pub const HCFG_FSLSPCLK_SEL_MASK: u32 = 0x3;
pub const HCFG_FSLSPCLK_SEL_30_60M: u32 = 0;
pub const HCFG_FSLSPCLK_SEL_48M: u32 = 1;

pub const HPRT_PRT_CONN_STS: u32 = 1 << 0;
pub const HPRT_PRT_CONN_DET: u32 = 1 << 1; // W1C
pub const HPRT_PRT_ENA: u32 = 1 << 2;
pub const HPRT_PRT_ENCHNG: u32 = 1 << 3; // W1C
pub const HPRT_PRT_OVRCURR_CHNG: u32 = 1 << 5; // W1C
pub const HPRT_PRT_RST: u32 = 1 << 8;
pub const HPRT_PRT_PWR: u32 = 1 << 12;
pub const HPRT_PRT_SPD_SHIFT: u32 = 17;
pub const HPRT_PRT_SPD_MASK: u32 = 0x3 << HPRT_PRT_SPD_SHIFT;

/// Mask of HPRT bits that are W1C ("write 1 to clear") — they must
/// be masked off before any RMW write to HPRT, or the read-modify
/// implicitly acks pending change-status bits.
pub const HPRT_W1C: u32 =
    HPRT_PRT_CONN_DET | HPRT_PRT_ENA | HPRT_PRT_ENCHNG | HPRT_PRT_OVRCURR_CHNG;

// HCCHARn channel-characteristic bits we set up per transfer.
pub const HCCHAR_MPS_MASK: u32 = 0x7FF;
pub const HCCHAR_EPNUM_SHIFT: u32 = 11;
pub const HCCHAR_EPDIR_IN: u32 = 1 << 15;
pub const HCCHAR_EPTYPE_SHIFT: u32 = 18; // 0=Ctrl,1=Iso,2=Bulk,3=Intr
pub const HCCHAR_DEV_ADDR_SHIFT: u32 = 22;
pub const HCCHAR_ODD_FRAME: u32 = 1 << 29;
pub const HCCHAR_CHDIS: u32 = 1 << 30;
pub const HCCHAR_CHENA: u32 = 1 << 31;

pub const HCINT_XFER_COMPL: u32 = 1 << 0;
pub const HCINT_CHHLTD: u32 = 1 << 1;
pub const HCINT_AHBERR: u32 = 1 << 2;
pub const HCINT_STALL: u32 = 1 << 3;
pub const HCINT_XACT_ERR: u32 = 1 << 7;
pub const HCINT_BBL_ERR: u32 = 1 << 8;

pub const HCTSIZ_PKTCNT_SHIFT: u32 = 19;
pub const HCTSIZ_PID_SHIFT: u32 = 29; // 00=DATA0, 10=DATA1, 11=MDATA, 11=SETUP for control

// GINTSTS / GINTMSK bits (same bit positions in both). Only the
// subset the IRQ-driven interrupt-IN path uses. GINTSTS.HCINT is
// read-only — it reflects the OR of HAINT and clears when the
// underlying per-channel HCINT bits are cleared (W1C).
pub const GINTSTS_HCINT: u32 = 1 << 25; // Host channels interrupt (RO)
