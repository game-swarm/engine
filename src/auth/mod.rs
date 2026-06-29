use ed25519_dalek::{Signer, SigningKey, Verifier};
use serde::{Deserialize, Serialize};

use crate::components::PlayerId;
use crate::mcp::{McpError, decode_ed25519_signature, encode_base64};

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
}

fn certificate_payload_bytes(payload: &PlayerCertificatePayload) -> Result<Vec<u8>, McpError> {
    serde_json::to_vec(payload).map_err(|error| McpError::invalid_params(error.to_string()))
}
