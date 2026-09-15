use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use hop_protocol::auth::{
    build_client_hello, build_client_proof, build_server_challenge, derive_session_key,
    verify_client_proof, verify_server_challenge, AuthChallenge, AuthHello, AuthProof,
};
use hop_protocol::control::{
    ControlMessage, Edge, HandoffTransport, NodeRole, ScreenSize as WireScreenSize,
};
use hop_protocol::crypto::CipherState;
use hop_protocol::datagram::{decode_datagram, encode_datagram, InputDatagram, InputEvent};
use hop_protocol::frame::{decode_control, decode_plain, encode_control, encode_plain};
use rand::thread_rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::{timeout, Duration, Instant, MissedTickBehavior};

use crate::clipboard::{ClipboardSync, CLIPBOARD_POLL_INTERVAL};
use crate::config::Config;
use crate::handoff::{FocusState, HandoffAction, HandoffController};
use crate::layout::{
    edge_for_peer_position, CursorPosition, ScreenBounds, SpatialLayout, SpatialNeighbor,
};
use crate::logi::LogiHandoff;
use crate::platform::{build_platform_adapters, CursorController, LocalInputCapture};

const STOPPED_MESSAGE: &str = "hop stopped; local input restored";
const MAX_CONTROL_FRAME_BYTES: usize = 32 * 1024 * 1024;

pub async fn run(
    config: Config,
    role_override: Option<NodeRole>,
    log_latency: bool,
    stop_signal_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let role = role_override.unwrap_or(config.local.role);
    match role {
        NodeRole::Server => run_server(config, stop_signal_path.as_deref()).await,
        NodeRole::Client => run_client(config, log_latency, stop_signal_path.as_deref()).await,
    }
}

async fn run_server(config: Config, stop_signal_path: Option<&Path>) -> anyhow::Result<()> {
    let mut adapters = build_platform_adapters();
    let mut logi_handoff = LogiHandoff::from_config(&config);
    let mut clipboard_sync = ClipboardSync::new(&config.local.machine_name);
    let local_screen = adapters
        .screen_provider
        .screen_bounds()
        .unwrap_or(ScreenBounds::from_size(
            config.local.screen_width,
            config.local.screen_height,
        ));
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
    let mut stop_ticker = tokio::time::interval(Duration::from_millis(200));
    stop_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let (stream, addr) = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("{STOPPED_MESSAGE}");
                return Ok(());
            }
            _ = stop_ticker.tick(), if stop_signal_path.is_some() => {
                if stop_requested(stop_signal_path) {
                    println!("{STOPPED_MESSAGE}");
                    return Ok(());
                }
                continue;
            }
            accepted = listener.accept() => accepted.context("failed to accept client")?,
        };
        println!("control client connected from {addr}");
        match run_server_session(
            &config,
            &mut adapters,
            &mut logi_handoff,
            &mut clipboard_sync,
            &udp_socket,
            stream,
            addr,
            ServerSessionParams {
                local_screen,
                stop_signal_path,
            },
        )
        .await
        {
            Ok(ServerSessionControl::StopRequested) => {
                println!("{STOPPED_MESSAGE}");
                return Ok(());
            }
            Err(error) => {
                eprintln!("server session ended: {error}");
            }
        }
    }
}

async fn run_client(
    config: Config,
    log_latency: bool,
    stop_signal_path: Option<&Path>,
) -> anyhow::Result<()> {
    let peer = config.first_peer();
    let mut adapters = build_platform_adapters();
    let logi_handoff = LogiHandoff::from_config(&config);
    let swap_ctrl_cmd = config
        .local
        .swap_ctrl_cmd
        .unwrap_or(cfg!(target_os = "macos"));
    adapters.input_injector.set_swap_ctrl_cmd(swap_ctrl_cmd);
    let mut clipboard_sync = ClipboardSync::new(&config.local.machine_name);
    let local_screen = adapters
        .screen_provider
        .screen_bounds()
        .unwrap_or(ScreenBounds::from_size(
            config.local.screen_width,
            config.local.screen_height,
        ));
    let return_edge = edge_for_peer_position(peer.position);
    let mut return_edge_detector = ReturnEdgeDetector::new(return_edge);
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
    let mut stop_ticker = tokio::time::interval(Duration::from_millis(200));
    stop_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    'reconnect: loop {
        let mut stream = match tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("{STOPPED_MESSAGE}");
                return Ok(());
            }
            _ = stop_ticker.tick(), if stop_signal_path.is_some() => {
                if stop_requested(stop_signal_path) {
                    println!("{STOPPED_MESSAGE}");
                    return Ok(());
                }
                continue;
            }
            result = connect_client_control(&peer.control_addr) => result
        } {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("failed to connect control channel: {error}; retrying");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        let (_hello, mut cipher) = match tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("{STOPPED_MESSAGE}");
                return Ok(());
            }
            result = complete_client_auth(
                &mut stream,
                config.local.shared_secret.as_bytes(),
                &config.local.machine_name,
                udp_port,
            ) => result
        } {
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
        clipboard_sync.on_control_channel_reset();

        let mut handoff_active = false;
        let mut owner_machine = peer.machine_name.clone();
        let mut active_transport = HandoffTransport::Network;
        let mut reconnect_required = false;
        let mut clipboard_ticker = tokio::time::interval(CLIPBOARD_POLL_INTERVAL);
        clipboard_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut session_stop_ticker = tokio::time::interval(Duration::from_millis(200));
        session_stop_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut logi_return_ticker = tokio::time::interval(Duration::from_millis(8));
        logi_return_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        while !reconnect_required {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    println!("{STOPPED_MESSAGE}");
                    return Ok(());
                }
                _ = session_stop_ticker.tick(), if stop_signal_path.is_some() => {
                    if stop_requested(stop_signal_path) {
                        println!("{STOPPED_MESSAGE}");
                        return Ok(());
                    }
                }
                result = recv_control_message(&mut stream, &mut cipher) => {
                    let message = match result {
                        Ok(Some(message)) => message,
                        Ok(None) => continue,
                        Err(error) => {
                            eprintln!("control channel read failed: {error}; reconnecting");
                            handoff_active = false;
                            active_transport = HandoffTransport::Network;
                            reconnect_required = true;
                            continue;
                        }
                    };
                    match message {
                        ControlMessage::HandoffStart {
                            from_machine,
                            to_machine,
                            edge,
                            transport,
                        } => {
                            let accepted = if to_machine != config.local.machine_name {
                                false
                            } else if matches!(transport, HandoffTransport::Logi) {
                                logi_handoff.can_accept_logi_from(&from_machine)
                            } else {
                                true
                            };
                            let reason = if accepted {
                                None
                            } else if to_machine != config.local.machine_name {
                                Some(format!(
                                    "handoff target {} does not match {}",
                                    to_machine, config.local.machine_name
                                ))
                            } else if matches!(transport, HandoffTransport::Logi) {
                                Some(format!(
                                    "logi handoff unavailable for owner {}",
                                    from_machine
                                ))
                            } else {
                                Some("handoff not accepted".to_owned())
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
                                active_transport = HandoffTransport::Network;
                                reconnect_required = true;
                                continue;
                            }
                            if accepted {
                                handoff_active = true;
                                active_transport = transport;
                                owner_machine = from_machine.clone();
                                return_edge_detector.reset();
                                let seed = seed_position_near_return_edge(return_edge, local_screen);
                                adapters.input_injector.seed_injected_cursor(seed);
                                if let Err(error) = adapters.cursor_controller.warp_cursor_to(seed) {
                                    eprintln!("failed to warp cursor on handoff start: {error}");
                                }
                                println!(
                                    "handoff begin: from={} to={} edge={:?} transport={:?}",
                                    from_machine, to_machine, edge, transport
                                );
                            } else {
                                handoff_active = false;
                                active_transport = HandoffTransport::Network;
                                return_edge_detector.reset();
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
                            active_transport = HandoffTransport::Network;
                            return_edge_detector.reset();
                            println!("handoff end: owner={owner_machine}; disabling client injection");
                        }
                        ControlMessage::Ping { at_millis } => {
                            let pong = ControlMessage::Pong { at_millis };
                            if let Err(error) = send_control_message(&mut stream, &mut cipher, &pong).await {
                                eprintln!("failed to reply with pong: {error}; reconnecting");
                                handoff_active = false;
                                active_transport = HandoffTransport::Network;
                                reconnect_required = true;
                            }
                        }
                        ControlMessage::ClipboardSync {
                            source_machine,
                            sequence,
                            sent_at_micros: _sent_at_micros,
                            content,
                        } => {
                            clipboard_sync.apply_remote_update(&source_machine, sequence, content);
                        }
                        other => println!("control message: {other:?}"),
                    }
                }
                _ = clipboard_ticker.tick() => {
                    if let Some(clipboard_message) = clipboard_sync.poll_local_update(handoff_active) {
                        if let Err(error) = send_control_message(&mut stream, &mut cipher, &clipboard_message).await {
                            eprintln!("failed to send clipboard sync: {error}; reconnecting");
                            reconnect_required = true;
                            handoff_active = false;
                            active_transport = HandoffTransport::Network;
                            continue;
                        }
                    }
                }
                _ = logi_return_ticker.tick(), if client_should_poll_local_return(handoff_active, active_transport) => {
                    let cursor = match resolve_client_return_cursor(
                        adapters.input_injector.as_ref(),
                        adapters.input_capture.as_mut(),
                    ) {
                        Some(cursor) => cursor,
                        None => continue,
                    };
                    let events = match adapters.input_capture.poll_input_events() {
                        Ok(events) => events,
                        Err(error) => {
                            eprintln!("failed to poll input events for logi return edge: {error}");
                            continue;
                        }
                    };
                    let mut should_release = false;
                    for event in &events {
                        if return_edge_detector.should_release(cursor, local_screen, event) {
                            should_release = true;
                            break;
                        }
                    }
                    if !should_release {
                        continue;
                    }

                    if matches!(active_transport, HandoffTransport::Logi) {
                        if let Err(error) = logi_handoff.switch_to_peer(&owner_machine) {
                            eprintln!(
                                "logi return switch failed for owner {}: {}",
                                owner_machine, error
                            );
                        }
                    }

                    let handoff_end = ControlMessage::HandoffEnd {
                        owner_machine: owner_machine.clone(),
                    };
                    match send_control_message(&mut stream, &mut cipher, &handoff_end).await {
                        Ok(()) => {
                            handoff_active = false;
                            let ended_transport = active_transport;
                            active_transport = HandoffTransport::Network;
                            return_edge_detector.reset();
                            println!(
                                "handoff end sent from client on {:?} return edge ({ended_transport:?})",
                                return_edge
                            );
                        }
                        Err(error) => {
                            eprintln!("failed to send handoff end: {error}; reconnecting");
                            handoff_active = false;
                            active_transport = HandoffTransport::Network;
                            reconnect_required = true;
                            continue;
                        }
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
                            // Inject for Network ownership and for Logi BLE-reconnect overlap.
                            if !matches!(
                                active_transport,
                                HandoffTransport::Network | HandoffTransport::Logi
                            ) {
                                continue;
                            }

                            let pre_cursor = resolve_client_return_cursor(
                                adapters.input_injector.as_ref(),
                                adapters.input_capture.as_mut(),
                            );

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

                            let post_cursor = adapters
                                .input_injector
                                .injected_cursor_position()
                                .or(pre_cursor);
                            if let Some(cursor) = post_cursor {
                                if return_edge_detector.should_release(
                                    cursor,
                                    local_screen,
                                    &message.event,
                                ) {
                                    let handoff_end = ControlMessage::HandoffEnd {
                                        owner_machine: owner_machine.clone(),
                                    };
                                    match send_control_message(&mut stream, &mut cipher, &handoff_end).await {
                                        Ok(()) => {
                                            handoff_active = false;
                                            active_transport = HandoffTransport::Network;
                                            return_edge_detector.reset();
                                            println!(
                                                "handoff end sent from client on {:?} return edge",
                                                return_edge
                                            );
                                            continue;
                                        }
                                        Err(error) => {
                                            eprintln!("failed to send handoff end: {error}; reconnecting");
                                            handoff_active = false;
                                            active_transport = HandoffTransport::Network;
                                            reconnect_required = true;
                                            continue;
                                        }
                                    }
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
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("{STOPPED_MESSAGE}");
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_server_session(
    config: &Config,
    adapters: &mut crate::platform::PlatformAdapters,
    logi_handoff: &mut LogiHandoff,
    clipboard_sync: &mut ClipboardSync,
    udp_socket: &UdpSocket,
    mut stream: TcpStream,
    addr: SocketAddr,
    session_params: ServerSessionParams<'_>,
) -> anyhow::Result<ServerSessionControl> {
    let local_screen = session_params.local_screen;
    let stop_signal_path = session_params.stop_signal_path;
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
    clipboard_sync.on_control_channel_reset();

    let mut handoff = HandoffController::new(config.local.machine_name.clone());
    let mut handoff_committed = false;
    let mut active_transport = HandoffTransport::Network;
    let mut cursor_hidden = false;
    let mut ticker = tokio::time::interval(Duration::from_millis(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut clipboard_ticker = tokio::time::interval(CLIPBOARD_POLL_INTERVAL);
    clipboard_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stop_ticker = tokio::time::interval(Duration::from_millis(200));
    stop_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                recover_server_focus_local(
                    &mut handoff,
                    adapters.input_capture.as_mut(),
                    adapters.cursor_controller.as_mut(),
                    &mut cursor_hidden,
                    local_screen,
                    "shutdown requested",
                );
                return Ok(ServerSessionControl::StopRequested);
            }
            _ = stop_ticker.tick(), if stop_signal_path.is_some() => {
                if stop_requested(stop_signal_path) {
                    recover_server_focus_local(
                        &mut handoff,
                        adapters.input_capture.as_mut(),
                        adapters.cursor_controller.as_mut(),
                        &mut cursor_hidden,
                        local_screen,
                        "stop requested by runtime signal",
                    );
                    return Ok(ServerSessionControl::StopRequested);
                }
            }
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
                        let requested_transport =
                            logi_handoff.preferred_transport_for_peer(&target_machine);
                        println!(
                            "handoff begin requested: from={} to={} edge={:?} transport={:?}",
                            config.local.machine_name, target_machine, edge, requested_transport
                        );

                        let mut committed_transport = requested_transport;
                        let mut ack_status = match request_handoff_start(
                            &mut stream,
                            &mut cipher,
                            &config.local.machine_name,
                            &target_machine,
                            edge,
                            requested_transport,
                        )
                        .await
                        {
                            Ok(status) => status,
                            Err(error) => {
                                recover_server_focus_local(
                                    &mut handoff,
                                    adapters.input_capture.as_mut(),
                                    adapters.cursor_controller.as_mut(),
                                    &mut cursor_hidden,
                                    local_screen,
                                    &format!("failed while requesting handoff start: {error}"),
                                );
                                return Err(error);
                            }
                        };

                        if matches!(requested_transport, HandoffTransport::Logi) {
                            let mut fallback_reason = None;
                            if matches!(ack_status, HandoffStartAckStatus::Accepted) {
                                let peer = target_machine.clone();
                                let switch_result = tokio::task::spawn_blocking({
                                    let logi = logi_handoff.clone();
                                    move || logi.switch_to_peer(&peer)
                                })
                                .await;
                                match switch_result {
                                    Ok(Ok(())) => {}
                                    Ok(Err(error)) => {
                                        fallback_reason = Some(format!(
                                            "logi switch failed: {error}; retrying with network transport"
                                        ));
                                    }
                                    Err(error) => {
                                        fallback_reason = Some(format!(
                                            "logi switch join failed: {error}; retrying with network transport"
                                        ));
                                    }
                                }
                            } else {
                                fallback_reason = Some(format!(
                                    "logi handoff not accepted ({ack_status:?}); retrying with network transport"
                                ));
                            }

                            if let Some(reason) = fallback_reason {
                                eprintln!("{reason}");
                                committed_transport = HandoffTransport::Network;
                                ack_status = match request_handoff_start(
                                    &mut stream,
                                    &mut cipher,
                                    &config.local.machine_name,
                                    &target_machine,
                                    edge,
                                    HandoffTransport::Network,
                                )
                                .await
                                {
                                    Ok(status) => status,
                                    Err(error) => {
                                        recover_server_focus_local(
                                            &mut handoff,
                                            adapters.input_capture.as_mut(),
                                            adapters.cursor_controller.as_mut(),
                                            &mut cursor_hidden,
                                            local_screen,
                                            &format!(
                                                "failed while requesting network fallback handoff: {error}"
                                            ),
                                        );
                                        return Err(error);
                                    }
                                };
                            }
                        }

                        match ack_status {
                            HandoffStartAckStatus::Accepted => {
                                if let Err(error) = adapters.input_capture.set_remote_focus(true) {
                                    recover_server_focus_local(
                                        &mut handoff,
                                        adapters.input_capture.as_mut(),
                                        adapters.cursor_controller.as_mut(),
                                        &mut cursor_hidden,
                                        local_screen,
                                        &format!("failed to enable remote input capture: {error}"),
                                    );
                                    handoff_committed = false;
                                    active_transport = HandoffTransport::Network;
                                    send_handoff_end(&mut stream, &mut cipher, &config.local.machine_name).await;
                                    continue;
                                }
                                if let Err(error) = adapters.cursor_controller.hide_cursor() {
                                    recover_server_focus_local(
                                        &mut handoff,
                                        adapters.input_capture.as_mut(),
                                        adapters.cursor_controller.as_mut(),
                                        &mut cursor_hidden,
                                        local_screen,
                                        &format!("failed to hide cursor after handoff ack: {error}"),
                                    );
                                    handoff_committed = false;
                                    active_transport = HandoffTransport::Network;
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
                                        adapters.input_capture.as_mut(),
                                        adapters.cursor_controller.as_mut(),
                                        &mut cursor_hidden,
                                        local_screen,
                                        &format!("failed to warp cursor after handoff ack: {error}"),
                                    );
                                    handoff_committed = false;
                                    active_transport = HandoffTransport::Network;
                                    send_handoff_end(&mut stream, &mut cipher, &config.local.machine_name).await;
                                    continue;
                                }
                                handoff_committed = true;
                                active_transport = committed_transport;
                                println!(
                                    "handoff begin confirmed: from={} to={} edge={:?} transport={:?}",
                                    config.local.machine_name, target_machine, edge, committed_transport
                                );
                            }
                            HandoffStartAckStatus::Rejected(reason) => {
                                let reason = reason.unwrap_or_else(|| "no reason provided".to_owned());
                                recover_server_focus_local(
                                    &mut handoff,
                                    adapters.input_capture.as_mut(),
                                    adapters.cursor_controller.as_mut(),
                                    &mut cursor_hidden,
                                    local_screen,
                                    &format!("handoff ack rejected: {reason}"),
                                );
                                handoff_committed = false;
                                active_transport = HandoffTransport::Network;
                                send_handoff_end(&mut stream, &mut cipher, &config.local.machine_name).await;
                            }
                            HandoffStartAckStatus::TimedOut => {
                                recover_server_focus_local(
                                    &mut handoff,
                                    adapters.input_capture.as_mut(),
                                    adapters.cursor_controller.as_mut(),
                                    &mut cursor_hidden,
                                    local_screen,
                                    "handoff ack timed out",
                                );
                                handoff_committed = false;
                                active_transport = HandoffTransport::Network;
                                send_handoff_end(&mut stream, &mut cipher, &config.local.machine_name).await;
                            }
                        }
                    }
                }

                // Network is the inject path; Logi also gets UDP overlap while Easy-Switch
                // devices reconnect (BLE often 2–8s).
                if handoff_committed
                    && matches!(handoff.focus_state(), FocusState::Remote { .. })
                    && matches!(
                        active_transport,
                        HandoffTransport::Network | HandoffTransport::Logi
                    )
                {
                    for event in pending_events {
                        let datagram = InputDatagram {
                            event,
                            sent_at_micros: now_micros(),
                        };
                        let packet = encode_datagram(&mut cipher, &datagram)?;
                        if let Err(error) = udp_socket.send_to(&packet, peer_data_addr).await {
                            recover_server_focus_local(
                                &mut handoff,
                                adapters.input_capture.as_mut(),
                                adapters.cursor_controller.as_mut(),
                                &mut cursor_hidden,
                                local_screen,
                                &format!("failed to forward input datagram: {error}"),
                            );
                            return Err(error.into());
                        }
                    }
                }
            }
            _ = clipboard_ticker.tick() => {
                let clipboard_enabled =
                    handoff_committed && matches!(handoff.focus_state(), FocusState::Remote { .. });
                if let Some(clipboard_message) = clipboard_sync.poll_local_update(clipboard_enabled) {
                    if let Err(error) = send_control_message(&mut stream, &mut cipher, &clipboard_message).await {
                        recover_server_focus_local(
                            &mut handoff,
                            adapters.input_capture.as_mut(),
                            adapters.cursor_controller.as_mut(),
                            &mut cursor_hidden,
                            local_screen,
                            &format!("failed to send clipboard sync: {error}"),
                        );
                        return Err(error);
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
                            if matches!(active_transport, HandoffTransport::Logi) {
                                let switch_result = tokio::task::spawn_blocking({
                                    let logi = logi_handoff.clone();
                                    move || logi.switch_back_local()
                                })
                                .await;
                                match switch_result {
                                    Ok(Ok(())) => {
                                        println!("logi switch back to local after remote return")
                                    }
                                    Ok(Err(error)) => eprintln!(
                                        "logi switch back to local after remote return failed: {error}"
                                    ),
                                    Err(error) => eprintln!(
                                        "logi switch back join failed after remote return: {error}"
                                    ),
                                }
                            }
                            recover_server_focus_local(
                                &mut handoff,
                                adapters.input_capture.as_mut(),
                                adapters.cursor_controller.as_mut(),
                                &mut cursor_hidden,
                                local_screen,
                                "handoff ended by remote peer",
                            );
                            handoff_committed = false;
                            active_transport = HandoffTransport::Network;
                        } else {
                            println!("handoff end ignored for owner {owner_machine}");
                        }
                    }
                    Ok(Some(ControlMessage::Ping { at_millis })) => {
                        let pong = ControlMessage::Pong { at_millis };
                        if let Err(error) = send_control_message(&mut stream, &mut cipher, &pong).await {
                            recover_server_focus_local(
                                &mut handoff,
                                adapters.input_capture.as_mut(),
                                adapters.cursor_controller.as_mut(),
                                &mut cursor_hidden,
                                local_screen,
                                &format!("failed to send pong response: {error}"),
                            );
                            return Err(error);
                        }
                    }
                    Ok(Some(ControlMessage::ClipboardSync {
                        source_machine,
                        sequence,
                        sent_at_micros: _sent_at_micros,
                        content,
                    })) => {
                        clipboard_sync.apply_remote_update(&source_machine, sequence, content);
                    }
                    Ok(Some(other)) => println!("control message: {other:?}"),
                    Ok(None) => {}
                    Err(error) => {
                        if matches!(active_transport, HandoffTransport::Logi) {
                            let switch_result = tokio::task::spawn_blocking({
                                let logi = logi_handoff.clone();
                                move || logi.switch_back_local()
                            })
                            .await;
                            match switch_result {
                                Ok(Ok(())) => {
                                    println!("logi switch back to local after control disconnect")
                                }
                                Ok(Err(switch_error)) => eprintln!(
                                    "logi switch back after disconnect failed: {switch_error}"
                                ),
                                Err(join_error) => eprintln!(
                                    "logi switch back join failed after disconnect: {join_error}"
                                ),
                            }
                        }
                        recover_server_focus_local(
                            &mut handoff,
                            adapters.input_capture.as_mut(),
                            adapters.cursor_controller.as_mut(),
                            &mut cursor_hidden,
                            local_screen,
                            &format!("control client disconnected: {error}"),
                        );
                        return Err(error);
                    }
                }
            }
        }
    }
}

enum ServerSessionControl {
    StopRequested,
}

struct ServerSessionParams<'a> {
    local_screen: ScreenBounds,
    stop_signal_path: Option<&'a Path>,
}

#[derive(Debug)]
enum HandoffStartAckStatus {
    Accepted,
    Rejected(Option<String>),
    TimedOut,
}

async fn request_handoff_start(
    stream: &mut TcpStream,
    cipher: &mut CipherState,
    from_machine: &str,
    target_machine: &str,
    edge: Edge,
    transport: HandoffTransport,
) -> anyhow::Result<HandoffStartAckStatus> {
    let message = ControlMessage::HandoffStart {
        from_machine: from_machine.to_owned(),
        to_machine: target_machine.to_owned(),
        edge,
        transport,
    };
    send_control_message(stream, cipher, &message).await?;
    await_handoff_start_ack(stream, cipher, target_machine, Duration::from_millis(150)).await
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
    input_capture: &mut dyn LocalInputCapture,
    cursor_controller: &mut dyn CursorController,
    cursor_hidden: &mut bool,
    screen: ScreenBounds,
    reason: &str,
) {
    let release_edge = match handoff.focus_state() {
        FocusState::Remote { edge, .. } => Some(*edge),
        FocusState::Local => None,
    };
    let was_remote = release_edge.is_some();
    handoff.force_local();
    if let Err(error) = input_capture.set_remote_focus(false) {
        eprintln!("failed to restore local input ownership: {error}");
    }
    if *cursor_hidden {
        if let Err(error) = cursor_controller.show_cursor() {
            eprintln!("failed to show cursor while recovering local focus: {error}");
        } else {
            *cursor_hidden = false;
        }
    }
    if let Some(edge) = release_edge {
        if let Err(error) = cursor_controller.warp_cursor_to_safe_point(opposite_edge(edge), screen)
        {
            eprintln!("failed to warp cursor while recovering local focus: {error}");
        }
    }
    if was_remote {
        eprintln!("focus recovered to local: {reason}");
    } else {
        eprintln!("handoff aborted: {reason}");
    }
}

fn opposite_edge(edge: Edge) -> Edge {
    match edge {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
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
    if len > MAX_CONTROL_FRAME_BYTES {
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

fn stop_requested(stop_signal_path: Option<&Path>) -> bool {
    stop_signal_path.map(|path| path.exists()).unwrap_or(false)
}

fn client_should_poll_local_return(handoff_active: bool, _transport: HandoffTransport) -> bool {
    // Network return used to be UDP-only; local trackpad/mouse must also be able to release.
    handoff_active
}

const STICKY_RETURN_DISTANCE_THRESHOLD: i32 = 6;

struct ReturnEdgeDetector {
    edge: Edge,
    outbound_distance: i32,
}

impl ReturnEdgeDetector {
    fn new(edge: Edge) -> Self {
        Self {
            edge,
            outbound_distance: 0,
        }
    }

    fn reset(&mut self) {
        self.outbound_distance = 0;
    }

    fn should_release(
        &mut self,
        cursor: CursorPosition,
        screen: ScreenBounds,
        event: &InputEvent,
    ) -> bool {
        let InputEvent::MouseMove { dx, dy } = event else {
            return false;
        };

        let outbound = match self.edge {
            Edge::Left => i32::from((-*dx).max(0)),
            Edge::Right => i32::from((*dx).max(0)),
            Edge::Top => i32::from((-*dy).max(0)),
            Edge::Bottom => i32::from((*dy).max(0)),
        };
        // Always use the virtual-desktop union edge (`screen`), not per-display edges.
        // Peer-facing return is the outer union boundary (e.g. Dell left at origin_x=-3440),
        // not MacBook's x=0 which is an interior boundary when another display sits to the left.
        if outbound == 0 || !cursor_is_on_edge(cursor, screen, self.edge) {
            self.reset();
            return false;
        }

        self.outbound_distance = self.outbound_distance.saturating_add(outbound);
        if self.outbound_distance < STICKY_RETURN_DISTANCE_THRESHOLD {
            return false;
        }

        self.reset();
        true
    }
}

fn seed_position_near_return_edge(edge: Edge, screen: ScreenBounds) -> CursorPosition {
    const INSET: i32 = 80;
    match edge {
        Edge::Left => CursorPosition {
            x: screen.origin_x + INSET,
            y: screen.origin_y + (screen.height as i32) / 2,
        },
        Edge::Right => CursorPosition {
            x: screen.max_x() - INSET,
            y: screen.origin_y + (screen.height as i32) / 2,
        },
        Edge::Top => CursorPosition {
            x: screen.origin_x + (screen.width as i32) / 2,
            y: screen.origin_y + INSET,
        },
        Edge::Bottom => CursorPosition {
            x: screen.origin_x + (screen.width as i32) / 2,
            y: screen.max_y() - INSET,
        },
    }
}

fn resolve_client_return_cursor(
    injector: &dyn crate::platform::RemoteInputInjector,
    capture: &mut dyn LocalInputCapture,
) -> Option<CursorPosition> {
    if let Some(cursor) = injector.injected_cursor_position() {
        return Some(cursor);
    }
    match capture.poll_cursor_position() {
        Ok(cursor) => cursor,
        Err(error) => {
            eprintln!("failed to poll cursor for return edge: {error}");
            None
        }
    }
}

fn cursor_is_on_edge(cursor: CursorPosition, screen: ScreenBounds, edge: Edge) -> bool {
    match edge {
        Edge::Left => cursor.x <= screen.origin_x,
        Edge::Right => cursor.x >= screen.max_x(),
        Edge::Top => cursor.y <= screen.origin_y,
        Edge::Bottom => cursor.y >= screen.max_y(),
    }
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
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::layout::{CursorPosition, RelativePosition};
    use crate::platform::{CursorController, LocalInputCapture};
    use hop_protocol::control::Edge;
    use hop_protocol::datagram::InputEvent;

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
            _screen: ScreenBounds,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct TestInputCapture {
        remote_focus_updates: Arc<Mutex<Vec<bool>>>,
    }

    impl LocalInputCapture for TestInputCapture {
        fn poll_cursor_position(&mut self) -> anyhow::Result<Option<CursorPosition>> {
            Ok(None)
        }

        fn poll_input_events(&mut self) -> anyhow::Result<Vec<InputEvent>> {
            Ok(Vec::new())
        }

        fn set_remote_focus(&mut self, active: bool) -> anyhow::Result<()> {
            self.remote_focus_updates
                .lock()
                .expect("lock focus updates")
                .push(active);
            Ok(())
        }
    }

    #[test]
    fn recover_server_focus_local_clears_remote_and_shows_cursor() {
        let mut handoff = HandoffController::new("macbook-pro");
        let screen = ScreenBounds::from_size(1920, 1080);
        let layout = SpatialLayout::new(vec![SpatialNeighbor {
            machine_name: "windows-box".to_owned(),
            position: RelativePosition::Right,
        }]);
        let action =
            handoff.on_local_cursor(CursorPosition { x: 1921, y: 200 }, screen, &layout, &[]);
        assert!(matches!(action, HandoffAction::Begin { .. }));

        let mut input_capture = TestInputCapture::default();
        let mut cursor_controller = TestCursorController::default();
        let mut cursor_hidden = true;
        recover_server_focus_local(
            &mut handoff,
            &mut input_capture,
            &mut cursor_controller,
            &mut cursor_hidden,
            screen,
            "control disconnect",
        );

        assert!(matches!(handoff.focus_state(), FocusState::Local));
        assert!(!cursor_hidden);
        assert_eq!(cursor_controller.show_calls, 1);
        let focus_updates = input_capture
            .remote_focus_updates
            .lock()
            .expect("lock focus updates")
            .clone();
        assert_eq!(focus_updates, vec![false]);
    }

    #[test]
    fn return_edge_detector_requires_edge_and_outbound_push_for_all_edges() {
        let screen = ScreenBounds::from_size(1920, 1080);
        let scenarios = [
            (
                Edge::Left,
                CursorPosition { x: 1, y: 500 },
                CursorPosition { x: 0, y: 500 },
                InputEvent::MouseMove { dx: 3, dy: 0 },
                InputEvent::MouseMove { dx: -3, dy: 0 },
            ),
            (
                Edge::Right,
                CursorPosition { x: 1918, y: 500 },
                CursorPosition { x: 1919, y: 500 },
                InputEvent::MouseMove { dx: -3, dy: 0 },
                InputEvent::MouseMove { dx: 3, dy: 0 },
            ),
            (
                Edge::Top,
                CursorPosition { x: 500, y: 1 },
                CursorPosition { x: 500, y: 0 },
                InputEvent::MouseMove { dx: 0, dy: 3 },
                InputEvent::MouseMove { dx: 0, dy: -3 },
            ),
            (
                Edge::Bottom,
                CursorPosition { x: 500, y: 1078 },
                CursorPosition { x: 500, y: 1079 },
                InputEvent::MouseMove { dx: 0, dy: -3 },
                InputEvent::MouseMove { dx: 0, dy: 3 },
            ),
        ];

        for (edge, off_edge, on_edge, inbound, outbound) in scenarios {
            let mut detector = ReturnEdgeDetector::new(edge);
            assert!(!detector.should_release(off_edge, screen, &outbound));
            assert!(!detector.should_release(on_edge, screen, &inbound));
            assert!(!detector.should_release(on_edge, screen, &outbound));
            assert!(detector.should_release(on_edge, screen, &outbound));
        }
    }

    #[test]
    fn return_edge_detector_respects_virtual_desktop_origin() {
        let screen = ScreenBounds {
            origin_x: -1600,
            origin_y: -200,
            width: 3200,
            height: 1400,
        };
        let mut detector = ReturnEdgeDetector::new(Edge::Left);
        assert!(!detector.should_release(
            CursorPosition {
                x: screen.origin_x + 5,
                y: 200
            },
            screen,
            &InputEvent::MouseMove { dx: -4, dy: 0 }
        ));
        assert!(!detector.should_release(
            CursorPosition {
                x: screen.origin_x,
                y: 200
            },
            screen,
            &InputEvent::MouseMove { dx: -2, dy: 0 }
        ));
        assert!(detector.should_release(
            CursorPosition {
                x: screen.origin_x,
                y: 200
            },
            screen,
            &InputEvent::MouseMove { dx: -4, dy: 0 }
        ));
    }

    #[test]
    fn return_edge_fires_on_macbook_left_with_dell_to_the_left() {
        // Peer-facing outer edge is the union left (-3440), not MacBook's x=0.
        // With Dell left of MacBook, macOS never sticky-pushes at x=0 (cursor enters Dell).
        let union = ScreenBounds {
            origin_x: -3440,
            origin_y: -458,
            width: 5240,
            height: 1627,
        };
        let mut detector = ReturnEdgeDetector::new(Edge::Left);
        let outbound = InputEvent::MouseMove { dx: -4, dy: 0 };
        // Interior MacBook left (x=0) must NOT fire return.
        assert!(!detector.should_release(CursorPosition { x: 0, y: 500 }, union, &outbound));
        assert!(!detector.should_release(CursorPosition { x: 0, y: 500 }, union, &outbound));
        // Union outer left fires after sticky threshold.
        assert!(!detector.should_release(
            CursorPosition {
                x: union.origin_x,
                y: 500
            },
            union,
            &outbound
        ));
        assert!(detector.should_release(
            CursorPosition {
                x: union.origin_x,
                y: 500
            },
            union,
            &outbound
        ));
    }

    #[test]
    fn return_edge_accumulates_outbound_from_just_inside_union_left() {
        let screen = ScreenBounds {
            origin_x: -3440,
            origin_y: -458,
            width: 5240,
            height: 1627,
        };
        let mut detector = ReturnEdgeDetector::new(Edge::Left);
        // Just on the union left edge (interior boundary of the virtual desktop).
        let cursor = CursorPosition {
            x: screen.origin_x,
            y: screen.origin_y + (screen.height as i32) / 2,
        };
        assert!(!detector.should_release(
            cursor,
            screen,
            &InputEvent::MouseMove { dx: -3, dy: 0 }
        ));
        assert!(!detector.should_release(
            cursor,
            screen,
            &InputEvent::MouseMove { dx: -2, dy: 0 }
        ));
        assert!(detector.should_release(
            cursor,
            screen,
            &InputEvent::MouseMove { dx: -2, dy: 0 }
        ));
    }

    #[test]
    fn network_and_logi_handoff_enable_local_return_polling() {
        assert!(
            client_should_poll_local_return(true, HandoffTransport::Network),
            "Network handoff must poll local cursor/trackpad for return edge (not UDP-only)"
        );
        assert!(client_should_poll_local_return(
            true,
            HandoffTransport::Logi
        ));
        assert!(!client_should_poll_local_return(
            false,
            HandoffTransport::Network
        ));
        assert!(!client_should_poll_local_return(
            false,
            HandoffTransport::Logi
        ));
    }

    #[tokio::test]
    async fn handoff_ack_times_out_when_client_does_not_ack() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("listener addr");
        let client_task =
            tokio::spawn(async move { TcpStream::connect(addr).await.expect("connect") });
        let (mut server_stream, _) = listener.accept().await.expect("accept");
        let _client_stream = client_task.await.expect("join");

        let mut server_cipher = CipherState::new(&[7_u8; 32]);
        let status = await_handoff_start_ack(
            &mut server_stream,
            &mut server_cipher,
            "macbook-pro",
            Duration::from_millis(30),
        )
        .await
        .expect("await handoff ack");
        assert!(matches!(status, HandoffStartAckStatus::TimedOut));
    }

    #[tokio::test]
    async fn handoff_ack_ignores_other_target_then_accepts_matching_ack() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("listener addr");
        let client_task =
            tokio::spawn(async move { TcpStream::connect(addr).await.expect("connect") });
        let (mut server_stream, _) = listener.accept().await.expect("accept");
        let mut client_stream = client_task.await.expect("join");

        let mut client_cipher = CipherState::new(&[9_u8; 32]);
        send_control_message(
            &mut client_stream,
            &mut client_cipher,
            &ControlMessage::HandoffStartAck {
                from_machine: "macbook-pro".to_owned(),
                to_machine: "other-target".to_owned(),
                accepted: true,
                reason: None,
            },
        )
        .await
        .expect("send wrong-target ack");
        send_control_message(
            &mut client_stream,
            &mut client_cipher,
            &ControlMessage::HandoffStartAck {
                from_machine: "macbook-pro".to_owned(),
                to_machine: "macbook-pro".to_owned(),
                accepted: true,
                reason: None,
            },
        )
        .await
        .expect("send matching-target ack");

        let mut server_cipher = CipherState::new(&[9_u8; 32]);
        let status = await_handoff_start_ack(
            &mut server_stream,
            &mut server_cipher,
            "macbook-pro",
            Duration::from_millis(100),
        )
        .await
        .expect("await handoff ack");
        assert!(matches!(status, HandoffStartAckStatus::Accepted));
    }
}
