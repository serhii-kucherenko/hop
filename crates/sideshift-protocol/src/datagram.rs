use serde::{Deserialize, Serialize};

use crate::crypto::{ChannelKind, CipherState, CryptoError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove { dx: i16, dy: i16 },
    MouseButton { button: MouseButton, pressed: bool },
    Key { scancode: u16, pressed: bool },
    Scroll { dx: i16, dy: i16 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDatagram {
    pub event: InputEvent,
    pub sent_at_micros: u64,
}

#[derive(thiserror::Error, Debug)]
pub enum DatagramError {
    #[error("failed to encode datagram")]
    EncodeFailed,
    #[error("failed to decode datagram")]
    DecodeFailed,
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

const EVENT_MOUSE_MOVE: u8 = 0;
const EVENT_MOUSE_BUTTON: u8 = 1;
const EVENT_KEY: u8 = 2;
const EVENT_SCROLL: u8 = 3;

const DATAGRAM_TS_LEN: usize = 8;
const MAX_EVENT_LEN: usize = 5;
const MAX_DATAGRAM_PLAINTEXT_LEN: usize = DATAGRAM_TS_LEN + MAX_EVENT_LEN;

pub fn encode_datagram(
    cipher: &mut CipherState,
    datagram: &InputDatagram,
) -> Result<Vec<u8>, DatagramError> {
    let mut encoded = [0_u8; MAX_DATAGRAM_PLAINTEXT_LEN];
    encoded[..DATAGRAM_TS_LEN].copy_from_slice(&datagram.sent_at_micros.to_be_bytes());

    let event_len = encode_event(&datagram.event, &mut encoded[DATAGRAM_TS_LEN..])?;
    let total_len = DATAGRAM_TS_LEN + event_len;
    cipher
        .seal(ChannelKind::Datagram, &encoded[..total_len])
        .map_err(Into::into)
}

pub fn decode_datagram(
    cipher: &mut CipherState,
    packet: &[u8],
) -> Result<InputDatagram, DatagramError> {
    let decoded = cipher.open(ChannelKind::Datagram, packet)?;
    if decoded.len() < DATAGRAM_TS_LEN {
        return Err(DatagramError::DecodeFailed);
    }

    let ts_bytes: [u8; DATAGRAM_TS_LEN] = decoded[..DATAGRAM_TS_LEN]
        .try_into()
        .map_err(|_| DatagramError::DecodeFailed)?;
    let sent_at_micros = u64::from_be_bytes(ts_bytes);
    let event = decode_event(&decoded[DATAGRAM_TS_LEN..])?;
    Ok(InputDatagram {
        event,
        sent_at_micros,
    })
}

fn encode_event(event: &InputEvent, out: &mut [u8]) -> Result<usize, DatagramError> {
    match event {
        InputEvent::MouseMove { dx, dy } => {
            if out.len() < 5 {
                return Err(DatagramError::EncodeFailed);
            }
            out[0] = EVENT_MOUSE_MOVE;
            out[1..3].copy_from_slice(&dx.to_be_bytes());
            out[3..5].copy_from_slice(&dy.to_be_bytes());
            Ok(5)
        }
        InputEvent::MouseButton { button, pressed } => {
            if out.len() < 3 {
                return Err(DatagramError::EncodeFailed);
            }
            out[0] = EVENT_MOUSE_BUTTON;
            out[1] = match button {
                MouseButton::Left => 0,
                MouseButton::Right => 1,
                MouseButton::Middle => 2,
            };
            out[2] = u8::from(*pressed);
            Ok(3)
        }
        InputEvent::Key { scancode, pressed } => {
            if out.len() < 4 {
                return Err(DatagramError::EncodeFailed);
            }
            out[0] = EVENT_KEY;
            out[1..3].copy_from_slice(&scancode.to_be_bytes());
            out[3] = u8::from(*pressed);
            Ok(4)
        }
        InputEvent::Scroll { dx, dy } => {
            if out.len() < 5 {
                return Err(DatagramError::EncodeFailed);
            }
            out[0] = EVENT_SCROLL;
            out[1..3].copy_from_slice(&dx.to_be_bytes());
            out[3..5].copy_from_slice(&dy.to_be_bytes());
            Ok(5)
        }
    }
}

fn decode_event(raw: &[u8]) -> Result<InputEvent, DatagramError> {
    let Some(kind) = raw.first().copied() else {
        return Err(DatagramError::DecodeFailed);
    };

    match kind {
        EVENT_MOUSE_MOVE => {
            if raw.len() != 5 {
                return Err(DatagramError::DecodeFailed);
            }
            let dx = i16::from_be_bytes([raw[1], raw[2]]);
            let dy = i16::from_be_bytes([raw[3], raw[4]]);
            Ok(InputEvent::MouseMove { dx, dy })
        }
        EVENT_MOUSE_BUTTON => {
            if raw.len() != 3 {
                return Err(DatagramError::DecodeFailed);
            }
            let button = match raw[1] {
                0 => MouseButton::Left,
                1 => MouseButton::Right,
                2 => MouseButton::Middle,
                _ => return Err(DatagramError::DecodeFailed),
            };
            let pressed = match raw[2] {
                0 => false,
                1 => true,
                _ => return Err(DatagramError::DecodeFailed),
            };
            Ok(InputEvent::MouseButton { button, pressed })
        }
        EVENT_KEY => {
            if raw.len() != 4 {
                return Err(DatagramError::DecodeFailed);
            }
            let scancode = u16::from_be_bytes([raw[1], raw[2]]);
            let pressed = match raw[3] {
                0 => false,
                1 => true,
                _ => return Err(DatagramError::DecodeFailed),
            };
            Ok(InputEvent::Key { scancode, pressed })
        }
        EVENT_SCROLL => {
            if raw.len() != 5 {
                return Err(DatagramError::DecodeFailed);
            }
            let dx = i16::from_be_bytes([raw[1], raw[2]]);
            let dy = i16::from_be_bytes([raw[3], raw[4]]);
            Ok(InputEvent::Scroll { dx, dy })
        }
        _ => Err(DatagramError::DecodeFailed),
    }
}
