use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use swarm_wasm_sandbox::{
    CompiledModuleCache, HostCallBudget, SandboxError, SandboxRuntime, TickOutput,
};

const DEFAULT_COLLECT_TIMEOUT_MS: u64 = 2_500;

#[derive(Clone)]
pub enum SandboxBackend {
    Local(SandboxRuntime),
    Remote {
        nats_client: async_nats::Client,
        instance_id: String,
    },
}

impl Default for SandboxBackend {
    fn default() -> Self {
        Self::Local(SandboxRuntime::default())
    }
}

impl SandboxBackend {
    pub fn local_runtime(&self) -> Option<&SandboxRuntime> {
        match self {
            Self::Local(runtime) => Some(runtime),
            Self::Remote { .. } => None,
        }
    }

    pub fn local_runtime_or_default(&self) -> SandboxRuntime {
        match self {
            Self::Local(runtime) => runtime.clone(),
            Self::Remote { .. } => SandboxRuntime::default(),
        }
    }
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

pub async fn execute_tick_local(
    runtime: &SandboxRuntime,
    cache: &mut CompiledModuleCache,
    wasm_bytes: &[u8],
    snapshot_json: &[u8],
) -> Result<TickOutput, SandboxError> {
    let compiled = runtime.compile_cached(cache, wasm_bytes)?;
    runtime.execute_tick(&compiled, snapshot_json)
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
    let runtime = SandboxRuntime::default();
    let cached = runtime
        .precompile_native(wasm_bytes)
        .map_err(|error| error.to_string())?;
    let artifact_hash = blake3::hash(&cached.native_bytes).as_bytes().to_vec();
    let request = SandboxDeployRequest {
        module_hash: module_hash.to_vec(),
        compiled_artifact_hash: artifact_hash.clone(),
        module_bytes: wasm_bytes.to_vec(),
        compiled_native_bytes: cached.native_bytes,
        wasmtime_version: cached.key.wasmtime_version,
        validation_policy_version: "v1".to_string(),
    };
    let subject = format!("swarm.deploy.{}", bytes_to_hex(&artifact_hash));
    let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    nats.publish(subject, payload.into())
        .await
        .map_err(|error| error.to_string())
}

pub fn metrics_from_host_budget(host_call_budget: &HostCallBudget) -> SandboxExecutionMetrics {
    SandboxExecutionMetrics {
        fuel_consumed: 0,
        wall_clock_ms: 0,
        memory_peak_bytes: 0,
        host_function_calls: host_call_budget.total_calls,
    }
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
