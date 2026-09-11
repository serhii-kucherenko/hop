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

pub fn encode_datagram(
    cipher: &mut CipherState,
    datagram: &InputDatagram,
) -> Result<Vec<u8>, DatagramError> {
    let encoded = bincode::serialize(datagram).map_err(|_| DatagramError::EncodeFailed)?;
    cipher
        .seal(ChannelKind::Datagram, &encoded)
        .map_err(Into::into)
}

pub fn decode_datagram(
    cipher: &mut CipherState,
    packet: &[u8],
) -> Result<InputDatagram, DatagramError> {
    let decoded = cipher.open(ChannelKind::Datagram, packet)?;
    bincode::deserialize(&decoded).map_err(|_| DatagramError::DecodeFailed)
}
