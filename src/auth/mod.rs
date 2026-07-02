use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::components::PlayerId;
use crate::mcp::{McpError, decode_ed25519_public_key, decode_ed25519_signature, encode_base64};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerCertificatePayload {
    pub audience: String,
    pub player_id: PlayerId,
    pub provider: String,
    pub subject: String,
    pub client_public_key: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerCertificate {
    pub payload: PlayerCertificatePayload,
    pub issuer_public_key: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthCertificatePayload {
    pub cert_id: String,
    pub usage: String,
    pub player_id: PlayerId,
    pub public_key: String,
    pub public_key_fingerprint: String,
    pub scope: String,
    pub audience: String,
    pub label: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthCertificate {
    pub payload: AuthCertificatePayload,
    pub issuer_public_key: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthChallenge {
    pub challenge_id: String,
    pub challenge: String,
    pub difficulty_bits: u32,
    pub expires_at: u64,
    pub consumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerRecord {
    pub username: String,
    pub public_key: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredCertificate {
    pub player_id: PlayerId,
    pub usage: String,
    pub fingerprint: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub revoked: bool,
    pub certificate_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertificateBundle {
    pub client_auth_cert: String,
    pub code_signing_cert: String,
    pub cert_id: String,
    pub player_id: PlayerId,
    pub public_key_fingerprint: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone)]
pub struct CertificateIssuer {
    signing_key: SigningKey,
}

impl Default for CertificateIssuer {
    fn default() -> Self {
        Self::new()
    }
}

impl CertificateIssuer {
    pub fn new() -> Self {
        let mut seed = [0_u8; 32];
        getrandom::getrandom(&mut seed).expect("OS randomness is required for certificate issuer");
        Self {
            signing_key: SigningKey::from_bytes(&seed),
        }
    }

    pub fn from_signing_key_for_tests(signing_key: SigningKey) -> Self {
        Self { signing_key }
    }

    pub fn issue(&self, payload: PlayerCertificatePayload) -> Result<PlayerCertificate, McpError> {
        let payload_bytes = certificate_payload_bytes(&payload)?;
        let signature = self.signing_key.sign(&payload_bytes);
        Ok(PlayerCertificate {
            payload,
            issuer_public_key: encode_base64(self.signing_key.verifying_key().as_bytes()),
            signature: encode_base64(&signature.to_bytes()),
        })
    }

    pub fn verify(&self, certificate: &PlayerCertificate) -> Result<(), McpError> {
        let expected_issuer = encode_base64(self.signing_key.verifying_key().as_bytes());
        if certificate.issuer_public_key != expected_issuer {
            return Err(McpError::invalid_params("certificate issuer is invalid"));
        }
        let payload_bytes = certificate_payload_bytes(&certificate.payload)?;
        let signature = decode_ed25519_signature(&certificate.signature, "certificate signature")?;
        self.signing_key
            .verifying_key()
            .verify(&payload_bytes, &signature)
            .map_err(|_| McpError::invalid_params("certificate signature is invalid"))
    }

    pub fn issue_auth(&self, payload: AuthCertificatePayload) -> Result<AuthCertificate, McpError> {
        let payload_bytes = auth_certificate_payload_bytes(&payload)?;
        let signature = self.signing_key.sign(&payload_bytes);
        Ok(AuthCertificate {
            payload,
            issuer_public_key: self.public_key(),
            signature: encode_base64(&signature.to_bytes()),
        })
    }

    pub fn verify_auth(&self, certificate: &AuthCertificate) -> Result<(), McpError> {
        if certificate.issuer_public_key != self.public_key() {
            return Err(McpError::invalid_params("certificate issuer is invalid"));
        }
        let payload_bytes = auth_certificate_payload_bytes(&certificate.payload)?;
        let signature = decode_ed25519_signature(&certificate.signature, "certificate signature")?;
        self.signing_key
            .verifying_key()
            .verify(&payload_bytes, &signature)
            .map_err(|_| McpError::invalid_params("certificate signature is invalid"))
    }

    pub fn public_key(&self) -> String {
        encode_base64(self.signing_key.verifying_key().as_bytes())
    }

    pub fn public_key_fingerprint(&self) -> String {
        blake3::hash(self.signing_key.verifying_key().as_bytes())
            .to_hex()
            .to_string()
    }
}

fn certificate_payload_bytes(payload: &PlayerCertificatePayload) -> Result<Vec<u8>, McpError> {
    serde_json::to_vec(payload).map_err(|error| McpError::invalid_params(error.to_string()))
}

fn auth_certificate_payload_bytes(payload: &AuthCertificatePayload) -> Result<Vec<u8>, McpError> {
    serde_json::to_vec(payload).map_err(|error| McpError::invalid_params(error.to_string()))
}

pub fn public_key_from_csr(csr: &str) -> Result<String, McpError> {
    let trimmed = csr.trim();
    if trimmed.is_empty() {
        return Err(McpError::invalid_params("csr is required"));
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        for field in ["public_key", "client_public_key", "ed25519_public_key"] {
            if let Some(public_key) = value.get(field).and_then(Value::as_str) {
                decode_ed25519_public_key(public_key, field)?;
                return Ok(public_key.to_string());
            }
        }
        return Err(McpError::invalid_params(
            "csr must contain public_key, client_public_key, or ed25519_public_key",
        ));
    }
    decode_ed25519_public_key(trimmed, "csr")?;
    Ok(trimmed.to_string())
}

pub fn verify_csr_signature(
    public_key: &str,
    csr: &str,
    challenge_id: &str,
    nonce: &str,
    signature: &str,
) -> Result<(), McpError> {
    let verifying_key = decode_ed25519_public_key(public_key, "csr public key")?;
    let signature = decode_ed25519_signature(signature, "csr_signature")?;
    let message = csr_signature_message(csr, challenge_id, nonce);
    verifying_key
        .verify(&message, &signature)
        .map_err(|_| McpError::invalid_params("csr_signature is invalid"))
}

pub fn verify_renewal_signature(
    verifying_key: &VerifyingKey,
    renewal_csr: &str,
    certificate_id: &str,
    proof_signature: &str,
) -> Result<(), McpError> {
    let signature = decode_ed25519_signature(proof_signature, "proof_signature")?;
    let message = csr_signature_message(renewal_csr, certificate_id, "");
    verifying_key
        .verify(&message, &signature)
        .map_err(|_| McpError::invalid_params("proof_signature is invalid"))
}

pub fn validate_pow(challenge: &str, nonce: &str, difficulty_bits: u32) -> bool {
    if difficulty_bits > 32 {
        return false;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(challenge.as_bytes());
    hasher.update(nonce.as_bytes());
    has_leading_zero_bits(hasher.finalize().as_bytes(), difficulty_bits)
}

fn csr_signature_message(csr: &str, challenge_id: &str, nonce: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(csr.len() + challenge_id.len() + nonce.len());
    message.extend_from_slice(csr.as_bytes());
    message.extend_from_slice(challenge_id.as_bytes());
    message.extend_from_slice(nonce.as_bytes());
    message
}

fn has_leading_zero_bits(bytes: &[u8], difficulty_bits: u32) -> bool {
    let whole_bytes = (difficulty_bits / 8) as usize;
    let remaining_bits = difficulty_bits % 8;
    if bytes.len() < whole_bytes {
        return false;
    }
    if bytes[..whole_bytes].iter().any(|byte| *byte != 0) {
        return false;
    }
    if remaining_bits == 0 {
        return true;
    }
    let mask = 0xff_u8 << (8 - remaining_bits);
    bytes
        .get(whole_bytes)
        .map(|byte| byte & mask == 0)
        .unwrap_or(false)
}
