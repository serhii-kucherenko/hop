use std::fs;
use std::path::Path;

use anyhow::Context;
use hop_protocol::control::NodeRole;
use serde::Deserialize;

use crate::layout::RelativePosition;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub local: LocalConfig,
    pub peers: Vec<PeerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocalConfig {
    pub machine_name: String,
    pub role: NodeRole,
    pub control_bind: String,
    pub data_bind: String,
    pub shared_secret: String,
    pub screen_width: u32,
    pub screen_height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PeerConfig {
    pub machine_name: String,
    pub control_addr: String,
    pub data_addr: String,
    pub position: RelativePosition,
}

impl Config {
    pub fn from_json_path(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        let parsed: Config = serde_json::from_str(&raw).context("failed to parse JSON config")?;
        parsed.validate()?;
        Ok(parsed)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.peers.is_empty() {
            anyhow::bail!("config must include at least one peer");
        }
        if self.local.shared_secret.trim().len() < 8 {
            anyhow::bail!("shared_secret must be at least 8 characters");
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
}
