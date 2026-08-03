//! Small JSON settings file for user-facing toggles.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use wisp_audiokit::{LocalModelId, RecognizerBackend};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default)]
    pub local_mcp: LocalMcpSettings,
    #[serde(default)]
    pub transcription: TranscriptionSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TranscriptionProvider {
    Platform,
    Whisper,
    Nemotron,
    /// Stable provider ID retained for future ONNX or plugin backends.
    Other(String),
}

impl Default for TranscriptionProvider {
    fn default() -> Self {
        if cfg!(target_os = "macos") {
            Self::Platform
        } else {
            Self::Whisper
        }
    }
}

impl From<TranscriptionProvider> for RecognizerBackend {
    fn from(provider: TranscriptionProvider) -> Self {
        match provider {
            TranscriptionProvider::Platform => Self::Platform,
            TranscriptionProvider::Whisper | TranscriptionProvider::Other(_) => Self::LocalModel,
            TranscriptionProvider::Nemotron => Self::Nemotron,
        }
    }
}

impl From<RecognizerBackend> for TranscriptionProvider {
    fn from(provider: RecognizerBackend) -> Self {
        match provider {
            RecognizerBackend::Platform => Self::Platform,
            RecognizerBackend::LocalModel => Self::Whisper,
            RecognizerBackend::Nemotron => Self::Nemotron,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptionSettings {
    #[serde(default)]
    pub provider: TranscriptionProvider,
    #[serde(default)]
    pub model: WhisperModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhisperModel {
    Tiny,
    #[default]
    Base,
}

impl From<WhisperModel> for LocalModelId {
    fn from(model: WhisperModel) -> Self {
        match model {
            WhisperModel::Tiny => Self::Tiny,
            WhisperModel::Base => Self::Base,
        }
    }
}

impl TryFrom<LocalModelId> for WhisperModel {
    type Error = ();

    fn try_from(model: LocalModelId) -> Result<Self, Self::Error> {
        match model {
            LocalModelId::Tiny => Ok(Self::Tiny),
            LocalModelId::Base => Ok(Self::Base),
            LocalModelId::Nemotron => Err(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalMcpSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ipc_addr")]
    pub addr: String,
}

impl Default for LocalMcpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            addr: default_ipc_addr(),
        }
    }
}

pub fn load(data_dir: &Path) -> AppSettings {
    let path = settings_path(data_dir);
    let Ok(text) = std::fs::read_to_string(path) else {
        return AppSettings::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(
    data_dir: &Path,
    settings: &AppSettings,
) -> io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let text = serde_json::to_string_pretty(settings)
        .map_err(|err| io::Error::other(format!("serialize settings: {err}")))?;
    std::fs::write(settings_path(data_dir), text)
}

pub fn default_ipc_addr() -> String {
    "127.0.0.1:8765".to_owned()
}

fn settings_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::{AppSettings, LocalMcpSettings, TranscriptionProvider, TranscriptionSettings};

    #[test]
    fn missing_fields_default() {
        let settings = serde_json::from_str::<AppSettings>("{}").expect("parse");
        assert!(!settings.local_mcp.enabled);
        assert_eq!(settings.local_mcp.addr, "127.0.0.1:8765");
        assert_eq!(
            settings.transcription.provider,
            if cfg!(target_os = "macos") {
                TranscriptionProvider::Platform
            } else {
                TranscriptionProvider::Whisper
            }
        );
        assert_eq!(settings.transcription.model, super::WhisperModel::Base);
    }

    #[test]
    fn local_mcp_roundtrips() {
        let settings = AppSettings {
            local_mcp: LocalMcpSettings {
                enabled: true,
                addr: "127.0.0.1:9001".into(),
            },
            transcription: TranscriptionSettings {
                provider: TranscriptionProvider::Whisper,
                model: super::WhisperModel::Base,
            },
        };
        let text = serde_json::to_string(&settings).expect("serialize");
        let parsed = serde_json::from_str::<AppSettings>(&text).expect("parse");
        assert!(parsed.local_mcp.enabled);
        assert_eq!(parsed.local_mcp.addr, "127.0.0.1:9001");
        assert_eq!(
            parsed.transcription.provider,
            TranscriptionProvider::Whisper
        );
        assert_eq!(parsed.transcription.model, super::WhisperModel::Base);
    }
}
