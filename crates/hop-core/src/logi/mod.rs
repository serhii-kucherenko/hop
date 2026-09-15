use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use hop_protocol::control::{HandoffTransport, NodeRole};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{Config, HandoffMode};
use crate::layout::ScreenBounds;

mod hidpp;

const IPC_CONNECT_TIMEOUT: Duration = Duration::from_millis(180);
const IPC_IO_TIMEOUT: Duration = Duration::from_millis(180);
const MAX_AGENT_PACKET_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESPONSE_SCAN: usize = 32;
const LOGI_FLOW_TCP_PORT: u16 = 59869;
const REQUIRED_SUBSCRIPTIONS: [&str; 4] = [
    "/devices/state/changed",
    "/battery/state/changed",
    "/devices/options/device_arrival",
    "/devices/options/device_removal",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogiDeviceKind {
    Keyboard,
    Mouse,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogiHostSlot {
    pub index: u8,
    pub paired: bool,
    pub connected: bool,
    pub os: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogiDevice {
    pub id: String,
    pub name: String,
    pub kind: LogiDeviceKind,
    pub hosts: Vec<LogiHostSlot>,
    pub current_host: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct LogiHandoff {
    requested_mode: HandoffMode,
    local_machine_name: String,
    endpoint: Option<AgentEndpoint>,
    devices: Vec<LogiDevice>,
    hidpp_targets: Vec<hidpp::HidppTarget>,
    peer_host_index: HashMap<String, u8>,
    peer_device_host_index: HashMap<String, HashMap<String, u8>>,
    local_host_index: Option<u8>,
    local_device_host_index: HashMap<String, u8>,
    status_note: String,
    options_status: String,
    hidpp_status: String,
}

#[derive(Debug, Clone)]
pub struct LogiSummary {
    pub os: String,
    pub machine_name: String,
    pub role: String,
    pub screen_width: u32,
    pub screen_height: u32,
    pub requested_mode: HandoffMode,
    pub selected_transport: String,
    pub options_agent_endpoint: Option<String>,
    pub options_status: String,
    pub hidpp_status: String,
    pub hidpp_device_count: usize,
    pub local_host_index: Option<u8>,
    pub local_device_host_index: BTreeMap<String, u8>,
    pub peer_host_index: BTreeMap<String, u8>,
    pub peer_device_host_index: BTreeMap<String, BTreeMap<String, u8>>,
    pub devices: Vec<LogiDevice>,
    pub status_note: String,
}

impl LogiHandoff {
    pub fn from_config(config: &Config) -> Self {
        let local_machine_name = config.local.machine_name.clone();
        let requested_mode = config.handoff.mode;
        let peer_names = config
            .peers
            .iter()
            .map(|peer| peer.machine_name.clone())
            .collect::<Vec<_>>();
        let explicit_map = config.handoff.logi_peer_host_index.clone();
        let peer_device_host_index = config.handoff.logi_peer_device_host_index.clone();
        let local_device_host_index = config.handoff.logi_local_device_host_index.clone();

        // HID++ ChangeHost is the primary Easy-Switch path; Options+ IPC is best-effort.
        let hidpp = hidpp::discover_change_host_devices();
        let options = detect_options_agent();

        let mut devices = hidpp.as_logi_devices();
        if devices.is_empty() {
            devices = options.devices.clone();
        } else if !options.devices.is_empty() {
            // Prefer Options+ host names/OS hints when available for mapping.
            devices = merge_devices_preferring_named(options.devices.clone(), devices);
        }

        let (peer_host_index, local_host_index, mapping_note) =
            build_peer_host_map(&local_machine_name, &peer_names, &explicit_map, &devices);

        let options_status = options.status_note.clone().unwrap_or_else(|| {
            if options.endpoint.is_some() {
                "options+ ipc ready".to_owned()
            } else {
                "options+ ipc unavailable".to_owned()
            }
        });
        let hidpp_status = hidpp.status_note.clone();

        let mut notes = Vec::new();
        notes.push(hidpp_status.clone());
        notes.push(options_status.clone());
        if let Some(extra) = mapping_note {
            notes.push(extra);
        }
        if hidpp.is_ready()
            && peer_host_index.is_empty()
            && peer_device_host_index.is_empty()
            && !peer_names.is_empty()
        {
            notes.push(
                "set handoff.logi_peer_host_index or logi_peer_device_host_index after doctor lists channels (HID++ has no host names)"
                    .to_owned(),
            );
        }
        let status_note = notes.join("; ");

        Self {
            requested_mode,
            local_machine_name,
            endpoint: options.endpoint,
            devices,
            hidpp_targets: hidpp.targets,
            peer_host_index,
            peer_device_host_index,
            local_host_index,
            local_device_host_index,
            status_note,
            options_status,
            hidpp_status,
        }
    }

    pub fn preferred_transport_for_peer(&self, peer_machine: &str) -> HandoffTransport {
        match self.requested_mode {
            HandoffMode::Network => HandoffTransport::Network,
            HandoffMode::Auto | HandoffMode::Logi => {
                if self.can_switch_to_peer(peer_machine) {
                    HandoffTransport::Logi
                } else {
                    HandoffTransport::Network
                }
            }
        }
    }

    pub fn can_accept_logi_from(&self, owner_machine: &str) -> bool {
        // Accept Logi ownership when we know the owner's channel map.
        // The *owner* performs ChangeHost on enter; the client may have no local HID++
        // (common on macOS BLE) and still must ACK Logi so Easy-Switch can run on the server.
        self.peer_host_map_for(owner_machine).is_some()
    }

    pub fn can_switch_to_peer(&self, peer_machine: &str) -> bool {
        let Some(host_by_kind) = self.peer_host_map_for(peer_machine) else {
            return false;
        };
        self.can_switch_with_map(&host_by_kind)
    }

    fn has_any_peer_mapping(&self) -> bool {
        !self.peer_host_index.is_empty() || !self.peer_device_host_index.is_empty()
    }

    fn device_kind_key(kind: LogiDeviceKind) -> Option<&'static str> {
        match kind {
            LogiDeviceKind::Keyboard => Some("keyboard"),
            LogiDeviceKind::Mouse => Some("mouse"),
            LogiDeviceKind::Other => None,
        }
    }

    fn peer_host_map_for(&self, peer_machine: &str) -> Option<HashMap<LogiDeviceKind, u8>> {
        resolve_device_host_map(
            self.peer_device_host_index.get(peer_machine),
            self.peer_host_index.get(peer_machine).copied(),
        )
    }

    fn local_host_map(&self) -> Option<HashMap<LogiDeviceKind, u8>> {
        resolve_device_host_map(
            Some(&self.local_device_host_index).filter(|map| !map.is_empty()),
            self.local_host_index,
        )
    }

    fn can_switch_with_map(&self, host_by_kind: &HashMap<LogiDeviceKind, u8>) -> bool {
        if host_by_kind.is_empty() {
            return false;
        }
        // Every mapped kind (keyboard AND mouse on this desk) must be switchable.
        // Previously we only checked discovered targets, so keyboard-only HID++
        // still selected Logi and left the mouse behind on Windows.
        let hidpp_covers = !self.hidpp_targets.is_empty()
            && host_by_kind.keys().all(|kind| {
                self.hidpp_targets.iter().any(|target| {
                    target.kind == *kind
                        && host_by_kind
                            .get(kind)
                            .map(|index| *index < target.host_count)
                            .unwrap_or(false)
                })
            });
        let options_covers = self.endpoint.is_some()
            && !self.devices.is_empty()
            && host_by_kind.keys().all(|kind| {
                self.devices.iter().any(|device| {
                    device.kind == *kind
                        && host_by_kind
                            .get(kind)
                            .map(|index| device.hosts.iter().any(|host| host.index == *index))
                            .unwrap_or(false)
                })
            });
        hidpp_covers || options_covers
    }

    fn logi_path_ready(&self) -> bool {
        !self.hidpp_targets.is_empty() || (self.endpoint.is_some() && !self.devices.is_empty())
    }

    pub fn switch_to_peer(&self, peer_machine: &str) -> Result<()> {
        let Some(host_by_kind) = self.peer_host_map_for(peer_machine) else {
            anyhow::bail!("no host mapping for peer {peer_machine}");
        };
        self.switch_with_map(&host_by_kind)
            .with_context(|| format!("failed to switch host for peer {peer_machine}"))
    }

    pub fn switch_back_local(&self) -> Result<()> {
        let Some(host_by_kind) = self.local_host_map() else {
            anyhow::bail!("local host slot was not detected");
        };
        self.switch_with_map(&host_by_kind)
            .context("failed to switch host back to local machine")
    }

    pub fn summary(&self, role: NodeRole, screen: ScreenBounds) -> LogiSummary {
        let selected_transport = match self.requested_mode {
            HandoffMode::Network => "network".to_owned(),
            HandoffMode::Auto => {
                let logi_peers_ready = self
                    .peer_device_host_index
                    .keys()
                    .chain(self.peer_host_index.keys())
                    .any(|peer| self.can_switch_to_peer(peer));
                if logi_peers_ready {
                    if !self.hidpp_targets.is_empty() {
                        "auto(hidpp-first)".to_owned()
                    } else {
                        "auto(logi-options+)".to_owned()
                    }
                } else {
                    "auto(network-fallback)".to_owned()
                }
            }
            HandoffMode::Logi => {
                if self.logi_path_ready() && self.has_any_peer_mapping() {
                    if !self.hidpp_targets.is_empty() {
                        "logi(hidpp)".to_owned()
                    } else {
                        "logi(options+)".to_owned()
                    }
                } else {
                    "logi(requested)-network(fallback)".to_owned()
                }
            }
        };

        LogiSummary {
            os: std::env::consts::OS.to_owned(),
            machine_name: self.local_machine_name.clone(),
            role: role_label(role).to_owned(),
            screen_width: screen.width,
            screen_height: screen.height,
            requested_mode: self.requested_mode,
            selected_transport,
            options_agent_endpoint: self.endpoint.as_ref().map(ToString::to_string),
            options_status: self.options_status.clone(),
            hidpp_status: self.hidpp_status.clone(),
            hidpp_device_count: self.hidpp_targets.len(),
            local_host_index: self.local_host_index,
            local_device_host_index: self
                .local_device_host_index
                .iter()
                .map(|(kind, index)| (kind.clone(), *index))
                .collect(),
            peer_host_index: self
                .peer_host_index
                .iter()
                .map(|(peer, index)| (peer.clone(), *index))
                .collect(),
            peer_device_host_index: self
                .peer_device_host_index
                .iter()
                .map(|(peer, devices)| {
                    (
                        peer.clone(),
                        devices
                            .iter()
                            .map(|(kind, index)| (kind.clone(), *index))
                            .collect(),
                    )
                })
                .collect(),
            devices: self.devices.clone(),
            status_note: self.status_note.clone(),
        }
    }

    pub fn summary_lines(&self, role: NodeRole, screen: ScreenBounds) -> Vec<String> {
        let summary = self.summary(role, screen);
        let mut lines = Vec::new();
        lines.push(format!(
            "machine: {} (os={}, role={}, screen={}x{})",
            summary.machine_name,
            summary.os,
            summary.role,
            summary.screen_width,
            summary.screen_height
        ));
        lines.push(format!(
            "handoff mode: {:?} -> {}",
            summary.requested_mode, summary.selected_transport
        ));
        lines.push(format!(
            "hid++ ChangeHost: {} ({})",
            if summary.hidpp_device_count > 0 {
                format!("{} device(s)", summary.hidpp_device_count)
            } else {
                "none".to_owned()
            },
            summary.hidpp_status
        ));
        match summary.options_agent_endpoint {
            Some(endpoint) => lines.push(format!(
                "logi options+ agent: detected ({endpoint}) [{}]",
                summary.options_status
            )),
            None => lines.push(format!(
                "logi options+ agent: unavailable [{}]",
                summary.options_status
            )),
        }
        if summary.devices.is_empty() {
            lines.push("easy-switch devices: none".to_owned());
        } else {
            let mut rendered = Vec::new();
            for device in summary.devices {
                rendered.push(format!(
                    "{} [{}] hosts={} current={}",
                    device.name,
                    kind_label(device.kind),
                    device.hosts.len(),
                    device
                        .current_host
                        .map(|host| (host + 1).to_string())
                        .unwrap_or_else(|| "?".to_owned()),
                ));
            }
            lines.push(format!("easy-switch devices: {}", rendered.join("; ")));
        }
        if summary.peer_host_index.is_empty() {
            lines.push("peer->channel map: none".to_owned());
        } else {
            let pairs = summary
                .peer_host_index
                .iter()
                .map(|(peer, host)| format!("{peer}:{}", host + 1))
                .collect::<Vec<_>>();
            lines.push(format!("peer->channel map: {}", pairs.join(", ")));
        }
        if summary.peer_device_host_index.is_empty() {
            lines.push("peer->device channel map: none".to_owned());
        } else {
            let peers = summary
                .peer_device_host_index
                .iter()
                .map(|(peer, devices)| {
                    let pairs = devices
                        .iter()
                        .map(|(kind, host)| format!("{kind}:{}", host + 1))
                        .collect::<Vec<_>>();
                    format!("{peer} {{{}}}", pairs.join(", "))
                })
                .collect::<Vec<_>>();
            lines.push(format!("peer->device channel map: {}", peers.join("; ")));
        }
        if summary.local_device_host_index.is_empty() {
            if let Some(local) = summary.local_host_index {
                lines.push(format!("local channel: {}", local + 1));
            } else {
                lines.push("local channel: unknown".to_owned());
            }
        } else {
            let pairs = summary
                .local_device_host_index
                .iter()
                .map(|(kind, host)| format!("{kind}:{}", host + 1))
                .collect::<Vec<_>>();
            lines.push(format!("local device channel map: {}", pairs.join(", ")));
        }
        lines.push(format!("logi status: {}", summary.status_note));
        lines
    }

    fn switch_with_map(&self, host_by_kind: &HashMap<LogiDeviceKind, u8>) -> Result<()> {
        if host_by_kind.is_empty() {
            anyhow::bail!("empty device host map");
        }
        if !self.hidpp_targets.is_empty() {
            let channels = host_by_kind
                .iter()
                .filter_map(|(kind, index)| {
                    Self::device_kind_key(*kind).map(|key| format!("{key}:{}", index + 1))
                })
                .collect::<Vec<_>>();
            return hidpp::switch_targets_with_map(&self.hidpp_targets, host_by_kind)
                .with_context(|| format!("hid++ switch to channels {}", channels.join(", ")));
        }

        let Some(endpoint) = self.endpoint.as_ref() else {
            anyhow::bail!("no hid++ ChangeHost devices and options+ agent is unavailable");
        };

        let mut client = OptionsAgentClient::connect(endpoint)?;
        client.bootstrap_subscriptions();
        let mut devices = self.devices.clone();
        devices.sort_by_key(|device| match device.kind {
            LogiDeviceKind::Mouse => 0,
            LogiDeviceKind::Keyboard => 1,
            LogiDeviceKind::Other => 2,
        });
        if devices.is_empty() {
            anyhow::bail!("no easy-switch devices were detected");
        }

        for (index, device) in devices.iter().enumerate() {
            let Some(&host_index) = host_by_kind.get(&device.kind) else {
                anyhow::bail!(
                    "no host mapping for device kind {}",
                    kind_label(device.kind)
                );
            };
            if !device.hosts.iter().any(|slot| slot.index == host_index) {
                anyhow::bail!(
                    "device {} does not have host slot {}",
                    device.name,
                    host_index + 1
                );
            }

            let path = format!("/change_host/{}/host", device.id);
            let payload = json!({
                "@type": "type.googleapis.com/logi.protocol.devices.ChangeHost",
                "host": host_index,
            });
            let await_reply = index + 1 < devices.len();
            client
                .request("SET", &path, payload, await_reply)
                .with_context(|| {
                    format!(
                        "failed to switch {} to channel {}",
                        device.name,
                        host_index + 1
                    )
                })?;
        }
        Ok(())
    }
}

fn merge_devices_preferring_named(
    named: Vec<LogiDevice>,
    hidpp_devices: Vec<LogiDevice>,
) -> Vec<LogiDevice> {
    if !named.is_empty() {
        named
    } else {
        hidpp_devices
    }
}

fn role_label(role: NodeRole) -> &'static str {
    match role {
        NodeRole::Server => "primary/server",
        NodeRole::Client => "secondary/client",
    }
}

fn kind_label(kind: LogiDeviceKind) -> &'static str {
    match kind {
        LogiDeviceKind::Keyboard => "keyboard",
        LogiDeviceKind::Mouse => "mouse",
        LogiDeviceKind::Other => "other",
    }
}

fn resolve_device_host_map(
    device_map: Option<&HashMap<String, u8>>,
    uniform: Option<u8>,
) -> Option<HashMap<LogiDeviceKind, u8>> {
    let mut out = HashMap::new();
    if let Some(device_map) = device_map {
        for (kind, key) in [
            (LogiDeviceKind::Keyboard, "keyboard"),
            (LogiDeviceKind::Mouse, "mouse"),
        ] {
            if let Some(&index) = device_map.get(key) {
                out.insert(kind, index);
            }
        }
    }
    if out.is_empty() {
        let index = uniform?;
        out.insert(LogiDeviceKind::Keyboard, index);
        out.insert(LogiDeviceKind::Mouse, index);
    } else if let Some(index) = uniform {
        out.entry(LogiDeviceKind::Keyboard).or_insert(index);
        out.entry(LogiDeviceKind::Mouse).or_insert(index);
    }
    Some(out)
}

fn build_peer_host_map(
    local_machine_name: &str,
    peers: &[String],
    explicit_map: &HashMap<String, u8>,
    devices: &[LogiDevice],
) -> (HashMap<String, u8>, Option<u8>, Option<String>) {
    let Some(reference_device) = pick_reference_device(devices) else {
        if peers.is_empty() {
            return (
                HashMap::new(),
                None,
                Some("no peer mapping needed".to_owned()),
            );
        }
        return (
            HashMap::new(),
            None,
            Some("no easy-switch hosts discovered for mapping".to_owned()),
        );
    };

    let local_host_index =
        match_host_by_name(&reference_device.hosts, local_machine_name).or_else(|| {
            reference_device
                .hosts
                .iter()
                .find(|host| host.connected)
                .map(|host| host.index)
        });

    let mut map = HashMap::new();
    let mut used_indexes = HashSet::new();
    if let Some(local) = local_host_index {
        used_indexes.insert(local);
    }

    let mut notes = Vec::new();
    for peer in peers {
        if let Some(explicit) = explicit_map.get(peer).copied() {
            map.insert(peer.clone(), explicit);
            used_indexes.insert(explicit);
            continue;
        }

        let Some(candidate) = best_host_for_machine(
            &reference_device.hosts,
            peer,
            &used_indexes,
            local_host_index,
        ) else {
            notes.push(format!("could not auto-map peer {peer}"));
            continue;
        };
        map.insert(peer.clone(), candidate);
        used_indexes.insert(candidate);
    }

    let note = if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    };
    (map, local_host_index, note)
}

fn pick_reference_device(devices: &[LogiDevice]) -> Option<&LogiDevice> {
    devices.iter().max_by_key(|device| {
        let kind_rank = match device.kind {
            LogiDeviceKind::Keyboard => 3_u8,
            LogiDeviceKind::Mouse => 2_u8,
            LogiDeviceKind::Other => 1_u8,
        };
        (kind_rank, device.hosts.len())
    })
}

fn best_host_for_machine(
    hosts: &[LogiHostSlot],
    machine_name: &str,
    used_indexes: &HashSet<u8>,
    local_host: Option<u8>,
) -> Option<u8> {
    let mut best: Option<(u8, i32)> = None;
    for host in hosts {
        if !host.paired {
            continue;
        }
        if used_indexes.contains(&host.index) {
            continue;
        }
        // Never auto-map a peer onto the currently connected local host slot.
        if Some(host.index) == local_host {
            continue;
        }
        let mut score = score_host_name_match(host, machine_name);
        if host.connected {
            // Connected-but-not-local is rare; still prefer named matches above it.
            score += 5;
        }
        match best {
            Some((_, best_score)) if best_score >= score => {}
            _ => best = Some((host.index, score)),
        }
    }
    best.map(|(index, _)| index)
}

fn score_host_name_match(host: &LogiHostSlot, machine_name: &str) -> i32 {
    let Some(host_name) = host.name.as_deref() else {
        return score_os_hint(host, machine_name);
    };
    let normalized_machine = normalize_name(machine_name);
    let normalized_host = normalize_name(host_name);
    if normalized_machine.is_empty() || normalized_host.is_empty() {
        return score_os_hint(host, machine_name);
    }

    let mut score = score_os_hint(host, machine_name);
    if normalized_machine == normalized_host {
        score += 240;
    } else if normalized_host.contains(&normalized_machine)
        || normalized_machine.contains(&normalized_host)
    {
        score += 180;
    } else {
        let machine_tokens = tokenize(&normalized_machine);
        let host_tokens = tokenize(&normalized_host);
        for token in machine_tokens {
            if token.len() >= 3 && host_tokens.contains(&token) {
                score += 50;
            }
        }
    }
    score
}

fn score_os_hint(host: &LogiHostSlot, machine_name: &str) -> i32 {
    let machine_hint = infer_os_hint(machine_name);
    let host_hint = host
        .os
        .as_deref()
        .map(infer_os_hint)
        .unwrap_or(OsHint::Unknown);
    if machine_hint != OsHint::Unknown && machine_hint == host_hint {
        40
    } else {
        0
    }
}

fn match_host_by_name(hosts: &[LogiHostSlot], machine_name: &str) -> Option<u8> {
    let mut best: Option<(u8, i32)> = None;
    for host in hosts {
        let score = score_host_name_match(host, machine_name);
        match best {
            Some((_, best_score)) if best_score >= score => {}
            _ => best = Some((host.index, score)),
        }
    }
    best.map(|(index, _)| index)
}

fn normalize_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn tokenize(value: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut current = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch);
            continue;
        }
        if !current.is_empty() {
            out.insert(current.clone());
            current.clear();
        }
    }
    if !current.is_empty() {
        out.insert(current);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OsHint {
    Windows,
    Macos,
    Linux,
    Unknown,
}

fn infer_os_hint(value: &str) -> OsHint {
    let normalized = value.to_ascii_lowercase();
    if normalized.contains("mac") || normalized.contains("osx") {
        return OsHint::Macos;
    }
    if normalized.contains("win")
        || normalized.contains("nuc")
        || normalized.contains("pc")
        || normalized.contains("desktop")
    {
        return OsHint::Windows;
    }
    if normalized.contains("linux") || normalized.contains("ubuntu") {
        return OsHint::Linux;
    }
    OsHint::Unknown
}

#[derive(Debug, Clone)]
struct AgentDiscovery {
    endpoint: Option<AgentEndpoint>,
    devices: Vec<LogiDevice>,
    status_note: Option<String>,
}

fn detect_options_agent() -> AgentDiscovery {
    let endpoints = candidate_endpoints();
    if endpoints.is_empty() {
        return AgentDiscovery {
            endpoint: None,
            devices: Vec::new(),
            status_note: Some("no options+ endpoints discovered".to_owned()),
        };
    }

    let mut connected_without_devices: Option<AgentEndpoint> = None;
    let mut last_error: Option<String> = None;
    for endpoint in endpoints {
        match OptionsAgentClient::connect(&endpoint) {
            Ok(mut client) => {
                if connected_without_devices.is_none() {
                    connected_without_devices = Some(endpoint.clone());
                }
                client.bootstrap_subscriptions();
                match fetch_easy_switch_devices(&mut client) {
                    Ok(devices) if !devices.is_empty() => {
                        return AgentDiscovery {
                            endpoint: Some(endpoint),
                            devices,
                            status_note: None,
                        };
                    }
                    Ok(_) => {
                        last_error =
                            Some("options+ detected but no easy-switch devices found".to_owned());
                    }
                    Err(error) => {
                        last_error = Some(format!(
                            "options+ device scan failed on {endpoint}: {error}"
                        ));
                    }
                }
            }
            Err(error) => {
                last_error = Some(format!("{endpoint}: {error}"));
            }
        }
    }

    // Connect without a working device list is not enough — Options+ IPC is often gated.
    let _ = connected_without_devices;
    AgentDiscovery {
        endpoint: None,
        devices: Vec::new(),
        status_note: Some(last_error.unwrap_or_else(|| {
            "options+ ipc unavailable/gated (handshake EOF/timeout or no easy-switch devices)"
                .to_owned()
        })),
    }
}

fn fetch_easy_switch_devices(client: &mut OptionsAgentClient) -> Result<Vec<LogiDevice>> {
    let list = client
        .request("GET", "/devices/list", json!({}), true)
        .or_else(|_| client.request("GET", "/devices/", json!({}), true))?
        .unwrap_or(Value::Null);
    let payload = response_payload(&list);
    let entries = payload
        .get("deviceInfos")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| payload.get("devices").and_then(Value::as_array).cloned())
        .unwrap_or_default();

    let mut discovered = Vec::new();
    for entry in entries {
        let Some(device_id) = read_string_field(&entry, &["id", "deviceId"]) else {
            continue;
        };
        let device_name = read_string_field(
            &entry,
            &[
                "name",
                "model",
                "displayName",
                "friendlyName",
                "productName",
            ],
        )
        .unwrap_or_else(|| device_id.clone());
        let device_kind = infer_device_kind(&entry, &device_name);
        if matches!(device_kind, LogiDeviceKind::Other) {
            continue;
        }

        let easy_switch_path = format!("/devices/{device_id}/easy_switch");
        let easy_switch = match client.request("GET", &easy_switch_path, json!({}), true) {
            Ok(Some(value)) => value,
            Ok(None) => continue,
            Err(_) => continue,
        };
        let easy_payload = response_payload(&easy_switch);
        let hosts = parse_easy_switch_hosts(easy_payload);
        if hosts.len() < 2 {
            continue;
        }
        let current_host = hosts
            .iter()
            .find(|host| host.connected)
            .map(|host| host.index)
            .or_else(|| {
                easy_payload
                    .get("currentHost")
                    .and_then(Value::as_u64)
                    .and_then(|host| u8::try_from(host).ok())
            });
        discovered.push(LogiDevice {
            id: device_id,
            name: device_name,
            kind: device_kind,
            hosts,
            current_host,
        });
    }
    Ok(discovered)
}

fn parse_easy_switch_hosts(payload: &Value) -> Vec<LogiHostSlot> {
    let hosts = payload
        .get("hosts")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| payload.get("hostInfos").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let mut parsed = Vec::new();
    for host in hosts {
        let index = host
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|idx| u8::try_from(idx).ok())
            .or_else(|| {
                host.get("index")
                    .and_then(Value::as_str)
                    .and_then(|idx| idx.parse::<u8>().ok())
            });
        let Some(index) = index else {
            continue;
        };
        parsed.push(LogiHostSlot {
            index,
            paired: host.get("paired").and_then(Value::as_bool).unwrap_or(true),
            connected: host
                .get("connected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            os: host.get("os").and_then(Value::as_str).map(str::to_owned),
            name: host.get("name").and_then(Value::as_str).map(str::to_owned),
        });
    }
    parsed.sort_by_key(|host| host.index);
    parsed
}

fn infer_device_kind(entry: &Value, fallback_name: &str) -> LogiDeviceKind {
    let type_hint = read_string_field(
        entry,
        &["type", "deviceType", "kind", "connectionType", "category"],
    )
    .unwrap_or_default()
    .to_ascii_lowercase();
    if type_hint.contains("keyboard") {
        return LogiDeviceKind::Keyboard;
    }
    if type_hint.contains("mouse") {
        return LogiDeviceKind::Mouse;
    }

    let lowered_name = fallback_name.to_ascii_lowercase();
    if lowered_name.contains("keys") || lowered_name.contains("keyboard") {
        return LogiDeviceKind::Keyboard;
    }
    if lowered_name.contains("mouse")
        || lowered_name.contains("mx ")
        || lowered_name.contains("mx-")
        || lowered_name.contains("mchncl")
    {
        return LogiDeviceKind::Mouse;
    }
    LogiDeviceKind::Other
}

fn read_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(found) = value.get(*key).and_then(Value::as_str) {
            let trimmed = found.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

fn response_payload(response: &Value) -> &Value {
    response.get("payload").unwrap_or(response)
}

#[derive(Debug, Clone)]
enum AgentEndpoint {
    #[cfg(unix)]
    UnixSocket(PathBuf),
    Tcp(std::net::SocketAddr),
    #[cfg(windows)]
    NamedPipe(String),
}

impl fmt::Display for AgentEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(unix)]
            AgentEndpoint::UnixSocket(path) => write!(f, "unix:{}", path.display()),
            AgentEndpoint::Tcp(addr) => write!(f, "tcp:{addr}"),
            #[cfg(windows)]
            AgentEndpoint::NamedPipe(path) => write!(f, "pipe:{path}"),
        }
    }
}

fn candidate_endpoints() -> Vec<AgentEndpoint> {
    let mut endpoints = Vec::new();
    #[cfg(unix)]
    {
        let mut unix_paths = discover_unix_kiros_sockets();
        unix_paths.sort();
        // Prefer live sockets that accept connect before TCP fallback.
        for path in unix_paths {
            endpoints.push(AgentEndpoint::UnixSocket(path));
        }
    }
    #[cfg(windows)]
    {
        for pipe_name in discover_windows_kiros_pipes() {
            endpoints.push(AgentEndpoint::NamedPipe(pipe_name));
        }
    }
    // TCP last: connect success without handshake is not treated as ready.
    if let Ok(addr) = format!("127.0.0.1:{LOGI_FLOW_TCP_PORT}").parse() {
        endpoints.push(AgentEndpoint::Tcp(addr));
    }
    dedupe_endpoints(endpoints)
}

pub fn is_kiros_agent_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("logitech_kiros_agent")
}

pub fn filter_kiros_pipe_candidates(names: &[String]) -> Vec<String> {
    let mut out = names
        .iter()
        .filter(|name| is_kiros_agent_name(name))
        .cloned()
        .collect::<Vec<_>>();
    out.sort();
    out.dedup();
    out
}

#[cfg(unix)]
fn discover_unix_kiros_sockets() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(entries) = fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_kiros_agent_name(&name) {
                let path = entry.path();
                if unix_socket_accepts_connect(&path) {
                    paths.insert(0, path);
                } else {
                    paths.push(path);
                }
            }
        }
    }
    paths.extend(discover_unix_sockets_via_process_scan());
    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        if seen.insert(path.clone()) {
            deduped.push(path);
        }
    }
    deduped
}

#[cfg(unix)]
fn unix_socket_accepts_connect(path: &std::path::Path) -> bool {
    match UnixStream::connect(path) {
        Ok(stream) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            true
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn discover_unix_sockets_via_process_scan() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Ok(output) = std::process::Command::new("sh")
        .arg("-c")
        .arg(
            "pgrep -f 'logi|cp-dev-mgr|kiros' 2>/dev/null | head -5 | while read pid; do lsof -p \"$pid\" -a -U 2>/dev/null; done | awk '{print $NF}' | grep -i logitech_kiros_agent | head -20",
        )
        .output()
    else {
        return paths;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('/') && is_kiros_agent_name(trimmed) {
            paths.push(PathBuf::from(trimmed));
        }
    }
    paths
}

#[cfg(windows)]
fn discover_windows_kiros_pipes() -> Vec<String> {
    let mut names = Vec::new();
    names.push(r"\\.\pipe\logitech_kiros_agent".to_owned());
    names.push(r"\\.\pipe\logitech_kiros_agent-main".to_owned());
    if let Ok(entries) = fs::read_dir(r"\\.\pipe\") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_kiros_agent_name(&name) {
                names.push(format!(r"\\.\pipe\{}", name));
            }
        }
    }
    filter_kiros_pipe_candidates(&names)
}

fn dedupe_endpoints(input: Vec<AgentEndpoint>) -> Vec<AgentEndpoint> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for endpoint in input {
        let key = endpoint.to_string();
        if seen.insert(key) {
            out.push(endpoint);
        }
    }
    out
}

struct OptionsAgentClient {
    stream: AgentStream,
    next_msg_id: u64,
}

impl OptionsAgentClient {
    fn connect(endpoint: &AgentEndpoint) -> Result<Self> {
        let mut stream = AgentStream::connect(endpoint)?;
        // Server-first handshake with short timeout; on EOF/timeout try client-first once.
        match read_packet(&mut stream) {
            Ok(_) => {
                write_packet(&mut stream, &[b"json"])
                    .context("failed to send options+ json announce")?;
            }
            Err(_) => {
                write_packet(&mut stream, &[b"json"]).context(
                    "options+ handshake timed out/EOF; client-first json announce also failed",
                )?;
                // Do not wait long for a reply from dead endpoints.
                let _ = read_packet(&mut stream);
            }
        }
        Ok(Self {
            stream,
            next_msg_id: 1,
        })
    }

    fn bootstrap_subscriptions(&mut self) {
        for path in REQUIRED_SUBSCRIPTIONS {
            let _ = self.request("SUBSCRIBE", path, json!({}), true);
        }
    }

    fn request(
        &mut self,
        verb: &str,
        path: &str,
        payload: Value,
        await_response: bool,
    ) -> Result<Option<Value>> {
        let msg_id = self.next_msg_id.to_string();
        self.next_msg_id = self.next_msg_id.saturating_add(1);
        let message = json!({
            "msg_id": msg_id,
            "verb": verb,
            "path": path,
            "payload": payload,
        });
        let raw = serde_json::to_vec(&message).context("failed to serialize options+ request")?;
        write_packet(&mut self.stream, &[b"json", &raw])?;
        if !await_response {
            return Ok(None);
        }

        for _ in 0..MAX_RESPONSE_SCAN {
            let frames = read_packet(&mut self.stream)?;
            let Some(last_frame) = frames.last() else {
                continue;
            };
            let response = match serde_json::from_slice::<Value>(last_frame) {
                Ok(response) => response,
                Err(_) => continue,
            };
            let response_id = response
                .get("msgId")
                .and_then(Value::as_str)
                .or_else(|| response.get("msg_id").and_then(Value::as_str));
            if response_id != Some(msg_id.as_str()) {
                continue;
            }
            if let Some(code) = response
                .get("result")
                .and_then(|value| value.get("code"))
                .and_then(Value::as_str)
            {
                if code != "SUCCESS" {
                    let what = response
                        .get("result")
                        .and_then(|value| value.get("what"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    anyhow::bail!("options+ {verb} {path} failed with {code}: {what}");
                }
            }
            return Ok(Some(response));
        }
        anyhow::bail!("options+ response timeout for {verb} {path}");
    }
}

enum AgentStream {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp(std::net::TcpStream),
    #[cfg(windows)]
    Pipe(std::fs::File),
}

impl AgentStream {
    fn connect(endpoint: &AgentEndpoint) -> Result<Self> {
        match endpoint {
            #[cfg(unix)]
            AgentEndpoint::UnixSocket(path) => {
                let stream = UnixStream::connect(path).with_context(|| {
                    format!("failed to connect options+ unix socket {}", path.display())
                })?;
                stream
                    .set_read_timeout(Some(IPC_IO_TIMEOUT))
                    .context("failed to set unix socket read timeout")?;
                stream
                    .set_write_timeout(Some(IPC_IO_TIMEOUT))
                    .context("failed to set unix socket write timeout")?;
                Ok(Self::Unix(stream))
            }
            AgentEndpoint::Tcp(addr) => {
                let stream = std::net::TcpStream::connect_timeout(addr, IPC_CONNECT_TIMEOUT)
                    .with_context(|| format!("failed to connect options+ tcp endpoint {addr}"))?;
                stream
                    .set_read_timeout(Some(IPC_IO_TIMEOUT))
                    .context("failed to set tcp read timeout")?;
                stream
                    .set_write_timeout(Some(IPC_IO_TIMEOUT))
                    .context("failed to set tcp write timeout")?;
                Ok(Self::Tcp(stream))
            }
            #[cfg(windows)]
            AgentEndpoint::NamedPipe(path) => {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .with_context(|| format!("failed to open options+ named pipe {path}"))?;
                Ok(Self::Pipe(file))
            }
        }
    }
}

impl Read for AgentStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            AgentStream::Unix(stream) => stream.read(buf),
            AgentStream::Tcp(stream) => stream.read(buf),
            #[cfg(windows)]
            AgentStream::Pipe(stream) => stream.read(buf),
        }
    }
}

impl Write for AgentStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            AgentStream::Unix(stream) => stream.write(buf),
            AgentStream::Tcp(stream) => stream.write(buf),
            #[cfg(windows)]
            AgentStream::Pipe(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            AgentStream::Unix(stream) => stream.flush(),
            AgentStream::Tcp(stream) => stream.flush(),
            #[cfg(windows)]
            AgentStream::Pipe(stream) => stream.flush(),
        }
    }
}

fn write_packet<W: Write>(writer: &mut W, frames: &[&[u8]]) -> Result<()> {
    let payload = encode_frames(frames);
    writer
        .write_all(&payload)
        .context("failed to write options+ packet")?;
    writer.flush().context("failed to flush options+ packet")?;
    Ok(())
}

fn read_packet<R: Read>(reader: &mut R) -> Result<Vec<Vec<u8>>> {
    let mut size_bytes = [0_u8; 4];
    reader
        .read_exact(&mut size_bytes)
        .context("failed to read options+ packet size")?;
    let total_size = u32::from_le_bytes(size_bytes) as usize;
    if total_size == 0 || total_size > MAX_AGENT_PACKET_BYTES {
        anyhow::bail!("invalid options+ packet size: {total_size}");
    }
    let mut payload = vec![0_u8; total_size];
    reader
        .read_exact(&mut payload)
        .context("failed to read options+ packet payload")?;
    decode_frames(&payload)
}

fn encode_frames(frames: &[&[u8]]) -> Vec<u8> {
    let total_size = frames
        .iter()
        .map(|frame| 4_usize.saturating_add(frame.len()))
        .sum::<usize>();
    let mut out = Vec::with_capacity(total_size + 4);
    out.extend_from_slice(&(total_size as u32).to_le_bytes());
    for frame in frames {
        out.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        out.extend_from_slice(frame);
    }
    out
}

fn decode_frames(payload: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut cursor = 0_usize;
    let mut frames = Vec::new();
    while cursor < payload.len() {
        if payload.len().saturating_sub(cursor) < 4 {
            anyhow::bail!("truncated options+ frame header");
        }
        let len = u32::from_be_bytes([
            payload[cursor],
            payload[cursor + 1],
            payload[cursor + 2],
            payload[cursor + 3],
        ]) as usize;
        cursor += 4;
        if payload.len().saturating_sub(cursor) < len {
            anyhow::bail!("truncated options+ frame payload");
        }
        frames.push(payload[cursor..cursor + len].to_vec());
        cursor += len;
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_packet_codec_roundtrips() {
        let encoded = encode_frames(&[b"json", br#"{"msg_id":"7"}"#]);
        let mut cursor = std::io::Cursor::new(encoded);
        let decoded = read_packet(&mut cursor).expect("decode packet");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], b"json");
        assert_eq!(decoded[1], br#"{"msg_id":"7"}"#);
    }

    #[test]
    fn peer_mapping_prefers_hostname_matches() {
        let devices = vec![LogiDevice {
            id: "dev1".to_owned(),
            name: "Casa Keys".to_owned(),
            kind: LogiDeviceKind::Keyboard,
            hosts: vec![
                LogiHostSlot {
                    index: 0,
                    paired: true,
                    connected: true,
                    os: Some("WINDOWS".to_owned()),
                    name: Some("NUCBOX_M7PRO".to_owned()),
                },
                LogiHostSlot {
                    index: 1,
                    paired: true,
                    connected: false,
                    os: Some("MACOS".to_owned()),
                    name: Some("MacBook-Pro".to_owned()),
                },
                LogiHostSlot {
                    index: 2,
                    paired: false,
                    connected: false,
                    os: None,
                    name: None,
                },
            ],
            current_host: Some(0),
        }];

        let peers = vec!["MacBook-Pro".to_owned()];
        let explicit = HashMap::new();
        let (map, local, note) = build_peer_host_map("NUCBOX_M7PRO", &peers, &explicit, &devices);
        assert_eq!(local, Some(0));
        assert_eq!(map.get("MacBook-Pro"), Some(&1));
        assert!(note.is_none());
    }

    #[test]
    fn explicit_peer_mapping_overrides_auto_mapping() {
        let devices = vec![LogiDevice {
            id: "dev1".to_owned(),
            name: "MX Mouse".to_owned(),
            kind: LogiDeviceKind::Mouse,
            hosts: vec![
                LogiHostSlot {
                    index: 0,
                    paired: true,
                    connected: false,
                    os: Some("WINDOWS".to_owned()),
                    name: Some("WIN-DESK".to_owned()),
                },
                LogiHostSlot {
                    index: 1,
                    paired: true,
                    connected: true,
                    os: Some("MACOS".to_owned()),
                    name: Some("MAC-LAPTOP".to_owned()),
                },
            ],
            current_host: Some(1),
        }];

        let peers = vec!["WIN-DESK".to_owned()];
        let mut explicit = HashMap::new();
        explicit.insert("WIN-DESK".to_owned(), 0);
        let (map, local, _) = build_peer_host_map("MAC-LAPTOP", &peers, &explicit, &devices);
        assert_eq!(local, Some(1));
        assert_eq!(map.get("WIN-DESK"), Some(&0));
    }

    #[test]
    fn filters_hashed_kiros_pipe_names() {
        let names = vec![
            "chrome.sync".to_owned(),
            "logitech_kiros_agent".to_owned(),
            "logitech_kiros_agent-ab12cd".to_owned(),
            "other_pipe".to_owned(),
        ];
        let filtered = filter_kiros_pipe_candidates(&names);
        assert_eq!(
            filtered,
            vec![
                "logitech_kiros_agent".to_owned(),
                "logitech_kiros_agent-ab12cd".to_owned(),
            ]
        );
    }

    #[test]
    fn kiros_socket_name_filter() {
        assert!(is_kiros_agent_name("logitech_kiros_agent-1234"));
        assert!(is_kiros_agent_name(
            r"\\.\pipe\logitech_kiros_agent-deadbeef"
        ));
        assert!(!is_kiros_agent_name("uuid-socket-without-prefix"));
    }

    #[test]
    fn preferred_transport_uses_hidpp_without_options() {
        let handoff = LogiHandoff {
            requested_mode: HandoffMode::Auto,
            local_machine_name: "windows-desktop".to_owned(),
            endpoint: None,
            devices: vec![LogiDevice {
                id: "hidpp:test".to_owned(),
                name: "MX Mouse".to_owned(),
                kind: LogiDeviceKind::Mouse,
                hosts: vec![
                    LogiHostSlot {
                        index: 0,
                        paired: true,
                        connected: true,
                        os: None,
                        name: None,
                    },
                    LogiHostSlot {
                        index: 1,
                        paired: true,
                        connected: false,
                        os: None,
                        name: None,
                    },
                ],
                current_host: Some(0),
            }],
            hidpp_targets: vec![
                hidpp::HidppTarget {
                    path: b"/dev/hidraw0".to_vec(),
                    name: "MX Mouse".to_owned(),
                    kind: LogiDeviceKind::Mouse,
                    product_id: 0x1234,
                    device_index: 0xFF,
                    report_id: hidpp::REPORT_SHORT,
                    feature_index: 0x0D,
                    host_count: 2,
                    current_host: 0,
                },
                hidpp::HidppTarget {
                    path: b"/dev/hidraw1".to_vec(),
                    name: "Casa Keys".to_owned(),
                    kind: LogiDeviceKind::Keyboard,
                    product_id: 0x5678,
                    device_index: 0xFF,
                    report_id: hidpp::REPORT_SHORT,
                    feature_index: 0x0D,
                    host_count: 2,
                    current_host: 0,
                },
            ],
            peer_host_index: {
                let mut map = HashMap::new();
                map.insert("macbook-pro".to_owned(), 1);
                map
            },
            peer_device_host_index: HashMap::new(),
            local_host_index: Some(0),
            local_device_host_index: HashMap::new(),
            status_note: "hid++ ready".to_owned(),
            options_status: "options+ ipc unavailable".to_owned(),
            hidpp_status: "hid++ ready".to_owned(),
        };
        assert_eq!(
            handoff.preferred_transport_for_peer("macbook-pro"),
            HandoffTransport::Logi
        );
        assert!(!handoff.can_switch_to_peer("missing-peer"));
    }

    #[test]
    fn resolve_device_host_map_prefers_per_device_entries() {
        let mut device_map = HashMap::new();
        device_map.insert("keyboard".to_owned(), 1);
        device_map.insert("mouse".to_owned(), 2);
        let resolved = resolve_device_host_map(Some(&device_map), Some(0)).expect("map");
        assert_eq!(resolved.get(&LogiDeviceKind::Keyboard), Some(&1));
        assert_eq!(resolved.get(&LogiDeviceKind::Mouse), Some(&2));
    }

    #[test]
    fn resolve_device_host_map_falls_back_to_uniform() {
        let resolved = resolve_device_host_map(None, Some(3)).expect("map");
        assert_eq!(resolved.get(&LogiDeviceKind::Keyboard), Some(&3));
        assert_eq!(resolved.get(&LogiDeviceKind::Mouse), Some(&3));
    }

    #[test]
    fn resolve_device_host_map_fills_missing_kind_from_uniform() {
        let mut device_map = HashMap::new();
        device_map.insert("keyboard".to_owned(), 1);
        let resolved = resolve_device_host_map(Some(&device_map), Some(0)).expect("map");
        assert_eq!(resolved.get(&LogiDeviceKind::Keyboard), Some(&1));
        assert_eq!(resolved.get(&LogiDeviceKind::Mouse), Some(&0));
    }

    #[test]
    fn peer_device_map_enables_switch_without_uniform_peer_index() {
        let handoff = LogiHandoff {
            requested_mode: HandoffMode::Auto,
            local_machine_name: "windows-desktop".to_owned(),
            endpoint: None,
            devices: Vec::new(),
            hidpp_targets: vec![
                hidpp::HidppTarget {
                    path: b"/dev/hidraw0".to_vec(),
                    name: "Casa Keys".to_owned(),
                    kind: LogiDeviceKind::Keyboard,
                    product_id: 0x1111,
                    device_index: 0xFF,
                    report_id: hidpp::REPORT_SHORT,
                    feature_index: 0x0D,
                    host_count: 3,
                    current_host: 1,
                },
                hidpp::HidppTarget {
                    path: b"/dev/hidraw1".to_vec(),
                    name: "MX Mouse".to_owned(),
                    kind: LogiDeviceKind::Mouse,
                    product_id: 0x2222,
                    device_index: 0xFF,
                    report_id: hidpp::REPORT_SHORT,
                    feature_index: 0x0D,
                    host_count: 3,
                    current_host: 2,
                },
            ],
            peer_host_index: HashMap::new(),
            peer_device_host_index: {
                let mut peer = HashMap::new();
                let mut devices = HashMap::new();
                devices.insert("keyboard".to_owned(), 0);
                devices.insert("mouse".to_owned(), 0);
                peer.insert("hop-machine".to_owned(), devices);
                peer
            },
            local_host_index: None,
            local_device_host_index: {
                let mut map = HashMap::new();
                map.insert("keyboard".to_owned(), 1);
                map.insert("mouse".to_owned(), 2);
                map
            },
            status_note: "hid++ ready".to_owned(),
            options_status: "options+ ipc unavailable".to_owned(),
            hidpp_status: "hid++ ready".to_owned(),
        };
        assert!(handoff.can_switch_to_peer("hop-machine"));
        assert_eq!(
            handoff.preferred_transport_for_peer("hop-machine"),
            HandoffTransport::Logi
        );
        let local = handoff.local_host_map().expect("local map");
        assert_eq!(local.get(&LogiDeviceKind::Keyboard), Some(&1));
        assert_eq!(local.get(&LogiDeviceKind::Mouse), Some(&2));
    }

    #[test]
    fn best_host_never_auto_maps_to_connected_local_host() {
        let hosts = vec![
            LogiHostSlot {
                index: 0,
                paired: true,
                connected: true,
                os: Some("WINDOWS".to_owned()),
                name: Some("NUCBOX_M7PRO".to_owned()),
            },
            LogiHostSlot {
                index: 1,
                paired: true,
                connected: false,
                os: Some("MACOS".to_owned()),
                name: Some("hop-machine".to_owned()),
            },
        ];
        let used = HashSet::new();
        let chosen = best_host_for_machine(&hosts, "hop-machine", &used, Some(0));
        assert_eq!(chosen, Some(1));
        // Even with a weak name match, local connected slot must stay unused.
        let chosen_bad_name = best_host_for_machine(&hosts, "unknown-peer", &used, Some(0));
        assert_eq!(chosen_bad_name, Some(1));
    }

    #[test]
    fn mac_client_accepts_logi_when_maps_exist_without_local_hidpp() {
        // Desk reality: Windows owns HID++ ChangeHost; Mac BLE has maps but no 0x1814.
        let handoff = LogiHandoff {
            requested_mode: HandoffMode::Auto,
            local_machine_name: "hop-machine".to_owned(),
            endpoint: None,
            devices: Vec::new(),
            hidpp_targets: Vec::new(),
            peer_host_index: HashMap::new(),
            peer_device_host_index: {
                let mut peer = HashMap::new();
                let mut kinds = HashMap::new();
                kinds.insert("keyboard".to_owned(), 1);
                kinds.insert("mouse".to_owned(), 2);
                peer.insert("NUCBOX_M7PRO".to_owned(), kinds);
                peer
            },
            local_host_index: None,
            local_device_host_index: {
                let mut local = HashMap::new();
                local.insert("keyboard".to_owned(), 0);
                local.insert("mouse".to_owned(), 0);
                local
            },
            status_note: "network-fallback".to_owned(),
            options_status: "options+ unavailable".to_owned(),
            hidpp_status: "no hid++".to_owned(),
        };
        assert!(
            handoff.can_accept_logi_from("NUCBOX_M7PRO"),
            "Mac must ACK Logi handoff when peer maps exist even without local HID++"
        );
        assert!(
            !handoff.can_switch_to_peer("NUCBOX_M7PRO"),
            "Mac still cannot initiate Easy-Switch without HID++/Options+"
        );
        assert!(!handoff.can_accept_logi_from("unknown-owner"));
    }

    #[test]
    fn logi_transport_requires_mouse_and_keyboard_targets() {
        let handoff = LogiHandoff {
            requested_mode: HandoffMode::Auto,
            local_machine_name: "windows-desktop".to_owned(),
            endpoint: None,
            devices: Vec::new(),
            hidpp_targets: vec![hidpp::HidppTarget {
                path: b"/dev/hidraw0".to_vec(),
                name: "Casa Keys".to_owned(),
                kind: LogiDeviceKind::Keyboard,
                product_id: 0x1111,
                device_index: 0xFF,
                report_id: hidpp::REPORT_SHORT,
                feature_index: 0x0D,
                host_count: 3,
                current_host: 1,
            }],
            peer_host_index: HashMap::new(),
            peer_device_host_index: {
                let mut peer = HashMap::new();
                let mut kinds = HashMap::new();
                kinds.insert("keyboard".to_owned(), 0);
                kinds.insert("mouse".to_owned(), 0);
                peer.insert("hop-machine".to_owned(), kinds);
                peer
            },
            local_host_index: None,
            local_device_host_index: HashMap::new(),
            status_note: "kbd only".to_owned(),
            options_status: "n/a".to_owned(),
            hidpp_status: "kbd only".to_owned(),
        };
        assert!(
            !handoff.can_switch_to_peer("hop-machine"),
            "keyboard-only HID++ must not claim full Logi switch when mouse is mapped"
        );
        assert_eq!(
            handoff.preferred_transport_for_peer("hop-machine"),
            HandoffTransport::Network
        );
    }
}
