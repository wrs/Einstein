// build.rs
//
// Selects between booting the real Newton ROM (default) and a small ARM
// guest test image (if $NH_GUEST_TEST is set to a .bin file). Sets the
// `nh_guest_test` cfg so `guest_mem.rs` takes the test-mode branch.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=NH_GUEST_TEST");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=roms/newton.rom");

    // Tell rustc that `nh_guest_test` is a known cfg so it doesn't warn.
    println!("cargo:rustc-check-cfg=cfg(nh_guest_test)");

    if let Ok(path) = env::var("NH_GUEST_TEST") {
        let p = PathBuf::from(&path);
        if !p.is_file() {
            panic!("NH_GUEST_TEST={} is not a file", path);
        }
        let abs = p.canonicalize().expect("canonicalize NH_GUEST_TEST");
        println!(
            "cargo:warning=nh-baremetal: guest-test mode, embedding {}",
            abs.display()
        );
        println!("cargo:rustc-env=NH_GUEST_TEST_PATH={}", abs.display());
        println!("cargo:rustc-cfg=nh_guest_test");
        println!("cargo:rerun-if-changed={}", abs.display());
    }
}
