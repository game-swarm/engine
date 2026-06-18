use std::{
    env, fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use swarm_engine::{
    BodyPart, WorldMode, create_world_with_mode,
    sim::{create_local_simulation_world, summarize_local_simulation},
};

const DEFAULT_HEALTH_ADDR: &str = "0.0.0.0:8080";
#[derive(Clone, Debug)]
struct Endpoint {
    host: String,
    port: u16,
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

    let fdb_cluster_file = env::var("FDB_CLUSTER_FILE")
        .unwrap_or_else(|_| "/etc/foundationdb/fdb.cluster".to_string());
    let dragonfly_url = env::var("DRAGONFLY_URL")
        .or_else(|_| env::var("REDIS_URL"))
        .unwrap_or_else(|_| "redis://127.0.0.1:6379/".to_string());
    let nats_url = env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
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
    start_health_server(health_addr, Arc::clone(&healthy));

    let fdb_store = swarm_engine::FoundationDbStore::connect(Some(&fdb_cluster_file));
    let dragonfly_cache = swarm_engine::DragonflyCache::connect(&dragonfly_url);
    let fdb_endpoint = read_fdb_endpoint(&fdb_cluster_file);
    let dragonfly_endpoint = parse_dragonfly_endpoint(&dragonfly_url);
    let nats_endpoint = parse_nats_endpoint(&nats_url);

    let fdb_connected = fdb_store.is_ok();
    match &fdb_store {
        Ok(_) => println!("fdb connected cluster_file={fdb_cluster_file}"),
        Err(error) => eprintln!("fdb unavailable: {error}"),
    }
    match &dragonfly_cache {
        Ok(_) => println!("dragonfly configured url={dragonfly_url}"),
        Err(error) => eprintln!("dragonfly unavailable: {error}"),
    }

    match &fdb_endpoint {
        Ok(endpoint) => println!(
            "fdb cluster file loaded path={} coordinator={}:{}",
            fdb_cluster_file, endpoint.host, endpoint.port
        ),
        Err(error) => eprintln!("fdb coordinator probe unavailable: {error}"),
    }

    match &nats_endpoint {
        Ok(endpoint) => println!(
            "nats configured url={} endpoint={}:{}",
            nats_url, endpoint.host, endpoint.port
        ),
        Err(error) => eprintln!("nats unavailable: {error}"),
    }

    let mut world = create_world_with_mode(mode);
    if let Ok(store) = fdb_store {
        world.app.insert_resource(store);
    }
    if let Ok(cache) = dragonfly_cache {
        world.app.insert_resource(cache);
    }
    world.spawn_drone(
        1,
        10,
        10,
        vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
    );

    let mut tick: u64 = 0;
    loop {
        let fdb_ok = fdb_connected
            && fdb_endpoint
                .as_ref()
                .map(|endpoint| tcp_check(endpoint))
                .unwrap_or(false);
        let nats_ok = nats_endpoint
            .as_ref()
            .map(|endpoint| tcp_check(endpoint))
            .unwrap_or(false);
        let dragonfly_ok = dragonfly_endpoint
            .as_ref()
            .map(|endpoint| tcp_check(endpoint))
            .unwrap_or(false);
        healthy.store(fdb_ok && dragonfly_ok && nats_ok, Ordering::Relaxed);

        if !fdb_ok {
            eprintln!(
                "tick={tick} dependency=fdb status=degraded action=continue_without_persistence"
            );
        }
        if !nats_ok {
            eprintln!(
                "tick={tick} dependency=nats status=degraded action=continue_without_broadcast"
            );
        }
        if !dragonfly_ok {
            eprintln!(
                "tick={tick} dependency=dragonfly status=degraded action=use_in_process_cache"
            );
        }

        world.run_tick();
        println!(
            "tick={} state_checksum={} fdb={} dragonfly={} nats={}",
            tick,
            world.state_checksum(),
            status(fdb_ok),
            status(dragonfly_ok),
            status(nats_ok)
        );
        tick += 1;
        thread::sleep(tick_interval);
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
        "mode=local-sim caveat=training-only-not-authoritative-no-fdb-no-nats ticks={ticks} speed={speed}"
    );
    let started_at = std::time::Instant::now();
    let mut world = create_local_simulation_world();
    let mut checksum = world.state_checksum();
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

fn start_health_server(addr: String, healthy: Arc<AtomicBool>) {
    thread::spawn(move || {
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
                Ok(mut stream) => respond_health(&mut stream, healthy.load(Ordering::Relaxed)),
                Err(error) => eprintln!("health server connection failed error={error}"),
            }
        }
    });
}

fn respond_health(stream: &mut TcpStream, healthy: bool) {
    let mut request = [0_u8; 512];
    let _ = stream.read(&mut request);
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

fn read_fdb_endpoint(path: &str) -> Result<Endpoint, String> {
    let contents =
        fs::read_to_string(path).map_err(|error| format!("cluster_file={path} error={error}"))?;
    let coordinator = contents
        .trim()
        .rsplit('@')
        .next()
        .ok_or_else(|| format!("cluster_file={path} has no coordinator"))?;
    parse_host_port(coordinator, 4500)
}

fn parse_nats_endpoint(url: &str) -> Result<Endpoint, String> {
    let without_scheme = url.strip_prefix("nats://").unwrap_or(url);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    parse_host_port(authority, 4222)
}

fn parse_dragonfly_endpoint(url: &str) -> Result<Endpoint, String> {
    let without_scheme = url
        .strip_prefix("redis://")
        .or_else(|| url.strip_prefix("rediss://"))
        .unwrap_or(url);
    let authority = without_scheme
        .split('@')
        .next_back()
        .unwrap_or(without_scheme)
        .split('/')
        .next()
        .unwrap_or(without_scheme);
    parse_host_port(authority, 6379)
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
