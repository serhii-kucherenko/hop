use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use clap::{Parser, ValueEnum};
use hop_core::config::{Config, LocalConfig, PeerConfig};
use hop_core::daemon;
use hop_core::layout::{RelativePosition, ScreenSize};
use hop_core::platform::{build_platform_adapters, permission_status, PermissionStatus};
use hop_protocol::control::NodeRole;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::timeout;

const DEFAULT_CONTROL_PORT: u16 = 4600;
const DEFAULT_DATA_PORT: u16 = 4601;
const DEFAULT_SCREEN_WIDTH: u32 = 1920;
const DEFAULT_SCREEN_HEIGHT: u32 = 1080;

#[derive(Debug, Parser)]
#[command(
    name = "hop",
    about = "hop daemon for cross-machine mouse and keyboard handoff"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, clap::Subcommand)]
enum Commands {
    /// Start the daemon in either server or client mode.
    Run {
        /// Path to hop JSON config.
        #[arg(long, default_value = "hop.json")]
        config: PathBuf,
        /// Override role from the config.
        #[arg(long)]
        role: Option<RoleArg>,
        /// Print one-way latency estimates at the client side.
        #[arg(long, default_value_t = false)]
        log_latency: bool,
        /// Open permission settings when a blocker is detected.
        #[arg(long, default_value_t = false)]
        open_permissions: bool,
    },
    /// Create a local hop config with sensible defaults.
    Init {
        /// Path to hop JSON config.
        #[arg(long, default_value = "hop.json")]
        config: PathBuf,
        /// Role for this machine.
        #[arg(long)]
        role: Option<RoleArg>,
        /// Peer host or host:port (optional; defer and pair later if omitted).
        #[arg(long)]
        peer: Option<String>,
        /// Relative position of the peer (server default: right, client default: left).
        #[arg(long)]
        position: Option<PositionArg>,
        /// Overwrite existing config file.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Print pairing code (server) or consume pairing code (client).
    Pair {
        /// Path to hop JSON config.
        #[arg(long, default_value = "hop.json")]
        config: PathBuf,
        /// Pairing code copied from the server.
        code: Option<String>,
        /// Override host from pairing code.
        #[arg(long)]
        host: Option<String>,
        /// Override client position on server when generating pairing code.
        #[arg(long)]
        position: Option<PositionArg>,
    },
    /// Join a known server host and write peer + secret into local config.
    Join {
        /// Server host or host:port.
        host: String,
        /// Path to hop JSON config.
        #[arg(long, default_value = "hop.json")]
        config: PathBuf,
        /// Shared secret from the server.
        #[arg(long)]
        secret: Option<String>,
        /// Control port when host omits a port.
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        control_port: u16,
        /// Data port for datagrams.
        #[arg(long, default_value_t = DEFAULT_DATA_PORT)]
        data_port: u16,
        /// Position of the server relative to this client.
        #[arg(long, default_value = "left")]
        position: PositionArg,
    },
    /// Run onboarding checks and report hard blockers.
    Doctor {
        /// Path to hop JSON config.
        #[arg(long, default_value = "hop.json")]
        config: PathBuf,
        /// Reachability timeout for peer control check.
        #[arg(long, default_value_t = 800)]
        timeout_ms: u64,
        /// Open permission settings when a blocker is detected.
        #[arg(long, default_value_t = false)]
        open_permissions: bool,
    },
    /// Reserved command for future network latency benchmarks.
    Bench,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, ValueEnum)]
enum RoleArg {
    Server,
    Client,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, ValueEnum)]
enum PositionArg {
    Left,
    Right,
    Above,
    Below,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingPayload {
    version: u8,
    server_machine_name: String,
    server_host: String,
    control_port: u16,
    data_port: u16,
    shared_secret: String,
    client_position: RelativePosition,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            config,
            role,
            log_latency,
            open_permissions,
        } => {
            print_run_permission_hints(open_permissions)?;
            let config = Config::from_json_path(&config)?;
            let override_role = role.map(|role| match role {
                RoleArg::Server => NodeRole::Server,
                RoleArg::Client => NodeRole::Client,
            });
            daemon::run(config, override_role, log_latency).await?;
        }
        Commands::Init {
            config,
            role,
            peer,
            position,
            force,
        } => {
            run_init(&config, role, peer, position, force)?;
        }
        Commands::Pair {
            config,
            code,
            host,
            position,
        } => {
            if let Some(code) = code {
                run_pair_client(&config, &code, host)?;
            } else {
                run_pair_server(&config, position)?;
            }
        }
        Commands::Join {
            host,
            config,
            secret,
            control_port,
            data_port,
            position,
        } => {
            run_join(&config, &host, secret, control_port, data_port, position)?;
        }
        Commands::Doctor {
            config,
            timeout_ms,
            open_permissions,
        } => {
            run_doctor(&config, timeout_ms, open_permissions).await?;
        }
        Commands::Bench => {
            println!("bench hook: reserved for future probes against 1-3ms goal and 5ms hard max");
        }
    }

    Ok(())
}

fn run_init(
    config_path: &Path,
    role: Option<RoleArg>,
    peer: Option<String>,
    position: Option<PositionArg>,
    force: bool,
) -> anyhow::Result<()> {
    if config_path.exists() && !force {
        bail!(
            "config already exists at {} (use --force to overwrite)",
            config_path.display()
        );
    }

    let role = match role {
        Some(role) => role_to_node_role(role),
        None => prompt_role()?,
    };
    let peer = if peer.is_some() || !io::stdin().is_terminal() {
        peer
    } else {
        prompt_optional_peer(role)?
    };
    let screen = detect_screen_size();
    let machine_name = detect_machine_name();
    let shared_secret = generate_shared_secret();
    let client_position_on_server =
        position
            .map(position_to_relative)
            .unwrap_or(if role == NodeRole::Server {
                RelativePosition::Right
            } else {
                RelativePosition::Left
            });
    let has_peer = peer.is_some();
    let config_peer = match role {
        NodeRole::Server => build_server_peer(peer.as_deref(), client_position_on_server)?,
        NodeRole::Client => build_client_peer(peer.as_deref(), client_position_on_server)?,
    };

    let config = Config {
        local: LocalConfig {
            machine_name,
            role,
            control_bind: format!("0.0.0.0:{DEFAULT_CONTROL_PORT}"),
            data_bind: format!("0.0.0.0:{DEFAULT_DATA_PORT}"),
            shared_secret,
            screen_width: screen.width,
            screen_height: screen.height,
        },
        peers: vec![config_peer],
    };
    config.write_json_path(config_path)?;
    println!("created {}", config_path.display());
    print_next_steps_after_init(config_path, role, has_peer);
    Ok(())
}

fn run_pair_server(config_path: &Path, position: Option<PositionArg>) -> anyhow::Result<()> {
    let mut config = Config::from_json_path_unvalidated(config_path)?;
    if config.local.role != NodeRole::Server {
        bail!("hop pair (without code) requires local.role=server");
    }

    let chosen_position = position
        .map(position_to_relative)
        .or_else(|| config.peers.first().map(|peer| peer.position))
        .unwrap_or(RelativePosition::Right);

    if config.peers.is_empty() {
        config.peers.push(build_server_peer(None, chosen_position)?);
    } else if let Some(first_peer) = config.peers.first_mut() {
        first_peer.position = chosen_position;
    }
    config.write_json_path(config_path)?;

    let control_port = parse_port(&config.local.control_bind)
        .with_context(|| format!("invalid control bind {}", config.local.control_bind))?;
    let data_port = parse_port(&config.local.data_bind)
        .with_context(|| format!("invalid data bind {}", config.local.data_bind))?;
    let lan_ips = detect_lan_ips();
    let server_host = lan_ips
        .first()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1".to_owned());

    let payload = PairingPayload {
        version: 1,
        server_machine_name: config.local.machine_name.clone(),
        server_host: server_host.clone(),
        control_port,
        data_port,
        shared_secret: config.local.shared_secret.clone(),
        client_position: chosen_position,
    };
    let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload)?);
    let code = format!("hop1.{encoded}");

    println!("pair code: {code}");
    println!("lan addresses:");
    for ip in &lan_ips {
        println!("  - {ip}");
    }
    if lan_ips.is_empty() {
        println!("  - (no LAN address detected; use --host on the client)");
    }
    println!(
        "on the client, run: hop pair {code} --config {}",
        config_path.display()
    );
    println!("trust model: pairing code is LAN-only and should be treated like a password.");
    Ok(())
}

fn run_pair_client(
    config_path: &Path,
    code: &str,
    host_override: Option<String>,
) -> anyhow::Result<()> {
    let payload = decode_pairing_code(code)?;
    let host = host_override.unwrap_or(payload.server_host);
    let mut config = load_or_create_config(config_path, NodeRole::Client);
    let server_position = invert_position(payload.client_position);
    config.local.role = NodeRole::Client;
    config.local.shared_secret = payload.shared_secret;
    config.peers = vec![PeerConfig {
        machine_name: payload.server_machine_name,
        control_addr: format_host_port(&host, payload.control_port),
        data_addr: format_host_port(&host, payload.data_port),
        position: server_position,
    }];
    config.write_json_path(config_path)?;

    println!("paired and updated {}", config_path.display());
    println!("next: hop doctor --config {}", config_path.display());
    println!("then: hop run --config {}", config_path.display());
    Ok(())
}

fn run_join(
    config_path: &Path,
    host: &str,
    secret: Option<String>,
    control_port: u16,
    data_port: u16,
    position: PositionArg,
) -> anyhow::Result<()> {
    let secret = match secret {
        Some(secret) => secret,
        None => prompt_secret()?,
    };
    if secret.trim().len() < 16 {
        bail!("shared secret must be at least 16 characters");
    }

    let (host_value, parsed_control_port) = parse_host_with_optional_port(host, control_port)?;
    let mut config = load_or_create_config(config_path, NodeRole::Client);
    config.local.role = NodeRole::Client;
    config.local.shared_secret = secret;
    config.peers = vec![PeerConfig {
        machine_name: "hop-server".to_owned(),
        control_addr: format_host_port(&host_value, parsed_control_port),
        data_addr: format_host_port(&host_value, data_port),
        position: position_to_relative(position),
    }];
    config.write_json_path(config_path)?;

    println!("joined server and updated {}", config_path.display());
    println!("next: hop doctor --config {}", config_path.display());
    println!("then: hop run --config {}", config_path.display());
    Ok(())
}

async fn run_doctor(
    config_path: &Path,
    timeout_ms: u64,
    open_permissions: bool,
) -> anyhow::Result<()> {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    println!("hop doctor");
    if !config_path.exists() {
        blockers.push(format!(
            "config missing: {} (run `hop init` first)",
            config_path.display()
        ));
    }

    let config = if config_path.exists() {
        match Config::from_json_path_unvalidated(config_path) {
            Ok(config) => {
                println!("config: found ({})", config_path.display());
                Some(config)
            }
            Err(error) => {
                blockers.push(format!("config parse failed: {error}"));
                None
            }
        }
    } else {
        None
    };

    let screen = detect_screen_size();
    println!("screen size: {}x{}", screen.width, screen.height);

    match permission_status() {
        PermissionStatus::Granted => {
            println!("permissions: ready");
        }
        PermissionStatus::Missing => {
            blockers.push(
                "permissions missing: grant Accessibility and Input Monitoring, then relaunch"
                    .to_owned(),
            );
            print_macos_permission_steps();
            if open_permissions {
                open_macos_permission_settings();
            }
        }
        PermissionStatus::Unknown => {
            warnings.push("permissions: unable to auto-verify on this platform".to_owned());
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(is_admin) = detect_windows_admin() {
            if !is_admin {
                warnings.push(
                    "windows elevation mismatch risk: run hop at the same privilege level as target apps"
                        .to_owned(),
                );
            }
        } else {
            warnings.push("windows elevation check unavailable".to_owned());
        }
    }

    if let Some(config) = config.as_ref() {
        if config.local.shared_secret.trim().len() >= 16 {
            println!("shared secret: set");
        } else {
            blockers.push("shared secret is missing or too short (<16 chars)".to_owned());
        }

        if config.peers.is_empty() {
            blockers.push("no peers configured (run `hop pair` or `hop join`)".to_owned());
        } else if config.local.role == NodeRole::Client {
            let peer = &config.peers[0];
            if peer.control_addr.trim().is_empty() {
                blockers.push("peer control address is missing".to_owned());
            } else {
                match check_peer_reachable(&peer.control_addr, timeout_ms).await {
                    Ok(()) => println!("peer control reachability: ok ({})", peer.control_addr),
                    Err(error) => blockers.push(format!(
                        "peer control unreachable ({}): {}",
                        peer.control_addr, error
                    )),
                }
            }
        } else {
            println!("peer control reachability: skipped for server role");
        }
    }

    if !warnings.is_empty() {
        println!("warnings:");
        for warning in &warnings {
            println!("  - {warning}");
        }
    }

    if blockers.is_empty() {
        println!("doctor result: ready");
        Ok(())
    } else {
        println!("blockers:");
        for blocker in &blockers {
            println!("  - {blocker}");
        }
        bail!("doctor found {} blocker(s)", blockers.len());
    }
}

fn print_run_permission_hints(open_permissions: bool) -> anyhow::Result<()> {
    match permission_status() {
        PermissionStatus::Granted => Ok(()),
        PermissionStatus::Missing => {
            print_macos_permission_steps();
            if open_permissions {
                open_macos_permission_settings();
            }
            Ok(())
        }
        PermissionStatus::Unknown => {
            #[cfg(target_os = "windows")]
            {
                if let Some(false) = detect_windows_admin() {
                    println!(
                        "warning: hop is not elevated; elevated apps may block hooks/injection. \
                         Match privilege level on both ends."
                    );
                }
            }
            Ok(())
        }
    }
}

fn print_macos_permission_steps() {
    #[cfg(target_os = "macos")]
    {
        println!("macOS permissions needed:");
        println!("  1) System Settings -> Privacy & Security -> Accessibility");
        println!("  2) System Settings -> Privacy & Security -> Input Monitoring");
        println!("  3) Add and enable hop (or Terminal while developing), then relaunch.");
    }
}

fn open_macos_permission_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .status();
        let _ = Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent")
            .status();
    }
}

fn detect_screen_size() -> ScreenSize {
    let adapters = build_platform_adapters();
    adapters
        .screen_provider
        .screen_size()
        .unwrap_or(ScreenSize {
            width: DEFAULT_SCREEN_WIDTH,
            height: DEFAULT_SCREEN_HEIGHT,
        })
}

fn detect_machine_name() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }

    if let Ok(hostname_file) = fs::read_to_string("/etc/hostname") {
        let trimmed = hostname_file.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    "hop-machine".to_owned()
}

fn generate_shared_secret() -> String {
    let mut bytes = [0_u8; 24];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn role_to_node_role(role: RoleArg) -> NodeRole {
    match role {
        RoleArg::Server => NodeRole::Server,
        RoleArg::Client => NodeRole::Client,
    }
}

fn position_to_relative(position: PositionArg) -> RelativePosition {
    match position {
        PositionArg::Left => RelativePosition::Left,
        PositionArg::Right => RelativePosition::Right,
        PositionArg::Above => RelativePosition::Above,
        PositionArg::Below => RelativePosition::Below,
    }
}

fn invert_position(position: RelativePosition) -> RelativePosition {
    match position {
        RelativePosition::Left => RelativePosition::Right,
        RelativePosition::Right => RelativePosition::Left,
        RelativePosition::Above => RelativePosition::Below,
        RelativePosition::Below => RelativePosition::Above,
    }
}

fn build_server_peer(peer: Option<&str>, position: RelativePosition) -> anyhow::Result<PeerConfig> {
    let (control_addr, data_addr) = if let Some(peer_value) = peer {
        let (host, control_port) = parse_host_with_optional_port(peer_value, DEFAULT_CONTROL_PORT)?;
        (
            format_host_port(&host, control_port),
            format_host_port(&host, DEFAULT_DATA_PORT),
        )
    } else {
        (String::new(), String::new())
    };
    Ok(PeerConfig {
        machine_name: "paired-client".to_owned(),
        control_addr,
        data_addr,
        position,
    })
}

fn build_client_peer(peer: Option<&str>, position: RelativePosition) -> anyhow::Result<PeerConfig> {
    let (control_addr, data_addr) = if let Some(peer_value) = peer {
        let (host, control_port) = parse_host_with_optional_port(peer_value, DEFAULT_CONTROL_PORT)?;
        (
            format_host_port(&host, control_port),
            format_host_port(&host, DEFAULT_DATA_PORT),
        )
    } else {
        (String::new(), String::new())
    };

    Ok(PeerConfig {
        machine_name: "hop-server".to_owned(),
        control_addr,
        data_addr,
        position,
    })
}

fn decode_pairing_code(code: &str) -> anyhow::Result<PairingPayload> {
    let payload = code
        .strip_prefix("hop1.")
        .ok_or_else(|| anyhow::anyhow!("invalid code prefix"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.as_bytes())
        .context("failed to decode pairing code")?;
    let parsed: PairingPayload =
        serde_json::from_slice(&bytes).context("failed to parse pairing code payload")?;
    if parsed.version != 1 {
        bail!("unsupported pairing code version {}", parsed.version);
    }
    if parsed.shared_secret.trim().len() < 16 {
        bail!("pairing code contains weak shared secret");
    }
    Ok(parsed)
}

fn load_or_create_config(config_path: &Path, role: NodeRole) -> Config {
    if config_path.exists() {
        if let Ok(config) = Config::from_json_path_unvalidated(config_path) {
            return config;
        }
    }

    let screen = detect_screen_size();
    let peer_position = if role == NodeRole::Server {
        RelativePosition::Right
    } else {
        RelativePosition::Left
    };
    Config {
        local: LocalConfig {
            machine_name: detect_machine_name(),
            role,
            control_bind: format!("0.0.0.0:{DEFAULT_CONTROL_PORT}"),
            data_bind: format!("0.0.0.0:{DEFAULT_DATA_PORT}"),
            shared_secret: generate_shared_secret(),
            screen_width: screen.width,
            screen_height: screen.height,
        },
        peers: vec![PeerConfig {
            machine_name: if role == NodeRole::Server {
                "paired-client".to_owned()
            } else {
                "hop-server".to_owned()
            },
            control_addr: String::new(),
            data_addr: String::new(),
            position: peer_position,
        }],
    }
}

fn parse_host_with_optional_port(host: &str, default_port: u16) -> anyhow::Result<(String, u16)> {
    if host.trim().is_empty() {
        bail!("host cannot be empty");
    }

    if let Ok(socket) = host.parse::<SocketAddr>() {
        return Ok((format_ip_host(socket.ip()), socket.port()));
    }

    if let Some((raw_host, raw_port)) = host.rsplit_once(':') {
        if !raw_host.contains(':') {
            let port = raw_port
                .parse::<u16>()
                .with_context(|| format!("invalid port in host {host}"))?;
            return Ok((raw_host.to_owned(), port));
        }
    }

    Ok((host.to_owned(), default_port))
}

fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') && !host.ends_with(']') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn format_ip_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

fn parse_port(bind_addr: &str) -> anyhow::Result<u16> {
    let (_, port_str) = bind_addr
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid bind address {bind_addr}"))?;
    let port = port_str
        .parse::<u16>()
        .with_context(|| format!("invalid port in bind address {bind_addr}"))?;
    Ok(port)
}

fn detect_lan_ips() -> Vec<String> {
    let mut ips = Vec::new();
    if let Some(ip) = detect_local_ip("0.0.0.0:0", "1.1.1.1:53") {
        ips.push(ip);
    }
    if let Some(ip) = detect_local_ip("[::]:0", "[2606:4700:4700::1111]:53") {
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    ips
}

fn detect_local_ip(bind_addr: &str, probe_addr: &str) -> Option<String> {
    let socket = UdpSocket::bind(bind_addr).ok()?;
    socket.connect(probe_addr).ok()?;
    let addr = socket.local_addr().ok()?;
    if addr.ip().is_loopback() {
        return None;
    }
    Some(addr.ip().to_string())
}

async fn check_peer_reachable(addr: &str, timeout_ms: u64) -> anyhow::Result<()> {
    let connect_future = TcpStream::connect(addr);
    timeout(Duration::from_millis(timeout_ms), connect_future)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}ms", timeout_ms))?
        .context("connect failed")?;
    Ok(())
}

fn prompt_role() -> anyhow::Result<NodeRole> {
    if !io::stdin().is_terminal() {
        bail!("missing --role (non-interactive session)");
    }
    let value = prompt_line("role [server/client] (default: server): ")?;
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == "server" {
        return Ok(NodeRole::Server);
    }
    if normalized == "client" {
        return Ok(NodeRole::Client);
    }
    bail!("unknown role {normalized}")
}

fn prompt_secret() -> anyhow::Result<String> {
    if !io::stdin().is_terminal() {
        bail!("missing --secret (non-interactive session)");
    }
    let value = prompt_line("shared secret: ")?;
    if value.trim().is_empty() {
        bail!("shared secret cannot be empty");
    }
    Ok(value.trim().to_owned())
}

fn prompt_optional_peer(role: NodeRole) -> anyhow::Result<Option<String>> {
    let prompt = if role == NodeRole::Server {
        "peer host (optional, press Enter to defer pairing): "
    } else {
        "server host (optional, press Enter to pair later): "
    };
    let value = prompt_line(prompt)?;
    if value.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(value))
}

fn prompt_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    io::stdout().flush().context("failed to flush stdout")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read stdin")?;
    Ok(input.trim().to_owned())
}

fn print_next_steps_after_init(config_path: &Path, role: NodeRole, has_peer: bool) {
    match role {
        NodeRole::Server => {
            println!("next: hop pair --config {}", config_path.display());
            println!("then: hop doctor --config {}", config_path.display());
            println!("then: hop run --config {}", config_path.display());
        }
        NodeRole::Client => {
            if has_peer {
                println!("next: hop doctor --config {}", config_path.display());
                println!("then: hop run --config {}", config_path.display());
            } else {
                println!("next: hop pair <code> --config {}", config_path.display());
                println!(
                    "or:   hop join <server-host> --secret <secret> --config {}",
                    config_path.display()
                );
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn detect_windows_admin() -> Option<bool> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "[bool](([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let normalized = text.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_roundtrip() {
        let payload = PairingPayload {
            version: 1,
            server_machine_name: "macbook".to_owned(),
            server_host: "192.168.1.40".to_owned(),
            control_port: 4600,
            data_port: 4601,
            shared_secret: "0123456789abcdef0123456789abcdef".to_owned(),
            client_position: RelativePosition::Right,
        };
        let encoded = format!(
            "hop1.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("encode"))
        );
        let decoded = decode_pairing_code(&encoded).expect("decode");
        assert_eq!(decoded.server_machine_name, payload.server_machine_name);
        assert_eq!(decoded.server_host, payload.server_host);
        assert_eq!(decoded.control_port, payload.control_port);
        assert_eq!(decoded.data_port, payload.data_port);
        assert_eq!(decoded.shared_secret, payload.shared_secret);
        assert_eq!(decoded.client_position, payload.client_position);
    }

    #[test]
    fn host_parser_handles_optional_port() {
        let (host, port) =
            parse_host_with_optional_port("example.local:4700", DEFAULT_CONTROL_PORT).expect("ok");
        assert_eq!(host, "example.local");
        assert_eq!(port, 4700);

        let (host, port) =
            parse_host_with_optional_port("example.local", DEFAULT_CONTROL_PORT).expect("ok");
        assert_eq!(host, "example.local");
        assert_eq!(port, DEFAULT_CONTROL_PORT);
    }

    #[test]
    fn invert_position_is_correct() {
        assert_eq!(
            invert_position(RelativePosition::Left),
            RelativePosition::Right
        );
        assert_eq!(
            invert_position(RelativePosition::Right),
            RelativePosition::Left
        );
        assert_eq!(
            invert_position(RelativePosition::Above),
            RelativePosition::Below
        );
        assert_eq!(
            invert_position(RelativePosition::Below),
            RelativePosition::Above
        );
    }
}
