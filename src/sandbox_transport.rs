use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bevy::prelude::Resource;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use swarm_engine_api::ids::{PlayerId, RoomId};

use crate::command::ObjectId;

const DEFAULT_COLLECT_TIMEOUT_MS: u64 = 2_500;
const AUTH_FRESHNESS_MS: u64 = 60_000;
const AUTH_FUTURE_SKEW_MS: u64 = 5_000;
pub const SANDBOX_TICK_SCHEMA: &str = "swarm.sandbox.tick.v2";
pub const SANDBOX_DEPLOY_SCHEMA: &str = "swarm.sandbox.deploy.v2";
pub const SANDBOX_MODULE_FETCH_SCHEMA: &str = "swarm.sandbox.module-fetch.v2";
pub const SANDBOX_VALIDATION_POLICY_VERSION: &str = "raw-wasm-v2";
type PendingDeploymentKey = (PlayerId, RoomId, u64);
type PendingDeploymentMap = BTreeMap<PendingDeploymentKey, ActiveDeployment>;

#[derive(Resource, Debug, Clone, Default)]
pub struct ActiveDeployments {
    inner: Arc<Mutex<BTreeMap<(PlayerId, RoomId), ActiveDeployment>>>,
    pending: Arc<Mutex<PendingDeploymentMap>>,
    paused_recovery: Arc<Mutex<BTreeSet<(PlayerId, RoomId)>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveDeployment {
    pub deploy_id: String,
    pub world_id: String,
    pub module_slot: String,
    pub player_id: PlayerId,
    pub room_id: RoomId,
    pub drone_id: ObjectId,
    pub module_hash: [u8; 32],
    pub metadata_hash: String,
    pub signed_payload_hash: String,
    pub compiled_artifact_hash: [u8; 32],
    pub client_version_counter: u64,
    pub redb_version_counter: u64,
    pub certificate_id: String,
    pub certificate_fingerprint: String,
    pub transport: String,
    pub signed_at: String,
    pub accepted_at_tick: u64,
    pub wasm_bytes: Vec<u8>,
    pub load_after_tick: u64,
}

impl ActiveDeployments {
    pub fn activate(&self, deployment: ActiveDeployment) {
        assert!(
            !deployment.wasm_bytes.is_empty(),
            "active deployment must include verified module bytes"
        );
        let key = (deployment.player_id, deployment.room_id);
        self.paused_recovery
            .lock()
            .expect("paused recovery lock poisoned")
            .remove(&key);
        let mut deployments = self.inner.lock().expect("active deployments lock poisoned");
        deployments.insert(key, deployment);
    }

    pub fn stage_activation(&self, deployment: ActiveDeployment) {
        assert!(
            !deployment.wasm_bytes.is_empty(),
            "pending deployment must include verified module bytes"
        );
        let mut pending = self
            .pending
            .lock()
            .expect("pending deployments lock poisoned");
        pending.retain(|(player_id, room_id, _), pending_deployment| {
            *player_id != deployment.player_id
                || *room_id != deployment.room_id
                || pending_deployment.redb_version_counter >= deployment.redb_version_counter
        });
        pending.insert(
            (
                deployment.player_id,
                deployment.room_id,
                deployment.load_after_tick,
            ),
            deployment,
        );
    }

    pub fn pending_ready_for_tick(&self, tick: u64) -> Vec<ActiveDeployment> {
        let pending = self
            .pending
            .lock()
            .expect("pending deployments lock poisoned");
        let mut ready = pending
            .values()
            .filter(|deployment| deployment.load_after_tick <= tick)
            .cloned()
            .collect::<Vec<_>>();
        ready.sort_by_key(|deployment| {
            (
                deployment.player_id,
                deployment.room_id.0,
                deployment.load_after_tick,
                deployment.module_hash,
            )
        });
        ready
    }

    pub fn consume_ready_for_tick(&self, tick: u64) -> Vec<ActiveDeployment> {
        let ready = self.pending_ready_for_tick(tick);
        if ready.is_empty() {
            return ready;
        }
        let mut pending = self
            .pending
            .lock()
            .expect("pending deployments lock poisoned");
        let mut deployments = self.inner.lock().expect("active deployments lock poisoned");
        for deployment in &ready {
            pending.remove(&(
                deployment.player_id,
                deployment.room_id,
                deployment.load_after_tick,
            ));
            deployments.insert(
                (deployment.player_id, deployment.room_id),
                deployment.clone(),
            );
            self.paused_recovery
                .lock()
                .expect("paused recovery lock poisoned")
                .remove(&(deployment.player_id, deployment.room_id));
        }
        ready
    }

    pub fn active_for_player(&self, player_id: PlayerId, tick: u64) -> Option<ActiveDeployment> {
        let deployments = self.inner.lock().expect("active deployments lock poisoned");
        let active = deployments
            .values()
            .filter(|deployment| {
                deployment.player_id == player_id && deployment.load_after_tick <= tick
            })
            .max_by_key(|deployment| deployment.load_after_tick)
            .cloned();
        drop(deployments);
        active.filter(|deployment| {
            !self.is_artifact_recovery_paused(deployment.player_id, deployment.room_id)
        })
    }

    pub fn pause_artifact_recovery(&self, player_id: PlayerId, room_id: RoomId) {
        self.paused_recovery
            .lock()
            .expect("paused recovery lock poisoned")
            .insert((player_id, room_id));
    }

    pub fn is_artifact_recovery_paused(&self, player_id: PlayerId, room_id: RoomId) -> bool {
        self.paused_recovery
            .lock()
            .expect("paused recovery lock poisoned")
            .contains(&(player_id, room_id))
    }
}

#[derive(Resource, Clone)]
pub enum SandboxBackend {
    Remote {
        nats_client: async_nats::Client,
        instance_id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SandboxTickRequest {
    pub schema: String,
    pub tick: u64,
    pub player_id: String,
    pub room_id: String,
    pub module_hash: [u8; 32],
    pub tick_input_bytes: Vec<u8>,
    pub fuel_budget: u64,
    pub collect_timeout_ms: u64,
    pub collect_deadline_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SandboxTickReply {
    pub tick: u64,
    pub player_id: String,
    pub tick_result_bytes: Vec<u8>,
    #[serde(default)]
    pub errors: Vec<String>,
    pub metrics: SandboxExecutionMetrics,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SandboxDeployAck {
    pub instance_id: String,
    pub module_hash: String,
    pub compiled_artifact_hash: String,
    pub status: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SandboxExecutionMetrics {
    pub fuel_consumed: u64,
    pub wall_clock_ms: u64,
    pub memory_peak_bytes: u64,
    pub host_function_calls: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct SandboxDeployRequest {
    schema: String,
    module_hash: [u8; 32],
    module_bytes: Vec<u8>,
    validation_policy_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthenticatedMessage<T> {
    pub request_id: String,
    pub nonce: String,
    pub timestamp_ms: u64,
    pub payload: T,
    pub auth_tag_hex: String,
}

#[derive(Debug, Serialize)]
struct AuthenticatedSigningMessage<'a, T: Serialize> {
    request_id: &'a str,
    nonce: &'a str,
    timestamp_ms: u64,
    payload: &'a T,
}

pub async fn execute_tick_remote(
    nats: &async_nats::Client,
    tick: u64,
    player_id: &str,
    room_id: &str,
    tick_input_bytes: &[u8],
    module_hash: &[u8; 32],
    fuel_budget: u64,
) -> Result<SandboxTickReply, String> {
    let collect_deadline_ms = current_time_ms()?.saturating_add(DEFAULT_COLLECT_TIMEOUT_MS);
    let request = SandboxTickRequest {
        schema: SANDBOX_TICK_SCHEMA.to_string(),
        tick,
        player_id: player_id.to_string(),
        room_id: room_id.to_string(),
        module_hash: *module_hash,
        tick_input_bytes: tick_input_bytes.to_vec(),
        fuel_budget,
        collect_timeout_ms: DEFAULT_COLLECT_TIMEOUT_MS,
        collect_deadline_ms,
    };
    let subject = format!("swarm.tick.{tick}.player.{player_id}");
    let request_id = new_hex_id(16)?;
    let payload = authenticated_payload_with_request_id(&request, &request_id)?;
    let response = tokio::time::timeout(
        remaining_collect_duration(collect_deadline_ms)?,
        nats.request(subject, payload.into()),
    )
    .await
    .map_err(|_| "sandbox request timed out".to_string())?
    .map_err(|error| error.to_string())?;

    let reply: SandboxTickReply = decode_authenticated_payload(&response.payload, &request_id)?;
    if reply.tick != tick || reply.player_id != player_id {
        return Err("sandbox reply did not match request".to_string());
    }
    Ok(reply)
}

pub async fn deploy_module_remote(
    nats: &async_nats::Client,
    module_hash: &[u8; 32],
    wasm_bytes: &[u8],
) -> Result<[u8; 32], String> {
    if blake3::hash(wasm_bytes).as_bytes() != module_hash {
        return Err("module_hash must equal BLAKE3(module_bytes)".to_string());
    }
    let validation_policy_version = SANDBOX_VALIDATION_POLICY_VERSION.to_string();
    let request = SandboxDeployRequest {
        schema: SANDBOX_DEPLOY_SCHEMA.to_string(),
        module_hash: *module_hash,
        module_bytes: wasm_bytes.to_vec(),
        validation_policy_version,
    };
    let subject = format!("swarm.deploy.{}", blake3::Hash::from(*module_hash).to_hex());
    let request_id = new_hex_id(16)?;
    let payload = authenticated_payload_with_request_id(&request, &request_id)?;
    let response = tokio::time::timeout(
        Duration::from_millis(DEFAULT_COLLECT_TIMEOUT_MS),
        nats.request(subject, payload.into()),
    )
    .await
    .map_err(|_| "sandbox deploy request timed out".to_string())?
    .map_err(|error| error.to_string())?;

    let ack: SandboxDeployAck = decode_authenticated_payload(&response.payload, &request_id)?;
    let expected_hash = blake3::Hash::from(*module_hash).to_hex().to_string();
    if ack.module_hash != expected_hash {
        return Err("sandbox deploy ack module_hash mismatch".to_string());
    }
    if !ack.status.starts_with("cached:") {
        return Err(format!("sandbox deploy failed: {}", ack.status));
    }
    module_hash_from_hex(&ack.compiled_artifact_hash)
        .map_err(|error| format!("sandbox deploy ack compiled_artifact_hash invalid: {error}"))
}

fn remaining_collect_duration(collect_deadline_ms: u64) -> Result<Duration, String> {
    let remaining_ms = collect_deadline_ms.saturating_sub(current_time_ms()?);
    if remaining_ms == 0 {
        return Err("sandbox collect deadline exceeded".to_string());
    }
    Ok(Duration::from_millis(remaining_ms))
}

pub fn module_hash_from_hex(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err("module_hash must be 64 lowercase hex characters".to_string());
    }
    let mut bytes = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = decode_hex_byte(chunk)?;
    }
    Ok(bytes)
}

pub fn authenticated_payload<T: Serialize>(payload: &T) -> Result<Vec<u8>, String> {
    let request_id = new_hex_id(16)?;
    authenticated_payload_with_request_id(payload, &request_id)
}

pub fn nats_auth_secret_from_env() -> Result<String, String> {
    let secret = std::env::var("SWARM_NATS_AUTH_SECRET")
        .map_err(|_| "SWARM_NATS_AUTH_SECRET is required for sandbox messages".to_string())?;
    validate_nats_auth_secret(&secret)
}

pub fn validate_nats_auth_secret(secret: &str) -> Result<String, String> {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return Err("SWARM_NATS_AUTH_SECRET must not be empty".to_string());
    }
    Ok(trimmed.to_string())
}

fn authenticated_payload_with_request_id<T: Serialize>(
    payload: &T,
    request_id: &str,
) -> Result<Vec<u8>, String> {
    validate_hex_id(request_id, 16, "request_id")?;
    let nonce = new_hex_id(16)?;
    let timestamp_ms = current_time_ms()?;
    let secret = nats_auth_secret_from_env()?;
    let signing = AuthenticatedSigningMessage {
        request_id,
        nonce: &nonce,
        timestamp_ms,
        payload,
    };
    let payload_bytes = serde_json::to_vec(&signing).map_err(|error| error.to_string())?;
    let envelope = AuthenticatedMessage {
        request_id: request_id.to_string(),
        nonce,
        timestamp_ms,
        payload,
        auth_tag_hex: hmac_sha256_hex(secret.as_bytes(), &payload_bytes),
    };
    serde_json::to_vec(&envelope).map_err(|error| error.to_string())
}

pub fn decode_authenticated_payload<T>(bytes: &[u8], expected_request_id: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    validate_hex_id(expected_request_id, 16, "expected_request_id")?;
    let envelope = decode_authenticated_message(bytes)?;
    if envelope.request_id != expected_request_id {
        return Err("sandbox reply request_id mismatch".to_string());
    }
    Ok(envelope.payload)
}

fn decode_authenticated_message<T>(bytes: &[u8]) -> Result<AuthenticatedMessage<T>, String>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let envelope: AuthenticatedMessage<T> =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    validate_hex_id(&envelope.request_id, 16, "request_id")?;
    validate_hex_id(&envelope.nonce, 16, "nonce")?;
    verify_fresh_timestamp(envelope.timestamp_ms)?;
    let secret = nats_auth_secret_from_env()?;
    let signing = AuthenticatedSigningMessage {
        request_id: &envelope.request_id,
        nonce: &envelope.nonce,
        timestamp_ms: envelope.timestamp_ms,
        payload: &envelope.payload,
    };
    let payload_bytes = serde_json::to_vec(&signing).map_err(|error| error.to_string())?;
    let expected = hmac_sha256_hex(secret.as_bytes(), &payload_bytes);
    if !constant_time_eq(expected.as_bytes(), envelope.auth_tag_hex.as_bytes()) {
        return Err("invalid sandbox message auth tag".to_string());
    }
    Ok(envelope)
}

fn new_hex_id(byte_len: usize) -> Result<String, String> {
    if byte_len == 0 {
        return Err("random identifier byte length must be greater than zero".to_string());
    }
    let mut bytes = vec![0_u8; byte_len];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(hex_encode(&bytes))
}

fn validate_hex_id(value: &str, byte_len: usize, field: &str) -> Result<(), String> {
    if value.len() != byte_len.saturating_mul(2) {
        return Err(format!(
            "{field} must be {} lowercase hex characters",
            byte_len * 2
        ));
    }
    if !value
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(format!("{field} must be lowercase hex"));
    }
    Ok(())
}

fn current_time_ms() -> Result<u64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?;
    Ok(elapsed.as_millis() as u64)
}

fn verify_fresh_timestamp(timestamp_ms: u64) -> Result<(), String> {
    let now = current_time_ms()?;
    if timestamp_ms > now.saturating_add(AUTH_FUTURE_SKEW_MS) {
        return Err("sandbox message timestamp is in the future".to_string());
    }
    if now.saturating_sub(timestamp_ms) > AUTH_FRESHNESS_MS {
        return Err("sandbox message timestamp is stale".to_string());
    }
    Ok(())
}

pub fn hmac_sha256_hex(secret: &[u8], message: &[u8]) -> String {
    const BLOCK_SIZE: usize = 64;
    let mut key = [0_u8; BLOCK_SIZE];
    if secret.len() > BLOCK_SIZE {
        key[..32].copy_from_slice(&Sha256::digest(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }

    let mut outer = [0x5c_u8; BLOCK_SIZE];
    let mut inner = [0x36_u8; BLOCK_SIZE];
    for index in 0..BLOCK_SIZE {
        outer[index] ^= key[index];
        inner[index] ^= key[index];
    }

    let mut inner_hasher = Sha256::new();
    inner_hasher.update(inner);
    inner_hasher.update(message);
    let inner_hash = inner_hasher.finalize();

    let mut outer_hasher = Sha256::new();
    outer_hasher.update(outer);
    outer_hasher.update(inner_hash);
    hex_encode(&outer_hasher.finalize())
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex_byte(chunk: &[u8]) -> Result<u8, String> {
    Ok((decode_hex_nibble(chunk[0])? << 4) | decode_hex_nibble(chunk[1])?)
}

fn decode_hex_nibble(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err("module_hash must be lowercase hex".to_string()),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn active_deployment(module_hash: [u8; 32], version: u64) -> ActiveDeployment {
        ActiveDeployment {
            deploy_id: format!("deploy-{version}"),
            world_id: "world".to_string(),
            module_slot: "main".to_string(),
            player_id: 7,
            room_id: RoomId(3),
            drone_id: 11,
            module_hash,
            metadata_hash: "metadata".to_string(),
            signed_payload_hash: "payload".to_string(),
            compiled_artifact_hash: [9; 32],
            client_version_counter: version,
            redb_version_counter: version,
            certificate_id: "certificate".to_string(),
            certificate_fingerprint: "fingerprint".to_string(),
            transport: "nats".to_string(),
            signed_at: "now".to_string(),
            accepted_at_tick: 0,
            wasm_bytes: vec![0, 97, 115, 109],
            load_after_tick: 0,
        }
    }

    #[test]
    fn artifact_recovery_pause_returns_no_active_slot_until_replacement_activation() {
        let deployments = ActiveDeployments::default();
        deployments.activate(active_deployment([1; 32], 1));
        assert!(deployments.active_for_player(7, 0).is_some());

        deployments.pause_artifact_recovery(7, RoomId(3));

        assert!(deployments.active_for_player(7, 1).is_none());
        assert!(deployments.is_artifact_recovery_paused(7, RoomId(3)));

        deployments.activate(active_deployment([2; 32], 2));

        assert!(deployments.active_for_player(7, 1).is_some());
        assert!(!deployments.is_artifact_recovery_paused(7, RoomId(3)));
    }

    static NATS_AUTH_SECRET_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        original: Option<String>,
    }

    impl EnvGuard {
        fn acquire() -> Self {
            let lock = NATS_AUTH_SECRET_ENV_LOCK.get_or_init(|| Mutex::new(()));
            let guard = lock.lock().expect("NATS auth secret env lock poisoned");
            let original = std::env::var("SWARM_NATS_AUTH_SECRET").ok();
            Self {
                _lock: guard,
                original,
            }
        }

        fn remove(&self) {
            unsafe { std::env::remove_var("SWARM_NATS_AUTH_SECRET") };
        }

        fn set(&self, value: &str) {
            unsafe { std::env::set_var("SWARM_NATS_AUTH_SECRET", value) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => unsafe { std::env::set_var("SWARM_NATS_AUTH_SECRET", value) },
                None => unsafe { std::env::remove_var("SWARM_NATS_AUTH_SECRET") },
            }
        }
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231_vector() {
        assert_eq!(
            hmac_sha256_hex(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn module_hash_hex_requires_raw_32_bytes() {
        let hash = module_hash_from_hex(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        )
        .unwrap();

        assert_eq!(hash[0], 0);
        assert_eq!(hash[31], 31);
        assert!(module_hash_from_hex("abc").is_err());
        assert!(
            module_hash_from_hex(
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1X"
            )
            .is_err()
        );
    }

    #[test]
    fn nats_auth_secret_validation_rejects_missing_and_empty_values() {
        let env = EnvGuard::acquire();
        env.remove();
        assert_eq!(
            nats_auth_secret_from_env().unwrap_err(),
            "SWARM_NATS_AUTH_SECRET is required for sandbox messages"
        );
        assert_eq!(
            validate_nats_auth_secret("   ").unwrap_err(),
            "SWARM_NATS_AUTH_SECRET must not be empty"
        );
        assert_eq!(validate_nats_auth_secret("  secret  ").unwrap(), "secret");
    }

    #[test]
    fn authenticated_payload_fails_closed_for_empty_secret() {
        let env = EnvGuard::acquire();
        env.set("  ");
        let request = SandboxTickRequest {
            schema: SANDBOX_TICK_SCHEMA.to_string(),
            tick: 9,
            player_id: "1".to_string(),
            room_id: "0".to_string(),
            module_hash: [7; 32],
            tick_input_bytes: b"tick-input".to_vec(),
            fuel_budget: 10,
            collect_timeout_ms: 20,
            collect_deadline_ms: current_time_ms().unwrap().saturating_add(20),
        };

        assert_eq!(
            authenticated_payload(&request).unwrap_err(),
            "SWARM_NATS_AUTH_SECRET must not be empty"
        );
    }

    #[test]
    fn authenticated_payload_wraps_declared_payload_bytes() {
        let env = EnvGuard::acquire();
        env.set("secret");
        let request = SandboxTickRequest {
            schema: SANDBOX_TICK_SCHEMA.to_string(),
            tick: 9,
            player_id: "1".to_string(),
            room_id: "0".to_string(),
            module_hash: [7; 32],
            tick_input_bytes: b"tick-input".to_vec(),
            fuel_budget: 10,
            collect_timeout_ms: 20,
            collect_deadline_ms: current_time_ms().unwrap().saturating_add(20),
        };

        let encoded = authenticated_payload(&request).unwrap();
        let envelope: AuthenticatedMessage<SandboxTickRequest> =
            serde_json::from_slice(&encoded).unwrap();
        let signing = AuthenticatedSigningMessage {
            request_id: &envelope.request_id,
            nonce: &envelope.nonce,
            timestamp_ms: envelope.timestamp_ms,
            payload: &request,
        };
        let payload_bytes = serde_json::to_vec(&signing).unwrap();

        assert_eq!(envelope.payload.module_hash, [7; 32]);
        assert_eq!(
            envelope.payload.collect_deadline_ms,
            request.collect_deadline_ms
        );
        assert_eq!(envelope.request_id.len(), 32);
        assert_eq!(envelope.nonce.len(), 32);
        assert_eq!(
            envelope.auth_tag_hex,
            hmac_sha256_hex(b"secret", &payload_bytes)
        );
    }

    #[test]
    fn authenticated_transport_rejects_malformed_rng_identifiers() {
        let env = EnvGuard::acquire();
        env.set("secret");
        let request = SandboxTickRequest {
            schema: SANDBOX_TICK_SCHEMA.to_string(),
            tick: 9,
            player_id: "1".to_string(),
            room_id: "0".to_string(),
            module_hash: [7; 32],
            tick_input_bytes: b"tick-input".to_vec(),
            fuel_budget: 10,
            collect_timeout_ms: 20,
            collect_deadline_ms: current_time_ms().unwrap().saturating_add(20),
        };

        assert_eq!(
            new_hex_id(0).unwrap_err(),
            "random identifier byte length must be greater than zero"
        );
        assert_eq!(
            authenticated_payload_with_request_id(&request, "short").unwrap_err(),
            "request_id must be 32 lowercase hex characters"
        );

        let mut envelope: AuthenticatedMessage<SandboxTickRequest> =
            serde_json::from_slice(&authenticated_payload(&request).unwrap()).unwrap();
        envelope.nonce = "ABCDEFABCDEFABCDEFABCDEFABCDEFAB".to_string();
        let encoded = serde_json::to_vec(&envelope).unwrap();
        assert_eq!(
            decode_authenticated_payload::<SandboxTickRequest>(&encoded, &envelope.request_id)
                .unwrap_err(),
            "nonce must be lowercase hex"
        );
    }

    #[test]
    fn absolute_collect_deadline_rejects_expired_requests() {
        assert_eq!(
            remaining_collect_duration(current_time_ms().unwrap().saturating_sub(1)).unwrap_err(),
            "sandbox collect deadline exceeded"
        );
    }

    #[test]
    fn engine_protocol_tags_match_sandbox_v2_contract() {
        let tick = SandboxTickRequest {
            schema: SANDBOX_TICK_SCHEMA.to_string(),
            tick: 1,
            player_id: "7".to_string(),
            room_id: "3".to_string(),
            module_hash: [1; 32],
            tick_input_bytes: vec![2],
            fuel_budget: 3,
            collect_timeout_ms: 4,
            collect_deadline_ms: 5,
        };
        let deploy = SandboxDeployRequest {
            schema: SANDBOX_DEPLOY_SCHEMA.to_string(),
            module_hash: [1; 32],
            module_bytes: vec![0, 97, 115, 109],
            validation_policy_version: SANDBOX_VALIDATION_POLICY_VERSION.to_string(),
        };

        assert_eq!(tick.schema, "swarm.sandbox.tick.v2");
        assert_eq!(deploy.schema, "swarm.sandbox.deploy.v2");
        assert_eq!(deploy.validation_policy_version, "raw-wasm-v2");
        assert_eq!(SANDBOX_MODULE_FETCH_SCHEMA, "swarm.sandbox.module-fetch.v2");
    }

    #[test]
    fn decode_authenticated_payload_accepts_matching_signed_reply() {
        let env = EnvGuard::acquire();
        env.set("secret");
        let reply = SandboxTickReply {
            tick: 9,
            player_id: "1".to_string(),
            tick_result_bytes: Vec::new(),
            errors: Vec::new(),
            metrics: SandboxExecutionMetrics::default(),
            status: "Ok".to_string(),
        };
        let encoded = signed_reply(
            &reply,
            "0123456789abcdef0123456789abcdef",
            current_time_ms().unwrap(),
        );

        let decoded: SandboxTickReply =
            decode_authenticated_payload(&encoded, "0123456789abcdef0123456789abcdef").unwrap();

        assert_eq!(decoded.status, "Ok");
        assert_eq!(decoded.tick, 9);
    }

    #[test]
    fn decode_authenticated_payload_rejects_mismatched_request_id() {
        let env = EnvGuard::acquire();
        env.set("secret");
        let ack = SandboxDeployAck {
            instance_id: "sandbox-1".to_string(),
            module_hash: "ab".repeat(32),
            compiled_artifact_hash: "cd".repeat(32),
            status: "cached:raw-wasm-v2".to_string(),
        };
        let encoded = signed_reply(
            &ack,
            "0123456789abcdef0123456789abcdef",
            current_time_ms().unwrap(),
        );

        let error = decode_authenticated_payload::<SandboxDeployAck>(
            &encoded,
            "ffffffffffffffffffffffffffffffff",
        )
        .unwrap_err();

        assert_eq!(error, "sandbox reply request_id mismatch");
    }

    #[test]
    fn decode_authenticated_payload_rejects_stale_reply() {
        let env = EnvGuard::acquire();
        env.set("secret");
        let ack = SandboxDeployAck {
            instance_id: "sandbox-1".to_string(),
            module_hash: "ab".repeat(32),
            compiled_artifact_hash: "cd".repeat(32),
            status: "cached:raw-wasm-v2".to_string(),
        };
        let timestamp_ms = current_time_ms().unwrap() - AUTH_FRESHNESS_MS - 1;
        let encoded = signed_reply(&ack, "0123456789abcdef0123456789abcdef", timestamp_ms);

        let error = decode_authenticated_payload::<SandboxDeployAck>(
            &encoded,
            "0123456789abcdef0123456789abcdef",
        )
        .unwrap_err();

        assert_eq!(error, "sandbox message timestamp is stale");
    }

    fn signed_reply<T: Serialize>(payload: &T, request_id: &str, timestamp_ms: u64) -> Vec<u8> {
        let nonce = "abcdef0123456789abcdef0123456789";
        let signing = AuthenticatedSigningMessage {
            request_id,
            nonce,
            timestamp_ms,
            payload,
        };
        let payload_bytes = serde_json::to_vec(&signing).unwrap();
        serde_json::to_vec(&AuthenticatedMessage {
            request_id: request_id.to_string(),
            nonce: nonce.to_string(),
            timestamp_ms,
            payload,
            auth_tag_hex: hmac_sha256_hex(b"secret", &payload_bytes),
        })
        .unwrap()
    }
}
