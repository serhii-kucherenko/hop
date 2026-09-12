use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use hop_protocol::auth::{
    build_client_hello, build_client_proof, build_server_challenge, derive_session_key,
    verify_client_proof, verify_server_challenge, AuthChallenge, AuthHello, AuthProof,
};
use hop_protocol::control::{ControlMessage, NodeRole, ScreenSize as WireScreenSize};
use hop_protocol::crypto::CipherState;
use hop_protocol::datagram::{decode_datagram, encode_datagram, InputDatagram};
use hop_protocol::frame::{decode_control, decode_plain, encode_control, encode_plain};
use rand::thread_rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::{timeout, Duration, Instant, MissedTickBehavior};

use crate::config::Config;
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
    println!(
        "starting hop server on control {} (data {}), waiting for client...",
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

    loop {
        let (stream, addr) = listener.accept().await.context("failed to accept client")?;
        println!("control client connected from {addr}");
        if let Err(error) = run_server_session(
            &config,
            &mut adapters,
            local_screen,
            &udp_socket,
            stream,
            addr,
        )
        .await
        {
            eprintln!("server session ended: {error}");
        }
    }
}

async fn run_client(config: Config, log_latency: bool) -> anyhow::Result<()> {
    let peer = config.first_peer();
    let mut adapters = build_platform_adapters();
    println!(
        "starting hop client; connecting control {} and listening data {}",
        peer.control_addr, config.local.data_bind
    );

    let udp_port = parse_port(&config.local.data_bind)?;
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
    'reconnect: loop {
        let mut stream = match connect_client_control(&peer.control_addr).await {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("failed to connect control channel: {error}; retrying");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        let (_hello, mut cipher) = match complete_client_auth(
            &mut stream,
            config.local.shared_secret.as_bytes(),
            &config.local.machine_name,
            udp_port,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                eprintln!("control channel auth failed: {error}; retrying");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        println!("control connection established and authenticated");

        let server_hello = loop {
            match recv_control_message(&mut stream, &mut cipher).await {
                Ok(Some(message)) => break message,
                Ok(None) => continue,
                Err(error) => {
                    eprintln!(
                        "failed to receive server hello on control channel: {error}; reconnecting"
                    );
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue 'reconnect;
                }
            }
        };
        println!("server info: {server_hello:?}");

        let mut handoff_active = false;
        let mut reconnect_required = false;
        while !reconnect_required {
            tokio::select! {
                result = recv_control_message(&mut stream, &mut cipher) => {
                    let message = match result {
                        Ok(Some(message)) => message,
                        Ok(None) => continue,
                        Err(error) => {
                            eprintln!("control channel read failed: {error}; reconnecting");
                            handoff_active = false;
                            reconnect_required = true;
                            continue;
                        }
                    };
                    match message {
                        ControlMessage::HandoffStart {
                            from_machine,
                            to_machine,
                            edge,
                        } => {
                            let accepted = to_machine == config.local.machine_name;
                            let reason = if accepted {
                                None
                            } else {
                                Some(format!(
                                    "handoff target {} does not match {}",
                                    to_machine, config.local.machine_name
                                ))
                            };
                            let ack = ControlMessage::HandoffStartAck {
                                from_machine: config.local.machine_name.clone(),
                                to_machine: to_machine.clone(),
                                accepted,
                                reason: reason.clone(),
                            };
                            if let Err(error) = send_control_message(&mut stream, &mut cipher, &ack).await {
                                eprintln!("failed to send handoff ACK: {error}; reconnecting");
                                handoff_active = false;
                                reconnect_required = true;
                                continue;
                            }
                            if accepted {
                                handoff_active = true;
                                println!(
                                    "handoff begin: from={} to={} edge={:?}; enabling client injection",
                                    from_machine, to_machine, edge
                                );
                            } else {
                                handoff_active = false;
                                println!(
                                    "handoff start rejected: from={} to={} edge={:?}; reason={}",
                                    from_machine,
                                    to_machine,
                                    edge,
                                    reason.unwrap_or_else(|| "unknown".to_owned())
                                );
                            }
                        }
                        ControlMessage::HandoffEnd { owner_machine } => {
                            handoff_active = false;
                            println!("handoff end: owner={owner_machine}; disabling client injection");
                        }
                        ControlMessage::Ping { at_millis } => {
                            let pong = ControlMessage::Pong { at_millis };
                            if let Err(error) = send_control_message(&mut stream, &mut cipher, &pong).await {
                                eprintln!("failed to reply with pong: {error}; reconnecting");
                                handoff_active = false;
                                reconnect_required = true;
                            }
                        }
                        other => println!("control message: {other:?}"),
                    }
                }
                datagram = udp_socket.recv_from(&mut datagram_buffer) => {
                    let (len, _) = match datagram {
                        Ok(parts) => parts,
                        Err(error) => {
                            eprintln!("failed to receive input datagram: {error}");
                            continue;
                        }
                    };
                    if len == 0 {
                        continue;
                    }
                    let payload = &datagram_buffer[..len];
                    match decode_datagram(&mut cipher, payload) {
                        Ok(message) => {
                            if !handoff_active {
                                continue;
                            }
                            latency_tracker.observe(message.sent_at_micros);
                            match catch_unwind(AssertUnwindSafe(|| {
                                adapters.input_injector.inject_event(&message.event)
                            })) {
                                Ok(Ok(())) => {}
                                Ok(Err(error)) => {
                                    eprintln!("input injection failed; continuing: {error}");
                                }
                                Err(_) => {
                                    eprintln!("input injection panicked; continuing client loop");
                                }
                            }
                        }
                        Err(error) => {
                            eprintln!("dropping invalid input datagram: {error}");
                        }
                    }
                }
            }
        }

        eprintln!("reconnecting control channel to {}", peer.control_addr);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn run_server_session(
    config: &Config,
    adapters: &mut crate::platform::PlatformAdapters,
    local_screen: ScreenSize,
    udp_socket: &UdpSocket,
    mut stream: TcpStream,
    addr: SocketAddr,
) -> anyhow::Result<()> {
    let (hello, mut cipher) =
        complete_server_auth(&mut stream, config.local.shared_secret.as_bytes()).await?;
    println!("authenticated peer machine {}", hello.machine_name);
    let peer_data_addr = SocketAddr::new(addr.ip(), hello.udp_port);

    let dynamic_layout = SpatialLayout::new(
        config
            .peers
            .iter()
            .map(|peer| SpatialNeighbor {
                machine_name: hello.machine_name.clone(),
                position: peer.position,
            })
            .collect(),
    );

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

    let mut handoff = HandoffController::new(config.local.machine_name.clone());
    let mut handoff_committed = false;
    let mut cursor_hidden = false;
    let mut ticker = tokio::time::interval(Duration::from_millis(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let pending_events = adapters.input_capture.poll_input_events()?;

                if let Some(cursor) = adapters.input_capture.poll_cursor_position()? {
                    let action =
                        handoff.on_local_cursor(cursor, local_screen, &dynamic_layout, &pending_events);
                    if let HandoffAction::Begin {
                        target_machine,
                        edge,
                    } = action
                    {
                        let message = ControlMessage::HandoffStart {
                            from_machine: config.local.machine_name.clone(),
                            to_machine: target_machine.clone(),
                            edge,
                        };
                        if let Err(error) = send_control_message(&mut stream, &mut cipher, &message).await {
                            recover_server_focus_local(
                                &mut handoff,
                                adapters.cursor_controller.as_mut(),
                                &mut cursor_hidden,
                                &format!("failed to send handoff start: {error}"),
                            );
                            return Err(error);
                        }
                        println!(
                            "handoff begin requested: from={} to={} edge={:?}",
                            config.local.machine_name, target_machine, edge
                        );

                        let ack_result = await_handoff_start_ack(
                            &mut stream,
                            &mut cipher,
                            &target_machine,
                            Duration::from_millis(150),
                        )
                        .await;
                        let ack_status = match ack_result {
                            Ok(status) => status,
                            Err(error) => {
                                recover_server_focus_local(
                                    &mut handoff,
                                    adapters.cursor_controller.as_mut(),
                                    &mut cursor_hidden,
                                    &format!("failed while waiting for handoff ack: {error}"),
                                );
                                return Err(error);
                            }
                        };

                        match ack_status {
                            HandoffStartAckStatus::Accepted => {
                                if let Err(error) = adapters.cursor_controller.hide_cursor() {
                                    recover_server_focus_local(
                                        &mut handoff,
                                        adapters.cursor_controller.as_mut(),
                                        &mut cursor_hidden,
                                        &format!("failed to hide cursor after handoff ack: {error}"),
                                    );
                                    handoff_committed = false;
                                    send_handoff_end(&mut stream, &mut cipher, &config.local.machine_name).await;
                                    continue;
                                }
                                cursor_hidden = true;
                                if let Err(error) = adapters
                                    .cursor_controller
                                    .warp_cursor_to_safe_point(edge, local_screen)
                                {
                                    recover_server_focus_local(
                                        &mut handoff,
                                        adapters.cursor_controller.as_mut(),
                                        &mut cursor_hidden,
                                        &format!("failed to warp cursor after handoff ack: {error}"),
                                    );
                                    handoff_committed = false;
                                    send_handoff_end(&mut stream, &mut cipher, &config.local.machine_name).await;
                                    continue;
                                }
                                handoff_committed = true;
                                println!(
                                    "handoff begin confirmed: from={} to={} edge={:?}",
                                    config.local.machine_name, target_machine, edge
                                );
                            }
                            HandoffStartAckStatus::Rejected(reason) => {
                                let reason = reason.unwrap_or_else(|| "no reason provided".to_owned());
                                recover_server_focus_local(
                                    &mut handoff,
                                    adapters.cursor_controller.as_mut(),
                                    &mut cursor_hidden,
                                    &format!("handoff ack rejected: {reason}"),
                                );
                                handoff_committed = false;
                                send_handoff_end(&mut stream, &mut cipher, &config.local.machine_name).await;
                            }
                            HandoffStartAckStatus::TimedOut => {
                                recover_server_focus_local(
                                    &mut handoff,
                                    adapters.cursor_controller.as_mut(),
                                    &mut cursor_hidden,
                                    "handoff ack timed out",
                                );
                                handoff_committed = false;
                                send_handoff_end(&mut stream, &mut cipher, &config.local.machine_name).await;
                            }
                        }
                    }
                }

                if handoff_committed && matches!(handoff.focus_state(), FocusState::Remote { .. }) {
                    for event in pending_events {
                        let datagram = InputDatagram {
                            event,
                            sent_at_micros: now_micros(),
                        };
                        let packet = encode_datagram(&mut cipher, &datagram)?;
                        if let Err(error) = udp_socket.send_to(&packet, peer_data_addr).await {
                            recover_server_focus_local(
                                &mut handoff,
                                adapters.cursor_controller.as_mut(),
                                &mut cursor_hidden,
                                &format!("failed to forward input datagram: {error}"),
                            );
                            return Err(error.into());
                        }
                    }
                }
            }
            result = recv_control_message(&mut stream, &mut cipher) => {
                match result {
                    Ok(Some(ControlMessage::HandoffStartAck { from_machine, to_machine, accepted, reason })) => {
                        println!(
                            "late handoff ack received: from={} to={} accepted={} reason={:?}",
                            from_machine, to_machine, accepted, reason
                        );
                    }
                    Ok(Some(ControlMessage::HandoffEnd { owner_machine })) => {
                        if owner_machine == config.local.machine_name {
                            recover_server_focus_local(
                                &mut handoff,
                                adapters.cursor_controller.as_mut(),
                                &mut cursor_hidden,
                                "handoff ended by remote peer",
                            );
                            handoff_committed = false;
                        } else {
                            println!("handoff end ignored for owner {owner_machine}");
                        }
                    }
                    Ok(Some(ControlMessage::Ping { at_millis })) => {
                        let pong = ControlMessage::Pong { at_millis };
                        if let Err(error) = send_control_message(&mut stream, &mut cipher, &pong).await {
                            recover_server_focus_local(
                                &mut handoff,
                                adapters.cursor_controller.as_mut(),
                                &mut cursor_hidden,
                                &format!("failed to send pong response: {error}"),
                            );
                            return Err(error);
                        }
                    }
                    Ok(Some(other)) => println!("control message: {other:?}"),
                    Ok(None) => {}
                    Err(error) => {
                        recover_server_focus_local(
                            &mut handoff,
                            adapters.cursor_controller.as_mut(),
                            &mut cursor_hidden,
                            &format!("control client disconnected: {error}"),
                        );
                        return Err(error);
                    }
                }
            }
        }
    }
}

enum HandoffStartAckStatus {
    Accepted,
    Rejected(Option<String>),
    TimedOut,
}

async fn await_handoff_start_ack(
    stream: &mut TcpStream,
    cipher: &mut CipherState,
    target_machine: &str,
    timeout_duration: Duration,
) -> anyhow::Result<HandoffStartAckStatus> {
    let deadline = Instant::now() + timeout_duration;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(HandoffStartAckStatus::TimedOut);
        }
        let remaining = deadline.saturating_duration_since(now);
        let response = timeout(remaining, recv_control_message(stream, cipher)).await;
        let Some(message) = (match response {
            Ok(result) => result?,
            Err(_) => return Ok(HandoffStartAckStatus::TimedOut),
        }) else {
            continue;
        };

        match message {
            ControlMessage::HandoffStartAck {
                from_machine,
                to_machine,
                accepted,
                reason,
            } => {
                if to_machine != target_machine {
                    println!(
                        "ignoring handoff ack for target {} from {} while waiting for {}",
                        to_machine, from_machine, target_machine
                    );
                    continue;
                }
                if accepted {
                    return Ok(HandoffStartAckStatus::Accepted);
                }
                return Ok(HandoffStartAckStatus::Rejected(reason));
            }
            ControlMessage::Ping { at_millis } => {
                let pong = ControlMessage::Pong { at_millis };
                send_control_message(stream, cipher, &pong).await?;
            }
            other => {
                println!("control message while awaiting handoff ack: {other:?}");
            }
        }
    }
}

async fn connect_client_control(control_addr: &str) -> anyhow::Result<TcpStream> {
    let mut connected = None;
    let mut last_error = None;
    for attempt in 1..=20 {
        match TcpStream::connect(control_addr).await {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(error) => {
                last_error = Some(error);
                if attempt < 20 {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    }
    match connected {
        Some(stream) => Ok(stream),
        None => {
            let error = last_error
                .map(|error| error.to_string())
                .unwrap_or_default();
            anyhow::bail!("failed to connect to server {}: {}", control_addr, error);
        }
    }
}

async fn send_handoff_end(stream: &mut TcpStream, cipher: &mut CipherState, owner_machine: &str) {
    let message = ControlMessage::HandoffEnd {
        owner_machine: owner_machine.to_owned(),
    };
    if let Err(error) = send_control_message(stream, cipher, &message).await {
        eprintln!("failed to send handoff end message: {error}");
    }
}

fn recover_server_focus_local(
    handoff: &mut HandoffController,
    cursor_controller: &mut dyn crate::platform::CursorController,
    cursor_hidden: &mut bool,
    reason: &str,
) {
    let was_remote = matches!(handoff.focus_state(), FocusState::Remote { .. });
    handoff.force_local();
    if *cursor_hidden {
        if let Err(error) = cursor_controller.show_cursor() {
            eprintln!("failed to show cursor while recovering local focus: {error}");
        } else {
            *cursor_hidden = false;
        }
    }
    if was_remote {
        eprintln!("focus recovered to local: {reason}");
    } else {
        eprintln!("handoff aborted: {reason}");
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
    udp_port: u16,
) -> anyhow::Result<(AuthChallenge, CipherState)> {
    let mut rng = thread_rng();
    let hello = build_client_hello(machine_name, udp_port, &mut rng);
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
) -> anyhow::Result<Option<ControlMessage>> {
    let payload = read_frame(stream).await?;
    match decode_control(cipher, &payload) {
        Ok(message) => Ok(Some(message)),
        Err(error) => {
            eprintln!("dropping invalid control frame: {error}");
            Ok(None)
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{CursorPosition, RelativePosition};
    use crate::platform::CursorController;
    use hop_protocol::control::Edge;

    #[derive(Default)]
    struct TestCursorController {
        show_calls: usize,
    }

    impl CursorController for TestCursorController {
        fn hide_cursor(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn show_cursor(&mut self) -> anyhow::Result<()> {
            self.show_calls += 1;
            Ok(())
        }

        fn warp_cursor_to_safe_point(
            &mut self,
            _edge: Edge,
            _screen: ScreenSize,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn recover_server_focus_local_clears_remote_and_shows_cursor() {
        let mut handoff = HandoffController::new("macbook-pro");
        let screen = ScreenSize {
            width: 1920,
            height: 1080,
        };
        let layout = SpatialLayout::new(vec![SpatialNeighbor {
            machine_name: "windows-box".to_owned(),
            position: RelativePosition::Right,
        }]);
        let action =
            handoff.on_local_cursor(CursorPosition { x: 1921, y: 200 }, screen, &layout, &[]);
        assert!(matches!(action, HandoffAction::Begin { .. }));

        let mut cursor_controller = TestCursorController::default();
        let mut cursor_hidden = true;
        recover_server_focus_local(
            &mut handoff,
            &mut cursor_controller,
            &mut cursor_hidden,
            "control disconnect",
        );

        assert!(matches!(handoff.focus_state(), FocusState::Local));
        assert!(!cursor_hidden);
        assert_eq!(cursor_controller.show_calls, 1);
    }
}
