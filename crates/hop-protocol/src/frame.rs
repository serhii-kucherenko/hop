use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::control::ControlMessage;
use crate::crypto::{ChannelKind, CipherState, CryptoError};

#[derive(thiserror::Error, Debug)]
pub enum FrameError {
    #[error("failed to encode control frame")]
    EncodeFailed,
    #[error("failed to decode control frame")]
    DecodeFailed,
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

pub fn encode_plain<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    bincode::serialize(message).map_err(|_| FrameError::EncodeFailed)
}

pub fn decode_plain<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    bincode::deserialize(payload).map_err(|_| FrameError::DecodeFailed)
}

pub fn encode_control(
    cipher: &mut CipherState,
    message: &ControlMessage,
) -> Result<Vec<u8>, FrameError> {
    let payload = encode_plain(message)?;
    cipher
        .seal(ChannelKind::Control, &payload)
        .map_err(Into::into)
}

pub fn decode_control(
    cipher: &mut CipherState,
    encrypted_payload: &[u8],
) -> Result<ControlMessage, FrameError> {
    let payload = cipher.open(ChannelKind::Control, encrypted_payload)?;
    decode_plain(&payload)
}
