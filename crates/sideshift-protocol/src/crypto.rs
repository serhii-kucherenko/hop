use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Control = 0x4354_524c,  // CTRL
    Datagram = 0x4447_524d, // DGRM
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedPacket {
    pub seq: u64,
    pub ciphertext: Vec<u8>,
}

#[derive(thiserror::Error, Debug)]
pub enum CryptoError {
    #[error("failed to serialize encrypted packet")]
    PacketEncoding,
    #[error("failed to decode encrypted packet")]
    PacketDecoding,
    #[error("packet replay or out-of-order frame rejected")]
    ReplayDetected,
    #[error("encryption failure")]
    EncryptFailed,
    #[error("decryption failure")]
    DecryptFailed,
}

#[derive(Clone)]
pub struct CipherState {
    cipher: ChaCha20Poly1305,
    next_send_seq: u64,
    min_recv_seq: u64,
}

impl CipherState {
    pub fn new(session_key: &[u8; 32]) -> Self {
        let key = Key::from_slice(session_key);
        Self {
            cipher: ChaCha20Poly1305::new(key),
            next_send_seq: 0,
            min_recv_seq: 0,
        }
    }

    pub fn seal(&mut self, channel: ChannelKind, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let seq = self.next_send_seq;
        self.next_send_seq = self
            .next_send_seq
            .checked_add(1)
            .expect("sequence number overflow");
        let nonce = build_nonce(channel, seq);
        let aad = (channel as u32).to_be_bytes();
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::EncryptFailed)?;

        let packet = EncryptedPacket { seq, ciphertext };
        bincode::serialize(&packet).map_err(|_| CryptoError::PacketEncoding)
    }

    pub fn open(
        &mut self,
        channel: ChannelKind,
        packet_bytes: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let packet: EncryptedPacket =
            bincode::deserialize(packet_bytes).map_err(|_| CryptoError::PacketDecoding)?;
        if packet.seq < self.min_recv_seq {
            return Err(CryptoError::ReplayDetected);
        }
        self.min_recv_seq = packet.seq.saturating_add(1);

        let nonce = build_nonce(channel, packet.seq);
        let aad = (channel as u32).to_be_bytes();
        self.cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: packet.ciphertext.as_slice(),
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::DecryptFailed)
    }
}

fn build_nonce(channel: ChannelKind, seq: u64) -> Nonce {
    let mut bytes = [0_u8; 12];
    bytes[..4].copy_from_slice(&(channel as u32).to_be_bytes());
    bytes[4..].copy_from_slice(&seq.to_be_bytes());
    *Nonce::from_slice(&bytes)
}
