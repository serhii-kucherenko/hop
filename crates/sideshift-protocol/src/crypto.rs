use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Control = 0x4354_524c,  // CTRL
    Datagram = 0x4447_524d, // DGRM
}

#[derive(thiserror::Error, Debug)]
pub enum CryptoError {
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

        let mut packet = Vec::with_capacity(8 + plaintext.len() + 16);
        packet.extend_from_slice(&seq.to_be_bytes());
        packet.extend_from_slice(plaintext);

        let tag = self
            .cipher
            .encrypt_in_place_detached(&nonce, &aad, &mut packet[8..])
            .map_err(|_| CryptoError::EncryptFailed)?;
        packet.extend_from_slice(tag.as_slice());
        Ok(packet)
    }

    pub fn open(
        &mut self,
        channel: ChannelKind,
        packet_bytes: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        if packet_bytes.len() < 24 {
            return Err(CryptoError::PacketDecoding);
        }

        let seq_bytes: [u8; 8] = packet_bytes[..8]
            .try_into()
            .map_err(|_| CryptoError::PacketDecoding)?;
        let seq = u64::from_be_bytes(seq_bytes);
        if seq < self.min_recv_seq {
            return Err(CryptoError::ReplayDetected);
        }

        let encrypted_payload_end = packet_bytes.len() - 16;
        let mut payload = packet_bytes[8..encrypted_payload_end].to_vec();
        let tag = Tag::from_slice(&packet_bytes[encrypted_payload_end..]);

        let nonce = build_nonce(channel, seq);
        let aad = (channel as u32).to_be_bytes();
        self.cipher
            .decrypt_in_place_detached(&nonce, &aad, &mut payload, tag)
            .map_err(|_| CryptoError::DecryptFailed)?;

        self.min_recv_seq = seq.saturating_add(1);
        Ok(payload)
    }
}

fn build_nonce(channel: ChannelKind, seq: u64) -> Nonce {
    let mut bytes = [0_u8; 12];
    bytes[..4].copy_from_slice(&(channel as u32).to_be_bytes());
    bytes[4..].copy_from_slice(&seq.to_be_bytes());
    *Nonce::from_slice(&bytes)
}
