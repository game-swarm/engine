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

use swarm_engine::{BodyPart, create_world};

const DEFAULT_HEALTH_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_TICK_INTERVAL_MS: u64 = 3_000;

#[derive(Clone, Debug)]
struct Endpoint {
    host: String,
    port: u16,
}

fn main() {
    let fdb_cluster_file = env::var("FDB_CLUSTER_FILE")
        .unwrap_or_else(|_| "/etc/foundationdb/fdb.cluster".to_string());
    let nats_url = env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
    let health_addr =
        env::var("ENGINE_HEALTH_ADDR").unwrap_or_else(|_| DEFAULT_HEALTH_ADDR.to_string());
    let tick_interval = Duration::from_millis(
        env::var("SWARM_TICK_INTERVAL_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_TICK_INTERVAL_MS),
    );

    let healthy = Arc::new(AtomicBool::new(false));
    start_health_server(health_addr, Arc::clone(&healthy));

    let fdb_endpoint = read_fdb_endpoint(&fdb_cluster_file);
    let nats_endpoint = parse_nats_endpoint(&nats_url);

    match &fdb_endpoint {
        Ok(endpoint) => println!(
            "fdb cluster file loaded path={} coordinator={}:{}",
            fdb_cluster_file, endpoint.host, endpoint.port
        ),
        Err(error) => eprintln!("fdb unavailable: {error}"),
    }

    match &nats_endpoint {
        Ok(endpoint) => println!(
            "nats configured url={} endpoint={}:{}",
            nats_url, endpoint.host, endpoint.port
        ),
        Err(error) => eprintln!("nats unavailable: {error}"),
    }

    let mut world = create_world();
    world.spawn_drone(
        1,
        10,
        10,
        vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
    );

    let mut tick: u64 = 0;
    loop {
        let fdb_ok = fdb_endpoint
            .as_ref()
            .map(|endpoint| tcp_check(endpoint))
            .unwrap_or(false);
        let nats_ok = nats_endpoint
            .as_ref()
            .map(|endpoint| tcp_check(endpoint))
            .unwrap_or(false);
        healthy.store(fdb_ok && nats_ok, Ordering::Relaxed);

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

        world.run_tick();
        println!(
            "tick={} state_checksum={} fdb={} nats={}",
            tick,
            world.state_checksum(),
            status(fdb_ok),
            status(nats_ok)
        );
        tick += 1;
        thread::sleep(tick_interval);
    }
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
