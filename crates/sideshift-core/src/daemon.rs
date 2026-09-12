use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rand::thread_rng;
use sideshift_protocol::auth::{
    build_client_hello, build_client_proof, build_server_challenge, derive_session_key,
    verify_client_proof, verify_server_challenge, AuthChallenge, AuthHello, AuthProof,
};
use sideshift_protocol::control::{ControlMessage, NodeRole, ScreenSize as WireScreenSize};
use sideshift_protocol::crypto::CipherState;
use sideshift_protocol::datagram::{decode_datagram, encode_datagram, InputDatagram};
use sideshift_protocol::frame::{decode_control, decode_plain, encode_control, encode_plain};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::{Duration, MissedTickBehavior};

use crate::config::{Config, PeerConfig};
use crate::handoff::{FocusState, HandoffAction, HandoffController};
use crate::layout::{ScreenSize, SpatialLayout, SpatialNeighbor};
use crate::platform::build_platform_adapters;

pub async fn run(
    config: Config,
    role_override: Option<NodeRole>,
    log_latency: bool,
) -> anyhow::Result<()> {
    let role = role_override.unwrap_or(config.local.role);
    match role {
        NodeRole::Server => run_server(config).await,
        NodeRole::Client => run_client(config, log_latency).await,
    }
}

async fn run_server(config: Config) -> anyhow::Result<()> {
    let mut adapters = build_platform_adapters();
    let local_screen = adapters
        .screen_provider
        .screen_size()
        .unwrap_or(ScreenSize {
            width: config.local.screen_width,
            height: config.local.screen_height,
        });
    let layout = SpatialLayout::new(
        config
            .peers
            .iter()
            .map(|peer| SpatialNeighbor {
                machine_name: peer.machine_name.clone(),
                position: peer.position,
            })
            .collect(),
    );

    println!(
        "starting SideShift server on control {} (data {}), waiting for client...",
        config.local.control_bind, config.local.data_bind
    );
    let listener = TcpListener::bind(&config.local.control_bind)
        .await
        .with_context(|| {
            format!(
                "failed to bind server control socket {}",
                config.local.control_bind
            )
        })?;
    let udp_socket = UdpSocket::bind(&config.local.data_bind)
        .await
        .with_context(|| {
            format!(
                "failed to bind server datagram socket {}",
                config.local.data_bind
            )
        })?;

    let (mut stream, addr) = listener.accept().await.context("failed to accept client")?;
    println!("control client connected from {addr}");
    let peer = find_peer_for_client(&config, addr.ip().to_string())
        .unwrap_or_else(|| config.first_peer().clone());
    let (hello, mut cipher) =
        complete_server_auth(&mut stream, config.local.shared_secret.as_bytes()).await?;
    println!("authenticated peer machine {}", hello.machine_name);

    let hello_msg = ControlMessage::Hello {
        machine_name: config.local.machine_name.clone(),
        role: NodeRole::Server,
        udp_port: parse_port(&config.local.data_bind)?,
        screen: WireScreenSize {
            width: local_screen.width,
            height: local_screen.height,
        },
    };
    send_control_message(&mut stream, &mut cipher, &hello_msg).await?;
    println!("control channel encrypted and ready");

    let peer_data_addr: SocketAddr = peer
        .data_addr
        .parse()
        .with_context(|| format!("invalid peer data_addr {}", peer.data_addr))?;

    let mut handoff = HandoffController::new(config.local.machine_name.clone());
    let mut ticker = tokio::time::interval(Duration::from_millis(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;

        if let Some(cursor) = adapters.input_capture.poll_cursor_position()? {
            let action = handoff.on_local_cursor(cursor, local_screen, &layout);
            if let HandoffAction::Begin {
                target_machine,
                edge,
            } = action
            {
                adapters.cursor_controller.hide_cursor()?;
                adapters
                    .cursor_controller
                    .warp_cursor_to_safe_point(edge, local_screen)?;
                let message = ControlMessage::HandoffStart {
                    from_machine: config.local.machine_name.clone(),
                    to_machine: target_machine.clone(),
                    edge,
                };
                send_control_message(&mut stream, &mut cipher, &message).await?;
                println!(
                    "handoff started from {} to {} through {:?}",
                    config.local.machine_name, target_machine, edge
                );
            }
        }

        if matches!(handoff.focus_state(), FocusState::Remote { .. }) {
            for event in adapters.input_capture.poll_input_events()? {
                let datagram = InputDatagram {
                    event,
                    sent_at_micros: now_micros(),
                };
                let packet = encode_datagram(&mut cipher, &datagram)?;
                udp_socket.send_to(&packet, peer_data_addr).await?;
            }
        }
    }
}

async fn run_client(config: Config, log_latency: bool) -> anyhow::Result<()> {
    let peer = config.first_peer();
    let mut adapters = build_platform_adapters();
    println!(
        "starting SideShift client; connecting control {} and listening data {}",
        peer.control_addr, config.local.data_bind
    );

    let mut stream = TcpStream::connect(&peer.control_addr)
        .await
        .with_context(|| format!("failed to connect to server {}", peer.control_addr))?;
    let (_hello, mut cipher) = complete_client_auth(
        &mut stream,
        config.local.shared_secret.as_bytes(),
        &config.local.machine_name,
    )
    .await?;
    println!("control connection established and authenticated");

    let server_hello = recv_control_message(&mut stream, &mut cipher).await?;
    println!("server info: {server_hello:?}");
    let udp_socket = UdpSocket::bind(&config.local.data_bind)
        .await
        .with_context(|| {
            format!(
                "failed to bind local data socket {}",
                config.local.data_bind
            )
        })?;
    println!("awaiting encrypted input datagrams...");

    let mut datagram_buffer = vec![0_u8; 4096];
    let mut latency_tracker = LatencyTracker::new(log_latency);
    let mut handoff_active = false;
    loop {
        tokio::select! {
            result = recv_control_message(&mut stream, &mut cipher) => {
                let message = result?;
                println!("control message: {message:?}");
                match message {
                    ControlMessage::HandoffStart { to_machine, .. } => {
                        if to_machine == config.local.machine_name {
                            handoff_active = true;
                        }
                    }
                    ControlMessage::HandoffEnd { owner_machine } => {
                        handoff_active = false;
                        println!("control returned to {owner_machine}");
                    }
                    _ => {}
                }
            }
            datagram = udp_socket.recv_from(&mut datagram_buffer) => {
                let (len, _) = datagram?;
                if len == 0 {
                    continue;
                }
                let payload = &datagram_buffer[..len];
                if let Ok(message) = decode_datagram(&mut cipher, payload) {
                    if !handoff_active {
                        continue;
                    }
                    latency_tracker.observe(message.sent_at_micros);
                    adapters.input_injector.inject_event(&message.event)?;
                }
            }
        }
    }
}

async fn complete_server_auth(
    stream: &mut TcpStream,
    secret: &[u8],
) -> anyhow::Result<(AuthHello, CipherState)> {
    let hello_bytes = read_frame(stream).await?;
    let hello: AuthHello = decode_plain(&hello_bytes)?;

    let mut rng = thread_rng();
    let challenge = build_server_challenge(secret, &hello, &mut rng);
    write_frame(stream, &encode_plain(&challenge)?).await?;

    let proof_bytes = read_frame(stream).await?;
    let proof: AuthProof = decode_plain(&proof_bytes)?;
    verify_client_proof(secret, &hello, &challenge, &proof)?;

    let session_key = derive_session_key(secret, &hello, &challenge)?;
    Ok((hello, CipherState::new(&session_key)))
}

async fn complete_client_auth(
    stream: &mut TcpStream,
    secret: &[u8],
    machine_name: &str,
) -> anyhow::Result<(AuthChallenge, CipherState)> {
    let mut rng = thread_rng();
    let hello = build_client_hello(machine_name, &mut rng);
    write_frame(stream, &encode_plain(&hello)?).await?;

    let challenge_bytes = read_frame(stream).await?;
    let challenge: AuthChallenge = decode_plain(&challenge_bytes)?;
    verify_server_challenge(secret, &hello, &challenge)?;
    let proof = build_client_proof(secret, &hello, &challenge);
    write_frame(stream, &encode_plain(&proof)?).await?;

    let session_key = derive_session_key(secret, &hello, &challenge)?;
    Ok((challenge, CipherState::new(&session_key)))
}

async fn send_control_message(
    stream: &mut TcpStream,
    cipher: &mut CipherState,
    message: &ControlMessage,
) -> anyhow::Result<()> {
    let payload = encode_control(cipher, message)?;
    write_frame(stream, &payload).await
}

async fn recv_control_message(
    stream: &mut TcpStream,
    cipher: &mut CipherState,
) -> anyhow::Result<ControlMessage> {
    let payload = read_frame(stream).await?;
    let message = decode_control(cipher, &payload)?;
    Ok(message)
}

async fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> anyhow::Result<()> {
    let len = payload.len() as u32;
    stream.write_u32(len).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn read_frame(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let len = stream.read_u32().await? as usize;
    if len > 1024 * 1024 {
        anyhow::bail!("refusing oversized frame ({len} bytes)");
    }
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

fn parse_port(bind_addr: &str) -> anyhow::Result<u16> {
    let (_, port_str) = bind_addr
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid bind address {bind_addr}"))?;
    Ok(port_str.parse::<u16>()?)
}

fn find_peer_for_client(config: &Config, client_ip: String) -> Option<PeerConfig> {
    config
        .peers
        .iter()
        .find(|peer| peer.control_addr.starts_with(client_ip.as_str()))
        .cloned()
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or(0)
}

struct LatencyTracker {
    enabled: bool,
    next_report_at_micros: u64,
    samples: u64,
    over_hard_max_samples: u64,
    total_micros: u128,
    max_micros: u64,
}

impl LatencyTracker {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            next_report_at_micros: now_micros().saturating_add(1_000_000),
            samples: 0,
            over_hard_max_samples: 0,
            total_micros: 0,
            max_micros: 0,
        }
    }

    fn observe(&mut self, sent_at_micros: u64) {
        if !self.enabled {
            return;
        }

        let now = now_micros();
        if now < sent_at_micros {
            return;
        }

        let one_way_micros = now - sent_at_micros;
        self.samples = self.samples.saturating_add(1);
        self.total_micros = self.total_micros.saturating_add(u128::from(one_way_micros));
        self.max_micros = self.max_micros.max(one_way_micros);
        if one_way_micros > 5_000 {
            self.over_hard_max_samples = self.over_hard_max_samples.saturating_add(1);
        }

        if now >= self.next_report_at_micros {
            self.report(now);
        }
    }

    fn report(&mut self, now: u64) {
        if self.samples == 0 {
            self.next_report_at_micros = now.saturating_add(1_000_000);
            return;
        }

        let avg_micros = (self.total_micros / u128::from(self.samples)) as f64;
        let avg_ms = avg_micros / 1000.0;
        let max_ms = self.max_micros as f64 / 1000.0;
        let hard_max_rate = (self.over_hard_max_samples as f64 / self.samples as f64) * 100.0;
        println!(
            "latency window: avg {:.3}ms, max {:.3}ms, over-5ms {:.2}% (clock-sync dependent)",
            avg_ms, max_ms, hard_max_rate
        );

        self.samples = 0;
        self.over_hard_max_samples = 0;
        self.total_micros = 0;
        self.max_micros = 0;
        self.next_report_at_micros = now.saturating_add(1_000_000);
    }
}
