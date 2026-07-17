use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    net::IpAddr,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ed25519_dalek::{Signer, SigningKey};
use reqwest::{
    Url,
    blocking::{Client, Response},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wasmparser::{Parser, Payload};

use crate::{
    DeployParams, DeployResult,
    mcp::{CertCheckParams, CertCheckResult, DeployPayload, encode_base64},
};

type CliResult<T> = Result<T, String>;

const DEPLOY_DOMAIN: &str = "SWARM-DEPLOY-V1";
const DEFAULT_ENGINE_ABI_VERSION: u32 = 1;
const ENGINE_MAX_WASM_BYTES: usize = 5 * 1024 * 1024;
const MAX_GATEWAY_RESPONSE_BYTES: usize = 1024 * 1024;
const TARGET_MANIFEST_SECTION_NAME: &str = "swarm.target_manifest";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(args: &[String], out: &mut dyn Write) -> CliResult<()> {
    let raw = DeployCliArgs::parse(args)?;
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let preauth_artifact = raw
        .artifact
        .clone()
        .or_else(|| env("SWARM_ARTIFACT").map(PathBuf::from));
    let raw_wasm = load_raw_wasm_input(&raw.input, preauth_artifact.as_deref())?;
    let auth_path = raw
        .auth_file
        .clone()
        .or_else(|| env("SWARM_AUTH_FILE").map(PathBuf::from))
        .ok_or_else(|| "missing required option --auth-file or SWARM_AUTH_FILE".to_string())?;
    let auth_file = read_deploy_auth_file(&auth_path)?;
    let auth = DeployAuth::from_auth_file(&auth_file)?;
    let now = unix_now()?;
    let resolved =
        ResolvedDeployOptions::from_sources(raw, &auth_file, &env, now.as_millis() as u64)?;
    if resolved.auth_file_artifact_ignored {
        return Err("auth-file artifact defaults cannot be used because artifacts are selected before auth secrets; use --artifact or SWARM_ARTIFACT".to_string());
    }
    let wasm_bytes = finalize_wasm_input(
        &raw_wasm,
        &resolved.target_manifest_hash,
        resolved.engine_abi_version,
    )?;
    let client = http_client()?;
    let cert_check = build_signed_cert_check_request(&resolved, &auth, unix_now()?)?;
    let cert_result: CertCheckResult =
        post_signed_json_rpc(&client, &resolved.gateway_url, &cert_check)?;
    validate_cert_check_result(&cert_result, &auth)?;
    let deploy = build_signed_deploy_request(&resolved, &auth, wasm_bytes, unix_now()?)?;
    let result: DeployResult =
        post_signed_json_rpc(&client, &resolved.gateway_url, &deploy.request)?;
    validate_deploy_result(&result, &deploy.module_hash)?;

    writeln!(
        out,
        "module_id={} status={} module_hash={} version_counter={}",
        result.module_id, result.status, result.module_hash, result.redb_version_counter
    )
    .map_err(|error| error.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeployCliArgs {
    input: PathBuf,
    auth_file: Option<PathBuf>,
    gateway_url: Option<String>,
    world_id: Option<String>,
    room_id: Option<String>,
    drone_id: Option<String>,
    target_manifest_hash: Option<String>,
    engine_abi_version: Option<String>,
    language: Option<String>,
    version_tag: Option<String>,
    version_counter: Option<String>,
    artifact: Option<PathBuf>,
    allow_insecure_loopback: bool,
}

impl DeployCliArgs {
    fn parse(args: &[String]) -> CliResult<Self> {
        let mut input = None;
        let mut parsed = Self {
            input: PathBuf::new(),
            auth_file: None,
            gateway_url: None,
            world_id: None,
            room_id: None,
            drone_id: None,
            target_manifest_hash: None,
            engine_abi_version: None,
            language: None,
            version_tag: None,
            version_counter: None,
            artifact: None,
            allow_insecure_loopback: false,
        };
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            if !arg.starts_with("--") {
                if input.is_some() {
                    return Err(format!("unexpected argument: {arg}"));
                }
                input = Some(PathBuf::from(arg));
                index += 1;
                continue;
            }
            if arg == "--allow-insecure-loopback" {
                parsed.allow_insecure_loopback = true;
                index += 1;
                continue;
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("missing value after {arg}"))?;
            if value.starts_with("--") {
                return Err(format!("missing value after {arg}"));
            }
            match arg.as_str() {
                "--auth-file" => parsed.auth_file = Some(PathBuf::from(value)),
                "--gateway-url" => parsed.gateway_url = Some(value.clone()),
                "--world-id" => parsed.world_id = Some(value.clone()),
                "--room-id" => parsed.room_id = Some(value.clone()),
                "--drone-id" => parsed.drone_id = Some(value.clone()),
                "--target-manifest-hash" => parsed.target_manifest_hash = Some(value.clone()),
                "--engine-abi-version" => parsed.engine_abi_version = Some(value.clone()),
                "--language" => parsed.language = Some(value.clone()),
                "--version-tag" => parsed.version_tag = Some(value.clone()),
                "--version-counter" => parsed.version_counter = Some(value.clone()),
                "--artifact" => parsed.artifact = Some(PathBuf::from(value)),
                _ => return Err(format!("unknown deploy option {arg}")),
            }
            index += 2;
        }
        parsed.input = input.ok_or_else(|| "missing deploy input <project-or-wasm>".to_string())?;
        Ok(parsed)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct DeployAuthFile {
    version: Option<u32>,
    private_key_hex: String,
    certificate_bundle: Value,
    gateway_url: Option<String>,
    world_id: Option<String>,
    room_id: Option<Value>,
    drone_id: Option<Value>,
    target_manifest_hash: Option<String>,
    engine_abi_version: Option<Value>,
    language: Option<String>,
    version_tag: Option<String>,
    version_counter: Option<Value>,
    artifact: Option<String>,
}

#[derive(Debug)]
struct DeployAuth {
    signing_key: SigningKey,
    certificate_bundle: Value,
    client_cert_id: String,
    player_id_header: String,
    deploy_player_id: u32,
    code_signing_cert_id: String,
}

impl DeployAuth {
    fn from_auth_file(file: &DeployAuthFile) -> CliResult<Self> {
        if file.version.unwrap_or(1) != 1 {
            return Err("auth file version must be 1".to_string());
        }
        if !file.certificate_bundle.is_object() {
            return Err("certificate_bundle must be a JSON object".to_string());
        }
        let key_bytes = hex_decode(file.private_key_hex.trim())?;
        let seed: [u8; 32] = key_bytes
            .try_into()
            .map_err(|_| "private_key_hex must encode a 32-byte Ed25519 private key".to_string())?;
        let signing_key = SigningKey::from_bytes(&seed);
        let client_cert_id = bundle_string_field(
            &file.certificate_bundle,
            &[
                &["cert_id"][..],
                &["certificate_id"],
                &["payload", "cert_id"],
                &["payload", "certificate_id"],
            ],
        )
        .ok_or_else(|| "certificate_bundle does not include a certificate id".to_string())?;
        let player_id_header = bundle_string_field(
            &file.certificate_bundle,
            &[&["player_id"][..], &["payload", "player_id"]],
        )
        .ok_or_else(|| "certificate_bundle does not include a player id".to_string())?;
        let deploy_player_id = parse_player_id_for_deploy(&player_id_header)?;
        let code_signing_cert_id = code_signing_cert_id(&file.certificate_bundle, &client_cert_id)?;
        Ok(Self {
            signing_key,
            certificate_bundle: file.certificate_bundle.clone(),
            client_cert_id,
            player_id_header,
            deploy_player_id,
            code_signing_cert_id,
        })
    }
}

#[derive(Debug, Clone)]
struct ResolvedDeployOptions {
    gateway_url: Url,
    world_id: String,
    room_id: u32,
    drone_id: u64,
    target_manifest_hash: String,
    engine_abi_version: u32,
    language: String,
    version_tag: String,
    version_counter: u64,
    auth_file_artifact_ignored: bool,
}

impl ResolvedDeployOptions {
    fn from_sources(
        raw: DeployCliArgs,
        auth_file: &DeployAuthFile,
        env: &dyn Fn(&str) -> Option<String>,
        default_counter: u64,
    ) -> CliResult<Self> {
        let gateway_url = resolve_required_string(
            raw.gateway_url,
            auth_file.gateway_url.clone(),
            "SWARM_GATEWAY_URL",
            "gateway-url",
            env,
        )?;
        let world_id = resolve_required_string(
            raw.world_id,
            auth_file.world_id.clone(),
            "SWARM_WORLD_ID",
            "world-id",
            env,
        )?;
        let room_id = parse_u32(
            &resolve_required_string(
                raw.room_id,
                value_string(auth_file.room_id.as_ref()),
                "SWARM_ROOM_ID",
                "room-id",
                env,
            )?,
            "room-id",
        )?;
        let drone_id = parse_u64(
            &resolve_required_string(
                raw.drone_id,
                value_string(auth_file.drone_id.as_ref()),
                "SWARM_DRONE_ID",
                "drone-id",
                env,
            )?,
            "drone-id",
        )?;
        let target_manifest_hash = resolve_required_string(
            raw.target_manifest_hash,
            auth_file.target_manifest_hash.clone(),
            "SWARM_TARGET_MANIFEST_HASH",
            "target-manifest-hash",
            env,
        )?;
        let engine_abi_version = match resolve_optional_string(
            raw.engine_abi_version,
            value_string(auth_file.engine_abi_version.as_ref()),
            "SWARM_ENGINE_ABI_VERSION",
            env,
        ) {
            Some(value) => parse_u32(&value, "engine-abi-version")?,
            None => DEFAULT_ENGINE_ABI_VERSION,
        };
        let language = resolve_optional_string(
            raw.language,
            auth_file.language.clone(),
            "SWARM_LANGUAGE",
            env,
        )
        .unwrap_or_else(|| default_language_for_input(&raw.input).to_string());
        let version_counter = match resolve_optional_string(
            raw.version_counter,
            value_string(auth_file.version_counter.as_ref()),
            "SWARM_VERSION_COUNTER",
            env,
        ) {
            Some(value) => parse_u64(&value, "version-counter")?,
            None => default_counter,
        };
        if version_counter == 0 {
            return Err("version-counter must be greater than zero".to_string());
        }
        let version_tag = resolve_optional_string(
            raw.version_tag,
            auth_file.version_tag.clone(),
            "SWARM_VERSION_TAG",
            env,
        )
        .unwrap_or_else(|| format!("cli-{version_counter}"));
        let auth_file_artifact_ignored = raw.artifact.is_none()
            && env("SWARM_ARTIFACT").is_none()
            && auth_file
                .artifact
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty());

        Ok(Self {
            gateway_url: normalize_gateway_url(&gateway_url, raw.allow_insecure_loopback)?,
            world_id,
            room_id,
            drone_id,
            target_manifest_hash,
            engine_abi_version,
            language,
            version_tag,
            version_counter,
            auth_file_artifact_ignored,
        })
    }
}

#[derive(Debug)]
struct SignedJsonRpcRequest {
    id: String,
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

#[derive(Debug)]
struct SignedDeployRequest {
    request: SignedJsonRpcRequest,
    module_hash: String,
}

#[derive(Serialize)]
struct JsonRpcRequest<T: Serialize> {
    jsonrpc: &'static str,
    id: String,
    method: &'static str,
    params: T,
}

#[derive(Deserialize)]
struct JsonRpcEnvelope<T> {
    jsonrpc: String,
    id: Value,
    result: Option<T>,
    error: Option<JsonRpcErrorValue>,
}

#[derive(Deserialize)]
struct JsonRpcErrorValue {
    message: Option<String>,
}

#[derive(Serialize)]
struct TargetManifestSection<'a> {
    target_manifest_hash: &'a str,
    engine_abi_version: u32,
}

#[derive(Serialize)]
struct DeployMetadata<'a> {
    name: &'a str,
    version: &'a str,
    language: &'a str,
    target_manifest_hash: &'a str,
    engine_abi_version: u32,
}

fn read_deploy_auth_file(path: &Path) -> CliResult<DeployAuthFile> {
    require_secret_file(path)?;
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("decode {}: {error}", path.display()))
}

fn require_secret_file(path: &Path) -> CliResult<()> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("symlink paths are not allowed: {}", path.display()));
    }
    if !metadata.is_file() {
        return Err(format!(
            "auth file must be a regular file: {}",
            path.display()
        ));
    }
    enforce_secret_file_owner_and_mode(path, &metadata)
}

#[cfg(unix)]
fn enforce_secret_file_owner_and_mode(path: &Path, metadata: &fs::Metadata) -> CliResult<()> {
    use std::os::unix::fs::MetadataExt;

    let euid = effective_uid();
    if metadata.uid() != euid {
        return Err(format!(
            "auth file must be owned by the effective user: {}",
            path.display()
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(format!(
            "auth file must not grant group/world permissions: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    unsafe { geteuid() }
}

#[cfg(not(unix))]
fn enforce_secret_file_owner_and_mode(_path: &Path, _metadata: &fs::Metadata) -> CliResult<()> {
    Ok(())
}

fn load_raw_wasm_input(input: &Path, artifact: Option<&Path>) -> CliResult<Vec<u8>> {
    let metadata = fs::symlink_metadata(input)
        .map_err(|error| format!("stat {}: {error}", input.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "symlink paths are not allowed: {}",
            input.display()
        ));
    }
    let artifact_path = if metadata.is_file() {
        if artifact.is_some() {
            return Err("--artifact is only valid when deploying a project directory".to_string());
        }
        if input.extension().and_then(|value| value.to_str()) != Some("wasm") {
            return Err(format!(
                "deploy file must have .wasm extension: {}",
                input.display()
            ));
        }
        reject_symlink_components(input)?;
        let artifact_path = input
            .canonicalize()
            .map_err(|error| format!("canonicalize {}: {error}", input.display()))?;
        validate_artifact_directory_security(&artifact_path)?;
        artifact_path
    } else if metadata.is_dir() {
        let project_root = secure_project_root(input)?;
        build_project(&project_root)?;
        select_project_artifact(&project_root, artifact)?
    } else {
        return Err(format!(
            "deploy input must be a regular .wasm file or project directory: {}",
            input.display()
        ));
    };
    read_wasm_artifact_once(&artifact_path)
}

fn finalize_wasm_input(
    raw: &[u8],
    target_manifest_hash: &str,
    engine_abi_version: u32,
) -> CliResult<Vec<u8>> {
    reject_existing_target_manifest_section(raw)?;
    let final_wasm = append_target_manifest_section(raw, target_manifest_hash, engine_abi_version)?;
    if final_wasm.len() > ENGINE_MAX_WASM_BYTES {
        return Err(format!(
            "wasm module exceeds Engine max size of {ENGINE_MAX_WASM_BYTES} bytes"
        ));
    }
    Ok(final_wasm)
}

fn build_project(project: &Path) -> CliResult<()> {
    let package = project.join("package.json");
    require_regular_file(&package, "package.json")?;
    let status = build_project_command(project)
        .status()
        .map_err(|error| format!("run npm build in {}: {error}", project.display()))?;
    if !status.success() {
        return Err(format!("npm run build failed in {}", project.display()));
    }
    Ok(())
}

fn build_project_command(project: &Path) -> Command {
    let mut command = Command::new("npm");
    command.arg("run").arg("build").current_dir(project);
    scrub_deploy_env(&mut command);
    command
}

fn scrub_deploy_env(command: &mut Command) {
    for name in deploy_env_names_to_scrub(std::env::vars().map(|(name, _)| name)) {
        command.env_remove(name);
    }
}

fn deploy_env_names_to_scrub<I>(env_names: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = String>,
{
    let mut names = env_names
        .into_iter()
        .filter(|name| name.starts_with("SWARM_"))
        .collect::<BTreeSet<_>>();
    for name in [
        "SWARM_AUTH_FILE",
        "SWARM_GATEWAY_URL",
        "SWARM_WORLD_ID",
        "SWARM_ROOM_ID",
        "SWARM_DRONE_ID",
        "SWARM_TARGET_MANIFEST_HASH",
        "SWARM_ENGINE_ABI_VERSION",
        "SWARM_LANGUAGE",
        "SWARM_VERSION_TAG",
        "SWARM_VERSION_COUNTER",
        "SWARM_ARTIFACT",
    ] {
        names.insert(name.to_string());
    }
    names
}

fn select_project_artifact(project: &Path, artifact: Option<&Path>) -> CliResult<PathBuf> {
    if let Some(relative) = artifact {
        reject_unsafe_relative_artifact(relative)?;
        let path = project.join(relative);
        reject_symlink_components(&path)?;
        require_regular_file(&path, "wasm artifact")?;
        if path.extension().and_then(|value| value.to_str()) != Some("wasm") {
            return Err("--artifact must reference a .wasm file".to_string());
        }
        let artifact = path
            .canonicalize()
            .map_err(|error| format!("canonicalize {}: {error}", path.display()))?;
        require_artifact_under_project(project, &artifact)?;
        validate_artifact_directory_security(&artifact)?;
        return Ok(artifact);
    }

    let build_dir = project.join("build");
    reject_symlink_components(&build_dir)?;
    validate_directory_security(&build_dir)?;
    let mut wasm_files = Vec::new();
    let entries = fs::read_dir(&build_dir)
        .map_err(|error| format!("read build directory {}: {error}", build_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("wasm") {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("stat {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("symlink paths are not allowed: {}", path.display()));
        }
        if metadata.is_file() {
            reject_symlink_components(&path)?;
            let artifact = path
                .canonicalize()
                .map_err(|error| format!("canonicalize {}: {error}", path.display()))?;
            require_artifact_under_project(project, &artifact)?;
            validate_artifact_directory_security(&artifact)?;
            wasm_files.push(artifact);
        }
    }
    match wasm_files.len() {
        1 => Ok(wasm_files.remove(0)),
        0 => Err(format!(
            "project build must produce exactly one regular build/*.wasm artifact in {}",
            build_dir.display()
        )),
        _ => Err(format!(
            "project build produced multiple build/*.wasm artifacts in {}",
            build_dir.display()
        )),
    }
}

fn reject_unsafe_relative_artifact(path: &Path) -> CliResult<()> {
    if path.is_absolute() {
        return Err("--artifact must be a relative path".to_string());
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err("--artifact must not contain traversal or prefixes".to_string());
    }
    Ok(())
}

fn secure_project_root(project: &Path) -> CliResult<PathBuf> {
    reject_symlink_components(project)?;
    let root = project
        .canonicalize()
        .map_err(|error| format!("canonicalize {}: {error}", project.display()))?;
    validate_directory_security(&root)?;
    Ok(root)
}

fn require_artifact_under_project(project: &Path, artifact: &Path) -> CliResult<()> {
    if !artifact.starts_with(project) {
        return Err(format!(
            "wasm artifact must remain under project root {}: {}",
            project.display(),
            artifact.display()
        ));
    }
    Ok(())
}

fn validate_artifact_directory_security(artifact: &Path) -> CliResult<()> {
    let parent = artifact
        .parent()
        .ok_or_else(|| format!("artifact has no parent: {}", artifact.display()))?;
    validate_directory_security(parent)
}

fn require_regular_file(path: &Path, label: &str) -> CliResult<()> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("symlink paths are not allowed: {}", path.display()));
    }
    if !metadata.is_file() {
        return Err(format!(
            "{label} must be a regular file: {}",
            path.display()
        ));
    }
    Ok(())
}

fn reject_symlink_components(path: &Path) -> CliResult<()> {
    let mut current = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().map_err(|error| format!("read current directory: {error}"))?
    };
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => current.push(component.as_os_str()),
            Component::Normal(part) => {
                current.push(part);
                let metadata = fs::symlink_metadata(&current)
                    .map_err(|error| format!("stat {}: {error}", current.display()))?;
                if metadata.file_type().is_symlink() {
                    return Err(format!(
                        "symlink paths are not allowed: {}",
                        current.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_directory_security(path: &Path) -> CliResult<()> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("symlink paths are not allowed: {}", path.display()));
    }
    if !metadata.is_dir() {
        return Err(format!("path must be a directory: {}", path.display()));
    }
    enforce_directory_owner_and_mode(path, &metadata)
}

#[cfg(unix)]
fn enforce_directory_owner_and_mode(path: &Path, metadata: &fs::Metadata) -> CliResult<()> {
    use std::os::unix::fs::MetadataExt;

    let euid = effective_uid();
    if metadata.uid() != euid {
        return Err(format!(
            "artifact directory must be owned by the effective user: {}",
            path.display()
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(format!(
            "artifact directory must not be group/world-writable: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_directory_owner_and_mode(_path: &Path, _metadata: &fs::Metadata) -> CliResult<()> {
    Ok(())
}

fn read_wasm_artifact_once(path: &Path) -> CliResult<Vec<u8>> {
    let file = open_artifact_no_follow(path)?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("stat opened artifact {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "wasm artifact must be a regular file: {}",
            path.display()
        ));
    }
    if metadata.len() > ENGINE_MAX_WASM_BYTES as u64 {
        return Err(format!(
            "wasm module exceeds Engine max size of {ENGINE_MAX_WASM_BYTES} bytes"
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut limited = file.take(ENGINE_MAX_WASM_BYTES as u64 + 1);
    limited
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() > ENGINE_MAX_WASM_BYTES {
        return Err(format!(
            "wasm module exceeds Engine max size of {ENGINE_MAX_WASM_BYTES} bytes"
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_artifact_no_follow(path: &Path) -> CliResult<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn open_artifact_no_follow(path: &Path) -> CliResult<fs::File> {
    fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))
}

fn build_signed_deploy_request(
    options: &ResolvedDeployOptions,
    auth: &DeployAuth,
    wasm_bytes: Vec<u8>,
    now: Duration,
) -> CliResult<SignedDeployRequest> {
    if wasm_bytes.is_empty() {
        return Err("wasm artifact is empty".to_string());
    }
    if !wasm_bytes.starts_with(b"\0asm") {
        return Err("wasm artifact must be a wasm module".to_string());
    }
    let metadata = deploy_metadata(
        &options.version_tag,
        &options.language,
        &options.target_manifest_hash,
        options.engine_abi_version,
    )?;
    let wasm_module_hash = blake3_label(blake3::hash(&wasm_bytes).as_bytes());
    let metadata_hash = blake3_label(blake3::hash(metadata.as_bytes()).as_bytes());
    let payload = DeployPayload {
        domain: DEPLOY_DOMAIN.to_string(),
        wasm_module_hash: wasm_module_hash.clone(),
        metadata_hash,
        player_id: auth.deploy_player_id,
        world_id: options.world_id.clone(),
        module_slot: format!("room:{}", options.room_id),
        target_manifest_hash: options.target_manifest_hash.clone(),
        engine_abi_version: options.engine_abi_version,
        version_counter: options.version_counter,
        transport: "mcp".to_string(),
        signed_at: now.as_secs(),
    };
    let payload_bytes = serde_json::to_vec(&payload).map_err(|error| error.to_string())?;
    let code_signature = auth.signing_key.sign(&payload_bytes);
    let params = DeployParams {
        player_id: auth.deploy_player_id,
        drone_id: options.drone_id,
        wasm_bytes: encode_base64(&wasm_bytes),
        metadata,
        deploy_payload: payload,
        code_signature: encode_base64(&code_signature.to_bytes()),
        certificate_id: auth.code_signing_cert_id.clone(),
        version_counter: options.version_counter,
    };
    Ok(SignedDeployRequest {
        request: build_signed_json_rpc_request(
            options,
            auth,
            format!("deploy-{}", options.version_tag),
            "swarm_deploy",
            params,
            now,
        )?,
        module_hash: wasm_module_hash,
    })
}

fn build_signed_cert_check_request(
    options: &ResolvedDeployOptions,
    auth: &DeployAuth,
    now: Duration,
) -> CliResult<SignedJsonRpcRequest> {
    build_signed_json_rpc_request(
        options,
        auth,
        format!("cert-check-{}", options.version_counter),
        "swarm_cert_check",
        CertCheckParams {
            certificate_id: auth.client_cert_id.clone(),
        },
        now,
    )
}

fn build_signed_json_rpc_request<T: Serialize>(
    options: &ResolvedDeployOptions,
    auth: &DeployAuth,
    id: String,
    method: &'static str,
    params: T,
    now: Duration,
) -> CliResult<SignedJsonRpcRequest> {
    let rpc = JsonRpcRequest {
        jsonrpc: "2.0",
        id: id.clone(),
        method,
        params,
    };
    let body_value = serde_json::to_value(&rpc).map_err(|error| error.to_string())?;
    let body = serde_json::to_vec(&rpc).map_err(|error| error.to_string())?;
    let timestamp = iso8601_utc(now);
    let nonce = random_hex_nonce()?;
    let canonical = canonical_client_request(
        "POST",
        options.gateway_url.path(),
        &body_value,
        &auth.client_cert_id,
        &auth.player_id_header,
        "",
        &timestamp,
        &nonce,
    );
    let request_signature = auth.signing_key.sign(canonical.as_bytes());
    let certificate_bundle =
        serde_json::to_string(&auth.certificate_bundle).map_err(|error| error.to_string())?;
    Ok(SignedJsonRpcRequest {
        id,
        body,
        headers: vec![
            ("Content-Type", "application/json".to_string()),
            ("Accept", "application/json".to_string()),
            ("X-Swarm-Transport", "mcp".to_string()),
            ("Swarm-Certificate", certificate_bundle),
            ("Swarm-Cert-Id", auth.client_cert_id.clone()),
            ("X-Swarm-Player-Id", auth.player_id_header.clone()),
            ("Swarm-Timestamp", timestamp),
            ("Swarm-Nonce", nonce),
            ("Swarm-Signature", hex_encode(&request_signature.to_bytes())),
        ],
    })
}

fn http_client() -> CliResult<Client> {
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .map_err(|error| format!("build HTTP client: {error}"))
}

fn post_signed_json_rpc<T: for<'de> Deserialize<'de>>(
    client: &Client,
    url: &Url,
    request: &SignedJsonRpcRequest,
) -> CliResult<T> {
    let mut builder = client.post(url.clone());
    for (name, value) in &request.headers {
        builder = builder.header(*name, value.as_str());
    }
    let response = builder
        .body(request.body.clone())
        .send()
        .map_err(|error| format!("gateway request failed: {error}"))?;
    let status = response.status();
    let body = read_response_limited(response)?;
    if !status.is_success() {
        return Err(format!("gateway returned HTTP {}", status.as_u16()));
    }
    parse_json_rpc_response(&body, &request.id)
}

fn read_response_limited(response: Response) -> CliResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_GATEWAY_RESPONSE_BYTES as u64)
    {
        return Err("gateway response exceeds maximum size".to_string());
    }
    let mut bytes = Vec::new();
    let mut limited = response.take(MAX_GATEWAY_RESPONSE_BYTES as u64 + 1);
    limited
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read gateway response: {error}"))?;
    if bytes.len() > MAX_GATEWAY_RESPONSE_BYTES {
        return Err("gateway response exceeds maximum size".to_string());
    }
    Ok(bytes)
}

fn parse_json_rpc_response<T: for<'de> Deserialize<'de>>(
    body: &[u8],
    expected_id: &str,
) -> CliResult<T> {
    let envelope: JsonRpcEnvelope<T> =
        serde_json::from_slice(body).map_err(|error| format!("decode gateway JSON: {error}"))?;
    if envelope.jsonrpc != "2.0" {
        return Err("gateway JSON-RPC response has invalid jsonrpc version".to_string());
    }
    if envelope.id != Value::String(expected_id.to_string()) {
        return Err("gateway JSON-RPC response id does not match request id".to_string());
    }
    if let Some(error) = envelope.error {
        let message = error
            .message
            .unwrap_or_else(|| "gateway returned a JSON-RPC error".to_string());
        return Err(format!("swarm_deploy failed: {message}"));
    }
    envelope
        .result
        .ok_or_else(|| "gateway JSON-RPC response missing result".to_string())
}

fn validate_cert_check_result(result: &CertCheckResult, auth: &DeployAuth) -> CliResult<()> {
    if !result.valid {
        return Err("swarm_cert_check did not return valid=true".to_string());
    }
    if result.revoked {
        return Err("swarm_cert_check returned revoked=true".to_string());
    }
    if result.certificate_id != auth.client_cert_id {
        return Err("swarm_cert_check certificate_id does not match auth certificate".to_string());
    }
    if result.player_id != auth.deploy_player_id {
        return Err("swarm_cert_check player_id does not match auth certificate".to_string());
    }
    Ok(())
}

fn validate_deploy_result(result: &DeployResult, requested_hash: &str) -> CliResult<()> {
    if !matches!(result.status.as_str(), "pending_next_tick" | "active") {
        return Err(format!(
            "deploy result status is not successful: {}",
            result.status
        ));
    }
    if result.module_id.trim().is_empty() {
        return Err("deploy result module_id is empty".to_string());
    }
    if result.module_hash != requested_hash {
        return Err("deploy result module_hash does not match requested module hash".to_string());
    }
    Ok(())
}

fn normalize_gateway_url(input: &str, allow_insecure_loopback: bool) -> CliResult<Url> {
    let mut url = Url::parse(input).map_err(|error| format!("invalid gateway-url: {error}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("gateway-url must not contain userinfo".to_string());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("gateway-url must not contain query or fragment".to_string());
    }
    match url.scheme() {
        "https" => {}
        "http" if allow_insecure_loopback && is_loopback_or_localhost(&url) => {}
        "http" if is_loopback_or_localhost(&url) => {
            return Err("http gateway-url requires --allow-insecure-loopback".to_string());
        }
        "http" => {
            return Err("gateway-url must use HTTPS".to_string());
        }
        _ => return Err("gateway-url must use http or https".to_string()),
    }
    match url.path() {
        "" | "/" | "/mcp" => url.set_path("/mcp"),
        _ => return Err("gateway-url path must be empty, /, or /mcp".to_string()),
    }
    Ok(url)
}

fn is_loopback_or_localhost(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|addr| addr.is_loopback())
}

fn canonical_client_request(
    method: &str,
    path: &str,
    body: &Value,
    cert_id: &str,
    player_id: &str,
    tick: &str,
    timestamp: &str,
    nonce: &str,
) -> String {
    let body_hash = blake3::hash(stable_json(body).as_bytes()).to_hex();
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        path,
        timestamp,
        nonce,
        cert_id,
        player_id,
        tick,
        body_hash
    )
}

fn stable_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) | Value::Number(_) | Value::String(_) => value.to_string(),
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(stable_json).collect::<Vec<_>>().join(",")
        ),
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| {
                        format!("{}:{}", Value::String(key.clone()), stable_json(&map[key]))
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

fn append_target_manifest_section(
    wasm: &[u8],
    target_manifest_hash: &str,
    engine_abi_version: u32,
) -> CliResult<Vec<u8>> {
    if wasm.len() < 8 || !wasm.starts_with(b"\0asm") {
        return Err("wasm artifact must be a wasm module".to_string());
    }
    let manifest = serde_json::to_vec(&TargetManifestSection {
        target_manifest_hash,
        engine_abi_version,
    })
    .map_err(|error| error.to_string())?;
    let name = TARGET_MANIFEST_SECTION_NAME.as_bytes();
    let mut payload = Vec::new();
    payload.extend(encode_var_u32(name.len() as u32));
    payload.extend(name);
    payload.extend(manifest);

    let mut output = Vec::with_capacity(wasm.len() + payload.len() + 6);
    output.extend_from_slice(wasm);
    output.push(0);
    output.extend(encode_var_u32(payload.len() as u32));
    output.extend(payload);
    Ok(output)
}

fn reject_existing_target_manifest_section(wasm: &[u8]) -> CliResult<()> {
    let mut count = 0_u32;
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.map_err(|error| format!("parse wasm artifact: {error}"))? {
            Payload::CustomSection(section) if section.name() == TARGET_MANIFEST_SECTION_NAME => {
                count = count.saturating_add(1);
            }
            _ => {}
        }
    }
    match count {
        0 => Ok(()),
        1 => Err(format!(
            "wasm artifact already contains {TARGET_MANIFEST_SECTION_NAME}; refusing to append duplicate"
        )),
        _ => Err(format!(
            "wasm artifact contains duplicate {TARGET_MANIFEST_SECTION_NAME} sections"
        )),
    }
}

fn encode_var_u32(mut value: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        bytes.push(byte);
        if value == 0 {
            break;
        }
    }
    bytes
}

fn deploy_metadata(
    version_tag: &str,
    language: &str,
    target_manifest_hash: &str,
    engine_abi_version: u32,
) -> CliResult<String> {
    toml::to_string(&DeployMetadata {
        name: "bot",
        version: version_tag,
        language,
        target_manifest_hash,
        engine_abi_version,
    })
    .map_err(|error| format!("encode deploy metadata: {error}"))
}

fn code_signing_cert_id(bundle: &Value, base_cert_id: &str) -> CliResult<String> {
    if let Some(code_cert) = bundle.get("code_signing_cert").and_then(Value::as_str) {
        let value: Value = serde_json::from_str(code_cert).map_err(|_| {
            "certificate_bundle includes an invalid code-signing certificate".to_string()
        })?;
        if let Some(cert_id) = bundle_string_field(
            &value,
            &[&["payload", "cert_id"][..], &["payload", "certificate_id"]],
        ) {
            return Ok(cert_id);
        }
    }
    Ok(format!("{base_cert_id}:code"))
}

fn bundle_string_field(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| {
        let mut current = value;
        for segment in *path {
            current = current.get(*segment)?;
        }
        match current {
            Value::String(value) => Some(value.trim().to_string()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        }
        .filter(|value| !value.is_empty())
    })
}

fn parse_player_id_for_deploy(value: &str) -> CliResult<u32> {
    value
        .strip_prefix("player-")
        .unwrap_or(value)
        .parse::<u32>()
        .map_err(|_| "certificate_bundle player id must resolve to a u32".to_string())
}

fn resolve_required_string(
    flag: Option<String>,
    auth_file: Option<String>,
    env_name: &str,
    label: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> CliResult<String> {
    resolve_optional_string(flag, auth_file, env_name, env)
        .ok_or_else(|| format!("missing required deploy option {label}"))
}

fn resolve_optional_string(
    flag: Option<String>,
    auth_file: Option<String>,
    env_name: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    flag.or(auth_file)
        .or_else(|| env(env_name))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn value_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn parse_u32(value: &str, label: &str) -> CliResult<u32> {
    value
        .parse::<u32>()
        .map_err(|_| format!("{label} must be an unsigned 32-bit integer"))
}

fn parse_u64(value: &str, label: &str) -> CliResult<u64> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{label} must be an unsigned 64-bit integer"))
}

fn default_language_for_input(input: &Path) -> &'static str {
    if input.is_dir() { "typescript" } else { "wasm" }
}

fn random_hex_nonce() -> CliResult<String> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| format!("generate request nonce: {error}"))?;
    Ok(hex_encode(&nonce))
}

fn unix_now() -> CliResult<Duration> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())
}

fn iso8601_utc(duration: Duration) -> String {
    let total_seconds = duration.as_secs() as i64;
    let millis = duration.subsec_millis();
    let days = total_seconds.div_euclid(86_400);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let day = doy - (153 * mp + 2).div_euclid(5) + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

fn blake3_label(bytes: &[u8; 32]) -> String {
    format!("blake3:{}", blake3::Hash::from(*bytes).to_hex())
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

fn hex_decode(input: &str) -> CliResult<Vec<u8>> {
    if !input.len().is_multiple_of(2) {
        return Err("hex value must contain an even number of digits".to_string());
    }
    let mut bytes = Vec::with_capacity(input.len() / 2);
    for pair in input.as_bytes().chunks(2) {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn hex_digit(byte: u8) -> CliResult<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("hex value contains a non-hex digit".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, net::TcpListener, thread};

    use ed25519_dalek::{Verifier, VerifyingKey};
    use serde_json::json;

    fn minimal_wasm() -> Vec<u8> {
        b"\0asm\x01\0\0\0".to_vec()
    }

    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7_u8; 32])
    }

    fn auth_file_json(signing_key: &SigningKey) -> String {
        let public_key = encode_base64(signing_key.verifying_key().as_bytes());
        let client_cert = json!({
            "payload": {
                "cert_id": "cert-base",
                "player_id": 7,
                "public_key": public_key,
                "scope": "deploy transport:mcp"
            }
        });
        let code_cert = json!({
            "payload": {
                "cert_id": "cert-base:code",
                "player_id": 7,
                "public_key": public_key,
                "scope": "deploy transport:mcp"
            }
        });
        json!({
            "version": 1,
            "private_key_hex": hex_encode(&[7_u8; 32]),
            "certificate_bundle": {
                "cert_id": "cert-base",
                "player_id": "player-7",
                "client_auth_cert": client_cert.to_string(),
                "code_signing_cert": code_cert.to_string()
            },
            "gateway_url": "http://127.0.0.1:9",
            "world_id": "auth-world",
            "room_id": 1,
            "drone_id": 2,
            "target_manifest_hash": "blake3:auth",
            "engine_abi_version": 1,
            "language": "auth-lang",
            "version_counter": 10
        })
        .to_string()
    }

    fn write_private_auth_file(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn parser_and_source_precedence_use_flags_then_auth_then_env() {
        let raw = DeployCliArgs::parse(&[
            "bot.wasm".into(),
            "--gateway-url".into(),
            "http://localhost:8080".into(),
            "--allow-insecure-loopback".into(),
            "--world-id".into(),
            "flag-world".into(),
            "--room-id".into(),
            "9".into(),
            "--drone-id".into(),
            "11".into(),
            "--target-manifest-hash".into(),
            "blake3:flag".into(),
        ])
        .unwrap();
        let auth_file: DeployAuthFile = serde_json::from_str(
            &json!({
                "private_key_hex": hex_encode(&[7_u8; 32]),
                "certificate_bundle": {"cert_id": "cert-base", "player_id": 7},
                "gateway_url": "http://127.0.0.1:1",
                "world_id": "auth-world",
                "room_id": 2,
                "drone_id": 3,
                "target_manifest_hash": "blake3:auth",
                "language": "auth-lang"
            })
            .to_string(),
        )
        .unwrap();
        let env = |name: &str| match name {
            "SWARM_ENGINE_ABI_VERSION" => Some("5".to_string()),
            "SWARM_VERSION_COUNTER" => Some("55".to_string()),
            _ => None,
        };

        let resolved = ResolvedDeployOptions::from_sources(raw, &auth_file, &env, 99).unwrap();

        assert_eq!(resolved.gateway_url.as_str(), "http://localhost:8080/mcp");
        assert_eq!(resolved.world_id, "flag-world");
        assert_eq!(resolved.room_id, 9);
        assert_eq!(resolved.drone_id, 11);
        assert_eq!(resolved.target_manifest_hash, "blake3:flag");
        assert_eq!(resolved.language, "auth-lang");
        assert_eq!(resolved.engine_abi_version, 5);
        assert_eq!(resolved.version_counter, 55);
        assert_eq!(resolved.version_tag, "cli-55");
        assert!(!resolved.auth_file_artifact_ignored);
    }

    #[test]
    fn auth_aliases_and_code_certificate_derivation_match_frontend() {
        let auth_file: DeployAuthFile = serde_json::from_str(
            &json!({
                "private_key_hex": hex_encode(&[7_u8; 32]),
                "certificate_bundle": {
                    "payload": {"certificate_id": "base-from-payload", "player_id": "player-42"}
                }
            })
            .to_string(),
        )
        .unwrap();
        let auth = DeployAuth::from_auth_file(&auth_file).unwrap();
        assert_eq!(auth.client_cert_id, "base-from-payload");
        assert_eq!(auth.player_id_header, "player-42");
        assert_eq!(auth.deploy_player_id, 42);
        assert_eq!(auth.code_signing_cert_id, "base-from-payload:code");

        let auth_file: DeployAuthFile = serde_json::from_str(&json!({
            "private_key_hex": hex_encode(&[7_u8; 32]),
            "certificate_bundle": {
                "cert_id": "base",
                "player_id": 7,
                "code_signing_cert": json!({"payload": {"certificate_id": "code-from-json"}}).to_string()
            }
        }).to_string()).unwrap();
        let auth = DeployAuth::from_auth_file(&auth_file).unwrap();
        assert_eq!(auth.code_signing_cert_id, "code-from-json");
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_permissions_are_strict_before_reading() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let auth_path = directory.path().join("auth.json");
        fs::write(&auth_path, auth_file_json(&test_key())).unwrap();
        fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = read_deploy_auth_file(&auth_path).unwrap_err();
        assert!(error.contains("group/world"));

        fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600)).unwrap();
        read_deploy_auth_file(&auth_path).unwrap();

        let link_path = directory.path().join("auth-link.json");
        std::os::unix::fs::symlink(&auth_path, &link_path).unwrap();
        let error = read_deploy_auth_file(&link_path).unwrap_err();
        assert!(error.contains("symlink"));
    }

    #[test]
    fn project_artifact_selection_rejects_traversal_symlinks_and_ambiguity() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(project, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir(project.join("build")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(project.join("build"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(project.join("build/a.wasm"), minimal_wasm()).unwrap();
        assert!(select_project_artifact(project, Some(Path::new("../a.wasm"))).is_err());
        assert!(select_project_artifact(project, Some(Path::new("/tmp/a.wasm"))).is_err());
        assert_eq!(
            select_project_artifact(project, Some(Path::new("build/a.wasm"))).unwrap(),
            project.join("build/a.wasm")
        );

        fs::write(project.join("build/b.wasm"), minimal_wasm()).unwrap();
        let error = select_project_artifact(project, None).unwrap_err();
        assert!(error.contains("multiple"));

        #[cfg(unix)]
        {
            fs::remove_file(project.join("build/b.wasm")).unwrap();
            fs::remove_file(project.join("build/a.wasm")).unwrap();
            fs::write(project.join("target.wasm"), minimal_wasm()).unwrap();
            std::os::unix::fs::symlink(project.join("target.wasm"), project.join("build/a.wasm"))
                .unwrap();
            let error = select_project_artifact(project, None).unwrap_err();
            assert!(error.contains("symlink"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_selection_rejects_symlink_ancestor_escape_and_writable_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        let outside = directory.path().join("outside");
        fs::create_dir(&project).unwrap();
        fs::create_dir(project.join("build")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::set_permissions(&project, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(project.join("build"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(outside.join("evil.wasm"), minimal_wasm()).unwrap();
        std::os::unix::fs::symlink(&outside, project.join("build/link")).unwrap();

        let error = select_project_artifact(
            &project.canonicalize().unwrap(),
            Some(Path::new("build/link/evil.wasm")),
        )
        .unwrap_err();
        assert!(error.contains("symlink"));

        fs::write(project.join("build/safe.wasm"), minimal_wasm()).unwrap();
        fs::set_permissions(project.join("build"), fs::Permissions::from_mode(0o722)).unwrap();
        let error = select_project_artifact(
            &project.canonicalize().unwrap(),
            Some(Path::new("build/safe.wasm")),
        )
        .unwrap_err();
        assert!(error.contains("group/world-writable"));
    }

    #[test]
    fn deploy_build_env_scrubs_swarm_variables() {
        let names = deploy_env_names_to_scrub([
            "PATH".to_string(),
            "SWARM_AUTH_FILE".to_string(),
            "SWARM_CUSTOM_SECRET".to_string(),
        ]);
        assert!(names.contains("SWARM_AUTH_FILE"));
        assert!(names.contains("SWARM_CUSTOM_SECRET"));
        assert!(names.contains("SWARM_GATEWAY_URL"));
        assert!(!names.contains("PATH"));
    }

    #[test]
    fn existing_target_manifest_section_is_rejected_before_append() {
        let wasm = append_target_manifest_section(&minimal_wasm(), "blake3:target", 1).unwrap();
        let error = reject_existing_target_manifest_section(&wasm).unwrap_err();
        assert!(error.contains("already contains"));

        let mut duplicate = wasm.clone();
        let second = append_target_manifest_section(&minimal_wasm(), "blake3:target", 1).unwrap();
        duplicate.extend_from_slice(&second[minimal_wasm().len()..]);
        let error = reject_existing_target_manifest_section(&duplicate).unwrap_err();
        assert!(error.contains("duplicate"));
    }

    #[test]
    fn typed_metadata_escapes_quotes_and_newlines() {
        let metadata = deploy_metadata("v\"1\nnext", "type\"script", "blake3:target", 1).unwrap();
        assert!(metadata.contains("target_manifest_hash"));
        let parsed: toml::Value = toml::from_str(&metadata).unwrap();
        assert_eq!(
            parsed.get("version").unwrap().as_str().unwrap(),
            "v\"1\nnext"
        );
        assert_eq!(
            parsed.get("language").unwrap().as_str().unwrap(),
            "type\"script"
        );
    }

    #[test]
    fn payload_and_gateway_signatures_verify_exactly() {
        let signing_key = test_key();
        let auth_file: DeployAuthFile =
            serde_json::from_str(&auth_file_json(&signing_key)).unwrap();
        let auth = DeployAuth::from_auth_file(&auth_file).unwrap();
        let options = ResolvedDeployOptions {
            gateway_url: normalize_gateway_url("http://127.0.0.1:8080", true).unwrap(),
            world_id: "world".to_string(),
            room_id: 3,
            drone_id: 99,
            target_manifest_hash: "blake3:target".to_string(),
            engine_abi_version: 1,
            language: "typescript".to_string(),
            version_tag: "cli-test".to_string(),
            version_counter: 123,
            auth_file_artifact_ignored: false,
        };
        let wasm = append_target_manifest_section(&minimal_wasm(), "blake3:target", 1).unwrap();
        let request = build_signed_deploy_request(
            &options,
            &auth,
            wasm,
            Duration::from_millis(1_700_000_000_123),
        )
        .unwrap();
        let body_value: Value = serde_json::from_slice(&request.request.body).unwrap();
        let payload: DeployPayload = serde_json::from_value(
            body_value
                .get("params")
                .unwrap()
                .get("deploy_payload")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let code_signature = body_value
            .get("params")
            .unwrap()
            .get("code_signature")
            .unwrap()
            .as_str()
            .unwrap();
        let code_signature =
            crate::mcp::decode_ed25519_signature(code_signature, "code_signature").unwrap();
        signing_key
            .verifying_key()
            .verify(&serde_json::to_vec(&payload).unwrap(), &code_signature)
            .unwrap();

        let headers = header_map(&request.request.headers);
        let canonical = canonical_client_request(
            "POST",
            "/mcp",
            &body_value,
            headers["Swarm-Cert-Id"],
            headers["X-Swarm-Player-Id"],
            "",
            headers["Swarm-Timestamp"],
            headers["Swarm-Nonce"],
        );
        let request_signature = signature_from_hex(headers["Swarm-Signature"]);
        signing_key
            .verifying_key()
            .verify(canonical.as_bytes(), &request_signature)
            .unwrap();
        assert_eq!(payload.domain, DEPLOY_DOMAIN);
        assert_eq!(payload.module_slot, "room:3");
        assert_eq!(headers["X-Swarm-Transport"], "mcp");
    }

    #[test]
    fn gateway_url_policy_normalizes_and_rejects_unsafe_urls() {
        assert_eq!(
            normalize_gateway_url("https://example.test", false)
                .unwrap()
                .as_str(),
            "https://example.test/mcp"
        );
        assert_eq!(
            normalize_gateway_url("http://127.0.0.1:8082/", true)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:8082/mcp"
        );
        assert!(normalize_gateway_url("http://127.0.0.1:8082/", false).is_err());
        assert!(normalize_gateway_url("http://example.test", true).is_err());
        assert!(normalize_gateway_url("https://user@example.test", false).is_err());
        assert!(normalize_gateway_url("https://example.test/mcp?secret=1", false).is_err());
        assert!(normalize_gateway_url("https://example.test/other", false).is_err());
    }

    #[test]
    fn json_rpc_errors_and_hash_mismatches_are_rejected() {
        let error = parse_json_rpc_response::<DeployResult>(
            br#"{"jsonrpc":"2.0","id":"deploy","error":{"code":-32602,"message":"bad deploy"}}"#,
            "deploy",
        )
        .unwrap_err();
        assert!(error.contains("bad deploy"));

        let result: DeployResult = serde_json::from_value(json!({
            "module_id": "mod",
            "status": "pending_next_tick",
            "deployed_at": "now",
            "module_hash": "blake3:other",
            "redb_version_counter": 1,
            "cache_status": "stored"
        }))
        .unwrap();
        let error = validate_deploy_result(&result, "blake3:requested").unwrap_err();
        assert!(error.contains("module_hash"));

        let rejected: DeployResult = serde_json::from_value(json!({
            "module_id": "mod",
            "status": "rejected",
            "deployed_at": "now",
            "module_hash": "blake3:requested",
            "redb_version_counter": 1,
            "cache_status": "stored"
        }))
        .unwrap();
        let error = validate_deploy_result(&rejected, "blake3:requested").unwrap_err();
        assert!(error.contains("not successful"));
    }

    #[test]
    fn local_mock_http_end_to_end_posts_signed_deploy() {
        let signing_key = test_key();
        let directory = tempfile::tempdir().unwrap();
        let wasm_path = directory.path().join("bot.wasm");
        fs::write(&wasm_path, minimal_wasm()).unwrap();
        let auth_path = directory.path().join("auth.json");
        write_private_auth_file(&auth_path, &auth_file_json(&signing_key));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let verifying_key = signing_key.verifying_key();
        let handle = thread::spawn(move || mock_deploy_gateway(listener, verifying_key));

        let mut output = Vec::new();
        run(
            &[
                wasm_path.to_string_lossy().into_owned(),
                "--auth-file".into(),
                auth_path.to_string_lossy().into_owned(),
                "--gateway-url".into(),
                format!("http://{addr}"),
                "--allow-insecure-loopback".into(),
                "--world-id".into(),
                "mock-world".into(),
                "--room-id".into(),
                "4".into(),
                "--drone-id".into(),
                "44".into(),
                "--target-manifest-hash".into(),
                "blake3:mock-target".into(),
                "--engine-abi-version".into(),
                "1".into(),
                "--version-counter".into(),
                "777".into(),
                "--version-tag".into(),
                "mock-version".into(),
            ],
            &mut output,
        )
        .unwrap();

        handle.join().unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("module_id=mock-module"));
        assert!(output.contains("status=pending_next_tick"));
        assert!(output.contains("version_counter=777"));
        assert!(!output.contains("Swarm-Signature"));
    }

    #[test]
    fn oversized_gateway_response_is_bounded() {
        let signing_key = test_key();
        let auth_file: DeployAuthFile =
            serde_json::from_str(&auth_file_json(&signing_key)).unwrap();
        let auth = DeployAuth::from_auth_file(&auth_file).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n",
                MAX_GATEWAY_RESPONSE_BYTES + 1
            )
            .unwrap();
        });
        let options = ResolvedDeployOptions {
            gateway_url: normalize_gateway_url(&format!("http://{addr}"), true).unwrap(),
            world_id: "world".to_string(),
            room_id: 1,
            drone_id: 2,
            target_manifest_hash: "blake3:target".to_string(),
            engine_abi_version: 1,
            language: "wasm".to_string(),
            version_tag: "v".to_string(),
            version_counter: 1,
            auth_file_artifact_ignored: false,
        };
        let wasm = append_target_manifest_section(&minimal_wasm(), "blake3:target", 1).unwrap();
        let request =
            build_signed_deploy_request(&options, &auth, wasm, Duration::from_secs(1)).unwrap();
        let client = http_client().unwrap();
        let error =
            post_signed_json_rpc::<DeployResult>(&client, &options.gateway_url, &request.request)
                .unwrap_err();
        handle.join().unwrap();
        assert!(error.contains("maximum size"));
    }

    fn mock_deploy_gateway(listener: TcpListener, verifying_key: VerifyingKey) {
        let (mut preflight_stream, _) = listener.accept().unwrap();
        let preflight = read_http_request(&mut preflight_stream);
        let preflight_body = verify_signed_http_request(&preflight, &verifying_key);
        assert_eq!(preflight_body.get("method").unwrap(), "swarm_cert_check");
        assert_eq!(
            preflight_body
                .get("params")
                .unwrap()
                .get("certificate_id")
                .unwrap(),
            "cert-base"
        );
        let preflight_nonce = preflight.headers["swarm-nonce"].clone();
        let preflight_id = preflight_body.get("id").cloned().unwrap();
        respond_json(
            &mut preflight_stream,
            json!({
                "jsonrpc": "2.0",
                "id": preflight_id,
                "result": {
                    "valid": true,
                    "certificate_id": "cert-base",
                    "player_id": 7,
                    "client_public_key": encode_base64(verifying_key.as_bytes()),
                    "public_key_fingerprint": blake3::hash(verifying_key.as_bytes()).to_hex().to_string(),
                    "usage": "client_auth",
                    "scope": "deploy transport:mcp",
                    "audience": "swarm-client-auth-v1",
                    "expires_at": 4_102_444_800_u64,
                    "revoked": false
                }
            }),
        );

        let (mut deploy_stream, _) = listener.accept().unwrap();
        let deploy = read_http_request(&mut deploy_stream);
        let deploy_body = verify_signed_http_request(&deploy, &verifying_key);
        assert_eq!(deploy_body.get("method").unwrap(), "swarm_deploy");
        assert_ne!(preflight_nonce, deploy.headers["swarm-nonce"]);
        let params = deploy_body.get("params").unwrap();
        let payload: DeployPayload =
            serde_json::from_value(params.get("deploy_payload").unwrap().clone()).unwrap();
        let code_signature = crate::mcp::decode_ed25519_signature(
            params.get("code_signature").unwrap().as_str().unwrap(),
            "code_signature",
        )
        .unwrap();
        verifying_key
            .verify(&serde_json::to_vec(&payload).unwrap(), &code_signature)
            .unwrap();
        assert_eq!(params.get("certificate_id").unwrap(), "cert-base:code");
        assert_eq!(params.get("version_counter").unwrap(), 777);
        assert_eq!(payload.world_id, "mock-world");

        respond_json(
            &mut deploy_stream,
            json!({
                "jsonrpc": "2.0",
                "id": deploy_body.get("id").cloned().unwrap(),
                "result": {
                    "module_id": "mock-module",
                    "status": "pending_next_tick",
                    "deployed_at": "now",
                    "module_hash": payload.wasm_module_hash,
                    "redb_version_counter": 777,
                    "cache_status": "stored"
                }
            }),
        );
    }

    fn verify_signed_http_request(request: &HttpRequest, verifying_key: &VerifyingKey) -> Value {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/mcp");
        let body_value: Value = serde_json::from_slice(&request.body).unwrap();
        let headers = &request.headers;
        let canonical = canonical_client_request(
            "POST",
            "/mcp",
            &body_value,
            headers["swarm-cert-id"].as_str(),
            headers["x-swarm-player-id"].as_str(),
            "",
            headers["swarm-timestamp"].as_str(),
            headers["swarm-nonce"].as_str(),
        );
        let request_signature = signature_from_hex(&headers["swarm-signature"]);
        verifying_key
            .verify(canonical.as_bytes(), &request_signature)
            .unwrap();
        body_value
    }

    fn respond_json(stream: &mut std::net::TcpStream, value: Value) {
        let body = value.to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    }

    struct HttpRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> HttpRequest {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let header_text = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().unwrap();
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap().to_string();
        let path = request_parts.next().unwrap().to_string();
        let mut headers = HashMap::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
        }
        let content_length = headers
            .get("content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        while bytes.len() - header_end < content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&buffer[..read]);
        }
        let body = bytes[header_end..header_end + content_length].to_vec();
        HttpRequest {
            method,
            path,
            headers,
            body,
        }
    }

    fn header_map<'a>(headers: &'a [(&'static str, String)]) -> HashMap<&'static str, &'a str> {
        headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect()
    }

    fn signature_from_hex(input: &str) -> ed25519_dalek::Signature {
        let bytes: [u8; 64] = hex_decode(input).unwrap().try_into().unwrap();
        ed25519_dalek::Signature::from_bytes(&bytes)
    }
}
