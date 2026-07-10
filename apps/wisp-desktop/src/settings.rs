//! Small JSON settings file for user-facing toggles.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default)]
    pub local_mcp: LocalMcpSettings,
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
    use super::{AppSettings, LocalMcpSettings};

    #[test]
    fn missing_fields_default() {
        let settings = serde_json::from_str::<AppSettings>("{}").expect("parse");
        assert!(!settings.local_mcp.enabled);
        assert_eq!(settings.local_mcp.addr, "127.0.0.1:8765");
    }

    #[test]
    fn local_mcp_roundtrips() {
        let settings = AppSettings {
            local_mcp: LocalMcpSettings {
                enabled: true,
                addr: "127.0.0.1:9001".into(),
            },
        };
        let text = serde_json::to_string(&settings).expect("serialize");
        let parsed = serde_json::from_str::<AppSettings>(&text).expect("parse");
        assert!(parsed.local_mcp.enabled);
        assert_eq!(parsed.local_mcp.addr, "127.0.0.1:9001");
    }
}
