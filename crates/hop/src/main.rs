use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use std::time::Instant;

use anyhow::{bail, Context};
use clap::{Parser, ValueEnum};
use hop_core::clipboard::{clipboard_backend_status, ClipboardBackendStatus};
use hop_core::config::{Config, LocalConfig, PeerConfig};
use hop_core::daemon;
use hop_core::layout::{invert_relative_position, RelativePosition, ScreenBounds};
use hop_core::platform::{build_platform_adapters, permission_status, PermissionStatus};
use hop_protocol::auth::{
    build_client_hello, build_client_proof, derive_session_key, verify_server_challenge,
    AuthChallenge,
};
use hop_protocol::control::{ControlMessage, NodeRole};
use hop_protocol::crypto::CipherState;
use hop_protocol::frame::{decode_control, decode_plain, encode_control, encode_plain};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket as TokioUdpSocket};
use tokio::time::timeout;

const DEFAULT_CONTROL_PORT: u16 = 4600;
const DEFAULT_DATA_PORT: u16 = 4601;
const DEFAULT_PAIRING_PORT: u16 = 4602;
const DEFAULT_SCREEN_WIDTH: u32 = 1920;
const DEFAULT_SCREEN_HEIGHT: u32 = 1080;
const PAIRING_CODE_LENGTH: usize = 4;
const PAIRING_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const PAIRING_DISCOVERY_ATTEMPTS: usize = 10;
const PAIRING_DISCOVERY_WAIT_MS: u64 = 800;
const BENCH_HARD_MAX_MS: f64 = 5.0;
const BENCH_GOAL_HIGH_MS: f64 = 3.0;
const BACKGROUND_PID_FILE_NAME: &str = "hop.pid";
const BACKGROUND_STOP_FILE_NAME: &str = "hop.stop";
const BACKGROUND_LOG_FILE_NAME: &str = "hop.log";

#[derive(Debug, Parser)]
#[command(
    name = "hop",
    about = "hop daemon for cross-machine mouse and keyboard handoff"
)]
struct Cli {
    /// Pairing code from the server machine (example: AB12).
    code: Option<String>,
    /// Path to hop JSON config.
    #[arg(long, global = true, default_value_os_t = default_config_path())]
    config: PathBuf,
    /// Print one-way latency estimates at the client side.
    #[arg(long, global = true, default_value_t = false)]
    log_latency: bool,
    /// Open permission settings when a blocker is detected.
    #[arg(long, global = true, default_value_t = false)]
    open_permissions: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, clap::Subcommand)]
enum Commands {
    /// Start the daemon in either server or client mode.
    Run {
        /// Override role from the config.
        #[arg(long)]
        role: Option<RoleArg>,
        /// Start hop as a detached background process.
        #[arg(long, default_value_t = false)]
        background: bool,
    },
    /// Create a local hop config with sensible defaults.
    Init {
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
        /// Pairing code copied from the server.
        code: Option<String>,
        /// Override server host for discovery.
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
        /// Reachability timeout for peer control check.
        #[arg(long, default_value_t = 800)]
        timeout_ms: u64,
    },
    /// Benchmark encrypted control-path latency to a running hop peer.
    Bench {
        /// Optional host or host:port override for the benchmark target.
        #[arg(long)]
        host: Option<String>,
        /// Number of ping/pong samples.
        #[arg(long, default_value_t = 40)]
        samples: usize,
        /// Per-sample timeout in milliseconds.
        #[arg(long, default_value_t = 400)]
        timeout_ms: u64,
        /// Exit non-zero when p95 one-way estimate exceeds hard max.
        #[arg(long, default_value_t = false)]
        strict: bool,
    },
    /// Gracefully stop a background hop process started via `hop run --background`.
    Stop,
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
struct PairingDiscoveryRequest {
    version: u8,
    code: String,
    client_machine_name: String,
    client_control_port: u16,
    client_data_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingDiscoveryResponse {
    version: u8,
    code: String,
    server_machine_name: String,
    control_port: u16,
    data_port: u16,
    shared_secret: String,
    client_position: RelativePosition,
}

struct ServerPairingResult {
    client_machine_name: String,
    client_host: String,
    client_control_port: u16,
    client_data_port: u16,
}

struct ClientPairingResult {
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
    if cli.code.is_some() && cli.command.is_some() {
        bail!("pairing code cannot be combined with subcommands");
    }
    let code = cli.code;
    let config_path = cli.config;
    let log_latency = cli.log_latency;
    let open_permissions = cli.open_permissions;

    match cli.command {
        Some(Commands::Run { role, background }) => {
            if background {
                start_background_run(&config_path, role, log_latency, open_permissions)?;
                return Ok(());
            }

            print_run_permission_hints(open_permissions)?;
            let config = Config::from_json_path(&config_path)?;
            let override_role = role.map(role_to_node_role);
            run_daemon_command(config, override_role, log_latency).await?;
        }
        Some(Commands::Init {
            role,
            peer,
            position,
            force,
        }) => {
            run_init(&config_path, role, peer, position, force)?;
        }
        Some(Commands::Pair {
            code,
            host,
            position,
        }) => {
            if let Some(code) = code {
                run_pair_client(&config_path, &code, host).await?;
                println!("next: hop --config {}", config_path.display());
            } else {
                run_pair_server(&config_path, position).await?;
                println!("next: hop --config {}", config_path.display());
            }
        }
        Some(Commands::Join {
            host,
            secret,
            control_port,
            data_port,
            position,
        }) => {
            run_join(
                &config_path,
                &host,
                secret,
                control_port,
                data_port,
                position,
            )?;
            println!("next: hop --config {}", config_path.display());
        }
        Some(Commands::Doctor { timeout_ms }) => {
            run_doctor(&config_path, timeout_ms, open_permissions).await?;
        }
        Some(Commands::Bench {
            host,
            samples,
            timeout_ms,
            strict,
        }) => {
            run_bench(&config_path, host.as_deref(), samples, timeout_ms, strict).await?;
        }
        Some(Commands::Stop) => stop_background_run(&config_path)?,
        None => {
            if let Some(code) = code {
                let config = run_pair_client(&config_path, &code, None).await?;
                print_run_permission_hints(true)?;
                run_daemon_command(config, Some(NodeRole::Client), log_latency).await?;
            } else if config_path.exists() {
                print_run_permission_hints(true)?;
                let config = Config::from_json_path(&config_path)?;
                run_daemon_command(config, None, log_latency).await?;
            } else {
                let config = run_pair_server(&config_path, None).await?;
                print_run_permission_hints(true)?;
                run_daemon_command(config, Some(NodeRole::Server), log_latency).await?;
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct BackgroundRuntimePaths {
    pid_file: PathBuf,
    stop_file: PathBuf,
}

struct BackgroundRuntimeGuard {
    paths: BackgroundRuntimePaths,
}

impl BackgroundRuntimeGuard {
    fn install(paths: BackgroundRuntimePaths) -> anyhow::Result<Self> {
        if let Some(parent) = paths.pid_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if paths.stop_file.exists() {
            let _ = fs::remove_file(&paths.stop_file);
        }
        fs::write(&paths.pid_file, std::process::id().to_string())
            .with_context(|| format!("failed to write {}", paths.pid_file.display()))?;
        Ok(Self { paths })
    }
}

impl Drop for BackgroundRuntimeGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.paths.pid_file);
        let _ = fs::remove_file(&self.paths.stop_file);
    }
}

async fn run_daemon_command(
    config: Config,
    role_override: Option<NodeRole>,
    log_latency: bool,
) -> anyhow::Result<()> {
    let runtime_paths = background_runtime_paths_from_env();
    let stop_signal_path = runtime_paths.as_ref().map(|paths| paths.stop_file.clone());
    let _runtime_guard = runtime_paths
        .map(BackgroundRuntimeGuard::install)
        .transpose()?;
    daemon::run(config, role_override, log_latency, stop_signal_path).await
}

fn start_background_run(
    config_path: &Path,
    role: Option<RoleArg>,
    log_latency: bool,
    open_permissions: bool,
) -> anyhow::Result<()> {
    print_run_permission_hints(open_permissions)?;
    let _ = Config::from_json_path(config_path)?;
    let paths = background_paths_for_config(config_path);
    if let Some(parent) = paths.pid_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if paths.stop_file.exists() {
        let _ = fs::remove_file(&paths.stop_file);
    }

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(background_log_path(config_path))
        .with_context(|| "failed to open hop background log file")?;

    let mut command =
        Command::new(std::env::current_exe().context("failed to resolve current executable")?);
    command.arg("--config").arg(config_path).arg("run");
    if let Some(role) = role {
        command.arg("--role").arg(match role {
            RoleArg::Server => "server",
            RoleArg::Client => "client",
        });
    }
    if log_latency {
        command.arg("--log-latency");
    }
    command
        .env("HOP_BACKGROUND_PID_FILE", &paths.pid_file)
        .env("HOP_BACKGROUND_STOP_FILE", &paths.stop_file)
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log_file.try_clone().context("clone log handle")?,
        ))
        .stderr(Stdio::from(log_file));

    let mut child = command
        .spawn()
        .with_context(|| "failed to start hop background process")?;
    std::thread::sleep(Duration::from_millis(200));
    if let Some(status) = child.try_wait().context("failed to check child status")? {
        bail!("background run exited immediately with status {status}");
    }

    println!(
        "hop started in background (pid {}). stop with: hop stop --config {}",
        child.id(),
        config_path.display()
    );
    Ok(())
}

fn stop_background_run(config_path: &Path) -> anyhow::Result<()> {
    let paths = background_paths_for_config(config_path);
    if !paths.pid_file.exists() {
        bail!(
            "no background pid file at {}",
            paths.pid_file.to_string_lossy()
        );
    }

    let pid_raw = fs::read_to_string(&paths.pid_file)
        .with_context(|| format!("failed to read {}", paths.pid_file.display()))?;
    let pid = pid_raw
        .trim()
        .parse::<u32>()
        .with_context(|| format!("invalid pid value in {}", paths.pid_file.display()))?;
    fs::write(&paths.stop_file, format!("stop:{pid}\n"))
        .with_context(|| format!("failed to write {}", paths.stop_file.display()))?;

    for _ in 0..25 {
        if !paths.pid_file.exists() {
            println!("hop background process stopped");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg("-INT")
            .arg(pid.to_string())
            .status();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("Stop-Process -Id {pid} -ErrorAction SilentlyContinue"),
            ])
            .status();
    }

    for _ in 0..10 {
        if !paths.pid_file.exists() {
            println!("hop background process stopped");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    bail!(
        "stop was requested but process still appears alive (pid {pid}); inspect {}",
        background_log_path(config_path).display()
    )
}

fn background_paths_for_config(config_path: &Path) -> BackgroundRuntimePaths {
    let runtime_dir = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    BackgroundRuntimePaths {
        pid_file: runtime_dir.join(BACKGROUND_PID_FILE_NAME),
        stop_file: runtime_dir.join(BACKGROUND_STOP_FILE_NAME),
    }
}

fn background_runtime_paths_from_env() -> Option<BackgroundRuntimePaths> {
    let pid_file = std::env::var_os("HOP_BACKGROUND_PID_FILE").map(PathBuf::from)?;
    let stop_file = std::env::var_os("HOP_BACKGROUND_STOP_FILE").map(PathBuf::from)?;
    Some(BackgroundRuntimePaths {
        pid_file,
        stop_file,
    })
}

fn background_log_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(BACKGROUND_LOG_FILE_NAME)
}

async fn run_bench(
    config_path: &Path,
    host_override: Option<&str>,
    samples: usize,
    timeout_ms: u64,
    strict: bool,
) -> anyhow::Result<()> {
    if samples == 0 {
        bail!("samples must be greater than zero");
    }

    let config = Config::from_json_path(config_path)?;
    let target_control_addr = if let Some(host) = host_override {
        let (host_value, port) = parse_host_with_optional_port(host, DEFAULT_CONTROL_PORT)?;
        format_host_port(&host_value, port)
    } else {
        let peer = config.first_peer();
        if peer.control_addr.trim().is_empty() {
            bail!("peer control_addr is empty; pass --host host:port for bench");
        }
        peer.control_addr.clone()
    };

    println!(
        "bench target: {} ({} samples, timeout {}ms)",
        target_control_addr, samples, timeout_ms
    );

    let mut stream = TcpStream::connect(&target_control_addr)
        .await
        .with_context(|| format!("failed to connect to {}", target_control_addr))?;
    let mut cipher = complete_bench_auth(&mut stream, &config).await?;

    let hello = recv_bench_control_message(&mut stream, &mut cipher).await?;
    if let ControlMessage::Hello {
        machine_name,
        role,
        udp_port: _udp_port,
        screen,
    } = hello
    {
        println!(
            "bench peer: {} ({:?}) screen={}x{}",
            machine_name, role, screen.width, screen.height
        );
    }

    let mut rtt_samples_ms = Vec::with_capacity(samples);
    for _ in 0..samples {
        let token = now_millis();
        let started_at = Instant::now();
        send_bench_control_message(
            &mut stream,
            &mut cipher,
            &ControlMessage::Ping { at_millis: token },
        )
        .await?;

        loop {
            let incoming = timeout(
                Duration::from_millis(timeout_ms),
                recv_bench_control_message(&mut stream, &mut cipher),
            )
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for pong after {timeout_ms}ms"))??;
            match incoming {
                ControlMessage::Pong { at_millis } if at_millis == token => {
                    rtt_samples_ms.push(started_at.elapsed().as_secs_f64() * 1000.0);
                    break;
                }
                ControlMessage::Ping { at_millis } => {
                    send_bench_control_message(
                        &mut stream,
                        &mut cipher,
                        &ControlMessage::Pong { at_millis },
                    )
                    .await?;
                }
                _ => {}
            }
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    rtt_samples_ms.sort_by(|left, right| left.total_cmp(right));
    let p50_rtt_ms = percentile(&rtt_samples_ms, 0.50);
    let p95_rtt_ms = percentile(&rtt_samples_ms, 0.95);
    let p50_one_way_ms = p50_rtt_ms / 2.0;
    let p95_one_way_ms = p95_rtt_ms / 2.0;

    println!(
        "bench results: one-way p50={:.3}ms p95={:.3}ms | rtt p50={:.3}ms p95={:.3}ms",
        p50_one_way_ms, p95_one_way_ms, p50_rtt_ms, p95_rtt_ms
    );
    println!(
        "targets: goal 1-{}ms one-way, hard max {}ms one-way",
        BENCH_GOAL_HIGH_MS, BENCH_HARD_MAX_MS
    );

    if p95_one_way_ms > BENCH_HARD_MAX_MS {
        let message = format!(
            "bench warning: p95 one-way {:.3}ms exceeds hard max {:.1}ms",
            p95_one_way_ms, BENCH_HARD_MAX_MS
        );
        if strict {
            bail!("{message}");
        }
        println!("{message}");
    } else if p95_one_way_ms > BENCH_GOAL_HIGH_MS {
        println!(
            "bench warning: p95 one-way {:.3}ms is above goal upper bound {:.1}ms",
            p95_one_way_ms, BENCH_GOAL_HIGH_MS
        );
    } else {
        println!("bench status: within target");
    }

    Ok(())
}

async fn complete_bench_auth(
    stream: &mut TcpStream,
    config: &Config,
) -> anyhow::Result<CipherState> {
    let mut rng = rand::thread_rng();
    let hello = build_client_hello(&format!("{}-bench", config.local.machine_name), 0, &mut rng);
    write_bench_frame(stream, &encode_plain(&hello)?).await?;

    let challenge_payload = read_bench_frame(stream).await?;
    let challenge: AuthChallenge = decode_plain(&challenge_payload)?;
    verify_server_challenge(config.local.shared_secret.as_bytes(), &hello, &challenge)?;

    let proof = build_client_proof(config.local.shared_secret.as_bytes(), &hello, &challenge);
    write_bench_frame(stream, &encode_plain(&proof)?).await?;

    let session_key =
        derive_session_key(config.local.shared_secret.as_bytes(), &hello, &challenge)?;
    Ok(CipherState::new(&session_key))
}

async fn send_bench_control_message(
    stream: &mut TcpStream,
    cipher: &mut CipherState,
    message: &ControlMessage,
) -> anyhow::Result<()> {
    let payload = encode_control(cipher, message)?;
    write_bench_frame(stream, &payload).await
}

async fn recv_bench_control_message(
    stream: &mut TcpStream,
    cipher: &mut CipherState,
) -> anyhow::Result<ControlMessage> {
    let payload = read_bench_frame(stream).await?;
    decode_control(cipher, &payload).map_err(Into::into)
}

async fn write_bench_frame(stream: &mut TcpStream, payload: &[u8]) -> anyhow::Result<()> {
    const MAX_CONTROL_FRAME_BYTES: usize = 32 * 1024 * 1024;
    if payload.len() > MAX_CONTROL_FRAME_BYTES {
        bail!("refusing oversized bench frame ({} bytes)", payload.len());
    }
    let len = payload.len() as u32;
    stream.write_u32(len).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn read_bench_frame(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    const MAX_CONTROL_FRAME_BYTES: usize = 32 * 1024 * 1024;
    let len = stream.read_u32().await? as usize;
    if len > MAX_CONTROL_FRAME_BYTES {
        bail!("refusing oversized bench frame ({} bytes)", len);
    }
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

fn percentile(sorted_samples: &[f64], ratio: f64) -> f64 {
    let last_index = sorted_samples.len().saturating_sub(1);
    let index = ((last_index as f64) * ratio).round() as usize;
    sorted_samples[index.min(last_index)]
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
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
    let screen = detect_screen_bounds();
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
            swap_ctrl_cmd: None,
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

async fn run_pair_server(
    config_path: &Path,
    position: Option<PositionArg>,
) -> anyhow::Result<Config> {
    let mut config = load_or_create_config(config_path, NodeRole::Server);
    config.local.role = NodeRole::Server;
    if config.local.shared_secret.trim().len() < 16 {
        config.local.shared_secret = generate_shared_secret();
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

    let control_port = parse_port(&config.local.control_bind)
        .with_context(|| format!("invalid control bind {}", config.local.control_bind))?;
    let data_port = parse_port(&config.local.data_bind)
        .with_context(|| format!("invalid data bind {}", config.local.data_bind))?;
    let code = generate_pairing_code();

    println!("pair code: {code}");
    println!("lan addresses:");
    let lan_ips = detect_lan_ips();
    for ip in &lan_ips {
        println!("  - {ip}");
    }
    if lan_ips.is_empty() {
        println!("  - (no LAN address detected)");
    }
    println!("on the other machine, run: hop {code}");
    println!("waiting for peer pairing request...");

    let pair_result = wait_for_pairing_client(
        &code,
        &config.local.machine_name,
        control_port,
        data_port,
        &config.local.shared_secret,
        chosen_position,
    )
    .await?;

    config.peers = vec![PeerConfig {
        machine_name: pair_result.client_machine_name.clone(),
        control_addr: format_host_port(&pair_result.client_host, pair_result.client_control_port),
        data_addr: format_host_port(&pair_result.client_host, pair_result.client_data_port),
        position: chosen_position,
    }];
    config.write_json_path(config_path)?;

    println!(
        "paired with {} ({})",
        pair_result.client_machine_name, pair_result.client_host
    );
    println!("updated {}", config_path.display());
    Ok(config)
}

async fn run_pair_client(
    config_path: &Path,
    code: &str,
    host_override: Option<String>,
) -> anyhow::Result<Config> {
    let normalized_code = normalize_pairing_code(code)?;
    let mut config = load_or_create_config(config_path, NodeRole::Client);
    config.local.role = NodeRole::Client;
    let local_control_port = parse_port(&config.local.control_bind)
        .with_context(|| format!("invalid control bind {}", config.local.control_bind))?;
    let local_data_port = parse_port(&config.local.data_bind)
        .with_context(|| format!("invalid data bind {}", config.local.data_bind))?;

    let pair_result = discover_pairing_server(
        &normalized_code,
        host_override.as_deref(),
        &config.local.machine_name,
        local_control_port,
        local_data_port,
    )
    .await?;
    let server_position = invert_relative_position(pair_result.client_position);
    config.local.shared_secret = pair_result.shared_secret;
    config.peers = vec![PeerConfig {
        machine_name: pair_result.server_machine_name,
        control_addr: format_host_port(&pair_result.server_host, pair_result.control_port),
        data_addr: format_host_port(&pair_result.server_host, pair_result.data_port),
        position: server_position,
    }];
    config.write_json_path(config_path)?;

    println!("paired and updated {}", config_path.display());
    Ok(config)
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
            "config missing: {} (run `hop` on the server, then `hop <code>` on the client)",
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

    let screen = detect_screen_bounds();
    println!(
        "screen geometry: origin=({}, {}), size={}x{}, max=({}, {})",
        screen.origin_x,
        screen.origin_y,
        screen.width,
        screen.height,
        screen.max_x(),
        screen.max_y()
    );
    println!("build source: {}", build_source_label());

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
        let (effective_swap, swap_source) = match config.local.swap_ctrl_cmd {
            Some(value) => (value, "config"),
            None => (cfg!(target_os = "macos"), "platform default"),
        };
        println!(
            "swap_ctrl_cmd effective: {} ({})",
            effective_swap, swap_source
        );

        let machine_name = config.local.machine_name.as_str();
        match clipboard_backend_status(machine_name) {
            ClipboardBackendStatus::Ready => {
                println!("clipboard backend: ready");
            }
            ClipboardBackendStatus::Disabled { reason } => {
                warnings.push(format!("clipboard backend disabled: {reason}"));
            }
        }

        if config.local.shared_secret.trim().len() >= 16 {
            println!("shared secret: set");
        } else {
            blockers.push("shared secret is missing or too short (<16 chars)".to_owned());
        }

        if config.peers.is_empty() {
            blockers.push(
                "no peers configured (pair with `hop` + `hop <code>`, or use `hop join`)"
                    .to_owned(),
            );
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
    } else {
        println!(
            "swap_ctrl_cmd effective: {} (platform default; config missing)",
            cfg!(target_os = "macos")
        );
        match clipboard_backend_status(&detect_machine_name()) {
            ClipboardBackendStatus::Ready => {
                println!("clipboard backend: ready");
            }
            ClipboardBackendStatus::Disabled { reason } => {
                warnings.push(format!("clipboard backend disabled: {reason}"));
            }
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

fn detect_screen_bounds() -> ScreenBounds {
    let adapters = build_platform_adapters();
    adapters
        .screen_provider
        .screen_bounds()
        .unwrap_or(ScreenBounds::from_size(
            DEFAULT_SCREEN_WIDTH,
            DEFAULT_SCREEN_HEIGHT,
        ))
}

fn build_source_label() -> &'static str {
    let build_channel = option_env!("HOP_BUILD_CHANNEL").unwrap_or("source");
    if build_channel.eq_ignore_ascii_case("release") {
        "release binary"
    } else {
        "source build"
    }
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

fn default_config_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("hop").join("config.json");
        }
        if let Ok(user_profile) = std::env::var("USERPROFILE") {
            return PathBuf::from(user_profile)
                .join("AppData")
                .join("Roaming")
                .join("hop")
                .join("config.json");
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg_config).join("hop").join("config.json");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join(".config")
                .join("hop")
                .join("config.json");
        }
    }

    PathBuf::from("hop.json")
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

fn generate_pairing_code() -> String {
    let mut out = String::with_capacity(PAIRING_CODE_LENGTH);
    for _ in 0..PAIRING_CODE_LENGTH {
        let index = (OsRng.next_u32() as usize) % PAIRING_CODE_ALPHABET.len();
        out.push(PAIRING_CODE_ALPHABET[index] as char);
    }
    out
}

fn normalize_pairing_code(code: &str) -> anyhow::Result<String> {
    let cleaned = code
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace() && *ch != '-')
        .collect::<String>()
        .to_ascii_uppercase();
    if cleaned.len() != PAIRING_CODE_LENGTH {
        bail!("pairing code must be {PAIRING_CODE_LENGTH} characters");
    }
    if cleaned
        .as_bytes()
        .iter()
        .any(|ch| !PAIRING_CODE_ALPHABET.contains(ch))
    {
        bail!(
            "pairing code may only use {}",
            String::from_utf8_lossy(PAIRING_CODE_ALPHABET)
        );
    }
    Ok(cleaned)
}

async fn wait_for_pairing_client(
    code: &str,
    server_machine_name: &str,
    control_port: u16,
    data_port: u16,
    shared_secret: &str,
    client_position: RelativePosition,
) -> anyhow::Result<ServerPairingResult> {
    let socket = TokioUdpSocket::bind(format!("0.0.0.0:{DEFAULT_PAIRING_PORT}"))
        .await
        .context("failed to bind pairing socket")?;
    let mut buffer = [0_u8; 2048];
    loop {
        let (len, addr) = socket
            .recv_from(&mut buffer)
            .await
            .context("failed to receive pairing request")?;
        let request = match serde_json::from_slice::<PairingDiscoveryRequest>(&buffer[..len]) {
            Ok(request) => request,
            Err(_) => continue,
        };
        if request.version != 1 || request.code != code {
            continue;
        }

        let response = PairingDiscoveryResponse {
            version: 1,
            code: code.to_owned(),
            server_machine_name: server_machine_name.to_owned(),
            control_port,
            data_port,
            shared_secret: shared_secret.to_owned(),
            client_position,
        };
        let payload = serde_json::to_vec(&response).context("failed to encode pairing response")?;
        socket
            .send_to(&payload, addr)
            .await
            .context("failed to send pairing response")?;
        return Ok(ServerPairingResult {
            client_machine_name: request.client_machine_name,
            client_host: format_ip_host(addr.ip()),
            client_control_port: request.client_control_port,
            client_data_port: request.client_data_port,
        });
    }
}

async fn discover_pairing_server(
    code: &str,
    host_override: Option<&str>,
    client_machine_name: &str,
    client_control_port: u16,
    client_data_port: u16,
) -> anyhow::Result<ClientPairingResult> {
    let socket = TokioUdpSocket::bind("0.0.0.0:0")
        .await
        .context("failed to bind pairing discovery socket")?;
    socket
        .set_broadcast(host_override.is_none())
        .context("failed to configure pairing discovery socket")?;

    let (targets, forced_server_host) = if let Some(host) = host_override {
        let (host_value, port) = parse_host_with_optional_port(host, DEFAULT_PAIRING_PORT)?;
        (vec![format_host_port(&host_value, port)], Some(host_value))
    } else {
        (
            vec![format!("255.255.255.255:{DEFAULT_PAIRING_PORT}")],
            None,
        )
    };
    let request = PairingDiscoveryRequest {
        version: 1,
        code: code.to_owned(),
        client_machine_name: client_machine_name.to_owned(),
        client_control_port,
        client_data_port,
    };
    let payload = serde_json::to_vec(&request).context("failed to encode pairing request")?;
    let mut buffer = [0_u8; 2048];

    for _ in 0..PAIRING_DISCOVERY_ATTEMPTS {
        for target in &targets {
            socket
                .send_to(&payload, target)
                .await
                .with_context(|| format!("failed to send pairing request to {target}"))?;
        }

        let received = timeout(
            Duration::from_millis(PAIRING_DISCOVERY_WAIT_MS),
            socket.recv_from(&mut buffer),
        )
        .await;
        let (len, addr) = match received {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => return Err(error).context("failed to receive pairing response"),
            Err(_) => continue,
        };
        let response = match serde_json::from_slice::<PairingDiscoveryResponse>(&buffer[..len]) {
            Ok(response) => response,
            Err(_) => continue,
        };
        if response.version != 1 || response.code != code || response.shared_secret.len() < 16 {
            continue;
        }

        let server_host = forced_server_host
            .clone()
            .unwrap_or_else(|| format_ip_host(addr.ip()));
        return Ok(ClientPairingResult {
            server_machine_name: response.server_machine_name,
            server_host,
            control_port: response.control_port,
            data_port: response.data_port,
            shared_secret: response.shared_secret,
            client_position: response.client_position,
        });
    }

    bail!(
        "could not find server for code {code}; ensure both machines are on the same LAN and try again"
    )
}

fn load_or_create_config(config_path: &Path, role: NodeRole) -> Config {
    if config_path.exists() {
        if let Ok(config) = Config::from_json_path_unvalidated(config_path) {
            return config;
        }
    }

    let screen = detect_screen_bounds();
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
            swap_ctrl_cmd: None,
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
            println!("next: hop --config {}", config_path.display());
        }
        NodeRole::Client => {
            if has_peer {
                println!("next: hop --config {}", config_path.display());
            } else {
                println!("next: hop <code> --config {}", config_path.display());
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
    fn pairing_code_generation_is_unambiguous() {
        let code = generate_pairing_code();
        assert_eq!(code.len(), PAIRING_CODE_LENGTH);
        assert!(code
            .as_bytes()
            .iter()
            .all(|ch| PAIRING_CODE_ALPHABET.contains(ch)));
    }

    #[test]
    fn pairing_code_normalization_rejects_ambiguous_characters() {
        let normalized = normalize_pairing_code("ab-23").expect("normalize");
        assert_eq!(normalized, "AB23");

        let error = normalize_pairing_code("A10O").expect_err("invalid");
        assert!(error.to_string().contains("pairing code may only use"));
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
            invert_relative_position(RelativePosition::Left),
            RelativePosition::Right
        );
        assert_eq!(
            invert_relative_position(RelativePosition::Right),
            RelativePosition::Left
        );
        assert_eq!(
            invert_relative_position(RelativePosition::Above),
            RelativePosition::Below
        );
        assert_eq!(
            invert_relative_position(RelativePosition::Below),
            RelativePosition::Above
        );
    }
}
