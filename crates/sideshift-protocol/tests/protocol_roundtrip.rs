use rand::rngs::StdRng;
use rand::SeedableRng;
use sideshift_protocol::auth::{
    build_client_hello, build_client_proof, build_server_challenge, derive_session_key,
    verify_client_proof, verify_server_challenge,
};
use sideshift_protocol::control::{ControlMessage, Edge, NodeRole, ScreenSize};
use sideshift_protocol::crypto::CipherState;
use sideshift_protocol::datagram::{decode_datagram, encode_datagram, InputDatagram, InputEvent};
use sideshift_protocol::frame::{decode_control, encode_control};

const SECRET: &[u8] = b"example-shared-secret";

#[test]
fn handshake_and_key_derivation_are_consistent() {
    let mut rng = StdRng::seed_from_u64(42);
    let hello = build_client_hello("macbook-pro", &mut rng);
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
