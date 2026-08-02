/// Convenience macros for formatted output. Use like `kprintln!("val={:#x}", x);`.
///
/// Every line is prefixed with `[s.uuuuuu]` — seconds.microseconds
/// since EL2 reset, sourced from CNTPCT_EL0. The prefix is added at
/// the start of every `kprint!` *line* (i.e., only when the message
/// starts a new line); a `kprint!` that adds to an existing line
/// just appends, since we can't tell where in a partial line we are
/// from inside the macro. `kprintln!` always emits the prefix.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::host::console::Writer, $($arg)*);
    }};
}

#[macro_export]
macro_rules! kprintln {
    () => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::host::console::Writer);
    }};
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _us = $crate::host::console::now_us();
        let _ = writeln!(
            $crate::host::console::Writer,
            "[{:>4}.{:06}] {}",
            _us / 1_000_000,
            _us % 1_000_000,
            format_args!($($arg)*),
        );
    }};
}

/// Polled-only variants of `kprint!` / `kprintln!`. Bytes go straight
/// to the PL011 wire via busy-waiting on FR.TXFF (or to host stdout
/// via per-byte SYS_WRITEC on the semihost build), bypassing the DMA
/// ring entirely. Use these:
///   * before `console::init()` returns (the ring isn't armed yet),
///   * inside diagnostics that are debugging the ring itself,
///   * from any path where you don't trust the fancy printer to
///     have made progress.
/// Slow — don't use on the hot path.
#[macro_export]
macro_rules! raw_print {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::host::console::RawWriter, $($arg)*);
    }};
}

#[macro_export]
macro_rules! raw_println {
    () => { $crate::raw_print!("\n"); };
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::host::console::RawWriter, $($arg)*);
    }};
}

/// Emit one raw byte on the host console *wire* (PL011 TX, busy-wait
/// on FIFO room) — NOT the `kprintln!` stream, which goes to
/// semihosting stdout on QEMU/FVP builds. For the rare paths that
/// must produce literal bytes on the physical serial line (the
/// guest-test print-byte HVC); everything else should use the
/// `kprint!` family. Macro (rather than a direct
/// `host::console::write_byte` call) so non-host layers can emit a
/// wire byte without importing `host::*`.
#[macro_export]
macro_rules! raw_wire_byte {
    ($b:expr) => {
        $crate::host::console::write_byte($b)
    };
}

/// Debug-log variant of `kprintln!` for recurring diagnostic messages
/// that dominate the console during phase-B bring-up (e.g., per-trap
/// ELR logs, stage-1 walk summaries, SCTLR writes). Expands to the
/// regular `kprintln!` by default and to a no-op when the `quiet`
/// feature is enabled.
#[cfg(not(feature = "quiet"))]
#[macro_export]
macro_rules! dprintln {
    () => { $crate::kprintln!() };
    ($($arg:tt)*) => { $crate::kprintln!($($arg)*) };
}

#[cfg(feature = "quiet")]
#[macro_export]
macro_rules! dprintln {
    () => {};
    ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }};
}

// ---- Per-category log macros ----------------------------------------
//
// Opt-in via the `log_traps`, `log_irqs`, `log_unaligned`,
// `log_host_io` Cargo features (see Cargo.toml for the category
// inventory and which of them the `default` set carries). The
// `pi-bare-metal*` aggregates omit them all, so real-hardware builds
// boot quietly. The `log_mmu` / `log_tasks` / `log_store` categories
// gate whole diagnostic bodies with `#[cfg]` directly and have no
// macro here.
//
// Each macro expands to `kprintln!` when its feature is enabled and
// to a no-op (formatting argument expression discarded) when it
// isn't. Sites should use these instead of `kprintln!` whenever the
// log line fires periodically — every timer IRQ, every N traps,
// every NativeGetSample, etc.

#[cfg(feature = "log_traps")]
#[macro_export]
macro_rules! log_traps {
    () => { $crate::kprintln!() };
    ($($arg:tt)*) => { $crate::kprintln!($($arg)*) };
}
#[cfg(not(feature = "log_traps"))]
#[macro_export]
macro_rules! log_traps {
    () => {};
    ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }};
}

#[cfg(feature = "log_irqs")]
#[macro_export]
macro_rules! log_irqs {
    () => { $crate::kprintln!() };
    ($($arg:tt)*) => { $crate::kprintln!($($arg)*) };
}
#[cfg(not(feature = "log_irqs"))]
#[macro_export]
macro_rules! log_irqs {
    () => {};
    ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }};
}

#[cfg(feature = "log_unaligned")]
#[macro_export]
macro_rules! log_unaligned {
    () => { $crate::kprintln!() };
    ($($arg:tt)*) => { $crate::kprintln!($($arg)*) };
}
#[cfg(not(feature = "log_unaligned"))]
#[macro_export]
macro_rules! log_unaligned {
    () => {};
    ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }};
}

#[cfg(feature = "log_host_io")]
#[macro_export]
macro_rules! log_host_io {
    () => { $crate::kprintln!() };
    ($($arg:tt)*) => { $crate::kprintln!($($arg)*) };
}
#[cfg(not(feature = "log_host_io"))]
#[macro_export]
macro_rules! log_host_io {
    () => {};
    ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }};
}
