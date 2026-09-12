use hop_protocol::auth::{
    build_client_hello, build_client_proof, build_server_challenge, derive_session_key,
    verify_client_proof, verify_server_challenge,
};
use hop_protocol::control::{ControlMessage, Edge, NodeRole, ScreenSize};
use hop_protocol::crypto::CipherState;
use hop_protocol::datagram::{decode_datagram, encode_datagram, InputDatagram, InputEvent};
use hop_protocol::frame::{decode_control, encode_control};
use rand::rngs::StdRng;
use rand::SeedableRng;

const SECRET: &[u8] = b"example-shared-secret";

#[test]
fn handshake_and_key_derivation_are_consistent() {
    let mut rng = StdRng::seed_from_u64(42);
    let hello = build_client_hello("macbook-pro", 4601, &mut rng);
    let challenge = build_server_challenge(SECRET, &hello, &mut rng);

    verify_server_challenge(SECRET, &hello, &challenge).expect("challenge should verify");

    let proof = build_client_proof(SECRET, &hello, &challenge);
    verify_client_proof(SECRET, &hello, &challenge, &proof).expect("proof should verify");

    let client_key = derive_session_key(SECRET, &hello, &challenge).expect("client key");
    let server_key = derive_session_key(SECRET, &hello, &challenge).expect("server key");
    assert_eq!(client_key, server_key);
}

#[test]
fn control_message_roundtrip_through_cipher() {
    let key = [7_u8; 32];
    let mut sender = CipherState::new(&key);
    let mut receiver = CipherState::new(&key);
    let message = ControlMessage::HandoffStart {
        from_machine: "macbook-pro".to_owned(),
        to_machine: "win11".to_owned(),
        edge: Edge::Right,
    };

    let packet = encode_control(&mut sender, &message).expect("encode control");
    let decoded = decode_control(&mut receiver, &packet).expect("decode control");

    assert_eq!(decoded, message);
}

#[test]
fn datagram_roundtrip_through_cipher() {
    let key = [2_u8; 32];
    let mut sender = CipherState::new(&key);
    let mut receiver = CipherState::new(&key);

    let input = InputDatagram {
        event: InputEvent::MouseMove { dx: 12, dy: -4 },
        sent_at_micros: 10,
    };

    let encrypted = encode_datagram(&mut sender, &input).expect("encode datagram");
    let decoded = decode_datagram(&mut receiver, &encrypted).expect("decode datagram");
    assert_eq!(decoded, input);
}

#[test]
fn replayed_packet_is_rejected() {
    let key = [5_u8; 32];
    let mut sender = CipherState::new(&key);
    let mut receiver = CipherState::new(&key);
    let message = ControlMessage::Hello {
        machine_name: "windows".to_owned(),
        role: NodeRole::Client,
        udp_port: 5001,
        screen: ScreenSize {
            width: 1920,
            height: 1080,
        },
    };

    let packet = encode_control(&mut sender, &message).expect("encoded");
    let first = decode_control(&mut receiver, &packet).expect("first decode works");
    assert_eq!(first, message);
    let second = decode_control(&mut receiver, &packet);
    assert!(second.is_err(), "same packet should be rejected");
}

#[test]
fn out_of_order_datagrams_within_window_are_accepted_once() {
    let key = [4_u8; 32];
    let mut sender = CipherState::new(&key);
    let mut receiver = CipherState::new(&key);

    let first = InputDatagram {
        event: InputEvent::MouseMove { dx: 1, dy: 1 },
        sent_at_micros: 1,
    };
    let second = InputDatagram {
        event: InputEvent::MouseMove { dx: 2, dy: 2 },
        sent_at_micros: 2,
    };
    let third = InputDatagram {
        event: InputEvent::MouseMove { dx: 3, dy: 3 },
        sent_at_micros: 3,
    };

    let first_packet = encode_datagram(&mut sender, &first).expect("first datagram");
    let second_packet = encode_datagram(&mut sender, &second).expect("second datagram");
    let third_packet = encode_datagram(&mut sender, &third).expect("third datagram");

    assert_eq!(
        decode_datagram(&mut receiver, &first_packet).expect("first decode"),
        first
    );
    assert_eq!(
        decode_datagram(&mut receiver, &third_packet).expect("third decode"),
        third
    );
    assert_eq!(
        decode_datagram(&mut receiver, &second_packet).expect("second decode out of order"),
        second
    );

    assert!(
        decode_datagram(&mut receiver, &second_packet).is_err(),
        "duplicate datagram should still be rejected"
    );
}

#[test]
fn control_frame_is_not_rejected_after_datagram_burst() {
    let key = [6_u8; 32];
    let mut sender = CipherState::new(&key);
    let mut receiver = CipherState::new(&key);

    let control = ControlMessage::HandoffStart {
        from_machine: "macbook-pro".to_owned(),
        to_machine: "nucbox".to_owned(),
        edge: Edge::Right,
    };
    let control_packet = encode_control(&mut sender, &control).expect("control frame");

    for sent_at in 0_u64..16 {
        let datagram = InputDatagram {
            event: InputEvent::MouseMove { dx: 4, dy: -1 },
            sent_at_micros: sent_at,
        };
        let packet = encode_datagram(&mut sender, &datagram).expect("encode datagram");
        let decoded = decode_datagram(&mut receiver, &packet).expect("decode datagram");
        assert_eq!(decoded, datagram);
    }

    let decoded_control = decode_control(&mut receiver, &control_packet).expect("decode control");
    assert_eq!(decoded_control, control);
}

#[test]
fn input_frames_stay_small_on_hot_path() {
    let key = [9_u8; 32];
    let mut sender = CipherState::new(&key);
    let datagram = InputDatagram {
        event: InputEvent::MouseMove { dx: 3, dy: -2 },
        sent_at_micros: 50,
    };
    let encrypted = encode_datagram(&mut sender, &datagram).expect("encode datagram");
    assert!(
        encrypted.len() <= 48,
        "mousemove frame should stay compact, got {} bytes",
        encrypted.len()
    );
}
