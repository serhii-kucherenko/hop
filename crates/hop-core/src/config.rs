use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::Context;
use hop_protocol::control::NodeRole;
use serde::{Deserialize, Serialize};

use crate::layout::RelativePosition;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub local: LocalConfig,
    pub peers: Vec<PeerConfig>,
    #[serde(default)]
    pub handoff: HandoffConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocalConfig {
    pub machine_name: String,
    pub role: NodeRole,
    pub control_bind: String,
    pub data_bind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_ctrl_cmd: Option<bool>,
    pub shared_secret: String,
    pub screen_width: u32,
    pub screen_height: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PeerConfig {
    pub machine_name: String,
    pub control_addr: String,
    pub data_addr: String,
    pub position: RelativePosition,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HandoffConfig {
    #[serde(default)]
    pub mode: HandoffMode,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub logi_peer_host_index: HashMap<String, u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HandoffMode {
    #[default]
    Auto,
    Logi,
    Network,
}

impl Config {
    pub fn from_json_path(path: &Path) -> anyhow::Result<Self> {
        let parsed = Self::from_json_path_unvalidated(path)?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub fn from_json_path_unvalidated(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        let parsed: Config = serde_json::from_str(&raw).context("failed to parse JSON config")?;
        Ok(parsed)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.peers.is_empty() {
            anyhow::bail!("config must include at least one peer");
        }
        if self.local.shared_secret.trim().len() < 16 {
            anyhow::bail!("shared_secret must be at least 16 characters");
        }
        if self.local.machine_name.trim().is_empty() {
            anyhow::bail!("local.machine_name cannot be empty");
        }
        if self.local.control_bind.trim().is_empty() || self.local.data_bind.trim().is_empty() {
            anyhow::bail!("local control/data bind addresses cannot be empty");
        }
        if self
            .peers
            .iter()
            .any(|peer| peer.machine_name.trim().is_empty())
        {
            anyhow::bail!("peer.machine_name cannot be empty");
        }

        if self.local.role == NodeRole::Client
            && self
                .peers
                .iter()
                .any(|peer| peer.control_addr.trim().is_empty() || peer.data_addr.trim().is_empty())
        {
            anyhow::bail!("client mode requires peer control_addr and data_addr");
        }
        Ok(())
    }

    pub fn peer_by_name(&self, machine_name: &str) -> Option<&PeerConfig> {
        self.peers
            .iter()
            .find(|peer| peer.machine_name == machine_name)
    }

    pub fn first_peer(&self) -> &PeerConfig {
        &self.peers[0]
    }

    pub fn write_json_path(&self, path: &Path) -> anyhow::Result<()> {
        let raw = serde_json::to_string_pretty(self).context("failed to serialize JSON config")?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create config directory {}", parent.display())
                })?;
            }
        }
        fs::write(path, raw).with_context(|| format!("failed to write config: {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_mode_defaults_to_auto_when_omitted() {
        let parsed: Config = serde_json::from_str(
            r#"{
                "local": {
                    "machine_name": "nucbox",
                    "role": "server",
                    "control_bind": "0.0.0.0:4600",
                    "data_bind": "0.0.0.0:4601",
                    "shared_secret": "0123456789abcdef",
                    "screen_width": 1920,
                    "screen_height": 1080
                },
                "peers": [
                    {
                        "machine_name": "macbook",
                        "control_addr": "192.168.1.2:4600",
                        "data_addr": "192.168.1.2:4601",
                        "position": "right"
                    }
                ]
            }"#,
        )
        .expect("parse config");

        assert_eq!(parsed.handoff.mode, HandoffMode::Auto);
        assert!(parsed.handoff.logi_peer_host_index.is_empty());
    }

    #[test]
    fn handoff_mode_and_logi_map_parse_from_json() {
        let parsed: Config = serde_json::from_str(
            r#"{
                "local": {
                    "machine_name": "nucbox",
                    "role": "server",
                    "control_bind": "0.0.0.0:4600",
                    "data_bind": "0.0.0.0:4601",
                    "shared_secret": "0123456789abcdef",
                    "screen_width": 1920,
                    "screen_height": 1080
                },
                "peers": [
                    {
                        "machine_name": "macbook",
                        "control_addr": "192.168.1.2:4600",
                        "data_addr": "192.168.1.2:4601",
                        "position": "right"
                    }
                ],
                "handoff": {
                    "mode": "logi",
                    "logi_peer_host_index": {
                        "macbook": 1
                    }
                }
            }"#,
        )
        .expect("parse config");

        assert_eq!(parsed.handoff.mode, HandoffMode::Logi);
        assert_eq!(parsed.handoff.logi_peer_host_index.get("macbook"), Some(&1));
    }
}
