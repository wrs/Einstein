//! Pass the fixed linker script to the linker.
//!
//! nhboot has a single target board (the Pi Zero 2 W), so unlike the
//! hypervisor's `build.rs` there is no platform-selected template —
//! `linker.ld` is committed with its load address filled in.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=build.rs");

    // Skip when building for a host target (e.g. `cargo test
    // --target aarch64-apple-darwin`): the host linker doesn't accept
    // GNU-style `-T script.ld`.
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("none") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let script = manifest_dir.join("linker.ld");
    println!("cargo:rustc-link-arg=-T{}", script.display());
}
