//! `air-drive` binary entry point.
//!
//! All real logic lives in the library crate (`air_drive`). This file is intentionally
//! thin so the surface that gets shipped as a binary is identical to what integration
//! tests exercise as a library.

#![forbid(unsafe_code)]

fn main() {
    println!("air-drive — not yet wired up. See specs/001-minimal-sync-daemon/.");
}
