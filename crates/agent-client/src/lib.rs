//! Windows-side client for the RustVNC macOS agent.
//!
//! The video path stays on plain RFB (`vnc-rs`); this crate is the second half
//! of the B1 architecture in `agent-design.md`:
//!
//! ```text
//! Windows client ── RFB/SSH (5900) ──► macOS screensharingd   video + input
//!                └─ QUIC/TLS 1.3 (8443) ─► RustVNC Agent       clipboard + audio + control
//! ```
//!
//! * [`pair`] — one-time code exchange; issues a client certificate, pins the
//!   agent CA after checking the fingerprint the agent announced against the
//!   certificate its TLS handshake actually presented.
//! * [`AgentSession`] — long-lived control channel: clipboard both ways, Opus
//!   audio playback, keepalive ping.
//!
//! Everything here is optional at runtime: if the agent is missing or the
//! channel drops, the RFB session keeps working and only the enhanced features
//! degrade.

pub mod audio;
pub mod protocol;
pub mod session;
pub mod state;
pub mod tls;

pub use protocol::{CAP_AUDIO, CAP_DISPLAY, CAP_LAYOUT, ControlMsg, PROTO_VERSION};
pub use session::{
    AgentCommand, AgentEvent, AgentSession, PairConfig, PairOutcome, SessionConfig, pair,
};
pub use state::default_state_dir;
