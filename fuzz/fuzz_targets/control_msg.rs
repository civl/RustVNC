//! Fuzz the agent control protocol decoder.
//!
//! The Windows client decodes every control message the Mac agent sends (and the
//! agent decodes every message the client sends), so a panic here is a remote
//! crash on either side. The wire format is postcard, which is compact but has no
//! length prefix of its own: a malformed length can ask for a huge allocation.
//!
//! The property under test: decoding arbitrary bytes either yields a message or
//! an error, and never panics.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rustvnc_agent_client::protocol::ControlMsg;

fuzz_target!(|data: &[u8]| {
    let _ = postcard::from_bytes::<ControlMsg>(data);
});
