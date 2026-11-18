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
pub enum ModifierTarget {
    Control,
    Command,
    Option,
    None,
}

impl ModifierTarget {
    pub const ALL: [Self; 4] = [Self::Control, Self::Command, Self::Option, Self::None];

    pub fn label(self) -> &'static str {
        match self {
            Self::Control => "⌃ Control",
            Self::Command => "⌘ Command",
            Self::Option => "⌥ Option",
            Self::None => "不发送",
        }
    }
}

/// Maps Windows modifier keys onto the modifiers sent to macOS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMapping {
    pub ctrl: ModifierTarget,
    pub alt: ModifierTarget,
    pub win: ModifierTarget,
}

impl Default for KeyMapping {
    fn default() -> Self {
        Self::standard()
    }
}

impl KeyMapping {
    pub fn standard() -> Self {
        Self {
            ctrl: ModifierTarget::Control,
            alt: ModifierTarget::Option,
            win: ModifierTarget::Command,
        }
    }

    pub fn windows_style() -> Self {
        Self {
            ctrl: ModifierTarget::Command,
            alt: ModifierTarget::Option,
            win: ModifierTarget::Control,
        }
    }

    pub fn is_standard(&self) -> bool {
        *self == Self::standard()
    }

    pub fn is_windows_style(&self) -> bool {
        *self == Self::windows_style()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ViewMode {
    #[default]
    Fit,
    /// One remote pixel per physical screen pixel.
    OneToOne,
    Percent50,
    Percent100,
    Percent200,
}

impl ViewMode {
    pub const ALL: [Self; 5] = [
        Self::Fit,
        Self::OneToOne,
        Self::Percent50,
        Self::Percent100,
        Self::Percent200,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Fit => "适应窗口",
            Self::OneToOne => "1:1 像素",
            Self::Percent50 => "50%",
            Self::Percent100 => "100%",
            Self::Percent200 => "200%",
        }
    }

    /// Size multiplier in egui logical points for one remote pixel.
    pub fn scale(self, pixels_per_point: f32) -> Option<f32> {
        match self {
            Self::Fit => None,
            Self::OneToOne => Some(1.0 / pixels_per_point.max(0.1)),
            Self::Percent50 => Some(0.5),
            Self::Percent100 => Some(1.0),
            Self::Percent200 => Some(2.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ResolutionPreset {
    /// Fit the Mac display mode to the client screen and step down when the
    /// frame rate drops. A 5K or 8K source must not stream at that size into a
    /// 1440p window: every extra pixel costs the Mac a render and the client a
    /// decode, for pixels the user cannot see.
    #[default]
    Auto,
    Native,
    P1080,
    P1440,
    P4K,
}

impl ResolutionPreset {
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Native,
        Self::P1080,
        Self::P1440,
        Self::P4K,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动（按本机屏幕）",
            Self::Native => "原生",
            Self::P1080 => "1920×1080",
            Self::P1440 => "2560×1440",
            Self::P4K => "3840×2160",
        }
    }

    /// A fixed target, if this preset names one.
    ///
    /// `Auto` returns `None` on purpose: its target depends on the client screen
    /// and on the measured frame rate, so it is resolved per session by
    /// [`Self::target_for_client`].
    pub fn dimensions(self) -> Option<(u16, u16)> {
        match self {
            Self::Auto | Self::Native => None,
            Self::P1080 => Some((1920, 1080)),
            Self::P1440 => Some((2560, 1440)),
            Self::P4K => Some((3840, 2160)),
        }
    }

    /// The best preset for a client screen of `client` physical pixels.
    ///
    /// Picking the largest preset that fits keeps text sharp while refusing to
    /// stream pixels the user's screen cannot show.
    pub fn target_for_client(self, client: (u32, u32)) -> Option<Self> {
        let candidates = [Self::P4K, Self::P1440, Self::P1080];
        match self {
            Self::Auto => candidates.into_iter().find(|preset| {
                preset.dimensions().is_some_and(|(width, height)| {
                    u32::from(width) <= client.0 && u32::from(height) <= client.1
                })
            }),
            other if other.dimensions().is_some() => Some(other),
            _ => None,
        }
    }

    /// One rung down the ladder, or `None` when already at the smallest.
    pub fn step_down(self) -> Option<Self> {
        match self {
            Self::Auto | Self::Native | Self::P4K => Some(Self::P1440),
            Self::P1440 => Some(Self::P1080),
            Self::P1080 => None,
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
    /// Frame-rate cap for the custom preset (5-60). Presets ignore it.
    #[serde(default = "default_custom_fps")]
    pub custom_fps: u32,
    pub encoding: EncodingPreference,
    /// Opt-in display-mode request applied at session start through the agent.
    #[serde(default)]
    pub resolution: ResolutionPreset,
    /// Remote-screen zoom policy (M2a). Applies immediately, no reconnect.
    #[serde(default)]
    pub view_mode: ViewMode,
    /// Adapt the request rate to measured frame latency (M2b). When false, the
    /// configured quality frame rate is used except for static-screen backoff.
    #[serde(default = "default_true")]
    pub adaptive: bool,
    /// Windows modifier keys -> macOS modifiers.
    #[serde(default)]
    pub key_mapping: KeyMapping,
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
            custom_fps: default_custom_fps(),
            encoding: EncodingPreference::Zrle,
            resolution: ResolutionPreset::Native,
            view_mode: ViewMode::Fit,
            adaptive: true,
            key_mapping: KeyMapping::standard(),
            apple_encodings: false,
            agent: AgentConfig::default(),
            security: SecurityMode::Direct,
            ssh: SshConfig::default(),
        }
    }
}

fn default_custom_fps() -> u32 {
    30
}

fn default_true() -> bool {
    true
}

impl ClientConfig {
    pub fn fps(&self) -> u32 {
        match self.quality {
            QualityPreset::Custom => self.custom_fps.clamp(5, 60),
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
        encodings.push(VncEncoding::PointerPosPseudo);
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
    fn view_modes_scale_for_hidpi() {
        assert_eq!(ViewMode::OneToOne.scale(2.0), Some(0.5));
        assert_eq!(ViewMode::OneToOne.scale(1.0), Some(1.0));
        assert_eq!(ViewMode::Percent100.scale(2.0), Some(1.0));
        assert_eq!(ViewMode::Percent200.scale(2.0), Some(2.0));
        assert_eq!(ViewMode::Fit.scale(2.0), None);
    }

    #[test]
    fn adaptive_defaults_on_and_can_be_disabled() {
        let mut config = ClientConfig::default();
        assert!(config.adaptive);
        config.adaptive = false;
        assert!(!config.adaptive);
    }

    #[test]
    fn raw_is_always_offered() {
        let config = ClientConfig::default();
        assert!(config.encodings().contains(&VncEncoding::Raw));
        assert!(config.encodings().contains(&VncEncoding::Zrle));
    }

    #[test]
    fn auto_resolution_fits_the_client_screen() {
        // The real case: a 2560x1440 Windows display must not pull 8960 pixels
        // per row from a Mac driving side-by-side virtual displays.
        let fits = ResolutionPreset::Auto.target_for_client((2560, 1440));
        assert_eq!(fits, Some(ResolutionPreset::P1440));
        assert_eq!(
            fits.and_then(|preset| preset.dimensions()),
            Some((2560, 1440))
        );

        assert_eq!(
            ResolutionPreset::Auto.target_for_client((1920, 1200)),
            Some(ResolutionPreset::P1080)
        );
        assert_eq!(
            ResolutionPreset::Auto.target_for_client((3840, 2160)),
            Some(ResolutionPreset::P4K)
        );
        // Smaller than the smallest rung: leave the Mac alone rather than
        // shrinking it to something the client cannot even display.
        assert_eq!(ResolutionPreset::Auto.target_for_client((1280, 720)), None);
    }

    #[test]
    fn fixed_presets_ignore_the_client_screen() {
        assert_eq!(
            ResolutionPreset::P4K.target_for_client((1920, 1080)),
            Some(ResolutionPreset::P4K)
        );
        assert_eq!(
            ResolutionPreset::Native.target_for_client((1920, 1080)),
            None
        );
        assert_eq!(ResolutionPreset::Auto.dimensions(), None);
        assert_eq!(ResolutionPreset::default(), ResolutionPreset::Auto);
    }

    #[test]
    fn the_ladder_ends_at_1080p() {
        assert_eq!(
            ResolutionPreset::P4K.step_down(),
            Some(ResolutionPreset::P1440)
        );
        assert_eq!(
            ResolutionPreset::P1440.step_down(),
            Some(ResolutionPreset::P1080)
        );
        assert_eq!(ResolutionPreset::P1080.step_down(), None);
    }
}
