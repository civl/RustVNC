use serde::{Deserialize, Serialize};
use vnc::{PixelFormat, VncEncoding};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualityPreset {
    Speed,
    Balanced,
    High,
    Ultra,
    Custom,
}

impl QualityPreset {
    pub const ALL: [Self; 5] = [
        Self::Speed,
        Self::Balanced,
        Self::High,
        Self::Ultra,
        Self::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Speed => "极速",
            Self::Balanced => "流畅",
            Self::High => "高清",
            Self::Ultra => "超清",
            Self::Custom => "自定义",
        }
    }

    pub fn defaults(self) -> (u8, EncodingPreference, u32) {
        match self {
            Self::Speed => (16, EncodingPreference::Zrle, 10),
            Self::Balanced => (16, EncodingPreference::Zrle, 20),
            Self::High => (32, EncodingPreference::Zrle, 30),
            Self::Ultra => (32, EncodingPreference::Zrle, 60),
            Self::Custom => (32, EncodingPreference::Zrle, 30),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncodingPreference {
    Zrle,
    Tight,
    Raw,
}

impl EncodingPreference {
    pub const ALL: [Self; 3] = [Self::Zrle, Self::Tight, Self::Raw];

    pub fn label(self) -> &'static str {
        match self {
            Self::Zrle => "ZRLE",
            Self::Tight => "Tight",
            Self::Raw => "Raw",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMode {
    Auto,
    Vnc,
    Ard,
}

impl AuthMode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Vnc, Self::Ard];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动（优先 ARD）",
            Self::Vnc => "VNC 密码",
            Self::Ard => "macOS 账号（ARD）",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecurityMode {
    Direct,
    Ssh,
}

impl SecurityMode {
    pub const ALL: [Self; 2] = [Self::Direct, Self::Ssh];

    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "直连（TLS 由 Tailscale 提供）",
            Self::Ssh => "SSH 隧道（推荐）",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshConfig {
    pub user: String,
    pub port: u16,
    pub key_path: String,
    pub known_hosts: String,
    /// Never written to `profiles.json`: filled in from the credential store
    /// (or the connection form) just before connecting.
    #[serde(skip)]
    pub password: String,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            user: String::new(),
            port: 22,
            key_path: String::new(),
            known_hosts: String::new(),
            password: String::new(),
        }
    }
}

/// The optional enhanced channel to the macOS agent (clipboard + audio + TLS).
///
/// Off by default: the agent is an extra install on the Mac, and the RFB path
/// works without it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub enabled: bool,
    pub port: u16,
    pub audio: bool,
    /// One-time code typed once while pairing; never persisted.
    #[serde(skip)]
    pub pair_code: String,
    /// Fingerprint to compare against when pairing; never persisted.
    #[serde(skip)]
    pub fingerprint: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8443,
            audio: true,
            pair_code: String::new(),
            fingerprint: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub host: String,
    pub port: u16,
    pub shared: bool,
    pub auto_reconnect: bool,
    pub user: String,
    pub auth: AuthMode,
    pub quality: QualityPreset,
    pub bpp: u8,
    pub encoding: EncodingPreference,
    /// Advertise Apple's private pseudo-encodings (1104/1105/1107/1109/1110).
    ///
    /// They are parsed safely (length-prefixed skip) but off by default: their
    /// bodies are undocumented, so enabling them is a per-connection experiment
    /// to be validated against the macOS build in use.
    #[serde(default)]
    pub apple_encodings: bool,
    #[serde(default)]
    pub agent: AgentConfig,
    pub security: SecurityMode,
    pub ssh: SshConfig,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 5900,
            shared: true,
            auto_reconnect: true,
            user: String::new(),
            auth: AuthMode::Auto,
            quality: QualityPreset::High,
            bpp: 32,
            encoding: EncodingPreference::Zrle,
            apple_encodings: false,
            agent: AgentConfig::default(),
            security: SecurityMode::Direct,
            ssh: SshConfig::default(),
        }
    }
}

impl ClientConfig {
    pub fn fps(&self) -> u32 {
        match self.quality {
            QualityPreset::Custom => 30,
            other => other.defaults().2,
        }
    }

    pub fn pixel_format(&self) -> PixelFormat {
        let mut format = PixelFormat::default();
        if self.bpp == 16 {
            format.bits_per_pixel = 16;
            format.depth = 16;
            format.red_max = 31;
            format.green_max = 63;
            format.blue_max = 31;
            format.red_shift = 11;
            format.green_shift = 5;
            format.blue_shift = 0;
        }
        format
    }

    pub fn encodings(&self) -> Vec<VncEncoding> {
        let mut encodings = Vec::new();
        match self.encoding {
            EncodingPreference::Zrle => encodings.push(VncEncoding::Zrle),
            EncodingPreference::Tight => encodings.push(VncEncoding::Tight),
            EncodingPreference::Raw => {}
        }
        if self.encoding != EncodingPreference::Raw {
            // zlib(6) is a separate encoding from ZRLE's internal stream and is
            // better for large flat/text areas; macOS supports both.
            encodings.push(VncEncoding::Zlib);
        }
        encodings.push(VncEncoding::CopyRect);
        // Cursor shapes are only defined for 32 bpp in this engine, and the UI
        // draws the local pointer anyway; at 16 bpp the updates would be
        // discarded and only cost bandwidth.
        if self.bpp == 32 {
            encodings.push(VncEncoding::CursorPseudo);
        }
        encodings.push(VncEncoding::DesktopSizePseudo);
        if self.apple_encodings {
            encodings.extend([
                VncEncoding::AppleCursorPseudo,
                VncEncoding::AppleDisplayPseudo,
                VncEncoding::AppleCapabilitiesPseudo,
                VncEncoding::AppleKeyboardPseudo,
                VncEncoding::AppleMachinePseudo,
            ]);
        }
        encodings.push(VncEncoding::Raw);
        encodings
    }

    pub fn apply_preset(&mut self, preset: QualityPreset) {
        self.quality = preset;
        let (bpp, encoding, _) = preset.defaults();
        self.bpp = bpp;
        self.encoding = encoding;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_set_bpp_and_encoding() {
        let mut config = ClientConfig::default();
        config.apply_preset(QualityPreset::Speed);
        assert_eq!(config.bpp, 16);
        assert_eq!(config.encoding, EncodingPreference::Zrle);
        assert_eq!(config.pixel_format().bits_per_pixel, 16);

        config.apply_preset(QualityPreset::Ultra);
        assert_eq!(config.bpp, 32);
        assert_eq!(config.pixel_format().bits_per_pixel, 32);
    }

    #[test]
    fn raw_is_always_offered() {
        let config = ClientConfig::default();
        assert!(config.encodings().contains(&VncEncoding::Raw));
        assert!(config.encodings().contains(&VncEncoding::Zrle));
    }
}
