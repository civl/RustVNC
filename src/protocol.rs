use anyhow::{Context, Result, bail};
use quinn::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};

pub const PROTO_VERSION: u16 = 1;
pub const CAP_AUDIO: u32 = 1 << 0;
pub const CAP_DISPLAY: u32 = 1 << 1;
/// The agent can switch between single-screen (mirrored) and multi-screen.
pub const CAP_LAYOUT: u32 = 1 << 2;

pub const AUDIO_VERSION: u8 = 1;
pub const AUDIO_HEADER_LEN: usize = 7;

#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub seq: u16,
    pub pts_ms: u32,
    pub payload: Vec<u8>,
}

pub fn encode_audio_packet(seq: u16, pts_ms: u32, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(AUDIO_HEADER_LEN + payload.len());
    packet.push(AUDIO_VERSION);
    packet.extend_from_slice(&seq.to_le_bytes());
    packet.extend_from_slice(&pts_ms.to_le_bytes());
    packet.extend_from_slice(payload);
    packet
}

pub fn parse_audio_packet(bytes: &[u8]) -> Result<AudioPacket> {
    if bytes.len() <= AUDIO_HEADER_LEN {
        bail!("audio packet too short: {}", bytes.len());
    }
    if bytes[0] != AUDIO_VERSION {
        bail!("unsupported audio packet version {}", bytes[0]);
    }
    let seq = u16::from_le_bytes([bytes[1], bytes[2]]);
    let pts_ms = u32::from_le_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]);
    Ok(AudioPacket {
        seq,
        pts_ms,
        payload: bytes[AUDIO_HEADER_LEN..].to_vec(),
    })
}
pub const ALPN: &[u8] = b"rustvnc-agent/1";

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_CONTROL_STREAMS: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMsg {
    Hello {
        proto: u16,
        client: String,
        capabilities: u32,
    },
    ServerHello {
        proto: u16,
        agent: String,
        capabilities: u32,
    },
    PairRequest {
        code: String,
        client_name: String,
        csr_pem: String,
    },
    PairResponse {
        client_cert_pem: String,
        ca_cert_pem: String,
        server_fingerprint: String,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    ClipboardText {
        text: String,
    },
    Goodbye,
    Error {
        code: u16,
        message: String,
    },
    /// Ask the macOS agent to switch the main display mode for this session.
    SetDisplayMode {
        width: u16,
        height: u16,
    },
    /// Result of [`ControlMsg::SetDisplayMode`].
    DisplayModeResult {
        width: u16,
        height: u16,
        applied: bool,
        message: String,
    },
    /// Ask the macOS agent to stream a single screen (mirror every display onto
    /// the main one, so the framebuffer shrinks to one screen) or restore the
    /// normal multi-display layout.
    SetDisplayLayout {
        single: bool,
    },
    /// Result of [`ControlMsg::SetDisplayLayout`].
    DisplayLayoutResult {
        single: bool,
        applied: bool,
        message: String,
    },
}

pub mod error_code {
    pub const UNAUTHORIZED: u16 = 1;
    pub const BAD_CODE: u16 = 2;
    pub const PAIRING_EXPIRED: u16 = 3;
    pub const ALREADY_PAIRED: u16 = 4;
    pub const TOO_MANY_ATTEMPTS: u16 = 5;
    pub const PROTOCOL: u16 = 6;
    pub const INTERNAL: u16 = 7;
}

pub fn error(code: u16, message: impl Into<String>) -> ControlMsg {
    ControlMsg::Error {
        code,
        message: message.into(),
    }
}

pub async fn write_msg(send: &mut SendStream, msg: &ControlMsg) -> Result<()> {
    let bytes = postcard::to_stdvec(msg).context("serialize control message")?;
    if bytes.len() > MAX_FRAME_BYTES {
        bail!("control message too large: {} bytes", bytes.len());
    }
    send.write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .context("write control frame length")?;
    send.write_all(&bytes)
        .await
        .context("write control frame payload")?;
    Ok(())
}

pub async fn read_msg(recv: &mut RecvStream) -> Result<ControlMsg> {
    let mut length = [0u8; 4];
    recv.read_exact(&mut length)
        .await
        .context("read control frame length")?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        bail!("invalid control frame length: {length}");
    }

    let mut payload = vec![0u8; length];
    recv.read_exact(&mut payload)
        .await
        .context("read control frame payload")?;
    postcard::from_bytes(&payload).context("deserialize control message")
}

pub fn max_control_streams() -> u32 {
    MAX_CONTROL_STREAMS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_roundtrip() {
        let msg = ControlMsg::Ping { nonce: 42 };
        let bytes = postcard::to_stdvec(&msg).unwrap();
        let decoded: ControlMsg = postcard::from_bytes(&bytes).unwrap();
        match decoded {
            ControlMsg::Ping { nonce } => assert_eq!(nonce, 42),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn error_roundtrip() {
        let msg = error(error_code::UNAUTHORIZED, "no certificate");
        let bytes = postcard::to_stdvec(&msg).unwrap();
        let decoded: ControlMsg = postcard::from_bytes(&bytes).unwrap();
        match decoded {
            ControlMsg::Error { code, message } => {
                assert_eq!(code, error_code::UNAUTHORIZED);
                assert_eq!(message, "no certificate");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
}
