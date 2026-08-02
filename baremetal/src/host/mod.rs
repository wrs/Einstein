//! Host drivers and backends: console, platform glue, DMA, SD, USB,
//! display, audio, input, host-IO and flash-persistence backends.

pub mod audio;
pub mod console;
#[cfg(feature = "platform-raspi3b")]
pub mod display;
pub mod flash_persist;
pub mod host_dma;
pub mod host_io;
pub mod input;
pub mod macros;
#[cfg(feature = "platform-raspi3b")]
pub mod mailbox;
pub mod platform;
#[cfg(feature = "platform-raspi3b")]
pub mod sd;
#[cfg(nh_input_mtouch)]
pub mod usb;
