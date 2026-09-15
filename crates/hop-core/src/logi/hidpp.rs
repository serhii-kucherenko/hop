//! Direct HID++ 2.0 ChangeHost (feature 0x1814) path — mxswitch-style.
//!
//! Priority path for Easy-Switch when Options+ IPC is unavailable.
//! Windows is first-class; macOS is best-effort (may need Input Monitoring).
//! Linux builds stub discovery (no hidapi) so CI stays dependency-light.

#[cfg(any(target_os = "macos", target_os = "windows"))]
use anyhow::Context;
use anyhow::{bail, Result};

use super::{LogiDevice, LogiDeviceKind, LogiHostSlot};

#[allow(dead_code)]
pub const LOGITECH_VID: u16 = 0x046D;
#[allow(dead_code)]
const SW_ID: u8 = 0x0A;
#[allow(dead_code)]
const ROOT_FEATURE: u8 = 0x00;
#[allow(dead_code)]
pub const FEAT_CHANGE_HOST: u16 = 0x1814;
pub const REPORT_SHORT: u8 = 0x10;
pub const REPORT_LONG: u8 = 0x11;

#[cfg(any(target_os = "macos", target_os = "windows"))]
const DEVICE_INDICES: [u8; 7] = [0xFF, 1, 2, 3, 4, 5, 6];

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct HidppTarget {
    pub path: Vec<u8>,
    pub name: String,
    pub kind: LogiDeviceKind,
    pub product_id: u16,
    pub device_index: u8,
    pub report_id: u8,
    pub feature_index: u8,
    pub host_count: u8,
    pub current_host: u8,
}

#[derive(Debug, Clone, Default)]
pub struct HidppDiscovery {
    pub targets: Vec<HidppTarget>,
    pub status_note: String,
}

impl HidppDiscovery {
    pub fn is_ready(&self) -> bool {
        !self.targets.is_empty()
    }

    pub fn as_logi_devices(&self) -> Vec<LogiDevice> {
        self.targets
            .iter()
            .map(|target| {
                let hosts = (0..target.host_count.max(1))
                    .map(|index| LogiHostSlot {
                        index,
                        paired: true,
                        connected: index == target.current_host,
                        os: None,
                        name: None,
                    })
                    .collect();
                LogiDevice {
                    id: format!("hidpp:{:04x}:{}", target.product_id, target.device_index),
                    name: target.name.clone(),
                    kind: target.kind,
                    hosts,
                    current_host: Some(target.current_host),
                }
            })
            .collect()
    }
}

pub fn discover_change_host_devices() -> HidppDiscovery {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        match discover_inner() {
            Ok(targets) if !targets.is_empty() => HidppDiscovery {
                status_note: format!(
                    "hid++ ChangeHost ready ({} device{})",
                    targets.len(),
                    if targets.len() == 1 { "" } else { "s" }
                ),
                targets,
            },
            Ok(_) => HidppDiscovery {
                targets: Vec::new(),
                status_note: "hid++: no Logitech device with feature 0x1814 found".to_owned(),
            },
            Err(error) => HidppDiscovery {
                targets: Vec::new(),
                status_note: format!("hid++ discovery failed: {error}"),
            },
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        HidppDiscovery {
            targets: Vec::new(),
            status_note: "hid++: unsupported on this OS build (macOS/Windows only)".to_owned(),
        }
    }
}

#[allow(dead_code)]
pub fn switch_targets_to_host(targets: &[HidppTarget], host_index: u8) -> Result<()> {
    let map = targets
        .iter()
        .map(|target| (target.kind, host_index))
        .collect::<std::collections::HashMap<_, _>>();
    switch_targets_with_map(targets, &map)
}

/// Switch each HID++ target to a host index selected by device kind.
/// Targets whose kind is missing from `host_by_kind` are skipped.
pub fn switch_targets_with_map(
    targets: &[HidppTarget],
    host_by_kind: &std::collections::HashMap<LogiDeviceKind, u8>,
) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        switch_targets_with_map_native(targets, host_by_kind)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (targets, host_by_kind);
        bail!("hid++ ChangeHost is only implemented on macOS and Windows");
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn discover_inner() -> Result<Vec<HidppTarget>> {
    use hidapi::HidApi;

    let api = HidApi::new().context("failed to init hidapi")?;
    let mut candidates = api
        .device_list()
        .filter(|info| info.vendor_id() == LOGITECH_VID)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|info| {
        device_rank(
            info.usage_page(),
            info.usage(),
            info.product_string().unwrap_or(""),
        )
    });

    let mut found = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();
    for info in candidates {
        let path_bytes = info.path().to_bytes().to_vec();
        if !seen_paths.insert(path_bytes.clone()) {
            continue;
        }
        let Ok(device) = api.open_path(info.path()) else {
            continue;
        };
        // Unifying/Bolt receivers expose multiple paired devices on one HID path.
        // Probe every device index so mouse + keyboard are both discovered.
        let base_name = info
            .product_string()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Logitech {:04x}", info.product_id()));
        for (dev_idx, report_id, feat_idx) in probe_all_change_host(&device) {
            let (host_count, current_host) =
                read_host_info(&device, report_id, dev_idx, feat_idx).unwrap_or((3, 0));
            let name = format!("{base_name}#{dev_idx}");
            found.push(HidppTarget {
                path: path_bytes.clone(),
                name: name.clone(),
                kind: infer_kind(info.usage_page(), info.usage(), &name),
                product_id: info.product_id(),
                device_index: dev_idx,
                report_id,
                feature_index: feat_idx,
                host_count: host_count.max(1),
                current_host,
            });
        }
        drop(device);
    }

    assign_kinds_for_receiver_slots(&mut found);
    Ok(dedupe_by_kind(found))
}

/// Receivers often expose paired devices as generic "USB Receiver#N" with kind Other.
/// Map distinct device indices on the same path to keyboard/mouse so per-kind maps work.
fn assign_kinds_for_receiver_slots(targets: &mut [HidppTarget]) {
    use std::collections::HashMap;
    let mut by_path: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (index, target) in targets.iter().enumerate() {
        by_path.entry(target.path.clone()).or_default().push(index);
    }
    for indices in by_path.values() {
        if indices.len() < 2 {
            continue;
        }
        let mut ordered = indices.clone();
        ordered.sort_by_key(|i| targets[*i].device_index);
        let all_other = ordered
            .iter()
            .all(|&i| matches!(targets[i].kind, LogiDeviceKind::Other));
        if !all_other {
            continue;
        }
        if let Some(&first) = ordered.first() {
            targets[first].kind = LogiDeviceKind::Keyboard;
            if targets[first].name.contains('#') {
                targets[first].name = format!(
                    "Keyboard{}",
                    &targets[first].name[targets[first].name.find('#').unwrap()..]
                );
            } else {
                targets[first].name = format!("Keyboard#{}", targets[first].device_index);
            }
        }
        if let Some(&second) = ordered.get(1) {
            targets[second].kind = LogiDeviceKind::Mouse;
            if targets[second].name.contains('#') {
                targets[second].name = format!(
                    "Mouse{}",
                    &targets[second].name[targets[second].name.find('#').unwrap()..]
                );
            } else {
                targets[second].name = format!("Mouse#{}", targets[second].device_index);
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn dedupe_by_kind(mut targets: Vec<HidppTarget>) -> Vec<HidppTarget> {
    let mut mouse = None;
    let mut keyboard = None;
    let mut others = Vec::new();
    for target in targets.drain(..) {
        match target.kind {
            LogiDeviceKind::Mouse if mouse.is_none() => mouse = Some(target),
            LogiDeviceKind::Keyboard if keyboard.is_none() => keyboard = Some(target),
            LogiDeviceKind::Mouse | LogiDeviceKind::Keyboard => {}
            LogiDeviceKind::Other => others.push(target),
        }
    }
    let mut out = Vec::new();
    if let Some(mouse) = mouse {
        out.push(mouse);
    }
    if let Some(keyboard) = keyboard {
        out.push(keyboard);
    }
    if out.is_empty() {
        out.extend(others);
    }
    out
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn switch_targets_with_map_native(
    targets: &[HidppTarget],
    host_by_kind: &std::collections::HashMap<LogiDeviceKind, u8>,
) -> Result<()> {
    use hidapi::HidApi;

    if targets.is_empty() {
        bail!("no hid++ ChangeHost targets available");
    }
    let api = HidApi::new().context("failed to init hidapi for switch")?;
    let mut errors = Vec::new();
    let mut switched = 0_usize;
    let mut attempted = 0_usize;
    for target in targets {
        let Some(&host_index) = host_by_kind.get(&target.kind) else {
            continue;
        };
        attempted += 1;
        if host_index >= target.host_count {
            errors.push(format!(
                "{} has only {} host slot(s); cannot use index {}",
                target.name, target.host_count, host_index
            ));
            continue;
        }
        let path = match std::ffi::CString::new(target.path.clone()) {
            Ok(path) => path,
            Err(_) => {
                errors.push(format!("{}: invalid hid path", target.name));
                continue;
            }
        };
        match api.open_path(&path) {
            Ok(device) => {
                if let Err(error) = set_current_host(
                    &device,
                    target.report_id,
                    target.device_index,
                    target.feature_index,
                    host_index,
                ) {
                    errors.push(format!("{}: {error}", target.name));
                } else {
                    switched += 1;
                }
            }
            Err(error) => errors.push(format!("{}: open failed: {error}", target.name)),
        }
    }
    if attempted == 0 {
        bail!("no hid++ targets matched the requested device-kind host map");
    }
    if switched == 0 {
        bail!("hid++ switch failed for all targets: {}", errors.join("; "));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn probe_all_change_host(device: &hidapi::HidDevice) -> Vec<(u8, u8, u8)> {
    let params = get_feature_params(FEAT_CHANGE_HOST);
    let mut found = Vec::new();
    let mut seen_idx = std::collections::HashSet::new();
    for &dev_idx in &DEVICE_INDICES {
        for &report_id in &[REPORT_LONG, REPORT_SHORT] {
            if let Some(reply) =
                hidpp_request(device, report_id, dev_idx, ROOT_FEATURE, 0, &params, 250)
            {
                if let Some(feature_index) = parse_get_feature_index(&reply) {
                    if seen_idx.insert(dev_idx) {
                        found.push((dev_idx, report_id, feature_index));
                    }
                    break;
                }
            }
        }
    }
    found
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn read_host_info(
    device: &hidapi::HidDevice,
    report_id: u8,
    device_index: u8,
    feature_index: u8,
) -> Option<(u8, u8)> {
    let reply = hidpp_request(device, report_id, device_index, feature_index, 0, &[], 400)?;
    Some((
        reply.get(4).copied().unwrap_or(3),
        reply.get(5).copied().unwrap_or(0),
    ))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn set_current_host(
    device: &hidapi::HidDevice,
    report_id: u8,
    device_index: u8,
    feature_index: u8,
    host_index: u8,
) -> Result<()> {
    let frame = build_set_current_host_frame(report_id, device_index, feature_index, host_index);
    device
        .write(&frame)
        .with_context(|| format!("hid++ setCurrentHost write failed (host={host_index})"))?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn hidpp_request(
    device: &hidapi::HidDevice,
    report_id: u8,
    device_index: u8,
    feature_index: u8,
    function: u8,
    params: &[u8],
    timeout_ms: u64,
) -> Option<Vec<u8>> {
    use std::time::{Duration, Instant};

    let frame = build_request_frame(report_id, device_index, feature_index, function, params);
    if device.write(&frame).is_err() {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.as_millis().min(100) as i32;
        let mut buf = [0_u8; 64];
        match device.read_timeout(&mut buf, wait) {
            Ok(n) if n >= 5 => {
                let reply = &buf[..n];
                if reply[1] != device_index {
                    continue;
                }
                if reply[2] == 0xFF || reply[2] == 0x8F {
                    return None;
                }
                if reply[2] != feature_index || (reply[3] & 0x0F) != SW_ID {
                    continue;
                }
                return Some(reply.to_vec());
            }
            _ => continue,
        }
    }
    None
}

#[allow(dead_code)]
pub fn frame_len(report_id: u8) -> usize {
    match report_id {
        REPORT_SHORT => 7,
        REPORT_LONG => 20,
        _ => 20,
    }
}

#[allow(dead_code)]
pub fn get_feature_params(feature_id: u16) -> [u8; 3] {
    [(feature_id >> 8) as u8, (feature_id & 0xFF) as u8, 0x00]
}

#[allow(dead_code)]
pub fn build_request_frame(
    report_id: u8,
    device_index: u8,
    feature_index: u8,
    function: u8,
    params: &[u8],
) -> Vec<u8> {
    let mut frame = vec![
        report_id,
        device_index,
        feature_index,
        (function << 4) | SW_ID,
    ];
    frame.extend_from_slice(params);
    frame.resize(frame_len(report_id), 0);
    frame
}

#[allow(dead_code)]
pub fn build_set_current_host_frame(
    report_id: u8,
    device_index: u8,
    feature_index: u8,
    host_index: u8,
) -> Vec<u8> {
    build_request_frame(report_id, device_index, feature_index, 1, &[host_index])
}

#[allow(dead_code)]
pub fn parse_get_feature_index(reply: &[u8]) -> Option<u8> {
    if reply.len() < 5 {
        return None;
    }
    let feature_index = reply[4];
    if feature_index == 0 {
        None
    } else {
        Some(feature_index)
    }
}

#[allow(dead_code)]
pub fn device_rank(usage_page: u16, usage: u16, name: &str) -> u8 {
    let lower = name.to_ascii_lowercase();
    let mouse = (usage_page == 0x0001 && usage == 0x0002)
        || [
            "master", "anywhere", "mouse", "ergo", "mchncl", "vertical", "mx m",
        ]
        .iter()
        .any(|w| lower.contains(w));
    let kbd = (usage_page == 0x0001 && usage == 0x0006)
        || ["keys", "keyboard", "casa"]
            .iter()
            .any(|w| lower.contains(w));
    let tier = if kbd && !mouse {
        2
    } else if mouse {
        0
    } else {
        1
    };
    let non_vendor = if usage_page >= 0xFF00 { 0 } else { 1 };
    tier * 10 + non_vendor
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn infer_kind(usage_page: u16, usage: u16, name: &str) -> LogiDeviceKind {
    let lower = name.to_ascii_lowercase();
    if (usage_page == 0x0001 && usage == 0x0002)
        || [
            "master", "anywhere", "mouse", "ergo", "mchncl", "vertical", "mx m",
        ]
        .iter()
        .any(|w| lower.contains(w))
    {
        return LogiDeviceKind::Mouse;
    }
    if (usage_page == 0x0001 && usage == 0x0006)
        || ["keys", "keyboard", "casa"]
            .iter()
            .any(|w| lower.contains(w))
    {
        return LogiDeviceKind::Keyboard;
    }
    LogiDeviceKind::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_current_host_frame_is_short_report() {
        let frame = build_set_current_host_frame(REPORT_SHORT, 0xFF, 0x0D, 1);
        assert_eq!(frame.len(), 7);
        assert_eq!(frame[0], REPORT_SHORT);
        assert_eq!(frame[1], 0xFF);
        assert_eq!(frame[2], 0x0D);
        assert_eq!(frame[3], (1 << 4) | SW_ID);
        assert_eq!(frame[4], 1);
        assert!(frame[5..].iter().all(|b| *b == 0));
    }

    #[test]
    fn get_feature_long_frame_encodes_0x1814() {
        let params = get_feature_params(FEAT_CHANGE_HOST);
        assert_eq!(params, [0x18, 0x14, 0x00]);
        let frame = build_request_frame(REPORT_LONG, 0xFF, ROOT_FEATURE, 0, &params);
        assert_eq!(frame.len(), 20);
        assert_eq!(&frame[4..7], &[0x18, 0x14, 0x00]);
    }

    #[test]
    fn parse_get_feature_rejects_unsupported() {
        assert_eq!(
            parse_get_feature_index(&[0x11, 0xFF, 0x00, 0x0A, 0x00]),
            None
        );
        assert_eq!(
            parse_get_feature_index(&[0x11, 0xFF, 0x00, 0x0A, 0x0D]),
            Some(0x0D)
        );
    }

    #[test]
    fn device_rank_prefers_vendor_mouse() {
        let mouse_vendor = device_rank(0xFF00, 0x0001, "MX Master 3S");
        let kbd_vendor = device_rank(0xFF00, 0x0002, "MX Keys");
        let mouse_generic = device_rank(0x0001, 0x0002, "MX Master 3S");
        assert!(mouse_vendor < kbd_vendor);
        assert!(mouse_vendor < mouse_generic);
    }

    #[test]
    fn assign_kinds_maps_receiver_slots_to_keyboard_and_mouse() {
        let mut targets = vec![
            HidppTarget {
                path: b"receiver".to_vec(),
                name: "USB Receiver#1".to_owned(),
                kind: LogiDeviceKind::Other,
                product_id: 0xc52b,
                device_index: 1,
                report_id: REPORT_SHORT,
                feature_index: 0x0D,
                host_count: 3,
                current_host: 1,
            },
            HidppTarget {
                path: b"receiver".to_vec(),
                name: "USB Receiver#2".to_owned(),
                kind: LogiDeviceKind::Other,
                product_id: 0xc52b,
                device_index: 2,
                report_id: REPORT_SHORT,
                feature_index: 0x0D,
                host_count: 3,
                current_host: 2,
            },
        ];
        assign_kinds_for_receiver_slots(&mut targets);
        assert_eq!(targets[0].kind, LogiDeviceKind::Keyboard);
        assert_eq!(targets[1].kind, LogiDeviceKind::Mouse);
    }
}
