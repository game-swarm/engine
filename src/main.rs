use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
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

use serde_json::json;
use sha2::{Digest, Sha256};

use swarm_engine::{
    BodyPart, CommandIntent, ExecutorError, PlayerExecutor, PlayerId, TickBroadcaster,
    TickSnapshot, WorldMode, create_world_with_mode,
    sandbox_transport::{
        ActiveDeployments, SandboxBackend, execute_tick_remote, hex_encode, hmac_sha256_hex,
        nats_auth_secret_from_env,
    },
    sim::{create_local_simulation_world, summarize_local_simulation},
};

#[cfg(feature = "mod_combat_core")]
#[path = "../mods/combat-core/src/lib.rs"]
mod swarm_mod_combat_core;
#[cfg(feature = "mod_depot_storage")]
#[path = "../mods/depot-storage/src/lib.rs"]
mod swarm_mod_depot_storage;
#[cfg(feature = "mod_empire_upkeep")]
#[path = "../mods/empire-upkeep/src/lib.rs"]
mod swarm_mod_empire_upkeep;
#[cfg(feature = "mod_fog_of_war")]
#[path = "../mods/fog-of-war/src/lib.rs"]
mod swarm_mod_fog_of_war;
#[cfg(feature = "mod_pve_spawning")]
#[path = "../mods/pve-spawning/src/lib.rs"]
mod swarm_mod_pve_spawning;
#[cfg(feature = "mod_resource_decay")]
#[path = "../mods/resource-decay/src/lib.rs"]
mod swarm_mod_resource_decay;
#[cfg(feature = "mod_special_attacks")]
#[path = "../mods/special-attacks/src/lib.rs"]
mod swarm_mod_special_attacks;
#[cfg(feature = "mod_vanilla_boss")]
#[path = "../mods/vanilla-boss/src/lib.rs"]
mod swarm_mod_vanilla_boss;

const DEFAULT_HEALTH_ADDR: &str = "0.0.0.0:8080";
const MCP_PROXY_REPLAY_WINDOW_SECONDS: i64 = 300;
const DEFAULT_PROXY_NONCE_PATH: &str = "swarm-proxy-nonces.db";

#[derive(Clone, Debug)]
struct Endpoint {
    host: String,
    port: u16,
}

struct McpHttpState {
    server: swarm_engine::McpServer,
    world: swarm_engine::world::SwarmWorld,
    seen_proxy_nonces: ProxyNonceStore,
}

struct ProxyNonceStore {
    path: PathBuf,
    seen: BTreeMap<String, i64>,
    persistence_error: Option<String>,
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
                WorldMode::Default | WorldMode::Arena => swarm_engine::DEFAULT_TICK_INTERVAL_MS,
            }),
    );

    let healthy = Arc::new(AtomicBool::new(false));

    let redb_store = swarm_engine::RedbStore::open(&redb_path);
    let nats_endpoint = parse_nats_endpoint(&nats_url);

    let redb_connected = redb_store.is_ok();
    match &redb_store {
        Ok(_) => println!("redb opened path={redb_path}"),
        Err(error) => eprintln!("redb unavailable: {error}"),
    }
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
    start_health_server(health_addr, Arc::clone(&healthy), mcp_runtime_rx, mode);
    let nats_client = connect_nats_client_with_retry(&nats_url, &healthy, tick_interval);
    let shared_nats_client = Some(nats_client.clone());
    let sandbox_backend = SandboxBackend::Remote {
        nats_client,
        instance_id: env::var("INSTANCE_ID")
            .or_else(|_| env::var("ENGINE_INSTANCE_ID"))
            .unwrap_or_else(|_| "engine-0".to_string()),
    };
    let active_deployments = ActiveDeployments::default();

    if mcp_runtime_tx
        .send((sandbox_backend.clone(), active_deployments.clone()))
        .is_err()
    {
        eprintln!("health server unavailable; mcp runtime state was not installed");
    }

    swarm_engine::world::ensure_world_config_exists("world.toml", "mods.lock");
    let mut world = create_world_with_mode(mode);
    add_feature_gated_mod_plugins(&mut world.app);
    world.app.insert_resource(sandbox_backend.clone());
    world.app.insert_resource(active_deployments.clone());
    world
        .app
        .insert_resource(swarm_engine::RedbStore::unavailable(
            "owned by tick scheduler",
        ));
    world
        .app
        .insert_resource(swarm_engine::InMemorySnapshotCache::in_process());
    world.spawn_drone(
        1,
        10,
        10,
        vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
    );

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
        swarm_engine::RedbTickCommitter::new(match redb_store {
            Ok(store) => store,
            Err(error) => swarm_engine::RedbStore::unavailable(error.to_string()),
        }),
        broadcaster,
    );

    let mut tick: u64 = 0;
    loop {
        let redb_ok = redb_connected;
        let nats_ok = nats_endpoint.as_ref().map(tcp_check).unwrap_or(false);
        healthy.store(redb_ok && nats_ok, Ordering::Relaxed);

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
        if redb_ok && nats_ok {
            let report = scheduler.tick();
            if !report.committed {
                eprintln!(
                    "tick={tick} scheduler_commit=failed commit_failures={}",
                    report.metrics.commit_failures
                );
            }
        } else {
            scheduler.world.run_tick();
        }
        println!(
            "tick={} state_checksum={} redb={} nats={}",
            tick,
            scheduler.world.state_checksum(),
            status(redb_ok),
            status(nats_ok)
        );
        tick += 1;
        thread::sleep(tick_interval);
    }
}

fn add_feature_gated_mod_plugins(app: &mut bevy::prelude::App) {
    let _ = app;
    #[cfg(feature = "mod_combat_core")]
    app.add_plugins(swarm_mod_combat_core::CombatCoreModPlugin);
    #[cfg(feature = "mod_depot_storage")]
    app.add_plugins(swarm_mod_depot_storage::DepotStorageModPlugin);
    #[cfg(feature = "mod_empire_upkeep")]
    app.add_plugins(swarm_mod_empire_upkeep::EmpireUpkeepModPlugin);
    #[cfg(feature = "mod_fog_of_war")]
    app.add_plugins(swarm_mod_fog_of_war::FogOfWarModPlugin);
    #[cfg(feature = "mod_pve_spawning")]
    app.add_plugins(swarm_mod_pve_spawning::PveSpawningModPlugin);
    #[cfg(feature = "mod_resource_decay")]
    app.add_plugins(swarm_mod_resource_decay::ResourceDecayModPlugin);
    #[cfg(feature = "mod_special_attacks")]
    app.add_plugins(swarm_mod_special_attacks::SpecialAttacksModPlugin);
    #[cfg(feature = "mod_vanilla_boss")]
    app.add_plugins(swarm_mod_vanilla_boss::VanillaBossPlugin::default());
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

impl PlayerExecutor for SandboxPlayerExecutor {
    fn collect(&mut self, snapshot: TickSnapshot) -> Result<Vec<CommandIntent>, ExecutorError> {
        match &self.backend {
            SandboxBackend::Remote { nats_client, .. } => {
                let Some(deployment) = self
                    .active_deployments
                    .active_for_player(snapshot.player_id, snapshot.tick)
                else {
                    return Ok(Vec::new());
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
                reply
                    .commands
                    .into_iter()
                    .map(serde_json::from_value)
                    .collect::<Result<Vec<CommandIntent>, _>>()
                    .map_err(|error| ExecutorError::Error(error.to_string()))
            }
        }
    }
}

fn connect_nats_client(nats_url: &str) -> Result<async_nats::Client, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime
        .block_on(async_nats::connect(nats_url))
        .map_err(|error| error.to_string())
}

fn connect_nats_client_with_retry(
    nats_url: &str,
    healthy: &Arc<AtomicBool>,
    retry_interval: Duration,
) -> async_nats::Client {
    loop {
        match connect_nats_client(nats_url) {
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
        _ => Err(format!("--mode must be default or tutorial, got {value}")),
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
    mcp_runtime_rx: mpsc::Receiver<(SandboxBackend, ActiveDeployments)>,
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
                    install_pending_mcp_state(&mut mcp_state, &mcp_runtime_rx, mode);
                    respond_http(
                        &mut stream,
                        healthy.load(Ordering::Relaxed),
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
    mcp_runtime_rx: &mpsc::Receiver<(SandboxBackend, ActiveDeployments)>,
    mode: WorldMode,
) {
    while let Ok((sandbox_backend, active_deployments)) = mcp_runtime_rx.try_recv() {
        let mut world = create_world_with_mode(mode);
        add_feature_gated_mod_plugins(&mut world.app);
        world.app.insert_resource(sandbox_backend.clone());
        world.app.insert_resource(active_deployments.clone());
        *mcp_state = Some(McpHttpState {
            server: swarm_engine::McpServer::with_runtime_state(
                sandbox_backend,
                active_deployments,
            ),
            world,
            seen_proxy_nonces: ProxyNonceStore::open(proxy_nonce_store_path()),
        });
        println!("health server mcp runtime state installed");
    }
}

fn respond_http(
    stream: &mut TcpStream,
    healthy: bool,
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
    let body_start = header_end + 4;
    while buffer.len().saturating_sub(body_start) < content_length {
        let mut chunk = [0_u8; 4096];
        bytes_read = stream.read(&mut chunk).ok()?;
        if bytes_read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..bytes_read]);
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

    let secret = match proxy_signature_secret_from_env() {
        Ok(secret) => secret,
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

    let player_id = match proxy_player_id(&request) {
        Ok(player_id) => player_id,
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
    let tick_header = request.headers.get("x-swarm-tick").cloned();
    let tick = match proxy_tick(tick_header.as_deref()) {
        Ok(tick) => tick,
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

    let state = mcp_state;
    if let Err(error) = verify_proxy_signature(
        &request,
        secret.as_bytes(),
        player_id,
        tick_header.as_deref().unwrap_or(""),
        &mut state.seen_proxy_nonces,
    ) {
        respond_bytes(
            stream,
            "HTTP/1.1 401 Unauthorized",
            "text/plain; charset=utf-8",
            format!("{error}\n").as_bytes(),
        );
        return;
    }

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
    let McpHttpState { server, world, .. } = &mut *state;
    let response = server.handle_json_rpc(
        world,
        swarm_engine::McpContext { player_id, tick },
        rpc_request,
    );
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

fn proxy_nonce_store_path() -> PathBuf {
    env::var("SWARM_PROXY_NONCE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_PROXY_NONCE_PATH))
}

impl ProxyNonceStore {
    fn open(path: PathBuf) -> Self {
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
        let mut seen = BTreeMap::new();
        if path.exists() {
            let contents = fs::read_to_string(&path)
                .map_err(|error| format!("proxy nonce store read failed: {error}"))?;
            for (line_index, line) in contents.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let (timestamp, nonce) = line.split_once('\t').ok_or_else(|| {
                    format!("proxy nonce store line {} is malformed", line_index + 1)
                })?;
                let timestamp = timestamp.parse::<i64>().map_err(|_| {
                    format!(
                        "proxy nonce store line {} has invalid timestamp",
                        line_index + 1
                    )
                })?;
                seen.insert(nonce.to_string(), timestamp);
            }
        }

        let mut store = Self {
            path,
            seen,
            persistence_error: None,
        };
        let now = current_unix_timestamp()?;
        if store.prune_expired(now) {
            store.persist()?;
        }
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
        self.prune_expired(now);
        self.seen.insert(nonce.to_string(), timestamp);
        self.persist()
    }

    fn prune_expired(&mut self, now: i64) -> bool {
        let before = self.seen.len();
        self.seen
            .retain(|_, timestamp| now - *timestamp <= MCP_PROXY_REPLAY_WINDOW_SECONDS);
        before != self.seen.len()
    }

    fn persist(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|error| format!("proxy nonce store mkdir failed: {error}"))?;
        }
        let mut contents = String::new();
        for (nonce, timestamp) in &self.seen {
            contents.push_str(&format!("{timestamp}\t{nonce}\n"));
        }
        let temp_path = self.path.with_extension("tmp");
        fs::write(&temp_path, contents)
            .map_err(|error| format!("proxy nonce store write failed: {error}"))?;
        fs::rename(&temp_path, &self.path)
            .map_err(|error| format!("proxy nonce store replace failed: {error}"))
    }
}

fn current_unix_timestamp() -> Result<i64, String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs() as i64)
}

fn proxy_signature_secret_from_env() -> Result<String, String> {
    let secret = env::var("SWARM_PROXY_SIGNATURE_SECRET")
        .map_err(|_| "proxy auth secret missing".to_string())?;
    validate_proxy_signature_secret(secret)
}

fn validate_proxy_signature_secret(secret: String) -> Result<String, String> {
    if secret.trim().is_empty() {
        return Err("proxy auth secret empty".to_string());
    }
    Ok(secret)
}

fn proxy_player_id(request: &HttpRequest) -> Result<PlayerId, String> {
    let value = request
        .headers
        .get("x-swarm-player-id")
        .ok_or_else(|| "missing X-Swarm-Player-Id".to_string())?;
    value
        .parse::<PlayerId>()
        .map_err(|_| "invalid X-Swarm-Player-Id".to_string())
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
    secret: &[u8],
    player_id: PlayerId,
    tick_header: &str,
    seen_nonces: &mut ProxyNonceStore,
) -> Result<(), String> {
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

    let body_hash = hex_encode(&Sha256::digest(&request.body));
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}",
        request.method, request.path, timestamp, nonce, player_id, tick_header, body_hash
    );
    let expected = hmac_sha256_hex(secret, canonical.as_bytes());
    if !constant_time_eq(signature.as_bytes(), expected.as_bytes()) {
        return Err("invalid proxy signature".to_string());
    }
    seen_nonces.record_verified(nonce, timestamp, now)?;
    Ok(())
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

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |acc, (left, right)| acc | (left ^ right))
        == 0
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
    let without_scheme = url.strip_prefix("nats://").unwrap_or(url);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    parse_host_port(authority, 4222)
}

fn parse_host_port(value: &str, default_port: u16) -> Result<Endpoint, String> {
    let mut parts = value.rsplitn(2, ':');
    let maybe_port = parts.next().unwrap_or(value);
    let port = maybe_port.parse::<u16>().unwrap_or(default_port);
    let host = if port == default_port && !value.ends_with(&format!(":{default_port}")) {
        value
    } else {
        parts.next().unwrap_or(value)
    }
    .trim();

    if host.is_empty() {
        return Err(format!("missing host in endpoint={value}"));
    }

    Ok(Endpoint {
        host: host.to_string(),
        port,
    })
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
            "swarm-engine-{name}-{}-{}.nonces",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path
    }

    fn temp_nonce_store(name: &str) -> ProxyNonceStore {
        ProxyNonceStore::open(temp_nonce_path(name))
    }

    fn signed_request(timestamp: i64, nonce: &str, body: &[u8]) -> HttpRequest {
        signed_request_for_player(timestamp, nonce, body, 1, "0")
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
        let body_hash = hex_encode(&Sha256::digest(body));
        let canonical =
            format!("POST\n/mcp\n{timestamp}\n{nonce}\n{player_id}\n{tick_header}\n{body_hash}");
        request
            .headers
            .insert("x-swarm-proxy-timestamp".to_string(), timestamp.to_string());
        request
            .headers
            .insert("x-swarm-proxy-nonce".to_string(), nonce.to_string());
        request
            .headers
            .insert("x-swarm-player-id".to_string(), player_id.to_string());
        if !tick_header.is_empty() {
            request
                .headers
                .insert("x-swarm-tick".to_string(), tick_header.to_string());
        }
        request.headers.insert(
            "x-swarm-proxy-signature".to_string(),
            hmac_sha256_hex(b"secret", canonical.as_bytes()),
        );
        request
    }

    #[test]
    fn proxy_signature_accepts_canonical_request_and_rejects_replay() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let request = signed_request(timestamp, "nonce-1", br#"{"jsonrpc":"2.0"}"#);
        let mut seen = temp_nonce_store("accept-replay");

        verify_proxy_signature(&request, b"secret", 1, "0", &mut seen).unwrap();

        assert!(verify_proxy_signature(&request, b"secret", 1, "0", &mut seen).is_err());
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

        assert!(verify_proxy_signature(&request, b"secret", 1, "0", &mut seen).is_err());
        assert!(seen.seen.is_empty());
        request.body = b"{}".to_vec();
        verify_proxy_signature(&request, b"secret", 1, "0", &mut seen).unwrap();
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
            .insert("x-swarm-player-id".to_string(), "2".to_string());
        let player_id = proxy_player_id(&request).unwrap();
        let mut seen = temp_nonce_store("player-tamper");

        assert!(verify_proxy_signature(&request, b"secret", player_id, "9", &mut seen).is_err());
        assert!(seen.seen.is_empty());
    }

    #[test]
    fn proxy_identity_requires_valid_player_header() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut request = signed_request(timestamp, "nonce-missing-player", b"{}");

        request.headers.remove("x-swarm-player-id");
        assert_eq!(
            proxy_player_id(&request).unwrap_err(),
            "missing X-Swarm-Player-Id"
        );

        request
            .headers
            .insert("x-swarm-player-id".to_string(), "not-a-player".to_string());
        assert_eq!(
            proxy_player_id(&request).unwrap_err(),
            "invalid X-Swarm-Player-Id"
        );
    }

    #[test]
    fn proxy_secret_rejects_empty_values() {
        assert_eq!(
            validate_proxy_signature_secret("   ".to_string()).unwrap_err(),
            "proxy auth secret empty"
        );
        assert_eq!(
            validate_proxy_signature_secret("secret".to_string()).unwrap(),
            "secret"
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
    fn proxy_nonce_store_survives_restart_and_prunes_expired_entries() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let path = temp_nonce_path("restart-reload");
        let request = signed_request(timestamp, "nonce-restart", b"{}");
        let mut first_store = ProxyNonceStore::open(path.clone());
        verify_proxy_signature(&request, b"secret", 1, "0", &mut first_store).unwrap();

        let mut reloaded_store = ProxyNonceStore::open(path.clone());
        assert!(verify_proxy_signature(&request, b"secret", 1, "0", &mut reloaded_store).is_err());

        let expired_timestamp = timestamp - MCP_PROXY_REPLAY_WINDOW_SECONDS - 1;
        let expired_store = ProxyNonceStore {
            path: path.clone(),
            seen: BTreeMap::from([("nonce-expired".to_string(), expired_timestamp)]),
            persistence_error: None,
        };
        expired_store.persist().unwrap();

        let mut pruned_store = ProxyNonceStore::open(path.clone());
        let reused_after_prune = signed_request(timestamp, "nonce-expired", b"{}");
        verify_proxy_signature(&reused_after_prune, b"secret", 1, "0", &mut pruned_store).unwrap();

        let _ = fs::remove_file(path);
    }
}
