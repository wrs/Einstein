//! Network-manager driver — Rust port of Einstein's `TNetworkManager`
//! native primitive class. Models a "no link, no packets" Ethernet
//! card: the driver initialises cleanly, GetDeviceAddress returns the
//! same MAC Einstein's TUsermodeNetwork advertises by default, and
//! every send/receive is a no-op.
//!
//! Dispatched from `peripherals::native_primitives::execute` for any
//! native call with driver=0x00000A. Subfunction codes match Einstein's
//! `TNativePrimitives::ExecuteNetworkManagerNative`
//! (`Emulator/TNativePrimitives.cpp:2889-3151`).

use crate::{cpu, kprintln, peripherals::guest_access, trap::TrapContext};

/// Network-manager driver class ID in the native-primitive encoding.
pub const DRIVER_ID: u32 = 0x00_000A;

/// MAC address Einstein's `TUsermodeNetwork::GetDeviceAddress` reports
/// when no host bridge is configured. Mirrored verbatim so a Newton
/// guest sees the same hardware identity it would on Einstein.
const DEFAULT_MAC: [u8; 6] = [0x58, 0xB0, 0x35, 0x77, 0xD7, 0x22];

pub fn handle(ctx: &mut TrapContext, subfn: u32, pc: u32) {
    match subfn {
        // 0x00 Unknown — no-op log slot. r0 untouched per Einstein.
        0x00 => {}

        // 0x01 Log — r0 holds a guest pointer to a NUL-terminated C
        // string; emit it through the hypervisor UART. r0 untouched
        // per Einstein.
        0x01 => log_string(ctx, pc),

        // New, Delete, Init, Enable, Disable, InterruptHandler — no-op
        // (Einstein doesn't write r0 for these).
        0x02 | 0x03 | 0x04 | 0x05 | 0x06 | 0x07 => {}

        // SendBuffer, SendCBufferList — no-op (Einstein doesn't write r0).
        0x08 | 0x09 => {}

        // SendPacket(buf=r1, size=r2) — drop on the floor. Einstein
        // leaves r0 untouched on the no-manager path; we match.
        0x0A => {
            // Helpful diagnostic: log size only, not the buffer (could
            // be huge). Budgeted via kprintln's natural rate-limiting.
            let size = ctx.x[2] as u32;
            if size != 0 {
                kprintln!("network.SendPacket: dropped {} bytes", size);
            }
        }

        // GetDeviceAddress(dst=r1, size=r2) — write up to `size` bytes
        // of DEFAULT_MAC into the guest buffer at *r1. Einstein writes
        // exactly `dstBufferSize` bytes regardless of the MAC length;
        // we mirror, clamping to 6 since DEFAULT_MAC is 6 bytes.
        // r0 = 0 (success / "no error").
        0x0B => {
            let dst = ctx.x[1] as u32;
            let size = (ctx.x[2] as u32).min(DEFAULT_MAC.len() as u32);
            for i in 0..size {
                guest_access::write_byte_or_halt(
                    dst.wrapping_add(i), DEFAULT_MAC[i as usize],
                    "network.GetDeviceAddress", pc);
            }
            ctx.x[0] = 0;
        }

        // AddMulticast / DelMulticast / GetLinkIntegrity / SetPromiscuous
        // / GetThroughput — no-op.
        0x0C | 0x0D | 0x0E | 0x0F | 0x10 => {}

        // TimerExpired — Einstein calls mNetworkManager->TimerExpired();
        // we have no manager, no-op. r0 untouched.
        0x11 => {}

        // NE2000-template: InitCard, SetCardInfo — no-op.
        0x12 | 0x13 => {}

        // DataAvailable — r0 = 0 (no data ever).
        0x14 => {
            ctx.x[0] = 0;
        }

        // ReceiveData — no-op (no data; Einstein writes nothing to *r1
        // when manager is absent).
        0x15 => {}

        // 0x16 — debug print of memory location; no-op.
        0x16 => {}

        _ => {
            kprintln!(
                "*** network: unknown subfn {:#x} @PC={:#x} r0={:#x} r1={:#x} r2={:#x}",
                subfn, pc, ctx.x[0] as u32, ctx.x[1] as u32, ctx.x[2] as u32
            );
            kprintln!(
                "    (extend peripherals/network.rs::handle to add this subfn)"
            );
            cpu::halt();
        }
    }
}

/// 0x01 Log: read C string at *r0 and emit through the UART. Length
/// capped at 1023 bytes to match Einstein's local buffer
/// (TNativePrimitives.cpp:2907).
fn log_string(ctx: &mut TrapContext, pc: u32) {
    let mut addr = ctx.x[0] as u32;
    let mut buf = [0u8; 1023];
    let mut len = 0usize;
    while len < buf.len() {
        // Einstein completes this log path, so a failed guest read is a
        // hypervisor emulation bug, not a guest bug — halt loudly like
        // every other native-prim guest access (periph-L6).
        let b = guest_access::read_byte_or_halt(addr, "network.Log", pc);
        if b == 0 {
            break;
        }
        buf[len] = b;
        len += 1;
        addr = addr.wrapping_add(1);
    }
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        kprintln!("network.Log: {}", s);
    } else {
        kprintln!("network.Log: <{} non-utf8 bytes @PC={:#x}>", len, pc);
    }
}
