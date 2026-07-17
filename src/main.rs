use std::{
    collections::{BTreeMap, HashMap},
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use serde_json::json;
use sha2::{Digest, Sha256};

use swarm_engine::{
    BodyPart, CommandIntent, ExecutorError, PlayerCollectMetrics, PlayerCollectOutput,
    PlayerExecutor, PlayerId, RoomId, TickBroadcaster, TickSnapshot, WorldMode,
    create_world_with_mode,
    sandbox_transport::{
        ActiveDeployment, ActiveDeployments, SandboxBackend, execute_tick_remote, hex_encode,
        nats_auth_secret_from_env, register_recovered_module_fetch_artifact,
    },
    sim::{create_local_simulation_world, summarize_local_simulation},
};

mod metrics;

#[cfg(feature = "mod_combat_core")]
#[allow(dead_code)]
#[path = "../mods/combat-core/src/lib.rs"]
mod swarm_mod_combat_core;
#[cfg(feature = "mod_depot_storage")]
#[path = "../mods/depot-storage/src/lib.rs"]
mod swarm_mod_depot_storage;
#[cfg(feature = "mod_empire_upkeep")]
#[allow(dead_code)]
#[path = "../mods/empire-upkeep/src/lib.rs"]
mod swarm_mod_empire_upkeep;
#[cfg(feature = "mod_fog_of_war")]
#[allow(dead_code)]
#[path = "../mods/fog-of-war/src/lib.rs"]
mod swarm_mod_fog_of_war;
#[cfg(feature = "mod_pve_spawning")]
#[allow(dead_code)]
#[path = "../mods/pve-spawning/src/lib.rs"]
mod swarm_mod_pve_spawning;
#[cfg(feature = "mod_resource_decay")]
#[path = "../mods/resource-decay/src/lib.rs"]
mod swarm_mod_resource_decay;
#[cfg(feature = "mod_special_attacks")]
#[allow(dead_code)]
#[path = "../mods/special-attacks/src/lib.rs"]
mod swarm_mod_special_attacks;
#[cfg(feature = "mod_vanilla_boss")]
#[path = "../mods/vanilla-boss/src/lib.rs"]
mod swarm_mod_vanilla_boss;

const DEFAULT_HEALTH_ADDR: &str = "127.0.0.1:8080";
const MAX_PRE_AUTH_HTTP_BODY_BYTES: usize = 8 * 1024 * 1024;
const MCP_PROXY_REPLAY_WINDOW_SECONDS: i64 = 300;
const DEFAULT_PROXY_NONCE_PATH: &str = "swarm-proxy-nonces.db";
const PRODUCTION_PROXY_NONCE_PATH: &str = "/var/lib/swarm-engine/proxy-nonces.db";
const NATS_DEFAULT_PORT: u16 = 4222;
const ENGINE_MODE_PRODUCTION: &str = "production";
const ENGINE_MODE_DEVELOPMENT: &str = "development";
const ENGINE_MODE_TEST: &str = "test";
const ISSUER_KEY_FILE_ENV: &str = "SWARM_ENGINE_ISSUER_KEY_FILE";
const ISSUER_KEY_B64_ENV: &str = "SWARM_ENGINE_ISSUER_KEY_B64";
const PROXY_NONCE_PATH_ENV: &str = "SWARM_PROXY_NONCE_PATH";

#[cfg(target_os = "linux")]
const O_NOFOLLOW_FLAG: i32 = 0o400000;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn geteuid() -> u32;
}

#[derive(Clone, Debug)]
struct Endpoint {
    host: String,
    port: u16,
}

struct McpHttpState {
    dispatch_tx: mpsc::Sender<McpDispatch>,
    proxy_verifier: Result<swarm_engine::mcp::GatewayProxyVerifier, String>,
    seen_proxy_nonces: ProxyNonceStore,
}

struct McpDispatch {
    player_id: PlayerId,
    principal: swarm_engine::mcp::VerifiedMcpPrincipal,
    request: swarm_engine::JsonRpcRequest,
    response_tx: mpsc::SyncSender<swarm_engine::JsonRpcResponse>,
    cancelled: Arc<AtomicBool>,
}

struct ProxyNonceStore {
    path: PathBuf,
    seen: BTreeMap<String, i64>,
    persistence_error: Option<String>,
}

#[derive(Debug, Clone)]
struct ProxyPrincipal {
    player_id: PlayerId,
    cert_id: String,
    cert_fingerprint: String,
    transport: String,
    scopes: String,
    auth_mode: String,
}

#[derive(Debug, Clone)]
struct NatsSecurityConfig {
    url: String,
    tls_required: bool,
    ca_file: Option<PathBuf>,
    client_cert_file: Option<PathBuf>,
    client_key_file: Option<PathBuf>,
    credentials_file: Option<PathBuf>,
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn main() {
    let cli_args = env::args().skip(1).collect::<Vec<_>>();
    if cli_args.first().map(|arg| arg.as_str()) == Some("sim") {
        if let Err(error) = run_sim(&cli_args[1..]) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }

    let (mode, cli_args) = match parse_mode_arg(cli_args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    match swarm_engine::mod_cli::try_run(cli_args.clone()) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }

    // ── SDK generation CLI ──────────────────────────────────────────
    if let Some(cmd) = cli_args.first().map(|s| s.as_str()) {
        match cmd {
            "dump-idl" => {
                let world_toml = cli_args.get(1).map(|s| s.as_str()).unwrap_or("world.toml");
                match swarm_engine::sdk_gen::cli_dump_idl(world_toml) {
                    Ok(json) => {
                        println!("{json}");
                        return;
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
            }
            "generate-sdk" => {
                let world_toml = cli_args.get(1).map(|s| s.as_str()).unwrap_or("world.toml");
                let out_dir = cli_args
                    .get(2)
                    .map(|s| s.as_str())
                    .unwrap_or("/data/swarm/sdk-cache");
                match swarm_engine::sdk_gen::cli_generate_sdk(world_toml, out_dir) {
                    Ok(()) => {
                        println!("SDK generated to {out_dir}");
                        return;
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
            }
            "export-contracts" => {
                let out_dir = cli_args
                    .get(1)
                    .map(|s| s.as_str())
                    .unwrap_or("../frontend/src/generated");
                match swarm_engine::contract_exports::export_contract_artifacts(out_dir) {
                    Ok(()) => {
                        println!("contracts exported to {out_dir}");
                        return;
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
            }
            _ => {}
        }
    }

    let redb_path = env::var("REDB_PATH").unwrap_or_else(|_| "swarm.redb".to_string());
    let nats_url = env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
    let requested_sandbox_backend =
        env::var("SANDBOX_BACKEND").unwrap_or_else(|_| "nats".to_string());
    let health_addr =
        env::var("ENGINE_HEALTH_ADDR").unwrap_or_else(|_| DEFAULT_HEALTH_ADDR.to_string());
    let tick_interval = Duration::from_millis(
        env::var("SWARM_TICK_INTERVAL_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(match mode {
                WorldMode::Tutorial => swarm_engine::TUTORIAL_TICK_INTERVAL_MS,
                WorldMode::Default | WorldMode::Novice | WorldMode::Arena => {
                    swarm_engine::DEFAULT_TICK_INTERVAL_MS
                }
            }),
    );

    let healthy = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(metrics::EngineMetrics::default());

    let redb_store_result = swarm_engine::RedbStore::open(&redb_path);
    let nats_endpoint = parse_nats_endpoint(&nats_url);
    let nats_security = match NatsSecurityConfig::from_env(&nats_url) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let certificate_issuer = match certificate_issuer_from_env() {
        Ok(issuer) => issuer,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    let redb_store = match redb_store_result {
        Ok(store) => {
            println!("redb opened path={redb_path}");
            store
        }
        Err(error) => {
            eprintln!("redb unavailable: {error}");
            std::process::exit(1);
        }
    };
    let redb_connected = true;
    let recovered_world = match redb_store.recover_latest() {
        Ok(Some(point)) => {
            let recovered_tick = point.tick;
            let expected_checksum = point.head.state_checksum;
            match redb_store
                .read_tick_state(recovered_tick)
                .ok()
                .flatten()
                .or_else(|| point.snapshot.map(|snapshot| snapshot.state))
            {
                Some(state) => Some((recovered_tick, expected_checksum, state)),
                None => {
                    eprintln!(
                        "redb recovery failed: tick {recovered_tick} has no recoverable state"
                    );
                    std::process::exit(1);
                }
            }
        }
        Ok(None) => None,
        Err(error) => {
            eprintln!("redb recovery failed: {error}");
            std::process::exit(1);
        }
    };
    match &nats_endpoint {
        Ok(endpoint) => println!(
            "nats configured url={} endpoint={}:{}",
            redact_url_userinfo(&nats_url),
            endpoint.host,
            endpoint.port
        ),
        Err(error) => eprintln!("nats unavailable: {error}"),
    }
    if requested_sandbox_backend != "nats" {
        eprintln!(
            "SANDBOX_BACKEND={requested_sandbox_backend} ignored; remote NATS sandbox is required"
        );
    }
    if let Err(error) = nats_auth_secret_from_env() {
        eprintln!("{error}");
        std::process::exit(1);
    }
    let (mcp_runtime_tx, mcp_runtime_rx) = mpsc::channel();
    let (mcp_dispatch_tx, mcp_dispatch_rx) = mpsc::channel();
    start_health_server(
        health_addr,
        Arc::clone(&healthy),
        Arc::clone(&metrics),
        mcp_runtime_rx,
        mode,
    );
    let nats_client = connect_nats_client_with_retry(&nats_security, &healthy, tick_interval);
    let shared_nats_client = Some(nats_client.clone());
    let sandbox_backend = SandboxBackend::Remote {
        nats_client,
        instance_id: env::var("INSTANCE_ID")
            .or_else(|_| env::var("ENGINE_INSTANCE_ID"))
            .unwrap_or_else(|_| "engine-0".to_string()),
    };
    let active_deployments = ActiveDeployments::default();
    restore_deployments_from_redb(&redb_store, &active_deployments);

    swarm_engine::world::ensure_world_config_exists("world.toml", "mods.lock");
    let mut world = create_world_with_mode(mode);
    if let Err(error) = add_feature_gated_mod_plugins(&mut world.app) {
        eprintln!("{error}");
        std::process::exit(1);
    }
    world.app.insert_resource(sandbox_backend.clone());
    world.app.insert_resource(active_deployments.clone());
    world.app.insert_resource(redb_store.clone());
    world
        .app
        .insert_resource(swarm_engine::InMemorySnapshotCache::in_process());
    if let Some((recovered_tick, expected_checksum, state)) = recovered_world {
        state.restore(world.app.world_mut());
        let actual_checksum = world.state_checksum();
        if actual_checksum != expected_checksum {
            eprintln!(
                "redb recovery failed: tick {recovered_tick} checksum expected={expected_checksum} actual={actual_checksum}"
            );
            std::process::exit(1);
        }
        world
            .app
            .world_mut()
            .resource_mut::<swarm_engine::CurrentTick>()
            .0 = recovered_tick.saturating_add(1);
        println!("redb recovered tick={recovered_tick}");
    } else {
        world.spawn_drone(
            1,
            10,
            10,
            vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
        );
    }

    let broadcaster: Arc<dyn TickBroadcaster> = if let Some(ref client) = shared_nats_client {
        Arc::new(swarm_engine::NatsTickBroadcaster::new(
            client.clone(),
            "swarm.realtime.v1",
        ))
    } else {
        Arc::new(swarm_engine::InMemoryTickBroadcaster::default())
    };

    let mut scheduler = swarm_engine::MultiPlayerTickScheduler::new(
        world,
        scheduler_executors(&sandbox_backend, &active_deployments),
        swarm_engine::RedbTickCommitter::new(redb_store),
        broadcaster,
    );
    let mut mcp_server = swarm_engine::McpServer::with_runtime_state_and_issuer(
        sandbox_backend.clone(),
        active_deployments.clone(),
        certificate_issuer,
    );
    if mcp_runtime_tx.send(mcp_dispatch_tx).is_err() {
        eprintln!("health server unavailable; mcp dispatcher was not installed");
    }

    loop {
        let tick = scheduler.tick_counter;
        metrics.set_authoritative_tick(tick);
        dispatch_pending_mcp_requests(
            &mut mcp_server,
            &mut scheduler.world,
            &mcp_dispatch_rx,
            metrics.authoritative_tick(),
        );
        let redb_ok = redb_connected;
        let nats_ok = nats_endpoint.as_ref().map(tcp_check).unwrap_or(false);
        let is_healthy = redb_ok && nats_ok;
        healthy.store(is_healthy, Ordering::Relaxed);
        metrics.set_dependencies(redb_ok, nats_ok);

        if !redb_ok {
            eprintln!(
                "tick={tick} dependency=redb status=degraded action=continue_without_persistence"
            );
        }
        if !nats_ok {
            eprintln!(
                "tick={tick} dependency=nats status=degraded action=continue_without_broadcast"
            );
        }
        let report = scheduler.tick();
        if !report.committed {
            eprintln!(
                "tick={tick} scheduler_commit=failed commit_failures={}",
                report.metrics.commit_failures
            );
        }
        println!(
            "tick={} state_checksum={} redb={} nats={}",
            tick,
            scheduler.world.state_checksum(),
            status(redb_ok),
            status(nats_ok)
        );
        thread::sleep(tick_interval);
    }
}

fn dispatch_pending_mcp_requests(
    server: &mut swarm_engine::McpServer,
    world: &mut swarm_engine::world::SwarmWorld,
    dispatch_rx: &mpsc::Receiver<McpDispatch>,
    tick: u64,
) {
    while let Ok(dispatch) = dispatch_rx.try_recv() {
        if dispatch.cancelled.load(Ordering::Acquire) {
            continue;
        }
        let response = server.handle_json_rpc_verified_proxy(
            world,
            swarm_engine::McpContext {
                player_id: dispatch.player_id,
                tick,
            },
            &dispatch.principal,
            dispatch.request,
        );
        let _ = dispatch.response_tx.send(response);
    }
}

fn add_feature_gated_mod_plugins(app: &mut bevy::prelude::App) -> Result<(), String> {
    let lock = app
        .world()
        .resource::<swarm_engine::plugins::PluginRegistry>()
        .lock
        .clone();
    lock.validate_enabled_features()?;
    #[cfg(not(any(
        feature = "mod_combat_core",
        feature = "mod_depot_storage",
        feature = "mod_empire_upkeep",
        feature = "mod_fog_of_war",
        feature = "mod_pve_spawning",
        feature = "mod_resource_decay",
        feature = "mod_special_attacks",
        feature = "mod_vanilla_boss"
    )))]
    {
        let _ = lock.runtime_config()?;
    }
    #[cfg(any(
        feature = "mod_combat_core",
        feature = "mod_depot_storage",
        feature = "mod_empire_upkeep",
        feature = "mod_fog_of_war",
        feature = "mod_pve_spawning",
        feature = "mod_resource_decay",
        feature = "mod_special_attacks",
        feature = "mod_vanilla_boss"
    ))]
    let runtime = lock.runtime_config()?;
    #[cfg(feature = "mod_special_attacks")]
    let mode = app.world().resource::<swarm_engine::WorldSettings>().mode;
    #[cfg(feature = "mod_combat_core")]
    if let Some(combat) = &runtime.combat_core {
        let mut config = swarm_mod_combat_core::CombatConfig::default();
        config.damage_multiplier_bp = combat.damage_multiplier;
        app.insert_resource(config);
        app.add_plugins(swarm_mod_combat_core::CombatCoreModPlugin);
    }
    #[cfg(feature = "mod_depot_storage")]
    if let Some(depot) = &runtime.depot_storage {
        app.insert_resource(swarm_mod_depot_storage::DepotStorageConfig {
            repair_range: depot.repair_range,
            repair_capacity: depot.repair_capacity,
            depot_hits: depot.depot_hits,
            depot_capacity: depot.depot_capacity,
        });
        app.add_plugins(swarm_mod_depot_storage::DepotStorageModPlugin);
    }
    #[cfg(feature = "mod_empire_upkeep")]
    if runtime.empire_upkeep.is_some() {
        app.add_plugins(swarm_mod_empire_upkeep::EmpireUpkeepModPlugin);
    }
    #[cfg(feature = "mod_fog_of_war")]
    if let Some(fog) = &runtime.fog_of_war {
        app.insert_resource(swarm_mod_fog_of_war::VisibilityConfig {
            fog_of_war: fog.fog_of_war,
        });
        app.add_plugins(swarm_mod_fog_of_war::FogOfWarModPlugin);
    }
    #[cfg(feature = "mod_pve_spawning")]
    if let Some(pve) = &runtime.pve_spawning {
        app.insert_resource(swarm_mod_pve_spawning::PveSpawningConfig {
            spawn_interval: pve.spawn_interval,
            max_npcs_per_room: pve.max_npcs_per_room,
            npc_drone_body: pve.npc_drone_body.clone(),
            npc_drop_table: pve.npc_drop_table.clone(),
        });
        app.add_plugins(swarm_mod_pve_spawning::PveSpawningModPlugin);
    }
    #[cfg(feature = "mod_resource_decay")]
    if let Some(decay) = &runtime.resource_decay {
        app.insert_resource(swarm_mod_resource_decay::ResourceDecayConfig {
            decay_rate_ppm: decay.decay_rate_ppm,
            per_resource_decay_rate_ppm: decay.per_resource_decay_rate_ppm.clone(),
        });
        app.add_plugins(swarm_mod_resource_decay::ResourceDecayModPlugin);
    }
    #[cfg(feature = "mod_special_attacks")]
    if let Some(special) = &runtime.special_attacks {
        app.insert_resource(swarm_mod_special_attacks::SpecialAttacksConfig {
            enabled: special.runtime_kinds_for_mode(mode),
            damage_multiplier: special.damage_multiplier,
        });
        app.add_plugins(swarm_mod_special_attacks::SpecialAttacksModPlugin);
    }
    #[cfg(feature = "mod_vanilla_boss")]
    if let Some(boss) = &runtime.vanilla_boss {
        let mut plugin = swarm_mod_vanilla_boss::VanillaBossPlugin::default();
        plugin.arena_bosses_enabled = boss.arena_bosses_enabled;
        plugin.world_bosses_enabled = boss.world_bosses_enabled;
        plugin.boss_spawn_interval = boss.boss_spawn_interval;
        plugin.boss_templates = boss
            .boss_templates
            .iter()
            .map(|template| swarm_mod_vanilla_boss::BossTemplate {
                name: template.name.clone(),
                mode: match template.mode {
                    swarm_engine::plugins::BossModeConfig::World => {
                        swarm_mod_vanilla_boss::BossMode::World
                    }
                    swarm_engine::plugins::BossModeConfig::Arena => {
                        swarm_mod_vanilla_boss::BossMode::Arena
                    }
                },
                hits: template.hits,
                phases: template.phases.clone(),
                drops: template.drops.clone(),
                spawn_position: template.spawn_position,
            })
            .collect();
        app.insert_resource(swarm_mod_vanilla_boss::WorldConfig {
            world_bosses_enabled: boss.world_bosses_enabled,
            arena_bosses_enabled: boss.arena_bosses_enabled,
            boss_spawn_interval: boss.boss_spawn_interval,
        });
        app.add_plugins(plugin);
    }
    Ok(())
}

fn scheduler_executors(
    backend: &SandboxBackend,
    active_deployments: &ActiveDeployments,
) -> HashMap<PlayerId, Box<dyn PlayerExecutor>> {
    HashMap::from([(
        1,
        Box::new(SandboxPlayerExecutor::new(
            backend.clone(),
            active_deployments.clone(),
        )) as Box<dyn PlayerExecutor>,
    )])
}

struct SandboxPlayerExecutor {
    backend: SandboxBackend,
    active_deployments: ActiveDeployments,
    runtime: tokio::runtime::Runtime,
}

impl SandboxPlayerExecutor {
    fn new(backend: SandboxBackend, active_deployments: ActiveDeployments) -> Self {
        Self {
            backend,
            active_deployments,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build sandbox executor runtime"),
        }
    }
}

fn sandbox_collect_metrics(
    metrics: &swarm_engine::sandbox_transport::SandboxExecutionMetrics,
) -> PlayerCollectMetrics {
    PlayerCollectMetrics {
        fuel_consumed: metrics.fuel_consumed,
        refund_events: 0,
        refunded: 0,
    }
}

impl PlayerExecutor for SandboxPlayerExecutor {
    fn collect(&mut self, snapshot: TickSnapshot) -> Result<Vec<CommandIntent>, ExecutorError> {
        self.collect_with_metrics(snapshot)
            .map(|output| output.intents)
    }

    fn collect_with_metrics(
        &mut self,
        snapshot: TickSnapshot,
    ) -> Result<PlayerCollectOutput, ExecutorError> {
        match &self.backend {
            SandboxBackend::Remote { nats_client, .. } => {
                let Some(deployment) = self
                    .active_deployments
                    .active_for_player(snapshot.player_id, snapshot.tick)
                else {
                    return Ok(PlayerCollectOutput {
                        intents: Vec::new(),
                        metrics: PlayerCollectMetrics::default(),
                    });
                };
                let player_id = snapshot.player_id.to_string();
                let room_id = deployment.room_id.0.to_string();
                let snapshot_json = serde_json::to_vec(&snapshot)
                    .map_err(|error| ExecutorError::Error(error.to_string()))?;
                let reply = self
                    .runtime
                    .block_on(execute_tick_remote(
                        nats_client,
                        snapshot.tick,
                        &player_id,
                        &room_id,
                        &snapshot_json,
                        &deployment.module_hash,
                        swarm_engine::MAX_FUEL,
                    ))
                    .map_err(ExecutorError::Error)?;
                if reply.status.eq_ignore_ascii_case("timeout") {
                    return Err(ExecutorError::Timeout);
                }
                if !reply.errors.is_empty() {
                    return Err(ExecutorError::Error(reply.errors.join("; ")));
                }
                let metrics = sandbox_collect_metrics(&reply.metrics);
                let intents = reply
                    .commands
                    .into_iter()
                    .map(serde_json::from_value)
                    .collect::<Result<Vec<CommandIntent>, _>>()
                    .map_err(|error| ExecutorError::Error(error.to_string()))?;
                Ok(PlayerCollectOutput { intents, metrics })
            }
        }
    }
}

fn connect_nats_client(config: &NatsSecurityConfig) -> Result<async_nats::Client, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let mut options = async_nats::ConnectOptions::new().require_tls(config.tls_required);
        if let Some(path) = &config.ca_file {
            options = options.add_root_certificates(path.clone());
        }
        if let (Some(cert), Some(key)) = (&config.client_cert_file, &config.client_key_file) {
            options = options.add_client_certificate(cert.clone(), key.clone());
        }
        if let Some(path) = &config.credentials_file {
            options = options
                .credentials_file(path)
                .await
                .map_err(|error| error.to_string())?;
        }
        options
            .connect(&config.url)
            .await
            .map_err(|error| error.to_string())
    })
}

fn connect_nats_client_with_retry(
    nats_config: &NatsSecurityConfig,
    healthy: &Arc<AtomicBool>,
    retry_interval: Duration,
) -> async_nats::Client {
    loop {
        match connect_nats_client(nats_config) {
            Ok(client) => {
                println!("sandbox_backend=nats nats_client=available");
                return client;
            }
            Err(error) => {
                healthy.store(false, Ordering::Relaxed);
                eprintln!(
                    "sandbox_backend=nats nats_client=unavailable error={error} action=retry"
                );
                thread::sleep(retry_interval);
            }
        }
    }
}

impl NatsSecurityConfig {
    fn from_env(url: &str) -> Result<Self, String> {
        let mode = engine_mode_from_env()?;
        let tls_required = env::var("NATS_TLS_REQUIRED").ok();
        Self::from_values_for_mode(
            &mode,
            url,
            tls_required.as_deref(),
            configured_file("NATS_TLS_CA_FILE")?,
            configured_file("NATS_TLS_CERT_FILE")?,
            configured_file("NATS_TLS_KEY_FILE")?,
            configured_file("NATS_CREDENTIALS_FILE")?,
        )
    }

    fn from_values_for_mode(
        mode: &str,
        url: &str,
        tls_required: Option<&str>,
        ca_file: Option<PathBuf>,
        client_cert_file: Option<PathBuf>,
        client_key_file: Option<PathBuf>,
        credentials_file: Option<PathBuf>,
    ) -> Result<Self, String> {
        let mode = mode.trim().to_ascii_lowercase();
        if !is_valid_engine_mode(&mode) {
            return Err(format!("invalid SWARM_ENGINE_MODE `{mode}`"));
        }
        let config = Self::from_values(
            url,
            tls_required,
            ca_file,
            client_cert_file,
            client_key_file,
            credentials_file,
        )?;
        if mode == ENGINE_MODE_PRODUCTION && !config.tls_required {
            return Err("production engine requires NATS TLS".to_string());
        }
        if mode == ENGINE_MODE_PRODUCTION && config.credentials_file.is_none() {
            return Err("production engine requires NATS role credentials".to_string());
        }
        Ok(config)
    }

    fn from_values(
        url: &str,
        tls_required: Option<&str>,
        ca_file: Option<PathBuf>,
        client_cert_file: Option<PathBuf>,
        client_key_file: Option<PathBuf>,
        credentials_file: Option<PathBuf>,
    ) -> Result<Self, String> {
        let url = url.trim();
        if url.is_empty() {
            return Err("NATS_URL must be non-empty".to_string());
        }
        if client_cert_file.is_some() != client_key_file.is_some() {
            return Err(
                "NATS_TLS_CERT_FILE and NATS_TLS_KEY_FILE must be configured together".to_string(),
            );
        }
        let explicitly_disabled = tls_required.is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        });
        let configured_tls = ca_file.is_some()
            || client_cert_file.is_some()
            || credentials_file.is_some()
            || url.starts_with("tls://");
        if explicitly_disabled && configured_tls {
            return Err(
                "NATS_TLS_REQUIRED cannot be false when TLS or credentials are configured"
                    .to_string(),
            );
        }
        let tls_required = match tls_required.map(str::trim) {
            None => configured_tls,
            Some(value)
                if matches!(
                    value.to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                ) =>
            {
                true
            }
            Some(value)
                if matches!(
                    value.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                ) =>
            {
                false
            }
            Some(_) => return Err("NATS_TLS_REQUIRED must be a boolean".to_string()),
        };
        for (name, path) in [
            ("NATS_TLS_CA_FILE", ca_file.as_ref()),
            ("NATS_TLS_CERT_FILE", client_cert_file.as_ref()),
            ("NATS_TLS_KEY_FILE", client_key_file.as_ref()),
            ("NATS_CREDENTIALS_FILE", credentials_file.as_ref()),
        ] {
            if let Some(path) = path {
                let metadata = fs::metadata(path).map_err(|error| {
                    format!("{name} is not readable ({}): {error}", path.display())
                })?;
                if !metadata.is_file() {
                    return Err(format!("{name} must reference a file: {}", path.display()));
                }
            }
        }
        Ok(Self {
            url: url.to_string(),
            tls_required,
            ca_file,
            client_cert_file,
            client_key_file,
            credentials_file,
        })
    }
}

fn engine_mode_from_env() -> Result<String, String> {
    let mode = env::var("SWARM_ENGINE_MODE")
        .or_else(|_| env::var("SWARM_ENV"))
        .unwrap_or_else(|_| ENGINE_MODE_PRODUCTION.to_string());
    let mode = mode.trim().to_ascii_lowercase();
    if !is_valid_engine_mode(&mode) {
        return Err(format!("invalid SWARM_ENGINE_MODE `{mode}`"));
    }
    Ok(mode)
}

fn is_valid_engine_mode(mode: &str) -> bool {
    matches!(
        mode,
        ENGINE_MODE_PRODUCTION | ENGINE_MODE_DEVELOPMENT | ENGINE_MODE_TEST
    )
}

fn certificate_issuer_from_env() -> Result<swarm_engine::CertificateIssuer, String> {
    let mode = engine_mode_from_env()?;
    let key_file = configured_file(ISSUER_KEY_FILE_ENV)?;
    let key_b64 = configured_secret(ISSUER_KEY_B64_ENV)?;
    certificate_issuer_from_values_for_mode(&mode, key_file, key_b64)
}

fn certificate_issuer_from_values_for_mode(
    mode: &str,
    key_file: Option<PathBuf>,
    key_b64: Option<String>,
) -> Result<swarm_engine::CertificateIssuer, String> {
    let mode = mode.trim().to_ascii_lowercase();
    if !is_valid_engine_mode(&mode) {
        return Err(format!("invalid SWARM_ENGINE_MODE `{mode}`"));
    }
    if mode != ENGINE_MODE_PRODUCTION {
        return Ok(swarm_engine::CertificateIssuer::new());
    }
    match (key_file, key_b64) {
        (Some(_), Some(_)) => Err(format!(
            "production engine requires exactly one issuer seed source: {ISSUER_KEY_FILE_ENV} or {ISSUER_KEY_B64_ENV}"
        )),
        (None, None) => Err(format!(
            "production engine requires exactly one issuer seed source: {ISSUER_KEY_FILE_ENV} or {ISSUER_KEY_B64_ENV}"
        )),
        (Some(path), None) => {
            let mut seed = read_issuer_seed_file(&path)?;
            let issuer = issuer_from_seed(&seed);
            seed.fill(0);
            issuer
        }
        (None, Some(encoded)) => {
            let mut seed = decode_issuer_seed_b64(&encoded)?;
            let issuer = issuer_from_seed(&seed);
            seed.fill(0);
            issuer
        }
    }
}

fn read_issuer_seed_file(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("{ISSUER_KEY_FILE_ENV} cannot be inspected: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{ISSUER_KEY_FILE_ENV} must not be a symlink"));
    }
    if !metadata.file_type().is_file() {
        return Err(format!(
            "{ISSUER_KEY_FILE_ENV} must point to a regular file"
        ));
    }
    #[cfg(unix)]
    validate_issuer_seed_file_metadata(&metadata, effective_uid())?;
    let seed =
        fs::read(path).map_err(|error| format!("{ISSUER_KEY_FILE_ENV} cannot be read: {error}"))?;
    if seed.len() != swarm_engine::CertificateIssuer::ED25519_SEED_LEN {
        return Err(format!(
            "{ISSUER_KEY_FILE_ENV} must contain exactly {} bytes",
            swarm_engine::CertificateIssuer::ED25519_SEED_LEN
        ));
    }
    Ok(seed)
}

#[cfg(unix)]
fn validate_issuer_seed_file_metadata(
    metadata: &fs::Metadata,
    owner_uid: u32,
) -> Result<(), String> {
    if metadata.mode() & 0o077 != 0 {
        return Err(format!("{ISSUER_KEY_FILE_ENV} must be owner-only"));
    }
    if metadata.uid() != owner_uid {
        return Err(format!(
            "{ISSUER_KEY_FILE_ENV} must be owned by the current user"
        ));
    }
    Ok(())
}

fn issuer_from_seed(seed: &[u8]) -> Result<swarm_engine::CertificateIssuer, String> {
    swarm_engine::CertificateIssuer::from_seed(seed).map_err(|error| error.message)
}

fn decode_issuer_seed_b64(encoded: &str) -> Result<Vec<u8>, String> {
    let seed = decode_base64_secret(encoded.trim())?;
    if seed.len() != swarm_engine::CertificateIssuer::ED25519_SEED_LEN {
        return Err(format!(
            "{ISSUER_KEY_B64_ENV} must decode to exactly {} bytes",
            swarm_engine::CertificateIssuer::ED25519_SEED_LEN
        ));
    }
    Ok(seed)
}

fn decode_base64_secret(input: &str) -> Result<Vec<u8>, String> {
    if input.is_empty() || !input.len().is_multiple_of(4) {
        return Err(format!("{ISSUER_KEY_B64_ENV} is not valid base64"));
    }
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let bytes = input.as_bytes();
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let is_last = index == bytes.len() / 4 - 1;
        let a = base64_secret_value(chunk[0])?;
        let b = base64_secret_value(chunk[1])?;
        let c_padding = chunk[2] == b'=';
        let d_padding = chunk[3] == b'=';
        if c_padding && !d_padding {
            return Err(format!("{ISSUER_KEY_B64_ENV} is not valid base64"));
        }
        if (c_padding || d_padding) && !is_last {
            return Err(format!("{ISSUER_KEY_B64_ENV} is not valid base64"));
        }
        let c = if c_padding {
            0
        } else {
            base64_secret_value(chunk[2])?
        };
        let d = if d_padding {
            0
        } else {
            base64_secret_value(chunk[3])?
        };
        output.push((a << 2) | (b >> 4));
        if !c_padding {
            output.push((b << 4) | (c >> 2));
        }
        if !d_padding {
            output.push((c << 6) | d);
        }
    }
    Ok(output)
}

fn base64_secret_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(format!("{ISSUER_KEY_B64_ENV} is not valid base64")),
    }
}

fn configured_file(name: &str) -> Result<Option<PathBuf>, String> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(PathBuf::from(value))),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("{name} is not valid unicode: {error}")),
    }
}

fn configured_secret(name: &str) -> Result<Option<String>, String> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("{name} is not valid unicode: {error}")),
    }
}

fn parse_mode_arg(args: Vec<String>) -> Result<(WorldMode, Vec<String>), String> {
    let mut mode = WorldMode::Default;
    let mut remaining = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--mode" {
            index += 1;
            mode = parse_world_mode(args.get(index).ok_or("missing value after --mode")?)?;
        } else if let Some(value) = arg.strip_prefix("--mode=") {
            mode = parse_world_mode(value)?;
        } else {
            remaining.push(arg.clone());
        }
        index += 1;
    }
    Ok((mode, remaining))
}

fn parse_world_mode(value: &str) -> Result<WorldMode, String> {
    match value {
        "default" => Ok(WorldMode::Default),
        "tutorial" => Ok(WorldMode::Tutorial),
        "novice" => Ok(WorldMode::Novice),
        "arena" => Ok(WorldMode::Arena),
        _ => Err(format!(
            "--mode must be default, tutorial, novice, or arena, got {value}"
        )),
    }
}

fn run_sim(args: &[String]) -> Result<(), String> {
    let mut ticks = 5_000_u64;
    let mut speed = "100x".to_string();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--ticks" {
            index += 1;
            ticks = parse_sim_ticks(args.get(index).ok_or("missing value after --ticks")?)?;
        } else if let Some(value) = arg.strip_prefix("--ticks=") {
            ticks = parse_sim_ticks(value)?;
        } else if arg == "--speed" {
            index += 1;
            speed = parse_sim_speed(args.get(index).ok_or("missing value after --speed")?)?;
        } else if let Some(value) = arg.strip_prefix("--speed=") {
            speed = parse_sim_speed(value)?;
        } else {
            return Err(format!(
                "usage: sim [--ticks N|--ticks=N] [--speed MULTIPLIER|--speed=MULTIPLIER]; unknown argument: {arg}"
            ));
        }
        index += 1;
    }

    println!(
        "mode=local-sim caveat=training-only-not-authoritative-no-redb-no-nats ticks={ticks} speed={speed}"
    );
    let started_at = std::time::Instant::now();
    let mut world = create_local_simulation_world();
    let mut checksum;
    for tick in 1..=ticks {
        world.run_tick();
        checksum = world.state_checksum();
        if tick == 1 || tick == ticks || tick % 1_000 == 0 {
            println!("progress tick={tick}/{ticks} state_checksum={checksum}");
        }
    }
    let elapsed_ms = started_at.elapsed().as_millis();
    let summary = summarize_local_simulation(&mut world, ticks, elapsed_ms);
    println!(
        "summary mode=local-sim caveat=training-only ticks={ticks} speed={speed} final_state_checksum={checksum} elapsed_ms={elapsed_ms} drones={drones} sources={sources} structures={structures} controllers={controllers}",
        checksum = summary.final_state_checksum,
        elapsed_ms = summary.elapsed_ms,
        drones = summary.drones,
        sources = summary.sources,
        structures = summary.structures,
        controllers = summary.controllers,
    );
    Ok(())
}

fn parse_sim_ticks(value: &str) -> Result<u64, String> {
    let ticks = value
        .parse::<u64>()
        .map_err(|_| format!("--ticks must be a positive integer, got {value}"))?;
    if ticks == 0 {
        return Err("--ticks must be greater than zero".to_string());
    }
    Ok(ticks)
}

fn parse_sim_speed(value: &str) -> Result<String, String> {
    let multiplier = value
        .trim()
        .strip_suffix('x')
        .ok_or_else(|| format!("--speed must use an x multiplier like 100x, got {value}"))?;
    let parsed = multiplier
        .parse::<u64>()
        .map_err(|_| format!("--speed multiplier must be a positive integer, got {value}"))?;
    if parsed == 0 {
        return Err("--speed multiplier must be greater than zero".to_string());
    }
    Ok(format!("{parsed}x"))
}

fn start_health_server(
    addr: String,
    healthy: Arc<AtomicBool>,
    metrics: Arc<metrics::EngineMetrics>,
    mcp_runtime_rx: mpsc::Receiver<mpsc::Sender<McpDispatch>>,
    mode: WorldMode,
) {
    thread::spawn(move || {
        let mut mcp_state = None;
        let sdk_output_dir =
            env::var("SDK_OUTPUT_DIR").unwrap_or_else(|_| "/app/sdk-output".to_string());
        let listener = match TcpListener::bind(&addr) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("health server bind failed addr={addr} error={error}");
                return;
            }
        };
        println!("health server listening addr={addr}");

        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    install_pending_mcp_state(&mut mcp_state, &mcp_runtime_rx);
                    respond_http(
                        &mut stream,
                        healthy.load(Ordering::Relaxed),
                        &metrics,
                        Path::new(&sdk_output_dir),
                        mcp_state.as_mut(),
                        mode,
                    );
                }
                Err(error) => eprintln!("health server connection failed error={error}"),
            }
        }
    });
}

fn install_pending_mcp_state(
    mcp_state: &mut Option<McpHttpState>,
    mcp_runtime_rx: &mpsc::Receiver<mpsc::Sender<McpDispatch>>,
) {
    while let Ok(dispatch_tx) = mcp_runtime_rx.try_recv() {
        *mcp_state = Some(McpHttpState {
            dispatch_tx,
            proxy_verifier: swarm_engine::mcp::GatewayProxyVerifier::from_env()
                .map_err(|error| error.message),
            seen_proxy_nonces: ProxyNonceStore::open(proxy_nonce_store_path()),
        });
        println!("health server mcp dispatcher installed");
    }
}

fn restore_deployments_from_redb(
    redb_store: &swarm_engine::RedbStore,
    active_deployments: &ActiveDeployments,
) {
    let manifests = match redb_store.recover_deploy_manifests() {
        Ok(manifests) => manifests,
        Err(error) => {
            eprintln!("deploy recovery unavailable: {error}");
            return;
        }
    };
    for manifest in manifests {
        let Some(room_id) = manifest
            .module_slot
            .strip_prefix("room:")
            .and_then(|value| value.parse::<u32>().ok())
            .map(RoomId)
        else {
            eprintln!(
                "deploy recovery failed deploy_id={} reason=invalid_module_slot",
                manifest.deploy_id
            );
            let _ = redb_store.mark_deploy_recovery_failed(
                &manifest.deploy_id,
                format!("invalid module_slot {}", manifest.module_slot),
            );
            continue;
        };
        let artifact = match redb_store.read_verified_deploy_artifact(&manifest) {
            Ok(artifact) => artifact,
            Err(error) => {
                eprintln!(
                    "deploy recovery failed deploy_id={} reason={error}",
                    manifest.deploy_id
                );
                let _ = redb_store.mark_deploy_recovery_failed(
                    &manifest.deploy_id,
                    format!("deploy artifact unavailable: {error}"),
                );
                continue;
            }
        };
        let deployment = ActiveDeployment {
            deploy_id: manifest.deploy_id,
            world_id: manifest.world_id,
            module_slot: manifest.module_slot,
            player_id: manifest.player_id,
            room_id,
            drone_id: manifest.drone_id,
            module_hash: manifest.wasm_module_hash,
            metadata_hash: manifest.metadata_hash,
            signed_payload_hash: manifest.signed_payload_hash,
            compiled_artifact_hash: manifest.compiled_artifact_hash,
            client_version_counter: manifest.client_version_counter,
            redb_version_counter: manifest.redb_version_counter,
            certificate_id: manifest.certificate_id,
            certificate_fingerprint: manifest.certificate_fingerprint,
            transport: manifest.transport,
            signed_at: manifest.signed_at,
            accepted_at_tick: manifest.accepted_at_tick,
            wasm_bytes: artifact.wasm_bytes,
            load_after_tick: manifest.activation_tick,
        };
        if let Err(error) = register_recovered_module_fetch_artifact(
            deployment.module_hash,
            deployment.wasm_bytes.clone(),
            "redb-recovered-v1",
        ) {
            eprintln!(
                "deploy recovery failed deploy_id={} reason={error}",
                deployment.deploy_id
            );
            let _ = redb_store.mark_deploy_recovery_failed(
                &deployment.deploy_id,
                format!("module fetch cache recovery failed: {error}"),
            );
            continue;
        }
        if manifest.status == "active" {
            active_deployments.activate(deployment);
        } else {
            active_deployments.stage_activation(deployment);
        }
    }
}

fn respond_http(
    stream: &mut TcpStream,
    healthy: bool,
    metrics: &metrics::EngineMetrics,
    sdk_output_dir: &Path,
    mcp_state: Option<&mut McpHttpState>,
    mode: WorldMode,
) {
    let request = match read_http_request(stream) {
        Some(request) => request,
        None => {
            respond_bytes(
                stream,
                "HTTP/1.1 400 Bad Request",
                "text/plain; charset=utf-8",
                b"bad request\n",
            );
            return;
        }
    };

    if request.path == "/" || request.path == "/healthz" {
        respond_health(stream, healthy);
    } else if request.path == "/metrics" {
        if request.method.eq_ignore_ascii_case("GET") {
            respond_metrics(stream, metrics);
        } else {
            respond_bytes(
                stream,
                "HTTP/1.1 405 Method Not Allowed",
                "text/plain; charset=utf-8",
                b"method not allowed\n",
            );
        }
    } else if request.path == "/mcp" {
        respond_mcp(stream, request, mcp_state, mode);
    } else if let Some(sdk_path) = request.path.strip_prefix("/sdk/") {
        respond_sdk_file(stream, sdk_output_dir, sdk_path);
    } else {
        respond_not_found(stream);
    }
}

fn read_http_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut buffer = vec![0_u8; 4096];
    let mut bytes_read = stream.read(&mut buffer).ok()?;
    if bytes_read == 0 {
        return None;
    }
    buffer.truncate(bytes_read);

    let header_end = loop {
        if let Some(index) = find_header_end(&buffer) {
            break index;
        }
        let mut chunk = [0_u8; 4096];
        bytes_read = stream.read(&mut chunk).ok()?;
        if bytes_read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..bytes_read]);
        if buffer.len() > 1024 * 1024 {
            return None;
        }
    };

    let header = std::str::from_utf8(&buffer[..header_end]).ok()?;
    let mut lines = header.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_PRE_AUTH_HTTP_BODY_BYTES {
        return None;
    }
    let body_start = header_end + 4;
    while buffer.len().saturating_sub(body_start) < content_length {
        let mut chunk = [0_u8; 4096];
        bytes_read = stream.read(&mut chunk).ok()?;
        if bytes_read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..bytes_read]);
        if buffer.len().saturating_sub(body_start) > MAX_PRE_AUTH_HTTP_BODY_BYTES {
            return None;
        }
    }

    Some(HttpRequest {
        method,
        path,
        headers,
        body: buffer[body_start..body_start + content_length].to_vec(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn respond_mcp(
    stream: &mut TcpStream,
    request: HttpRequest,
    mcp_state: Option<&mut McpHttpState>,
    _mode: WorldMode,
) {
    if request.method != "POST" {
        respond_bytes(
            stream,
            "HTTP/1.1 405 Method Not Allowed",
            "text/plain; charset=utf-8",
            b"method not allowed\n",
        );
        return;
    }
    let Some(mcp_state) = mcp_state else {
        respond_bytes(
            stream,
            "HTTP/1.1 503 Service Unavailable",
            "text/plain; charset=utf-8",
            b"mcp unavailable\n",
        );
        return;
    };

    let state = mcp_state;
    let verifier = match state.proxy_verifier.as_ref() {
        Ok(verifier) => verifier,
        Err(error) => {
            respond_bytes(
                stream,
                "HTTP/1.1 503 Service Unavailable",
                "text/plain; charset=utf-8",
                format!("{error}\n").as_bytes(),
            );
            return;
        }
    };

    let principal = match proxy_principal(&request) {
        Ok(principal) => principal,
        Err(error) => {
            respond_bytes(
                stream,
                "HTTP/1.1 401 Unauthorized",
                "text/plain; charset=utf-8",
                format!("{error}\n").as_bytes(),
            );
            return;
        }
    };
    let player_id = principal.player_id;
    let tick_header = request.headers.get("x-swarm-tick").cloned();
    if let Err(error) = proxy_tick(tick_header.as_deref()) {
        respond_bytes(
            stream,
            "HTTP/1.1 401 Unauthorized",
            "text/plain; charset=utf-8",
            format!("{error}\n").as_bytes(),
        );
        return;
    }
    let request_tick = tick_header.as_deref().unwrap_or("");

    let mcp_principal = match verify_proxy_signature(
        &request,
        verifier,
        &principal,
        request_tick,
        &mut state.seen_proxy_nonces,
    ) {
        Ok(principal) => principal,
        Err(error) => {
            respond_bytes(
                stream,
                "HTTP/1.1 401 Unauthorized",
                "text/plain; charset=utf-8",
                format!("{error}\n").as_bytes(),
            );
            return;
        }
    };

    let rpc_request = match serde_json::from_slice::<swarm_engine::JsonRpcRequest>(&request.body) {
        Ok(request) => request,
        Err(error) => {
            respond_bytes(
                stream,
                "HTTP/1.1 400 Bad Request",
                "application/json",
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}})
                    .to_string()
                    .as_bytes(),
            );
            return;
        }
    };
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    if state
        .dispatch_tx
        .send(McpDispatch {
            player_id,
            principal: mcp_principal,
            request: rpc_request,
            response_tx,
            cancelled: Arc::clone(&cancelled),
        })
        .is_err()
    {
        respond_bytes(
            stream,
            "HTTP/1.1 503 Service Unavailable",
            "text/plain; charset=utf-8",
            b"mcp dispatcher unavailable\n",
        );
        return;
    }
    let response = match response_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(response) => response,
        Err(error) => {
            cancelled.store(true, Ordering::Release);
            respond_bytes(
                stream,
                "HTTP/1.1 503 Service Unavailable",
                "text/plain; charset=utf-8",
                format!("mcp dispatch failed: {error}\n").as_bytes(),
            );
            return;
        }
    };
    match serde_json::to_vec(&response) {
        Ok(body) => respond_bytes(stream, "HTTP/1.1 200 OK", "application/json", &body),
        Err(error) => respond_bytes(
            stream,
            "HTTP/1.1 500 Internal Server Error",
            "text/plain; charset=utf-8",
            format!("{error}\n").as_bytes(),
        ),
    }
}

fn proxy_nonce_store_path() -> Result<PathBuf, String> {
    let mode = engine_mode_from_env()?;
    let configured = env::var(PROXY_NONCE_PATH_ENV).ok().map(PathBuf::from);
    let path = match (mode.as_str(), configured) {
        (ENGINE_MODE_PRODUCTION, Some(path)) => path,
        (ENGINE_MODE_PRODUCTION, None) => PathBuf::from(PRODUCTION_PROXY_NONCE_PATH),
        (_, Some(path)) => path,
        (_, None) => PathBuf::from(DEFAULT_PROXY_NONCE_PATH),
    };
    validate_proxy_nonce_path_for_mode(&mode, &path)?;
    Ok(path)
}

impl ProxyNonceStore {
    fn open(path: Result<PathBuf, String>) -> Self {
        let path = match path {
            Ok(path) => path,
            Err(error) => {
                return Self {
                    path: PathBuf::from(PRODUCTION_PROXY_NONCE_PATH),
                    seen: BTreeMap::new(),
                    persistence_error: Some(error),
                };
            }
        };
        match Self::load(path.clone()) {
            Ok(store) => store,
            Err(error) => Self {
                path,
                seen: BTreeMap::new(),
                persistence_error: Some(error),
            },
        }
    }

    fn load(path: PathBuf) -> Result<Self, String> {
        ensure_proxy_nonce_parent(&path)?;
        let mut store = Self {
            path,
            seen: BTreeMap::new(),
            persistence_error: None,
        };
        let now = current_unix_timestamp()?;
        store.seen = store.with_store_lock(|| {
            let mut seen = load_proxy_nonce_entries(&store.path)?;
            let before = seen.len();
            seen.retain(|_, timestamp| now - *timestamp <= MCP_PROXY_REPLAY_WINDOW_SECONDS);
            if seen.len() != before {
                persist_proxy_nonce_entries(&store.path, &seen)?;
            }
            Ok(seen)
        })?;
        Ok(store)
    }

    fn contains(&self, nonce: &str) -> Result<bool, String> {
        if let Some(error) = &self.persistence_error {
            return Err(format!("proxy nonce store unavailable: {error}"));
        }
        Ok(self.seen.contains_key(nonce))
    }

    fn record_verified(&mut self, nonce: &str, timestamp: i64, now: i64) -> Result<(), String> {
        if let Some(error) = &self.persistence_error {
            return Err(format!("proxy nonce store unavailable: {error}"));
        }
        if nonce.contains('\n') || nonce.contains('\r') || nonce.contains('\t') {
            return Err("invalid proxy nonce".to_string());
        }
        let mut locked_seen = self.with_store_lock(|| {
            let mut locked_seen = load_proxy_nonce_entries(&self.path)?;
            locked_seen.retain(|_, seen_at| now - *seen_at <= MCP_PROXY_REPLAY_WINDOW_SECONDS);
            if locked_seen.contains_key(nonce) {
                return Err("proxy nonce replayed".to_string());
            }
            locked_seen.insert(nonce.to_string(), timestamp);
            persist_proxy_nonce_entries(&self.path, &locked_seen)?;
            Ok(locked_seen)
        })?;
        std::mem::swap(&mut self.seen, &mut locked_seen);
        Ok(())
    }

    fn prune_expired(&mut self, now: i64) -> bool {
        let before = self.seen.len();
        self.seen
            .retain(|_, timestamp| now - *timestamp <= MCP_PROXY_REPLAY_WINDOW_SECONDS);
        before != self.seen.len()
    }

    #[cfg(test)]
    fn persist(&self) -> Result<(), String> {
        self.with_store_lock(|| persist_proxy_nonce_entries(&self.path, &self.seen))
    }

    fn with_store_lock<T>(&self, action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let lock_path = self.path.with_extension("lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            #[cfg(target_os = "linux")]
            options.custom_flags(O_NOFOLLOW_FLAG);
        }
        let lock_file = options
            .open(&lock_path)
            .map_err(|error| format!("proxy nonce store lock open failed: {error}"))?;
        lock_file
            .lock()
            .map_err(|error| format!("proxy nonce store lock failed: {error}"))?;
        let result = action();
        let unlock_result = lock_file.unlock();
        match (result, unlock_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(format!("proxy nonce store unlock failed: {error}")),
        }
    }
}

fn validate_proxy_nonce_path_for_mode(mode: &str, path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err(format!("{PROXY_NONCE_PATH_ENV} must not be empty"));
    }
    if mode == ENGINE_MODE_PRODUCTION {
        if !path.is_absolute() {
            return Err(format!(
                "{PROXY_NONCE_PATH_ENV} must be absolute in production"
            ));
        }
        if path.starts_with(env::temp_dir()) || path.starts_with("/tmp") {
            return Err(format!(
                "{PROXY_NONCE_PATH_ENV} must not be under /tmp in production"
            ));
        }
    }
    Ok(())
}

fn ensure_proxy_nonce_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "proxy nonce store path must have a parent directory".to_string())?;
    let parent_exists = parent.exists();
    fs::create_dir_all(parent)
        .map_err(|error| format!("proxy nonce store mkdir failed: {error}"))?;
    #[cfg(unix)]
    if !parent_exists {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("proxy nonce store parent chmod failed: {error}"))?;
    }
    validate_proxy_nonce_parent(parent)?;
    Ok(())
}

fn validate_proxy_nonce_parent(parent: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(parent)
        .map_err(|error| format!("proxy nonce store parent inspect failed: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("proxy nonce store parent must not be a symlink".to_string());
    }
    if !metadata.file_type().is_dir() {
        return Err("proxy nonce store parent must be a directory".to_string());
    }
    #[cfg(unix)]
    {
        if metadata.mode() & 0o077 != 0 {
            return Err("proxy nonce store parent must be private".to_string());
        }
        if metadata.uid() != effective_uid() {
            return Err("proxy nonce store parent must be owned by the engine user".to_string());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn effective_uid() -> u32 {
    unsafe { geteuid() }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn effective_uid() -> u32 {
    fs::metadata(".")
        .map(|metadata| metadata.uid())
        .unwrap_or(0)
}

fn validate_proxy_nonce_target(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err("proxy nonce store must not be a symlink".to_string());
            }
            if !metadata.file_type().is_file() {
                return Err("proxy nonce store must be a regular file".to_string());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("proxy nonce store inspect failed: {error}")),
    }
    Ok(())
}

fn load_proxy_nonce_entries(path: &Path) -> Result<BTreeMap<String, i64>, String> {
    validate_proxy_nonce_target(path)?;
    let mut seen = BTreeMap::new();
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(seen),
        Err(error) => return Err(format!("proxy nonce store read failed: {error}")),
    };
    for (line_index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (timestamp, nonce) = line
            .split_once('\t')
            .ok_or_else(|| format!("proxy nonce store line {} is malformed", line_index + 1))?;
        let timestamp = timestamp.parse::<i64>().map_err(|_| {
            format!(
                "proxy nonce store line {} has invalid timestamp",
                line_index + 1
            )
        })?;
        seen.insert(nonce.to_string(), timestamp);
    }
    Ok(seen)
}

fn persist_proxy_nonce_entries(path: &Path, seen: &BTreeMap<String, i64>) -> Result<(), String> {
    ensure_proxy_nonce_parent(path)?;
    validate_proxy_nonce_target(path)?;
    let mut contents = String::new();
    for (nonce, timestamp) in seen {
        contents.push_str(&format!("{timestamp}\t{nonce}\n"));
    }
    let (temp_path, mut temp_file) = create_proxy_nonce_temp(path)?;
    temp_file
        .write_all(contents.as_bytes())
        .map_err(|error| format!("proxy nonce store write failed: {error}"))?;
    temp_file
        .sync_all()
        .map_err(|error| format!("proxy nonce store sync failed: {error}"))?;
    drop(temp_file);
    fs::rename(&temp_path, path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        format!("proxy nonce store replace failed: {error}")
    })?;
    sync_parent_dir(path)?;
    Ok(())
}

fn create_proxy_nonce_temp(path: &Path) -> Result<(PathBuf, File), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "proxy nonce store path must have a parent directory".to_string())?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("proxy-nonces.db");
    for _ in 0..16 {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| format!("proxy nonce temp randomness failed: {error}"))?;
        let suffix = hex_encode(&random);
        let temp_path = parent.join(format!(".{file_name}.{suffix}.tmp"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            #[cfg(target_os = "linux")]
            options.custom_flags(O_NOFOLLOW_FLAG);
        }
        match options.open(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("proxy nonce temp create failed: {error}")),
        }
    }
    Err("proxy nonce temp create failed: exhausted random names".to_string())
}

fn sync_parent_dir(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "proxy nonce store path must have a parent directory".to_string())?;
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| format!("proxy nonce store parent sync failed: {error}"))
}

fn current_unix_timestamp() -> Result<i64, String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs() as i64)
}

fn proxy_principal(request: &HttpRequest) -> Result<ProxyPrincipal, String> {
    let player_id = required_header(request, "x-swarm-principal-player-id")?
        .parse::<PlayerId>()
        .map_err(|_| "invalid X-Swarm-Principal-Player-Id".to_string())?;
    let cert_id = required_header(request, "x-swarm-principal-cert-id")?.to_string();
    let cert_fingerprint =
        required_header(request, "x-swarm-principal-cert-fingerprint")?.to_string();
    let transport = required_header(request, "x-swarm-principal-transport")?.to_string();
    let scopes = canonicalize_scopes(required_header(request, "x-swarm-principal-scopes")?);
    let auth_mode = required_header(request, "x-swarm-principal-auth-mode")?.to_string();
    if matches!(auth_mode.as_str(), "app_cert" | "admin_cert") {
        if cert_id.trim().is_empty() {
            return Err("empty X-Swarm-Principal-Cert-Id".to_string());
        }
        if cert_fingerprint.trim().is_empty() {
            return Err("empty X-Swarm-Principal-Cert-Fingerprint".to_string());
        }
    }
    if transport.trim().is_empty() || contains_canonical_delimiter(&transport) {
        return Err("invalid X-Swarm-Principal-Transport".to_string());
    }
    if !matches!(
        auth_mode.as_str(),
        "unauthenticated" | "web_session" | "app_cert" | "admin_cert"
    ) {
        return Err("invalid X-Swarm-Principal-Auth-Mode".to_string());
    }
    for (name, value) in [
        ("X-Swarm-Principal-Cert-Id", cert_id.as_str()),
        (
            "X-Swarm-Principal-Cert-Fingerprint",
            cert_fingerprint.as_str(),
        ),
        ("X-Swarm-Principal-Scopes", scopes.as_str()),
        ("X-Swarm-Principal-Auth-Mode", auth_mode.as_str()),
    ] {
        if contains_canonical_delimiter(value) {
            return Err(format!("invalid {name}"));
        }
    }
    Ok(ProxyPrincipal {
        player_id,
        cert_id,
        cert_fingerprint,
        transport,
        scopes,
        auth_mode,
    })
}

fn required_header<'a>(request: &'a HttpRequest, name: &str) -> Result<&'a str, String> {
    request
        .headers
        .get(name)
        .map(|value| value.as_str())
        .ok_or_else(|| format!("missing {name}"))
}

fn canonicalize_scopes(scopes: &str) -> String {
    let mut scopes = scopes.split_ascii_whitespace().collect::<Vec<_>>();
    scopes.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    scopes.dedup();
    scopes.join(" ")
}

fn contains_canonical_delimiter(value: &str) -> bool {
    value.contains('\n') || value.contains('\r') || value.contains('\t')
}

fn proxy_tick(value: Option<&str>) -> Result<u64, String> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| "invalid X-Swarm-Tick".to_string())
        })
        .unwrap_or(Ok(0))
}

fn verify_proxy_signature(
    request: &HttpRequest,
    verifier: &swarm_engine::mcp::GatewayProxyVerifier,
    principal: &ProxyPrincipal,
    tick_header: &str,
    seen_nonces: &mut ProxyNonceStore,
) -> Result<swarm_engine::mcp::VerifiedMcpPrincipal, String> {
    let timestamp = request
        .headers
        .get("x-swarm-proxy-timestamp")
        .ok_or_else(|| "missing X-Swarm-Proxy-Timestamp".to_string())?;
    let nonce = request
        .headers
        .get("x-swarm-proxy-nonce")
        .ok_or_else(|| "missing X-Swarm-Proxy-Nonce".to_string())?;
    let signature = request
        .headers
        .get("x-swarm-proxy-signature")
        .ok_or_else(|| "missing X-Swarm-Proxy-Signature".to_string())?;
    let timestamp = timestamp
        .parse::<i64>()
        .map_err(|_| "invalid proxy timestamp".to_string())?;
    let now = current_unix_timestamp()?;
    if (now - timestamp).abs() > MCP_PROXY_REPLAY_WINDOW_SECONDS {
        return Err("proxy timestamp outside replay window".to_string());
    }
    if nonce.contains('\n') || nonce.contains('\r') || nonce.contains('\t') {
        return Err("invalid proxy nonce".to_string());
    }
    seen_nonces.prune_expired(now);
    if seen_nonces.contains(nonce)? {
        return Err("proxy nonce replayed".to_string());
    }

    if contains_canonical_delimiter(tick_header) {
        return Err("invalid X-Swarm-Tick".to_string());
    }
    let body_hash = hex_encode(&Sha256::digest(&request.body));
    let verified = verifier
        .verify_signed_proxy(
            swarm_engine::mcp::SignedProxyRequest {
                method: request.method.clone(),
                path: request.path.clone(),
                timestamp,
                nonce: nonce.clone(),
                player_id: principal.player_id,
                tick_header: tick_header.to_string(),
                cert_id: principal.cert_id.clone(),
                cert_fingerprint: principal.cert_fingerprint.clone(),
                transport: principal.transport.clone(),
                scopes: principal.scopes.clone(),
                auth_mode: principal.auth_mode.clone(),
                body_sha256_hex: body_hash,
                signature: signature.clone(),
            },
            now,
            Duration::from_secs(MCP_PROXY_REPLAY_WINDOW_SECONDS as u64),
        )
        .map_err(|error| error.message)?;
    seen_nonces.record_verified(nonce, timestamp, now)?;
    Ok(verified)
}

#[cfg(test)]
fn proxy_signature_canonical(
    request: &HttpRequest,
    timestamp: i64,
    nonce: &str,
    principal: &ProxyPrincipal,
    tick_header: &str,
) -> String {
    let body_hash = hex_encode(&Sha256::digest(&request.body));
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        request.method.to_ascii_uppercase(),
        request.path,
        timestamp,
        nonce,
        principal.player_id,
        tick_header,
        principal.cert_id,
        principal.cert_fingerprint,
        principal.transport,
        principal.scopes,
        principal.auth_mode,
        body_hash
    )
}

fn redact_url_userinfo(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find('/')
        .map(|offset| authority_start + offset)
        .unwrap_or(url.len());
    let authority = &url[authority_start..authority_end];
    let Some(userinfo_end) = authority.rfind('@') else {
        return url.to_string();
    };

    format!(
        "{}{}{}",
        &url[..authority_start],
        &authority[userinfo_end + 1..],
        &url[authority_end..]
    )
}

fn respond_health(stream: &mut TcpStream, healthy: bool) {
    let (status_line, body) = if healthy {
        ("HTTP/1.1 200 OK", "ok\n")
    } else {
        ("HTTP/1.1 503 Service Unavailable", "degraded\n")
    };
    let response = format!(
        "{status_line}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn respond_metrics(stream: &mut TcpStream, metrics: &metrics::EngineMetrics) {
    let body = metrics.render();
    respond_bytes(
        stream,
        "HTTP/1.1 200 OK",
        metrics::PROMETHEUS_CONTENT_TYPE,
        body.as_bytes(),
    );
}

fn respond_sdk_file(stream: &mut TcpStream, sdk_output_dir: &Path, sdk_path: &str) {
    let Some(relative_path) = clean_relative_path(sdk_path) else {
        respond_not_found(stream);
        return;
    };
    let mut file_path = sdk_output_dir.join(relative_path);

    if file_path.is_dir() {
        let index_path = file_path.join("index.html");
        if index_path.is_file() {
            file_path = index_path;
        } else {
            respond_directory_listing(stream, sdk_output_dir, &file_path);
            return;
        }
    }

    match std::fs::read(&file_path) {
        Ok(body) => {
            let content_type = content_type_for(&file_path);
            respond_bytes(stream, "HTTP/1.1 200 OK", content_type, &body);
        }
        Err(_) => respond_not_found(stream),
    }
}

fn clean_relative_path(path: &str) -> Option<PathBuf> {
    let path = path.split('?').next().unwrap_or(path);
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return Some(PathBuf::new());
    }

    let mut cleaned = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => cleaned.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(cleaned)
}

fn respond_directory_listing(stream: &mut TcpStream, sdk_output_dir: &Path, dir: &Path) {
    let mut entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect::<Vec<_>>(),
        Err(_) => {
            respond_not_found(stream);
            return;
        }
    };
    entries.sort();

    let title = dir
        .strip_prefix(sdk_output_dir)
        .ok()
        .and_then(|path| path.to_str())
        .unwrap_or("");
    let mut body = format!("<!doctype html><html><body><h1>/sdk/{title}</h1><ul>");
    for entry in entries {
        body.push_str("<li>");
        body.push_str(&html_escape(&entry));
        body.push_str("</li>");
    }
    body.push_str("</ul></body></html>");
    respond_bytes(
        stream,
        "HTTP/1.1 200 OK",
        "text/html; charset=utf-8",
        body.as_bytes(),
    );
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("ts") => "text/typescript; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn respond_not_found(stream: &mut TcpStream) {
    respond_bytes(
        stream,
        "HTTP/1.1 404 Not Found",
        "text/plain; charset=utf-8",
        b"not found\n",
    );
}

fn respond_bytes(stream: &mut TcpStream, status_line: &str, content_type: &str, body: &[u8]) {
    let header = format!(
        "{status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn parse_nats_endpoint(url: &str) -> Result<Endpoint, String> {
    let without_scheme = url
        .strip_prefix("nats://")
        .or_else(|| url.strip_prefix("tls://"))
        .unwrap_or(url);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    parse_host_port(authority, NATS_DEFAULT_PORT)
}

fn parse_host_port(value: &str, default_port: u16) -> Result<Endpoint, String> {
    let authority = value
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(value)
        .trim();
    if authority.is_empty() {
        return Err(format!("missing host in endpoint={value}"));
    }

    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, remainder)) = rest.split_once(']') else {
            return Err(format!("invalid bracketed host in endpoint={value}"));
        };
        let port = if remainder.is_empty() {
            default_port
        } else if let Some(port) = remainder.strip_prefix(':') {
            parse_port(port, value)?
        } else {
            return Err(format!("invalid bracketed host in endpoint={value}"));
        };
        (host, port)
    } else if authority.matches(':').count() > 1 {
        (authority, default_port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if port.is_empty() {
            return Err(format!("missing port in endpoint={value}"));
        }
        (host, parse_port(port, value)?)
    } else {
        (authority, default_port)
    };

    if host.is_empty() {
        return Err(format!("missing host in endpoint={value}"));
    }

    Ok(Endpoint {
        host: host.to_string(),
        port,
    })
}

fn parse_port(port: &str, endpoint: &str) -> Result<u16, String> {
    port.parse::<u16>()
        .map_err(|_| format!("invalid port in endpoint={endpoint}"))
}

fn tcp_check(endpoint: &Endpoint) -> bool {
    match (endpoint.host.as_str(), endpoint.port).to_socket_addrs() {
        Ok(addrs) => addrs
            .into_iter()
            .any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()),
        Err(error) => {
            eprintln!(
                "dependency endpoint resolve failed host={} port={} error={error}",
                endpoint.host, endpoint.port
            );
            false
        }
    }
}

fn status(ok: bool) -> &'static str {
    if ok { "ok" } else { "degraded" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_nonce_path(name: &str) -> PathBuf {
        let mut path = env::temp_dir();
        path.push(format!(
            "swarm-engine-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path.push("proxy-nonces.db");
        path
    }

    fn temp_nonce_store(name: &str) -> ProxyNonceStore {
        ProxyNonceStore::open(Ok(temp_nonce_path(name)))
    }

    #[test]
    fn mode_arg_accepts_novice_and_preserves_remaining_args() {
        let (mode, remaining) = parse_mode_arg(vec![
            "--mode".to_string(),
            "novice".to_string(),
            "sim".to_string(),
        ])
        .unwrap();

        assert_eq!(mode, WorldMode::Novice);
        assert_eq!(remaining, vec!["sim"]);
    }

    #[test]
    fn world_mode_parser_accepts_all_runtime_modes() {
        assert_eq!(parse_world_mode("default").unwrap(), WorldMode::Default);
        assert_eq!(parse_world_mode("tutorial").unwrap(), WorldMode::Tutorial);
        assert_eq!(parse_world_mode("novice").unwrap(), WorldMode::Novice);
        assert_eq!(parse_world_mode("arena").unwrap(), WorldMode::Arena);
        assert!(parse_world_mode("standard").is_err());
    }

    #[test]
    fn default_health_addr_is_loopback_only() {
        assert_eq!(DEFAULT_HEALTH_ADDR, "127.0.0.1:8080");
    }

    #[test]
    fn http_reader_rejects_oversized_body_before_reading_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream).is_none()
        });

        let mut client = TcpStream::connect(addr).unwrap();
        write!(
            client,
            "POST /mcp HTTP/1.1\r\ncontent-length: {}\r\n\r\n",
            MAX_PRE_AUTH_HTTP_BODY_BYTES + 1
        )
        .unwrap();

        assert!(handle.join().unwrap());
    }

    #[test]
    fn metrics_endpoint_returns_prometheus_text() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let metrics = Arc::new(metrics::EngineMetrics::default());
        metrics.set_authoritative_tick(9);
        metrics.set_dependencies(true, true);
        let server_metrics = Arc::clone(&metrics);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            respond_http(
                &mut stream,
                true,
                &server_metrics,
                Path::new("/tmp"),
                None,
                WorldMode::Default,
            );
        });

        let mut client = TcpStream::connect(addr).unwrap();
        write!(client, "GET /metrics HTTP/1.1\r\nhost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        handle.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains(metrics::PROMETHEUS_CONTENT_TYPE));
        assert!(response.contains("swarm_engine_up 1\n"));
        assert!(response.contains("swarm_engine_authoritative_tick 9\n"));
    }

    #[test]
    #[cfg(all(
        feature = "mod_combat_core",
        feature = "mod_depot_storage",
        feature = "mod_empire_upkeep",
        feature = "mod_fog_of_war",
        feature = "mod_pve_spawning",
        feature = "mod_resource_decay",
        feature = "mod_special_attacks",
        feature = "mod_vanilla_boss"
    ))]
    fn feature_gated_mod_resources_are_preinserted_from_lock() {
        let mut world = create_world_with_mode(WorldMode::Default);

        add_feature_gated_mod_plugins(&mut world.app).unwrap();

        assert_eq!(
            world
                .app
                .world()
                .resource::<swarm_mod_combat_core::CombatConfig>()
                .damage_multiplier_bp,
            10_000
        );
        assert_eq!(
            world
                .app
                .world()
                .resource::<swarm_mod_depot_storage::DepotStorageConfig>()
                .depot_capacity,
            10_000
        );
        assert!(
            world
                .app
                .world()
                .resource::<swarm_mod_fog_of_war::VisibilityConfig>()
                .fog_of_war
        );
        assert_eq!(
            world
                .app
                .world()
                .resource::<swarm_mod_pve_spawning::PveSpawningConfig>()
                .spawn_interval,
            300
        );
        assert_eq!(
            world
                .app
                .world()
                .resource::<swarm_mod_resource_decay::ResourceDecayConfig>()
                .decay_rate_ppm,
            1_000
        );
        assert!(
            world
                .app
                .world()
                .resource::<swarm_mod_special_attacks::SpecialAttacksConfig>()
                .enabled
                .contains(&swarm_engine::systems::SpecialAttackKind::Hack)
        );
        let boss_config = world
            .app
            .world()
            .resource::<swarm_mod_vanilla_boss::VanillaBossConfig>();
        assert!(boss_config.world_bosses_enabled);
        assert_eq!(boss_config.boss_templates.len(), 2);
    }

    #[test]
    fn metrics_endpoint_rejects_non_get_methods() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let metrics = Arc::new(metrics::EngineMetrics::default());
        let server_metrics = Arc::clone(&metrics);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            respond_http(
                &mut stream,
                false,
                &server_metrics,
                Path::new("/tmp"),
                None,
                WorldMode::Default,
            );
        });

        let mut client = TcpStream::connect(addr).unwrap();
        write!(client, "POST /metrics HTTP/1.1\r\nhost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        handle.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 405 Method Not Allowed"));
    }

    fn signed_request(timestamp: i64, nonce: &str, body: &[u8]) -> HttpRequest {
        signed_request_for_player(timestamp, nonce, body, 1, "0")
    }

    fn test_proxy_verifier() -> swarm_engine::mcp::GatewayProxyVerifier {
        // SAFETY: This fixed key is confined to the debug test process.
        unsafe {
            swarm_engine::mcp::GatewayProxyVerifier::from_trusted_secret_for_debug(
                b"secret".to_vec(),
            )
            .unwrap()
        }
    }

    fn signed_request_for_player(
        timestamp: i64,
        nonce: &str,
        body: &[u8],
        player_id: PlayerId,
        tick_header: &str,
    ) -> HttpRequest {
        let mut request = HttpRequest {
            method: "POST".to_string(),
            path: "/mcp".to_string(),
            headers: HashMap::new(),
            body: body.to_vec(),
        };
        request
            .headers
            .insert("x-swarm-proxy-timestamp".to_string(), timestamp.to_string());
        request
            .headers
            .insert("x-swarm-proxy-nonce".to_string(), nonce.to_string());
        request.headers.insert(
            "x-swarm-principal-player-id".to_string(),
            player_id.to_string(),
        );
        request.headers.insert(
            "x-swarm-principal-cert-id".to_string(),
            "cert-1".to_string(),
        );
        request.headers.insert(
            "x-swarm-principal-cert-fingerprint".to_string(),
            "fingerprint-1".to_string(),
        );
        request
            .headers
            .insert("x-swarm-principal-transport".to_string(), "mcp".to_string());
        request.headers.insert(
            "x-swarm-principal-scopes".to_string(),
            "swarm:read swarm:deploy".to_string(),
        );
        request.headers.insert(
            "x-swarm-principal-auth-mode".to_string(),
            "app_cert".to_string(),
        );
        if !tick_header.is_empty() {
            request
                .headers
                .insert("x-swarm-tick".to_string(), tick_header.to_string());
        }
        let principal = proxy_principal(&request).unwrap();
        let body_hash = hex_encode(&Sha256::digest(body));
        let canonical = format!(
            "POST\n/mcp\n{timestamp}\n{nonce}\n{player_id}\n{tick_header}\n{}\n{}\n{}\n{}\n{}\n{body_hash}",
            principal.cert_id,
            principal.cert_fingerprint,
            principal.transport,
            principal.scopes,
            principal.auth_mode,
        );
        request.headers.insert(
            "x-swarm-proxy-signature".to_string(),
            swarm_engine::sandbox_transport::hmac_sha256_hex(b"secret", canonical.as_bytes()),
        );
        request
    }

    #[test]
    fn verified_proxy_principal_maps_to_mcp_principal_without_scope_loss() {
        let request = signed_request(current_unix_timestamp().unwrap(), "principal-map", br#"{}"#);
        let proxy = proxy_principal(&request).unwrap();
        let mut seen = temp_nonce_store("principal-map");

        let principal =
            verify_proxy_signature(&request, &test_proxy_verifier(), &proxy, "0", &mut seen)
                .unwrap();
        let principal = principal.principal();

        assert_eq!(principal.kind(), swarm_engine::McpPrincipalKind::ClientCert);
        assert_eq!(principal.player_id(), Some(1));
        assert_eq!(principal.subject(), Some("cert-1"));
        assert_eq!(principal.scopes(), "swarm:deploy swarm:read");
        assert_eq!(principal.observed_transport(), Some("mcp"));
    }

    #[test]
    fn web_and_bootstrap_proxy_principals_do_not_require_certificate_fields() {
        for auth_mode in ["web_session", "unauthenticated"] {
            let mut request =
                signed_request(current_unix_timestamp().unwrap(), auth_mode, br#"{}"#);
            request.headers.insert(
                "x-swarm-principal-auth-mode".to_string(),
                auth_mode.to_string(),
            );
            request
                .headers
                .insert("x-swarm-principal-cert-id".to_string(), String::new());
            request.headers.insert(
                "x-swarm-principal-cert-fingerprint".to_string(),
                String::new(),
            );

            assert!(proxy_principal(&request).is_ok(), "auth_mode={auth_mode}");
        }
    }

    #[test]
    fn proxy_signature_accepts_canonical_request_and_rejects_replay() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let request = signed_request(timestamp, "nonce-1", br#"{"jsonrpc":"2.0"}"#);
        let mut seen = temp_nonce_store("accept-replay");
        let principal = proxy_principal(&request).unwrap();

        let verifier = test_proxy_verifier();
        verify_proxy_signature(&request, &verifier, &principal, "0", &mut seen).unwrap();

        assert!(verify_proxy_signature(&request, &verifier, &principal, "0", &mut seen).is_err());
    }

    #[test]
    fn proxy_signature_rejects_tampered_body() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut request = signed_request(timestamp, "nonce-2", b"{}");
        request.body = b"[]".to_vec();
        let mut seen = temp_nonce_store("tampered-body");
        let principal = proxy_principal(&request).unwrap();

        let verifier = test_proxy_verifier();
        assert!(verify_proxy_signature(&request, &verifier, &principal, "0", &mut seen).is_err());
        assert!(seen.seen.is_empty());
        request.body = b"{}".to_vec();
        verify_proxy_signature(&request, &verifier, &principal, "0", &mut seen).unwrap();
    }

    #[test]
    fn proxy_signature_rejects_player_identity_tamper() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut request = signed_request_for_player(timestamp, "nonce-player", b"{}", 1, "9");
        request
            .headers
            .insert("x-swarm-principal-player-id".to_string(), "2".to_string());
        let principal = proxy_principal(&request).unwrap();
        let mut seen = temp_nonce_store("player-tamper");

        assert!(
            verify_proxy_signature(&request, &test_proxy_verifier(), &principal, "9", &mut seen)
                .is_err()
        );
        assert!(seen.seen.is_empty());
    }

    #[test]
    fn production_proxy_nonce_path_rejects_tmp_and_relative_paths() {
        assert!(
            validate_proxy_nonce_path_for_mode(
                ENGINE_MODE_PRODUCTION,
                &env::temp_dir().join("swarm-proxy-nonces.db")
            )
            .is_err()
        );
        assert!(
            validate_proxy_nonce_path_for_mode(
                ENGINE_MODE_PRODUCTION,
                Path::new("swarm-proxy-nonces.db")
            )
            .is_err()
        );
        assert!(
            validate_proxy_nonce_path_for_mode(
                ENGINE_MODE_PRODUCTION,
                Path::new(PRODUCTION_PROXY_NONCE_PATH)
            )
            .is_ok()
        );
    }

    #[test]
    fn proxy_nonce_store_fails_closed_for_public_parent() {
        let path = temp_nonce_path("public-parent");
        let parent = path.parent().unwrap();
        #[cfg(unix)]
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();

        let mut store = ProxyNonceStore::open(Ok(path));

        assert!(store.contains("nonce-public-parent").is_err());
        assert!(
            store
                .record_verified(
                    "nonce-public-parent",
                    current_unix_timestamp().unwrap(),
                    current_unix_timestamp().unwrap()
                )
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn proxy_nonce_store_fails_closed_for_symlink_target() {
        use std::os::unix::fs::symlink;

        let target = temp_nonce_path("symlink-target-real");
        let link = temp_nonce_path("symlink-target-link");
        fs::write(&target, "99\texisting\n").unwrap();
        fs::remove_file(&link).ok();
        symlink(&target, &link).unwrap();

        let mut store = ProxyNonceStore::open(Ok(link));

        assert!(store.contains("existing").is_err());
        assert!(
            store
                .record_verified(
                    "nonce-symlink",
                    current_unix_timestamp().unwrap(),
                    current_unix_timestamp().unwrap()
                )
                .is_err()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "99\texisting\n");
    }

    #[test]
    fn proxy_identity_requires_valid_principal_headers() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut request = signed_request(timestamp, "nonce-missing-player", b"{}");

        request.headers.remove("x-swarm-principal-player-id");
        assert_eq!(
            proxy_principal(&request).unwrap_err(),
            "missing x-swarm-principal-player-id"
        );

        request.headers.insert(
            "x-swarm-principal-player-id".to_string(),
            "not-a-player".to_string(),
        );
        assert_eq!(
            proxy_principal(&request).unwrap_err(),
            "invalid X-Swarm-Principal-Player-Id"
        );
    }

    #[test]
    fn gateway_proxy_verifier_rejects_missing_or_empty_config() {
        assert_eq!(
            // SAFETY: The test intentionally supplies an invalid debug-only key.
            unsafe {
                swarm_engine::mcp::GatewayProxyVerifier::from_trusted_secret_for_debug(
                    b"   ".to_vec(),
                )
            }
            .unwrap_err()
            .message,
            "proxy auth secret empty"
        );
        // SAFETY: This fixed key is confined to the debug test process.
        assert!(
            unsafe {
                swarm_engine::mcp::GatewayProxyVerifier::from_trusted_secret_for_debug(
                    b"secret".to_vec(),
                )
            }
            .is_ok()
        );
    }

    #[test]
    fn redacts_nats_url_userinfo_for_logs() {
        assert_eq!(
            redact_url_userinfo("nats://user:pass@example.test:4222/path"),
            "nats://example.test:4222/path"
        );
        assert_eq!(
            redact_url_userinfo("nats://example.test:4222"),
            "nats://example.test:4222"
        );
    }

    #[test]
    fn oracle_gateway_principal_fixture_matches_expected_hmac() {
        let body = br#"{"jsonrpc":"2.0","method":"swarm_deploy"}"#;
        let mut request = HttpRequest {
            method: "post".to_string(),
            path: "/mcp".to_string(),
            headers: HashMap::new(),
            body: body.to_vec(),
        };
        for (name, value) in [
            ("x-swarm-proxy-timestamp", "1700000000"),
            ("x-swarm-proxy-nonce", "oracle-nonce-1"),
            ("x-swarm-principal-player-id", "42"),
            ("x-swarm-tick", "4523"),
            ("x-swarm-principal-cert-id", "cert-abc"),
            (
                "x-swarm-principal-cert-fingerprint",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ),
            ("x-swarm-principal-transport", "mcp"),
            (
                "x-swarm-principal-scopes",
                "swarm:read swarm:admin swarm:deploy swarm:read",
            ),
            ("x-swarm-principal-auth-mode", "admin_cert"),
        ] {
            request.headers.insert(name.to_string(), value.to_string());
        }
        request.headers.insert(
            "x-swarm-proxy-signature".to_string(),
            "a9b21c9cb946efd127e4a79a46b0f1539324f9357bb606c2b87c8da316fb9ab6".to_string(),
        );
        let principal = proxy_principal(&request).unwrap();
        assert_eq!(principal.scopes, "swarm:admin swarm:deploy swarm:read");
        let canonical = proxy_signature_canonical(
            &request,
            1_700_000_000,
            "oracle-nonce-1",
            &principal,
            "4523",
        );
        assert_eq!(
            canonical,
            "POST\n/mcp\n1700000000\noracle-nonce-1\n42\n4523\ncert-abc\n0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\nmcp\nswarm:admin swarm:deploy swarm:read\nadmin_cert\n54b97b14427c5e331d73eb86d8407ffe60f7f3827e5fae0ab556bc2810850349"
        );
        assert_eq!(
            swarm_engine::sandbox_transport::hmac_sha256_hex(
                b"oracle-fixture-secret",
                canonical.as_bytes(),
            ),
            "a9b21c9cb946efd127e4a79a46b0f1539324f9357bb606c2b87c8da316fb9ab6"
        );
    }

    #[test]
    fn proxy_signature_rejects_tampered_principal_field() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut request = signed_request(timestamp, "nonce-principal-tamper", b"{}");
        request.headers.insert(
            "x-swarm-principal-cert-id".to_string(),
            "cert-tampered".to_string(),
        );
        let principal = proxy_principal(&request).unwrap();
        let mut seen = temp_nonce_store("principal-tamper");

        assert!(
            verify_proxy_signature(&request, &test_proxy_verifier(), &principal, "0", &mut seen)
                .is_err()
        );
        assert!(seen.seen.is_empty());
    }

    #[test]
    fn proxy_signature_accepts_web_session_read_scope() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut request = signed_request(timestamp, "nonce-web-session", b"{}");
        request.headers.insert(
            "x-swarm-principal-scopes".to_string(),
            "swarm:read".to_string(),
        );
        request.headers.insert(
            "x-swarm-principal-auth-mode".to_string(),
            "web_session".to_string(),
        );
        let principal = proxy_principal(&request).unwrap();
        let canonical =
            proxy_signature_canonical(&request, timestamp, "nonce-web-session", &principal, "0");
        request.headers.insert(
            "x-swarm-proxy-signature".to_string(),
            swarm_engine::sandbox_transport::hmac_sha256_hex(b"secret", canonical.as_bytes()),
        );
        let mut seen = temp_nonce_store("web-session");

        assert_eq!(principal.scopes, "swarm:read");
        verify_proxy_signature(&request, &test_proxy_verifier(), &principal, "0", &mut seen)
            .unwrap();
    }

    #[test]
    fn http_reader_accepts_exact_8mib_body_and_rejects_larger() {
        assert_eq!(MAX_PRE_AUTH_HTTP_BODY_BYTES, 8 * 1024 * 1024);
        let ok_body = vec![b'a'; MAX_PRE_AUTH_HTTP_BODY_BYTES];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let ok_handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream).map(|request| request.body.len())
        });
        let mut client = TcpStream::connect(addr).unwrap();
        write!(
            client,
            "POST /mcp HTTP/1.1\r\ncontent-length: {}\r\n\r\n",
            ok_body.len()
        )
        .unwrap();
        client.write_all(&ok_body).unwrap();
        assert_eq!(
            ok_handle.join().unwrap(),
            Some(MAX_PRE_AUTH_HTTP_BODY_BYTES)
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let reject_handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream).is_none()
        });
        let mut client = TcpStream::connect(addr).unwrap();
        write!(
            client,
            "POST /mcp HTTP/1.1\r\ncontent-length: {}\r\n\r\n",
            MAX_PRE_AUTH_HTTP_BODY_BYTES + 1
        )
        .unwrap();
        assert!(reject_handle.join().unwrap());
    }

    #[test]
    fn nats_uri_parser_handles_userinfo_tls_and_ipv6() {
        let endpoint = parse_nats_endpoint("nats://user:pass@example.test:4333/path").unwrap();
        assert_eq!(endpoint.host, "example.test");
        assert_eq!(endpoint.port, 4333);

        let endpoint = parse_nats_endpoint("tls://[2001:db8::1]:4443").unwrap();
        assert_eq!(endpoint.host, "2001:db8::1");
        assert_eq!(endpoint.port, 4443);

        let endpoint = parse_nats_endpoint("nats://2001:db8::2").unwrap();
        assert_eq!(endpoint.host, "2001:db8::2");
        assert_eq!(endpoint.port, NATS_DEFAULT_PORT);
        assert!(parse_nats_endpoint("nats://example.test:notaport").is_err());
    }

    #[test]
    fn nats_security_policy_fails_closed_in_production() {
        assert!(
            NatsSecurityConfig::from_values_for_mode(
                "production",
                "nats://127.0.0.1:4222",
                None,
                None,
                None,
                None,
                None,
            )
            .is_err()
        );
        let creds = temp_nonce_path("nats-creds");
        fs::write(&creds, "creds").unwrap();
        let config = NatsSecurityConfig::from_values_for_mode(
            "production",
            "tls://nats.example.test:4222",
            Some("true"),
            None,
            None,
            None,
            Some(creds.clone()),
        )
        .unwrap();
        assert!(config.tls_required);
        assert_eq!(config.credentials_file, Some(creds.clone()));
        assert!(
            NatsSecurityConfig::from_values_for_mode(
                "development",
                "nats://127.0.0.1:4222",
                Some("false"),
                None,
                None,
                None,
                None,
            )
            .unwrap()
            .credentials_file
            .is_none()
        );
        let _ = fs::remove_file(creds);
    }

    #[test]
    fn production_certificate_issuer_requires_exactly_one_seed_source() {
        assert!(
            certificate_issuer_from_values_for_mode(ENGINE_MODE_PRODUCTION, None, None).is_err()
        );
        let seed_path = temp_nonce_path("issuer-seed");
        fs::write(
            &seed_path,
            [9_u8; swarm_engine::CertificateIssuer::ED25519_SEED_LEN],
        )
        .unwrap();
        let error = certificate_issuer_from_values_for_mode(
            ENGINE_MODE_PRODUCTION,
            Some(seed_path.clone()),
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()),
        )
        .unwrap_err();
        assert!(error.contains("requires exactly one issuer seed source"));
        let _ = fs::remove_file(seed_path);
    }

    #[test]
    fn production_certificate_issuer_rejects_bad_seed_sources() {
        let bad_b64 = certificate_issuer_from_values_for_mode(
            ENGINE_MODE_PRODUCTION,
            None,
            Some("not base64".to_string()),
        )
        .unwrap_err();
        assert_eq!(bad_b64, "SWARM_ENGINE_ISSUER_KEY_B64 is not valid base64");

        let wrong_len_b64 = certificate_issuer_from_values_for_mode(
            ENGINE_MODE_PRODUCTION,
            None,
            Some("AAAA".to_string()),
        )
        .unwrap_err();
        assert_eq!(
            wrong_len_b64,
            "SWARM_ENGINE_ISSUER_KEY_B64 must decode to exactly 32 bytes"
        );

        let short_path = temp_nonce_path("issuer-short-seed");
        fs::write(&short_path, [3_u8; 31]).unwrap();
        fs::set_permissions(&short_path, fs::Permissions::from_mode(0o600)).unwrap();
        let short_file = certificate_issuer_from_values_for_mode(
            ENGINE_MODE_PRODUCTION,
            Some(short_path.clone()),
            None,
        )
        .unwrap_err();
        assert_eq!(
            short_file,
            "SWARM_ENGINE_ISSUER_KEY_FILE must contain exactly 32 bytes"
        );
        let _ = fs::remove_file(short_path);
    }

    #[test]
    fn production_certificate_issuer_accepts_file_or_base64_seed() {
        let seed_path = temp_nonce_path("issuer-valid-seed");
        fs::write(
            &seed_path,
            [4_u8; swarm_engine::CertificateIssuer::ED25519_SEED_LEN],
        )
        .unwrap();
        fs::set_permissions(&seed_path, fs::Permissions::from_mode(0o600)).unwrap();
        let file_issuer = certificate_issuer_from_values_for_mode(
            ENGINE_MODE_PRODUCTION,
            Some(seed_path.clone()),
            None,
        )
        .unwrap();
        assert!(!file_issuer.public_key().is_empty());
        let _ = fs::remove_file(seed_path);

        let b64_issuer = certificate_issuer_from_values_for_mode(
            ENGINE_MODE_PRODUCTION,
            None,
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()),
        )
        .unwrap();
        assert!(!b64_issuer.public_key().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn production_certificate_issuer_rejects_symlink_seed_file() {
        use std::os::unix::fs::symlink;

        let target = temp_nonce_path("issuer-seed-target");
        let link = temp_nonce_path("issuer-seed-link");
        fs::write(
            &target,
            [5_u8; swarm_engine::CertificateIssuer::ED25519_SEED_LEN],
        )
        .unwrap();
        symlink(&target, &link).unwrap();

        let error = certificate_issuer_from_values_for_mode(
            ENGINE_MODE_PRODUCTION,
            Some(link.clone()),
            None,
        )
        .unwrap_err();

        assert_eq!(error, "SWARM_ENGINE_ISSUER_KEY_FILE must not be a symlink");
        let _ = fs::remove_file(link);
        let _ = fs::remove_file(target);
    }

    #[cfg(unix)]
    #[test]
    fn production_certificate_issuer_rejects_group_or_world_readable_seed_file() {
        let seed_path = temp_nonce_path("issuer-seed-readable");
        fs::write(
            &seed_path,
            [6_u8; swarm_engine::CertificateIssuer::ED25519_SEED_LEN],
        )
        .unwrap();
        fs::set_permissions(&seed_path, fs::Permissions::from_mode(0o640)).unwrap();

        let error = certificate_issuer_from_values_for_mode(
            ENGINE_MODE_PRODUCTION,
            Some(seed_path.clone()),
            None,
        )
        .unwrap_err();

        assert_eq!(error, "SWARM_ENGINE_ISSUER_KEY_FILE must be owner-only");
        let _ = fs::remove_file(seed_path);
    }

    #[cfg(unix)]
    #[test]
    fn production_certificate_issuer_accepts_owner_only_seed_file() {
        let seed_path = temp_nonce_path("issuer-seed-owner-only");
        fs::write(
            &seed_path,
            [7_u8; swarm_engine::CertificateIssuer::ED25519_SEED_LEN],
        )
        .unwrap();
        fs::set_permissions(&seed_path, fs::Permissions::from_mode(0o600)).unwrap();

        let issuer = certificate_issuer_from_values_for_mode(
            ENGINE_MODE_PRODUCTION,
            Some(seed_path.clone()),
            None,
        )
        .unwrap();

        assert!(!issuer.public_key().is_empty());
        let _ = fs::remove_file(seed_path);
    }

    #[cfg(unix)]
    #[test]
    fn issuer_seed_file_metadata_rejects_wrong_owner() {
        let seed_path = temp_nonce_path("issuer-seed-wrong-owner");
        fs::write(
            &seed_path,
            [8_u8; swarm_engine::CertificateIssuer::ED25519_SEED_LEN],
        )
        .unwrap();
        fs::set_permissions(&seed_path, fs::Permissions::from_mode(0o600)).unwrap();

        let metadata = fs::symlink_metadata(&seed_path).unwrap();
        let error = validate_issuer_seed_file_metadata(&metadata, effective_uid().wrapping_add(1))
            .unwrap_err();

        assert_eq!(
            error,
            "SWARM_ENGINE_ISSUER_KEY_FILE must be owned by the current user"
        );
        let _ = fs::remove_file(seed_path);
    }

    #[test]
    fn sandbox_reply_metrics_map_to_player_collect_metrics() {
        let metrics = swarm_engine::sandbox_transport::SandboxExecutionMetrics {
            fuel_consumed: 123_456,
            wall_clock_ms: 17,
            memory_peak_bytes: 65_536,
            host_function_calls: 9,
        };
        assert_eq!(
            sandbox_collect_metrics(&metrics),
            PlayerCollectMetrics {
                fuel_consumed: 123_456,
                refund_events: 0,
                refunded: 0,
            }
        );
    }

    #[test]
    fn proxy_nonce_store_survives_restart_and_prunes_expired_entries() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let path = temp_nonce_path("restart-reload");
        let request = signed_request(timestamp, "nonce-restart", b"{}");
        let principal = proxy_principal(&request).unwrap();
        let verifier = test_proxy_verifier();
        let mut first_store = ProxyNonceStore::open(Ok(path.clone()));
        verify_proxy_signature(&request, &verifier, &principal, "0", &mut first_store).unwrap();

        let mut reloaded_store = ProxyNonceStore::open(Ok(path.clone()));
        assert!(
            verify_proxy_signature(&request, &verifier, &principal, "0", &mut reloaded_store)
                .is_err()
        );

        let expired_timestamp = timestamp - MCP_PROXY_REPLAY_WINDOW_SECONDS - 1;
        let expired_store = ProxyNonceStore {
            path: path.clone(),
            seen: BTreeMap::from([("nonce-expired".to_string(), expired_timestamp)]),
            persistence_error: None,
        };
        expired_store.persist().unwrap();

        let mut pruned_store = ProxyNonceStore::open(Ok(path.clone()));
        let reused_after_prune = signed_request(timestamp, "nonce-expired", b"{}");
        let principal = proxy_principal(&reused_after_prune).unwrap();
        verify_proxy_signature(
            &reused_after_prune,
            &verifier,
            &principal,
            "0",
            &mut pruned_store,
        )
        .unwrap();

        let _ = fs::remove_file(path);
    }

    #[test]
    fn proxy_nonce_store_persists_atomically_without_temp_residue() {
        let timestamp = current_unix_timestamp().unwrap();
        let path = temp_nonce_path("atomic-persist");
        let mut store = ProxyNonceStore::open(Ok(path.clone()));

        store
            .record_verified("nonce-atomic", timestamp, timestamp)
            .unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, format!("{timestamp}\tnonce-atomic\n"));
        let parent = path.parent().unwrap();
        for entry in fs::read_dir(parent).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(!name.ends_with(".tmp"), "left temp file {name}");
        }
        let reloaded = ProxyNonceStore::open(Ok(path));
        assert!(reloaded.seen.contains_key("nonce-atomic"));
    }
}
