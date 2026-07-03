use std::time::Duration;

use bevy::prelude::Resource;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DEFAULT_COLLECT_TIMEOUT_MS: u64 = 2_500;

#[derive(Resource, Clone)]
pub enum SandboxBackend {
    Remote {
        nats_client: async_nats::Client,
        instance_id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SandboxTickRequest {
    pub tick: u64,
    pub player_id: String,
    pub snapshot_json: String,
    pub module_hash: Vec<u8>,
    pub fuel_budget: u64,
    pub collect_timeout_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SandboxTickReply {
    pub tick: u64,
    pub player_id: String,
    pub commands: Vec<Value>,
    pub metrics: SandboxExecutionMetrics,
    pub status: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SandboxExecutionMetrics {
    pub fuel_consumed: u64,
    pub wall_clock_ms: u64,
    pub memory_peak_bytes: u64,
    pub host_function_calls: u32,
}

#[derive(Debug, Serialize)]
struct SandboxDeployRequest {
    module_hash: Vec<u8>,
    compiled_artifact_hash: Vec<u8>,
    module_bytes: Vec<u8>,
    compiled_native_bytes: Vec<u8>,
    wasmtime_version: String,
    validation_policy_version: String,
}

pub async fn execute_tick_remote(
    nats: &async_nats::Client,
    tick: u64,
    player_id: &str,
    snapshot_json: &[u8],
    module_hash: &[u8],
    fuel_budget: u64,
) -> Result<SandboxTickReply, String> {
    let request = SandboxTickRequest {
        tick,
        player_id: player_id.to_string(),
        snapshot_json: String::from_utf8_lossy(snapshot_json).into_owned(),
        module_hash: module_hash.to_vec(),
        fuel_budget,
        collect_timeout_ms: DEFAULT_COLLECT_TIMEOUT_MS,
    };
    let subject = format!("swarm.tick.{tick}.player.{player_id}");
    let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    let response = tokio::time::timeout(
        Duration::from_millis(DEFAULT_COLLECT_TIMEOUT_MS),
        nats.request(subject, payload.into()),
    )
    .await
    .map_err(|_| "sandbox request timed out".to_string())?
    .map_err(|error| error.to_string())?;

    serde_json::from_slice(&response.payload).map_err(|error| error.to_string())
}

pub async fn deploy_module_remote(
    nats: &async_nats::Client,
    module_hash: &[u8],
    wasm_bytes: &[u8],
) -> Result<(), String> {
    let artifact_hash = blake3::hash(wasm_bytes).as_bytes().to_vec();
    let request = SandboxDeployRequest {
        module_hash: module_hash.to_vec(),
        compiled_artifact_hash: artifact_hash.clone(),
        module_bytes: wasm_bytes.to_vec(),
        compiled_native_bytes: Vec::new(),
        wasmtime_version: String::new(),
        validation_policy_version: "raw-wasm-v1".to_string(),
    };
    let subject = format!("swarm.deploy.{}", blake3::hash(wasm_bytes).to_hex());
    let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    nats.publish(subject, payload.into())
        .await
        .map_err(|error| error.to_string())
}
