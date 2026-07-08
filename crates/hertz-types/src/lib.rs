use serde::{Deserialize, Serialize};

pub mod wire;
pub use wire::{
    ActivityEntry, DecodedAudioFrame, DoctorReport, DongleDoctorEntry, DongleSummary,
    ListenRequest, RecordingFileEntry, RecordingRequest, SquelchRequest, StatusResponse,
    TranscriptEntry, TuneRequest, WsServerMsg,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    Audio {
        dongle_id: String,
        channel: Option<String>,
        freq_hz: u64,
        samples: Vec<f32>,
        signal_db: f32,
    },
    SignalLevel {
        dongle_id: String,
        channel: Option<String>,
        freq_hz: u64,
        signal_db: f32,
        noise_floor: f32,
        squelch_open: bool,
        audio_flatness: f32,
    },
    SquelchEvent {
        dongle_id: String,
        channel: Option<String>,
        freq_hz: u64,
        open: bool,
        signal_db: f32,
        classification: String,
    },
    ChannelActivity {
        dongle_id: String,
        active: Vec<ChannelActivityInfo>,
        noise_floor: f32,
    },
    Transcription {
        dongle_id: String,
        channel: Option<String>,
        freq_hz: u64,
        text: String,
    },
    Translation {
        dongle_id: String,
        channel: Option<String>,
        freq_hz: u64,
        text: String,
        language: String,
    },
    RecordingSaved {
        dongle_id: String,
        channel: Option<String>,
        freq_hz: u64,
        filename: String,
        filepath: String,
        duration_sec: f32,
    },
    ScanState {
        dongle_id: String,
        state: String,
        current_channel: Option<String>,
        current_freq_hz: u64,
    },
    DongleStatus {
        dongle_id: String,
        serial: String,
        online: bool,
        role: DongleRole,
        message: Option<String>,
    },
    TxEvent {
        dongle_id: String,
        channel: String,
        freq_hz: u64,
        status: String,
        text: Option<String>,
        reason: Option<String>,
    },
    VoicePaint {
        dongle_id: String,
        painting: VoicePaintingData,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelActivityInfo {
    pub channel: String,
    pub label: String,
    pub freq_hz: u64,
    pub signal_db: f32,
    pub classification: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaintRegion {
    pub label: String,
    pub freq_lo: f32,
    pub freq_hi: f32,
    pub time_start: f32,
    pub time_end: f32,
    pub color: String,
    pub opacity: f32,
    pub style: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoicePaintingData {
    pub description: String,
    pub regions: Vec<PaintRegion>,
    pub timestamp: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Nfm,
    Am,
    Usb,
    Lsb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LicenseReq {
    None,
    Gmrs,
    Ham,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TxPolicy {
    Never,
    CertifiedRadio { license: LicenseReq },
    SdrOrRadio { license: LicenseReq },
}

impl<'de> serde::Deserialize<'de> for TxPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let s_lower = s.to_lowercase();
        if s_lower == "never" {
            return Ok(TxPolicy::Never);
        }
        let parts: Vec<&str> = s_lower.split('+').collect();
        let policy_type = parts[0];
        let license_str = parts.get(1).copied().unwrap_or("none");

        let license = match license_str {
            "none" | "no-license" | "license-free" | "free" => LicenseReq::None,
            "gmrs" | "gmrs-license" => LicenseReq::Gmrs,
            "ham" | "ham-license" => LicenseReq::Ham,
            _ => {
                return Err(serde::de::Error::custom(format!(
                    "Unknown license requirement: {}",
                    license_str
                )))
            }
        };

        match policy_type {
            "certified-radio" | "certified" => Ok(TxPolicy::CertifiedRadio { license }),
            "sdr-or-radio" | "sdr_or_radio" | "sdr" => Ok(TxPolicy::SdrOrRadio { license }),
            _ => Err(serde::de::Error::custom(format!(
                "Unknown tx policy type: {}",
                policy_type
            ))),
        }
    }
}

impl serde::Serialize for TxPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            TxPolicy::Never => serializer.serialize_str("never"),
            TxPolicy::CertifiedRadio { license } => {
                let lic_str = match license {
                    LicenseReq::None => "none",
                    LicenseReq::Gmrs => "gmrs",
                    LicenseReq::Ham => "ham",
                };
                serializer.serialize_str(&format!("certified-radio+{}", lic_str))
            }
            TxPolicy::SdrOrRadio { license } => {
                let lic_str = match license {
                    LicenseReq::None => "none",
                    LicenseReq::Gmrs => "gmrs",
                    LicenseReq::Ham => "ham",
                };
                serializer.serialize_str(&format!("sdr-or-radio+{}", lic_str))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DongleRole {
    Channelized,
    Hopscan,
    Monitor,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub freq_hz: u64,
    pub mode: Mode,
    pub bandwidth_hz: u32,
    pub group: String,
    pub label: String,
    pub rx: bool,
    pub tx_policy: TxPolicy,
    pub ctcss_hz: Option<f32>,
    #[serde(default)]
    pub continuous_carrier: bool,
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DongleConfig {
    pub serial: String,
    pub role: DongleRole,
    pub bandplan: Option<String>,
    pub tap_channel: Option<String>,
    pub groups: Option<Vec<String>>,
    #[serde(default = "default_dwell_ms")]
    pub dwell_ms: Option<u64>,
    #[serde(default)]
    pub priority: Option<Vec<String>>,
    pub squelch_db: f32,
    pub record: bool,
    pub frequency_hz: Option<u64>,
}

fn default_dwell_ms() -> Option<u64> {
    Some(150)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonSettings {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    pub auth_token: Option<String>,
}

fn default_listen() -> String {
    "0.0.0.0:9080".to_string()
}

fn default_data_dir() -> String {
    "/data".to_string()
}

impl DaemonSettings {
    pub fn get_auth_token(&self) -> Option<String> {
        self.auth_token.as_ref().map(|t| {
            if let Some(env_var) = t.strip_prefix("env:") {
                std::env::var(env_var).unwrap_or_default()
            } else {
                t.clone()
            }
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WhisperHttpConfig {
    pub url: String,
    pub model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptionConfig {
    #[serde(default = "default_engine")]
    pub engine: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub translate_to: String,
    pub whisper_http: Option<WhisperHttpConfig>,
    pub cleanup: Option<CleanupConfig>,
}

fn default_engine() -> String {
    "whisper-internal".to_string()
}
fn default_model() -> String {
    "small".to_string()
}
fn default_language() -> String {
    "auto".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LicenseConfig {
    pub gmrs: Option<String>,
    pub ham_callsign: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxConfig {
    pub license: Option<LicenseConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub daemon: DaemonSettings,
    #[serde(rename = "dongle")]
    pub dongles: Vec<DongleConfig>,
    pub transcription: Option<TranscriptionConfig>,
    pub tx: Option<TxConfig>,
}

impl DaemonConfig {
    pub fn load(path: Option<&str>) -> Result<Self, anyhow::Error> {
        let config_path = if let Some(p) = path {
            p.to_string()
        } else if let Ok(env_path) = std::env::var("HERTZ_CONFIG") {
            env_path
        } else {
            if std::path::Path::new("hertz.toml").exists() {
                "hertz.toml".to_string()
            } else if std::path::Path::new("/etc/hertz/hertz.toml").exists() {
                "/etc/hertz/hertz.toml".to_string()
            } else {
                return Err(anyhow::anyhow!("No config file found"));
            }
        };

        let content = std::fs::read_to_string(&config_path)?;
        let config: DaemonConfig = toml::from_str(&content)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_config_parses() {
        let toml_str = r#"
            [daemon]
            listen = "0.0.0.0:9080"
            data_dir = "/data"
            auth_token = "env:HERTZ_TOKEN"

            [[dongle]]
            serial = "MARINE01"
            role = "channelized"
            bandplan = "marine-vhf-us"
            tap_channel = "16"
            squelch_db = 12.0
            record = true

            [[dongle]]
            serial = "PUBLIC01"
            role = "hopscan"
            groups = ["frs-gmrs", "murs", "noaa-wx", "ham-2m-simplex", "railroad-aar"]
            dwell_ms = 150
            priority = ["noaa-wx:WX2"]
            squelch_db = 9.0
            record = true
        "#;
        let config: DaemonConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.daemon.listen, "0.0.0.0:9080");
        assert_eq!(config.daemon.data_dir, "/data");
        assert_eq!(config.daemon.auth_token.as_deref(), Some("env:HERTZ_TOKEN"));
        assert_eq!(config.dongles.len(), 2);
        assert_eq!(config.dongles[0].serial, "MARINE01");
        assert_eq!(config.dongles[0].role, DongleRole::Channelized);
        assert_eq!(config.dongles[1].serial, "PUBLIC01");
        assert_eq!(config.dongles[1].role, DongleRole::Hopscan);
        assert_eq!(config.dongles[1].dwell_ms, Some(150));
    }
}
