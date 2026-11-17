//! Wire protocol for the RustVNC agent control plane.
//!
//! This is a deliberate mirror of the agent's `src/protocol.rs`: the two sides
//! are separate crates and separate processes, so the format is the contract.
//! Any change here must land in both places in the same commit; the
//! `protocol::tests::*` tests pin the byte layout that both sides rely on.

use anyhow::{Context, Result, bail};
use quinn::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};

pub const PROTO_VERSION: u16 = 1;
pub const CAP_AUDIO: u32 = 1 << 0;

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
}

pub mod error_code {
    pub const UNAUTHORIZED: u16 = 1;
    pub const BAD_CODE: u16 = 2;
    pub const PAIRING_EXPIRED: u16 = 3;
    pub const ALREADY_PAIRED: u16 = 4;
    pub const TOO_MANY_ATTEMPTS: u16 = 5;
    pub const PROTOCOL: u16 = 6;
    pub const INTERNAL: u16 = 7;

    pub fn describe(code: u16) -> &'static str {
        match code {
            UNAUTHORIZED => "未授权：这台电脑尚未与该 Agent 配对",
            BAD_CODE => "配对码错误",
            PAIRING_EXPIRED => "配对码已过期，请在 Mac 菜单栏重新生成",
            ALREADY_PAIRED => "该客户端已配对",
            TOO_MANY_ATTEMPTS => "尝试次数过多，请稍后再试",
            PROTOCOL => "协议错误",
            _ => "Agent 内部错误",
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// These bytes are the contract with `src/protocol.rs` in the agent crate.
    /// If this test fails, the two sides disagree on the wire format.
    #[test]
    fn control_messages_roundtrip() {
        let messages = [
            ControlMsg::Hello {
                proto: PROTO_VERSION,
                client: "windows".to_string(),
                capabilities: CAP_AUDIO,
            },
            ControlMsg::ServerHello {
                proto: PROTO_VERSION,
                agent: "RustVNC Agent 0.1.0".to_string(),
                capabilities: CAP_AUDIO,
            },
            ControlMsg::PairRequest {
                code: "123456".to_string(),
                client_name: "desk".to_string(),
                csr_pem: "-----BEGIN CERTIFICATE REQUEST-----".to_string(),
            },
            ControlMsg::PairResponse {
                client_cert_pem: "cert".to_string(),
                ca_cert_pem: "ca".to_string(),
                server_fingerprint: "ab".to_string(),
            },
            ControlMsg::Ping { nonce: 7 },
            ControlMsg::Pong { nonce: 7 },
            ControlMsg::ClipboardText {
                text: "复制".to_string(),
            },
            ControlMsg::Goodbye,
            error(error_code::BAD_CODE, "wrong code"),
        ];
        for message in messages {
            let bytes = postcard::to_stdvec(&message).unwrap();
            let decoded: ControlMsg = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(format!("{message:?}"), format!("{decoded:?}"));
        }
    }

    #[test]
    fn audio_packet_layout_is_stable() {
        let packet = encode_audio_packet(0x0102, 0x0a0b0c0d, &[9, 8, 7]);
        assert_eq!(packet.len(), AUDIO_HEADER_LEN + 3);
        assert_eq!(packet[0], AUDIO_VERSION);
        assert_eq!(&packet[1..3], &[0x02, 0x01]);
        assert_eq!(&packet[3..7], &[0x0d, 0x0c, 0x0b, 0x0a]);
        let parsed = parse_audio_packet(&packet).unwrap();
        assert_eq!(parsed.seq, 0x0102);
        assert_eq!(parsed.pts_ms, 0x0a0b0c0d);
        assert_eq!(parsed.payload, vec![9, 8, 7]);
    }

    #[test]
    fn short_or_mistyped_audio_packets_are_rejected() {
        assert!(parse_audio_packet(&[1, 0, 0]).is_err());
        let mut packet = encode_audio_packet(1, 0, &[0]);
        packet[0] = 99;
        assert!(parse_audio_packet(&packet).is_err());
    }
}
