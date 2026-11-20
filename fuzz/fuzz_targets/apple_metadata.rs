//! Fuzz the Apple pseudo-encoding parser.
//!
//! The parser reads counts and lengths out of a server-controlled payload, so it
//! is the most exposed piece of custom decoding in the project: a length field
//! that is trusted a little too much is a panic (or worse) on a hostile server.
//!
//! `parse` must never panic and never allocate more than a bounded amount,
//! whatever the body says — the caps in `vendor/vnc-rs/src/limits.rs` are the
//! defence being exercised here.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Every Apple pseudo-encoding the client requests, plus a few values around them
/// so the default arm is exercised too.
const CODES: [i32; 8] = [-1104, -1105, -1107, -1109, -1110, 0, 1107, i32::MIN];

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let code = CODES[data[0] as usize % CODES.len()];
    let body = &data[1..];
    // The contract is "returns something, never panics"; the interesting cases
    // are truncated headers and lying length prefixes.
    let _ = vnc::apple::parse(code, body);
});
