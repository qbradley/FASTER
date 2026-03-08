//! Build script for faster-ffi: regenerate `include/faster.h` via cbindgen.
//!
//! If cbindgen is not installed, the build proceeds with the checked-in
//! header (if present), emitting a warning. This lets CI and downstream
//! consumers build without requiring cbindgen as a host tool.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Re-run when the FFI source or cbindgen config changes.
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=src/error.rs");
    println!("cargo:rerun-if-changed=src/handle.rs");
    println!("cargo:rerun-if-changed=src/session.rs");
    println!("cargo:rerun-if-changed=src/functions.rs");
    println!("cargo:rerun-if-changed=src/callbacks.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let crate_dir = PathBuf::from(crate_dir);
    let output = crate_dir.join("include").join("faster.h");

    // Attempt to run cbindgen.
    let status = Command::new("cbindgen")
        .arg("--config")
        .arg(crate_dir.join("cbindgen.toml"))
        .arg("--crate")
        .arg("faster-ffi")
        .arg("--output")
        .arg(&output)
        .current_dir(crate_dir.parent().unwrap().parent().unwrap()) // workspace root
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:warning=cbindgen: regenerated include/faster.h");
        }
        Ok(s) => {
            println!("cargo:warning=cbindgen exited with {s}; using existing header if available");
        }
        Err(e) => {
            println!("cargo:warning=cbindgen not found ({e}); using existing header if available");
        }
    }
}
