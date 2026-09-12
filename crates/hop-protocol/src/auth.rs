use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::PROTOCOL_VERSION;

type HmacSha256 = Hmac<Sha256>;

const AUTH_LABEL: &[u8] = b"hop-auth-v1";
const PROOF_LABEL: &[u8] = b"hop-proof-v1";
const SESSION_LABEL: &[u8] = b"hop-session-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthHello {
    pub machine_name: String,
    pub protocol_version: u16,
    pub client_nonce: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthChallenge {
    pub server_nonce: [u8; 16],
    pub mac: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthProof {
    pub mac: [u8; 32],
}

#[derive(thiserror::Error, Debug)]
pub enum AuthError {
    #[error("protocol version mismatch")]
    ProtocolVersionMismatch,
    #[error("invalid message authentication code")]
    InvalidMac,
    #[error("key derivation failed")]
    KeyDerivationFailed,
}

pub fn build_client_hello(machine_name: &str, rng: &mut impl Rng) -> AuthHello {
    let mut nonce = [0_u8; 16];
    rng.fill(&mut nonce);
    AuthHello {
        machine_name: machine_name.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        client_nonce: nonce,
    }
}

pub fn build_server_challenge(
    secret: &[u8],
    hello: &AuthHello,
    rng: &mut impl Rng,
) -> AuthChallenge {
    let mut server_nonce = [0_u8; 16];
    rng.fill(&mut server_nonce);
    let mac = compute_mac(
        secret,
        AUTH_LABEL,
        &[
            hello.machine_name.as_bytes(),
            &hello.client_nonce,
            &server_nonce,
            &hello.protocol_version.to_be_bytes(),
        ],
    );
    AuthChallenge { server_nonce, mac }
}

pub fn verify_server_challenge(
    secret: &[u8],
    hello: &AuthHello,
    challenge: &AuthChallenge,
) -> Result<(), AuthError> {
    validate_protocol_version(hello)?;
    let expected = compute_mac(
        secret,
        AUTH_LABEL,
        &[
            hello.machine_name.as_bytes(),
            &hello.client_nonce,
            &challenge.server_nonce,
            &hello.protocol_version.to_be_bytes(),
        ],
    );

    if expected != challenge.mac {
        return Err(AuthError::InvalidMac);
    }

    Ok(())
}

pub fn build_client_proof(
    secret: &[u8],
    hello: &AuthHello,
    challenge: &AuthChallenge,
) -> AuthProof {
    let mac = compute_mac(
        secret,
        PROOF_LABEL,
        &[
            hello.machine_name.as_bytes(),
            &challenge.server_nonce,
            &hello.client_nonce,
            &hello.protocol_version.to_be_bytes(),
        ],
    );
    AuthProof { mac }
}

pub fn verify_client_proof(
    secret: &[u8],
    hello: &AuthHello,
    challenge: &AuthChallenge,
    proof: &AuthProof,
) -> Result<(), AuthError> {
    validate_protocol_version(hello)?;
    let expected = compute_mac(
        secret,
        PROOF_LABEL,
        &[
            hello.machine_name.as_bytes(),
            &challenge.server_nonce,
            &hello.client_nonce,
            &hello.protocol_version.to_be_bytes(),
        ],
    );
    if expected != proof.mac {
        return Err(AuthError::InvalidMac);
    }

    Ok(())
}

pub fn derive_session_key(
    secret: &[u8],
    hello: &AuthHello,
    challenge: &AuthChallenge,
) -> Result<[u8; 32], AuthError> {
    let mut salt = [0_u8; 32];
    salt[..16].copy_from_slice(&hello.client_nonce);
    salt[16..].copy_from_slice(&challenge.server_nonce);

    let hkdf = Hkdf::<Sha256>::new(Some(&salt), secret);
    let mut key = [0_u8; 32];
    hkdf.expand(SESSION_LABEL, &mut key)
        .map_err(|_| AuthError::KeyDerivationFailed)?;
    Ok(key)
}

fn validate_protocol_version(hello: &AuthHello) -> Result<(), AuthError> {
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(AuthError::ProtocolVersionMismatch);
    }
    Ok(())
}

fn compute_mac(secret: &[u8], label: &[u8], fields: &[&[u8]]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts variable-size keys");
    mac.update(label);
    for field in fields {
        mac.update(field);
    }
    let bytes = mac.finalize().into_bytes();
    let mut out = [0_u8; 32];
    out.copy_from_slice(&bytes);
    out
}
