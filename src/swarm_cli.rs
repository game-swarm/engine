use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    auth::CertificateIssuer,
    command::Tick,
    redb_store::{OperationalInspect, RedbError, RedbStore},
};

type CliResult<T> = Result<T, String>;

const HELP: &str = "swarm operational CLI\n\nUSAGE:\n  swarm deploy <project-or-wasm> --auth-file <json> [--gateway-url <url>] [--world-id <id>] [--room-id <u32>] [--drone-id <u64>] [--target-manifest-hash <hash>] [--engine-abi-version <u32>] [--language <name>] [--version-tag <tag>] [--version-counter <u64>] [--artifact <relative-wasm>] [--allow-insecure-loopback]\n  swarm backup create --db <path> --out <dir>\n  swarm verify (--db <path>|--backup <dir>) [--tick <tick>]\n  swarm keyframe backup --db <path> --tick <tick> --out <file>\n  swarm keyframe restore --db <path> --tick <tick> --from <file>\n  swarm inspect --db <path>\n  swarm ca init --auth-state <file>\n  swarm ca fingerprint --auth-state <file>\n  swarm cert revoke --auth-state <file> --cert-id <id>\n  swarm auth epoch-bump --auth-state <file>\n\nOPTIONS:\n  --allow-insecure-loopback      Permit http://localhost or http://127.0.0.1 gateway URLs for local testing only\n  --keyframe-backup-root <dir>   Override the default <db>.keyframe-backup root\n  -h, --help                     Show help\n";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupManifest {
    version: u32,
    created_at_unix: u64,
    db_file: String,
    files: Vec<BackupFileManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BackupFileManifest {
    path: String,
    len: u64,
    blake3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AuthState {
    version: u32,
    ca_seed_hex: Option<String>,
    ca_public_key: Option<String>,
    ca_fingerprint: Option<String>,
    revoked_cert_ids: BTreeSet<String>,
    auth_epoch: u64,
}

pub fn run_from_env() -> i32 {
    match run(std::env::args().skip(1), &mut io::stdout()) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

pub fn run(args: impl IntoIterator<Item = String>, out: &mut dyn Write) -> CliResult<()> {
    let args = args.into_iter().collect::<Vec<_>>();
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        writeln!(out, "{HELP}").map_err(|error| error.to_string())?;
        return Ok(());
    }
    match args.as_slice() {
        [command, rest @ ..] if command == "deploy" => crate::swarm_deploy::run(rest, out),
        [area, command, rest @ ..] if area == "backup" && command == "create" => {
            backup_create(rest, out)
        }
        [command, rest @ ..] if command == "verify" => verify(rest, out),
        [area, command, rest @ ..] if area == "keyframe" && command == "backup" => {
            keyframe_backup(rest, out)
        }
        [area, command, rest @ ..] if area == "keyframe" && command == "restore" => {
            keyframe_restore(rest, out)
        }
        [command, rest @ ..] if command == "inspect" => inspect(rest, out),
        [area, command, rest @ ..] if area == "ca" && command == "init" => ca_init(rest, out),
        [area, command, rest @ ..] if area == "ca" && command == "fingerprint" => {
            ca_fingerprint(rest, out)
        }
        [area, command, rest @ ..] if area == "cert" && command == "revoke" => {
            cert_revoke(rest, out)
        }
        [area, command, rest @ ..] if area == "auth" && command == "epoch-bump" => {
            auth_epoch_bump(rest, out)
        }
        _ => Err(format!("unknown command\n\n{HELP}")),
    }
}

fn backup_create(args: &[String], out: &mut dyn Write) -> CliResult<()> {
    let options = Options::parse(args)?;
    let db = options.required_path("--db")?;
    let destination = options.required_path("--out")?;
    require_existing_file(&db, "database")?;
    require_new_path(&destination, "backup destination")?;
    require_safe_parent(&destination)?;
    fs::create_dir(&destination)
        .map_err(|error| format!("create backup directory {}: {error}", destination.display()))?;
    let db_name = db
        .file_name()
        .ok_or_else(|| format!("database path has no file name: {}", db.display()))?;
    copy_file_new(&db, &destination.join(db_name))?;
    for sidecar in sidecar_paths(&db, options.path("--keyframe-backup-root"))? {
        if sidecar.source.exists() {
            copy_dir_new(&sidecar.source, &destination.join(sidecar.name))?;
        }
    }
    let manifest = build_backup_manifest(&destination, db_name.to_string_lossy().as_ref())?;
    write_json_new(&destination.join("manifest.json"), &manifest)?;
    writeln!(out, "created backup {}", destination.display()).map_err(|error| error.to_string())
}

fn verify(args: &[String], out: &mut dyn Write) -> CliResult<()> {
    let options = Options::parse(args)?;
    let store = if let Some(backup) = options.path("--backup") {
        require_existing_dir(&backup, "backup")?;
        verify_backup_manifest(&backup)?;
        let manifest: BackupManifest = read_json(&backup.join("manifest.json"))?;
        let db = backup.join(manifest.db_file);
        require_existing_file(&db, "database")?;
        RedbStore::open_with_artifact_paths(
            &db.to_string_lossy(),
            backup.join("objects"),
            backup.join("wal"),
            backup.join("keyframes"),
            backup.join("keyframe-backup"),
        )
        .map_err(to_cli_error)?
    } else {
        let db = options.required_path("--db")?;
        require_existing_file(&db, "database")?;
        open_store(&db, options.path("--keyframe-backup-root"))?
    };
    if let Some(tick) = options.optional_tick("--tick")? {
        store.verify_tick(tick).map_err(to_cli_error)?;
        writeln!(out, "verified tick {tick}").map_err(|error| error.to_string())?;
    } else {
        let inspect = store.operational_inspect().map_err(to_cli_error)?;
        if let Some(tick) = inspect.latest_tick {
            store.verify_tick(tick).map_err(to_cli_error)?;
        }
        writeln!(
            out,
            "verified database latest_tick={:?} ticks={}",
            inspect.latest_tick, inspect.verified_ticks
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn keyframe_backup(args: &[String], out: &mut dyn Write) -> CliResult<()> {
    let options = Options::parse(args)?;
    let db = options.required_path("--db")?;
    let tick = options.required_tick("--tick")?;
    let destination = options.required_path("--out")?;
    require_existing_file(&db, "database")?;
    require_new_path(&destination, "keyframe export")?;
    require_safe_parent(&destination)?;
    open_store(&db, options.path("--keyframe-backup-root"))?
        .export_verified_keyframe(tick, &destination)
        .map_err(to_cli_error)?;
    writeln!(
        out,
        "exported keyframe tick {tick} to {}",
        destination.display()
    )
    .map_err(|error| error.to_string())
}

fn keyframe_restore(args: &[String], out: &mut dyn Write) -> CliResult<()> {
    let options = Options::parse(args)?;
    let db = options.required_path("--db")?;
    let tick = options.required_tick("--tick")?;
    let source = options.required_path("--from")?;
    require_existing_file(&db, "database")?;
    require_existing_file(&source, "keyframe restore source")?;
    open_store(&db, options.path("--keyframe-backup-root"))?
        .restore_verified_keyframe(tick, &source)
        .map_err(to_cli_error)?;
    writeln!(
        out,
        "restored keyframe tick {tick} from {}",
        source.display()
    )
    .map_err(|error| error.to_string())
}

fn inspect(args: &[String], out: &mut dyn Write) -> CliResult<()> {
    let options = Options::parse(args)?;
    let db = options.required_path("--db")?;
    require_existing_file(&db, "database")?;
    let summary: OperationalInspect = open_store(&db, options.path("--keyframe-backup-root"))?
        .operational_inspect()
        .map_err(to_cli_error)?;
    serde_json::to_writer_pretty(&mut *out, &summary).map_err(|error| error.to_string())?;
    writeln!(out).map_err(|error| error.to_string())
}

fn ca_init(args: &[String], out: &mut dyn Write) -> CliResult<()> {
    let options = Options::parse(args)?;
    let path = options.required_path("--auth-state")?;
    require_safe_parent(&path)?;
    let mut state = read_auth_state_if_exists(&path)?;
    if state.ca_seed_hex.is_some() {
        return Err("CA already initialized".to_string());
    }
    let mut seed = [0_u8; CertificateIssuer::ED25519_SEED_LEN];
    getrandom::fill(&mut seed).map_err(|error| format!("generate CA seed: {error}"))?;
    let issuer = CertificateIssuer::from_seed(&seed).map_err(|error| error.message)?;
    state.version = 1;
    state.ca_seed_hex = Some(hex_encode(&seed));
    state.ca_public_key = Some(issuer.public_key());
    state.ca_fingerprint = Some(issuer.public_key_fingerprint());
    write_auth_state(&path, &state)?;
    seed.fill(0);
    writeln!(
        out,
        "initialized ca fingerprint={}",
        state.ca_fingerprint.as_deref().unwrap_or("")
    )
    .map_err(|error| error.to_string())
}

fn ca_fingerprint(args: &[String], out: &mut dyn Write) -> CliResult<()> {
    let path = Options::parse(args)?.required_path("--auth-state")?;
    let state = read_auth_state(&path)?;
    let fingerprint = state
        .ca_fingerprint
        .ok_or_else(|| "CA is not initialized".to_string())?;
    writeln!(out, "{fingerprint}").map_err(|error| error.to_string())
}

fn cert_revoke(args: &[String], out: &mut dyn Write) -> CliResult<()> {
    let options = Options::parse(args)?;
    let path = options.required_path("--auth-state")?;
    let cert_id = options.required("--cert-id")?;
    let mut state = read_auth_state(&path)?;
    state.revoked_cert_ids.insert(cert_id.clone());
    write_auth_state(&path, &state)?;
    writeln!(out, "revoked cert {cert_id}").map_err(|error| error.to_string())
}

fn auth_epoch_bump(args: &[String], out: &mut dyn Write) -> CliResult<()> {
    let path = Options::parse(args)?.required_path("--auth-state")?;
    let mut state = read_auth_state(&path)?;
    state.auth_epoch = state.auth_epoch.saturating_add(1);
    write_auth_state(&path, &state)?;
    writeln!(out, "auth epoch {}", state.auth_epoch).map_err(|error| error.to_string())
}

fn open_store(db: &Path, keyframe_backup_root: Option<PathBuf>) -> CliResult<RedbStore> {
    let keyframe_root = PathBuf::from(format!("{}.keyframes", db.display()));
    let backup_root = keyframe_backup_root
        .unwrap_or_else(|| PathBuf::from(format!("{}.keyframe-backup", db.display())));
    require_safe_existing_or_absent_dir(&keyframe_root, "keyframe root")?;
    require_safe_existing_or_absent_dir(&backup_root, "keyframe backup root")?;
    RedbStore::validate_keyframe_backup_root_isolated(&keyframe_root, &backup_root)
        .map_err(to_cli_error)?;
    RedbStore::open_with_artifact_paths(
        &db.to_string_lossy(),
        PathBuf::from(format!("{}.objects", db.display())),
        PathBuf::from(format!("{}.wal", db.display())),
        keyframe_root,
        backup_root,
    )
    .map_err(to_cli_error)
}

#[derive(Debug)]
struct SidecarPath {
    source: PathBuf,
    name: &'static str,
}

fn sidecar_paths(db: &Path, keyframe_backup_root: Option<PathBuf>) -> CliResult<Vec<SidecarPath>> {
    let keyframes = PathBuf::from(format!("{}.keyframes", db.display()));
    let backup = keyframe_backup_root
        .unwrap_or_else(|| PathBuf::from(format!("{}.keyframe-backup", db.display())));
    RedbStore::validate_keyframe_backup_root_isolated(&keyframes, &backup).map_err(to_cli_error)?;
    Ok(vec![
        SidecarPath {
            source: PathBuf::from(format!("{}.objects", db.display())),
            name: "objects",
        },
        SidecarPath {
            source: PathBuf::from(format!("{}.wal", db.display())),
            name: "wal",
        },
        SidecarPath {
            source: keyframes,
            name: "keyframes",
        },
        SidecarPath {
            source: backup,
            name: "keyframe-backup",
        },
    ])
}

fn build_backup_manifest(root: &Path, db_file: &str) -> CliResult<BackupManifest> {
    let mut files = Vec::new();
    collect_file_manifest(root, root, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(BackupManifest {
        version: 1,
        created_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs(),
        db_file: db_file.to_string(),
        files,
    })
}

fn verify_backup_manifest(root: &Path) -> CliResult<()> {
    let manifest: BackupManifest = read_json(&root.join("manifest.json"))?;
    let expected = manifest
        .files
        .iter()
        .filter(|entry| entry.path != "manifest.json")
        .cloned()
        .collect::<Vec<_>>();
    let mut actual = Vec::new();
    collect_file_manifest(root, root, &mut actual)?;
    actual.retain(|entry| entry.path != "manifest.json");
    actual.sort_by(|left, right| left.path.cmp(&right.path));
    if actual != expected {
        return Err("backup manifest checksum mismatch".to_string());
    }
    Ok(())
}

fn collect_file_manifest(
    root: &Path,
    current: &Path,
    files: &mut Vec<BackupFileManifest>,
) -> CliResult<()> {
    for entry in
        fs::read_dir(current).map_err(|error| format!("read {}: {error}", current.display()))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        reject_symlink(&path)?;
        if path.is_dir() {
            collect_file_manifest(root, &path, files)?;
        } else if path.is_file() {
            let relative = path.strip_prefix(root).map_err(|error| error.to_string())?;
            let bytes =
                fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
            files.push(BackupFileManifest {
                path: relative.to_string_lossy().replace('\\', "/"),
                len: bytes.len() as u64,
                blake3: blake3::hash(&bytes).to_hex().to_string(),
            });
        }
    }
    Ok(())
}

fn copy_dir_new(source: &Path, destination: &Path) -> CliResult<()> {
    require_existing_dir(source, "source directory")?;
    require_new_path(destination, "destination directory")?;
    fs::create_dir(destination)
        .map_err(|error| format!("create {}: {error}", destination.display()))?;
    for entry in
        fs::read_dir(source).map_err(|error| format!("read {}: {error}", source.display()))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let child_source = entry.path();
        reject_symlink(&child_source)?;
        let child_destination = destination.join(entry.file_name());
        if child_source.is_dir() {
            copy_dir_new(&child_source, &child_destination)?;
        } else if child_source.is_file() {
            copy_file_new(&child_source, &child_destination)?;
        }
    }
    Ok(())
}

fn copy_file_new(source: &Path, destination: &Path) -> CliResult<()> {
    require_existing_file(source, "source file")?;
    require_new_path(destination, "destination file")?;
    require_safe_parent(destination)?;
    let mut source_file =
        fs::File::open(source).map_err(|error| format!("open {}: {error}", source.display()))?;
    let mut destination_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| format!("create {}: {error}", destination.display()))?;
    io::copy(&mut source_file, &mut destination_file).map_err(|error| {
        format!(
            "copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    destination_file
        .sync_all()
        .map_err(|error| format!("sync {}: {error}", destination.display()))
}

fn read_auth_state(path: &Path) -> CliResult<AuthState> {
    require_existing_file(path, "auth state")?;
    read_json(path)
}

fn read_auth_state_if_exists(path: &Path) -> CliResult<AuthState> {
    if path.exists() {
        read_auth_state(path)
    } else {
        Ok(AuthState::default())
    }
}

fn write_auth_state(path: &Path, state: &AuthState) -> CliResult<()> {
    if path.exists() {
        reject_symlink(path)?;
    }
    require_safe_parent(path)?;
    let temp = path.with_extension("json.tmp");
    if temp.exists() {
        return Err(format!(
            "temporary auth state path already exists: {}",
            temp.display()
        ));
    }
    write_secret_json_new(&temp, state)?;
    fs::rename(&temp, path)
        .map_err(|error| format!("rename {} to {}: {error}", temp.display(), path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> CliResult<T> {
    reject_symlink(path)?;
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("decode {}: {error}", path.display()))
}

fn write_json_new<T: Serialize>(path: &Path, value: &T) -> CliResult<()> {
    require_new_path(path, "json output")?;
    require_safe_parent(path)?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.write_all(b"\n")
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

fn write_secret_json_new<T: Serialize>(path: &Path, value: &T) -> CliResult<()> {
    require_new_path(path, "secret json output")?;
    require_safe_parent(path)?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    configure_secret_open_options(&mut options);
    let mut file = options
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.write_all(b"\n")
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    enforce_secret_permissions(path)
}

#[cfg(unix)]
fn configure_secret_open_options(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn configure_secret_open_options(_options: &mut fs::OpenOptions) {}

#[cfg(unix)]
fn enforce_secret_permissions(path: &Path) -> CliResult<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("set permissions on {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn enforce_secret_permissions(path: &Path) -> CliResult<()> {
    let _ = fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    Ok(())
}

fn require_existing_file(path: &Path, label: &str) -> CliResult<()> {
    reject_symlink(path)?;
    if !path.is_file() {
        return Err(format!(
            "{label} must be an existing regular file: {}",
            path.display()
        ));
    }
    Ok(())
}

fn require_existing_dir(path: &Path, label: &str) -> CliResult<()> {
    reject_symlink(path)?;
    if !path.is_dir() {
        return Err(format!(
            "{label} must be an existing directory: {}",
            path.display()
        ));
    }
    Ok(())
}

fn require_safe_existing_or_absent_dir(path: &Path, label: &str) -> CliResult<()> {
    if path.exists() {
        require_existing_dir(path, label)
    } else {
        require_safe_parent(path)
    }
}

fn require_new_path(path: &Path, label: &str) -> CliResult<()> {
    if fs::symlink_metadata(path).is_ok() {
        return Err(format!("{label} already exists: {}", path.display()));
    }
    Ok(())
}

fn require_safe_parent(path: &Path) -> CliResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("path has no parent: {}", path.display()))?;
    require_existing_dir(parent, "parent directory")
}

fn reject_symlink(path: &Path) -> CliResult<()> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(format!("symlink paths are not allowed: {}", path.display()));
    }
    Ok(())
}

fn to_cli_error(error: RedbError) -> String {
    error.to_string()
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Debug)]
struct Options {
    values: Vec<(String, String)>,
}

impl Options {
    fn parse(args: &[String]) -> CliResult<Self> {
        let mut values = Vec::new();
        let mut index = 0;
        while index < args.len() {
            let key = &args[index];
            if !key.starts_with("--") {
                return Err(format!("unexpected argument: {key}"));
            }
            index += 1;
            let value = args
                .get(index)
                .ok_or_else(|| format!("missing value after {key}"))?
                .clone();
            if value.starts_with("--") {
                return Err(format!("missing value after {key}"));
            }
            values.push((key.clone(), value));
            index += 1;
        }
        Ok(Self { values })
    }

    fn required(&self, key: &str) -> CliResult<String> {
        self.values
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| format!("missing required option {key}"))
    }

    fn required_path(&self, key: &str) -> CliResult<PathBuf> {
        Ok(PathBuf::from(self.required(key)?))
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        self.values
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| PathBuf::from(value))
    }

    fn required_tick(&self, key: &str) -> CliResult<Tick> {
        self.required(key)?
            .parse()
            .map_err(|_| format!("{key} must be an integer tick"))
    }

    fn optional_tick(&self, key: &str) -> CliResult<Option<Tick>> {
        self.values
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| {
                value
                    .parse()
                    .map_err(|_| format!("{key} must be an integer tick"))
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::{
        redb_store::{SnapshotRow, TickCommitPayload, TickTerminalState},
        tick::{
            ReplayInputEnvelope, TickCommitRecord, TickFuelLedger, WorldSnapshot, commands_hash,
        },
        world::create_world,
    };

    fn run_ok(args: Vec<String>) -> String {
        let mut output = Vec::new();
        run(args, &mut output).expect("command succeeds");
        String::from_utf8(output).expect("utf8 output")
    }

    fn payload(tick: Tick, checksum: u64, keyframe: bool) -> TickCommitPayload {
        let mut commit_record = TickCommitRecord {
            commands: Vec::new(),
            rejections: Vec::new(),
            fuel: TickFuelLedger::default(),
            deploy_activation_decision: Vec::new(),
            canonical_codec_version: 1,
            snapshot_hash: [4; 32],
            commands_hash: commands_hash(&Vec::new(), &Vec::new()),
            state_checksum: checksum,
            manifest_hash: [1; 32],
            world_config_hash: [2; 32],
            mods_lock_hash: [3; 32],
            resolved_config_hash: [5; 32],
        };
        let snapshot = keyframe.then(|| snapshot_row(tick, checksum));
        if snapshot.is_some() {
            commit_record.snapshot_hash = [8; 32];
        }
        let mods_lock_hash = commit_record.mods_lock_hash;
        let replay_input_envelopes: Vec<ReplayInputEnvelope> = Vec::new();
        let replay_input_envelope_bytes = serde_json::to_vec(&replay_input_envelopes).unwrap();
        TickCommitPayload {
            tick,
            commit_record,
            tick_trace_blob: format!("trace-{tick}").into_bytes(),
            recovery_state_blob: None,
            object_id: format!("tick-trace/{tick}.zst"),
            terminal_state: TickTerminalState::Verified,
            system_manifest_hash: [6; 32],
            mods_lock_hash,
            keyframe: snapshot,
            replay_critical_writes: vec![(
                format!("/tick/{tick}/replay_input_envelope").into_bytes(),
                replay_input_envelope_bytes,
            )],
        }
    }

    fn snapshot_row(tick: Tick, state_checksum: u64) -> SnapshotRow {
        let mut world = create_world();
        let state = WorldSnapshot::capture(world.app.world_mut());
        let content_hash = RedbStore::snapshot_content_hash_for_state(&state).unwrap();
        SnapshotRow {
            tick,
            state_checksum,
            content_hash,
            state,
        }
    }

    fn open_fixture_store(db: &Path) -> RedbStore {
        RedbStore::open_with_artifact_paths(
            &db.to_string_lossy(),
            PathBuf::from(format!("{}.objects", db.display())),
            PathBuf::from(format!("{}.wal", db.display())),
            PathBuf::from(format!("{}.keyframes", db.display())),
            PathBuf::from(format!("{}.keyframe-backup", db.display())),
        )
        .unwrap()
    }

    fn seeded_store(directory: &tempfile::TempDir) -> PathBuf {
        let db = directory.path().join("swarm.redb");
        let mut store = open_fixture_store(&db);
        store.commit_tick_payload(payload(0, 77, true)).unwrap();
        store.commit_tick_payload(payload(1, 88, false)).unwrap();
        store.wait_for_archive(0, Duration::from_secs(1)).unwrap();
        store.wait_for_archive(1, Duration::from_secs(1)).unwrap();
        drop(store);
        db
    }

    #[test]
    fn help_documents_operational_commands() {
        let help = run_ok(vec!["--help".to_string()]);

        assert!(help.contains("swarm deploy"));
        assert!(help.contains("backup create"));
        assert!(help.contains("keyframe restore"));
        assert!(help.contains("auth epoch-bump"));
    }

    #[test]
    fn cold_backup_verify_and_inspect_use_real_store() {
        let directory = tempfile::tempdir().unwrap();
        let db = seeded_store(&directory);
        let backup = directory.path().join("backup");

        let backup_output = run_ok(vec![
            "backup".into(),
            "create".into(),
            "--db".into(),
            db.to_string_lossy().into_owned(),
            "--out".into(),
            backup.to_string_lossy().into_owned(),
        ]);
        assert!(backup_output.contains("created backup"));

        let verify_output = run_ok(vec![
            "verify".into(),
            "--backup".into(),
            backup.to_string_lossy().into_owned(),
        ]);
        assert!(verify_output.contains("verified database"));

        let inspect_output = run_ok(vec![
            "inspect".into(),
            "--db".into(),
            db.to_string_lossy().into_owned(),
        ]);
        assert!(inspect_output.contains("\"verified_ticks\": 2"));
        assert!(inspect_output.contains("\"keyframes\": 1"));
    }

    #[test]
    fn backup_rejects_existing_destination_and_symlink_database() {
        let directory = tempfile::tempdir().unwrap();
        let db = seeded_store(&directory);
        let backup = directory.path().join("backup");
        fs::create_dir(&backup).unwrap();

        let error = run(
            vec![
                "backup".into(),
                "create".into(),
                "--db".into(),
                db.to_string_lossy().into_owned(),
                "--out".into(),
                backup.to_string_lossy().into_owned(),
            ],
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.contains("already exists"));

        #[cfg(unix)]
        {
            let link = directory.path().join("db-link");
            std::os::unix::fs::symlink(&db, &link).unwrap();
            let error = run(
                vec![
                    "verify".into(),
                    "--db".into(),
                    link.to_string_lossy().into_owned(),
                ],
                &mut Vec::new(),
            )
            .unwrap_err();
            assert!(error.contains("symlink"));
        }
    }

    #[test]
    fn keyframe_export_restore_validates_and_refuses_clobber() {
        let directory = tempfile::tempdir().unwrap();
        let db = seeded_store(&directory);
        let export = directory.path().join("keyframe.snap");

        run_ok(vec![
            "keyframe".into(),
            "backup".into(),
            "--db".into(),
            db.to_string_lossy().into_owned(),
            "--tick".into(),
            "0".into(),
            "--out".into(),
            export.to_string_lossy().into_owned(),
        ]);

        let primary = PathBuf::from(format!("{}.keyframes/0.snap", db.display()));
        let backup = PathBuf::from(format!(
            "{}.keyframe-backup/default/default/0.snap",
            db.display()
        ));
        fs::remove_file(&primary).unwrap();
        fs::remove_file(&backup).unwrap();

        run_ok(vec![
            "keyframe".into(),
            "restore".into(),
            "--db".into(),
            db.to_string_lossy().into_owned(),
            "--tick".into(),
            "0".into(),
            "--from".into(),
            export.to_string_lossy().into_owned(),
        ]);

        let error = run(
            vec![
                "keyframe".into(),
                "restore".into(),
                "--db".into(),
                db.to_string_lossy().into_owned(),
                "--tick".into(),
                "0".into(),
                "--from".into(),
                export.to_string_lossy().into_owned(),
            ],
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.contains("create primary keyframe restore"));
    }

    #[test]
    fn ca_lifecycle_revocation_and_epoch_persist() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("auth.json");

        let init = run_ok(vec![
            "ca".into(),
            "init".into(),
            "--auth-state".into(),
            state.to_string_lossy().into_owned(),
        ]);
        assert!(init.contains("initialized ca fingerprint="));
        let fingerprint = run_ok(vec![
            "ca".into(),
            "fingerprint".into(),
            "--auth-state".into(),
            state.to_string_lossy().into_owned(),
        ]);
        assert_eq!(fingerprint.trim().len(), 64);

        run_ok(vec![
            "cert".into(),
            "revoke".into(),
            "--auth-state".into(),
            state.to_string_lossy().into_owned(),
            "--cert-id".into(),
            "cert-1".into(),
        ]);
        let epoch = run_ok(vec![
            "auth".into(),
            "epoch-bump".into(),
            "--auth-state".into(),
            state.to_string_lossy().into_owned(),
        ]);
        assert!(epoch.contains("auth epoch 1"));

        let persisted: AuthState = read_json(&state).unwrap();
        assert!(persisted.revoked_cert_ids.contains("cert-1"));
        assert_eq!(persisted.auth_epoch, 1);
    }

    #[cfg(unix)]
    #[test]
    fn auth_state_with_ca_seed_is_created_and_rewritten_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("auth.json");

        run_ok(vec![
            "ca".into(),
            "init".into(),
            "--auth-state".into(),
            state.to_string_lossy().into_owned(),
        ]);
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::set_permissions(&state, fs::Permissions::from_mode(0o644)).unwrap();
        run_ok(vec![
            "auth".into(),
            "epoch-bump".into(),
            "--auth-state".into(),
            state.to_string_lossy().into_owned(),
        ]);

        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
