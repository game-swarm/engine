use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
};

use bevy::prelude::*;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use swarm_engine_api::ids::{BodyPart, RoomId};
use swarm_engine_plugin_sdk::buffers::SpecialAttackKind;
use swarm_engine_plugin_sdk::components::Position;

use crate::components::WorldMode;
use crate::world::{PlayerViewMode, WorldConfig};

#[derive(Resource, Debug, Clone, Default)]
pub struct PluginRegistry {
    pub enabled: HashSet<String>,
    pub lock: PluginLock,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginLock {
    #[serde(default)]
    pub plugins: HashMap<String, PluginEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginSource {
    Registry,
    Git,
    LocalBuild,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginTrustClass {
    TrustedLocalBuild,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEntry {
    pub plugin_id: String,
    pub version: String,
    pub enabled: bool,
    pub source: PluginSource,
    #[serde(default)]
    pub package_hash: Option<String>,
    #[serde(default)]
    pub signature_hash: Option<String>,
    #[serde(default)]
    pub package_path: Option<PathBuf>,
    #[serde(default)]
    pub signature_path: Option<PathBuf>,
    #[serde(default)]
    pub trusted_signing_key: Option<String>,
    #[serde(default)]
    pub trust_class: Option<PluginTrustClass>,
}

impl Default for PluginEntry {
    fn default() -> Self {
        Self {
            plugin_id: String::new(),
            version: "0.1.0".to_string(),
            enabled: true,
            source: PluginSource::LocalBuild,
            package_hash: None,
            signature_hash: None,
            package_path: None,
            signature_path: None,
            trusted_signing_key: None,
            trust_class: Some(PluginTrustClass::TrustedLocalBuild),
        }
    }
}

impl PluginEntry {
    pub fn trusted_local_build(plugin_id: &str, enabled: bool) -> Self {
        Self {
            plugin_id: plugin_id.to_string(),
            enabled,
            ..Self::default()
        }
    }

    fn validate_identity(&self, plugin_name: &str) -> Result<(), String> {
        if self.plugin_id != plugin_name {
            return Err(format!(
                "mods.lock plugin '{plugin_name}' has mismatched plugin_id '{}'",
                self.plugin_id
            ));
        }
        if self.version.trim().is_empty() {
            return Err(format!(
                "mods.lock plugin '{plugin_name}' missing required identity version"
            ));
        }
        match self.source {
            PluginSource::LocalBuild => {
                if self.trust_class != Some(PluginTrustClass::TrustedLocalBuild) {
                    return Err(format!(
                        "mods.lock plugin '{plugin_name}' missing required identity trusted local-build class"
                    ));
                }
                if self.package_hash.is_some()
                    || self.signature_hash.is_some()
                    || self.package_path.is_some()
                    || self.signature_path.is_some()
                    || self.trusted_signing_key.is_some()
                {
                    return Err(format!(
                        "mods.lock plugin '{plugin_name}' cannot mix package provenance with trusted local-build class"
                    ));
                }
            }
            PluginSource::Registry | PluginSource::Git => {
                validate_identity_hash(plugin_name, "package_hash", &self.package_hash)?;
                validate_identity_hash(plugin_name, "signature_hash", &self.signature_hash)?;
                validate_identity_path(plugin_name, "package_path", &self.package_path)?;
                validate_identity_path(plugin_name, "signature_path", &self.signature_path)?;
                let trusted_signing_key = self.trusted_signing_key.as_deref().ok_or_else(|| {
                    format!(
                        "mods.lock plugin '{plugin_name}' missing required identity trusted_signing_key"
                    )
                })?;
                if self.trust_class.is_some() {
                    return Err(format!(
                        "mods.lock plugin '{plugin_name}' cannot mix package hashes with trusted local-build class"
                    ));
                }
                parse_content_hash(
                    plugin_name,
                    "package_hash",
                    self.package_hash.as_deref().expect("validated above"),
                )?;
                parse_content_hash(
                    plugin_name,
                    "signature_hash",
                    self.signature_hash.as_deref().expect("validated above"),
                )?;
                parse_trusted_signing_key(plugin_name, trusted_signing_key)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorldModConfigs {
    #[serde(rename = "combat-core")]
    pub combat_core: Option<CombatCoreRuntimeConfig>,
    #[serde(rename = "depot-storage")]
    pub depot_storage: Option<DepotStorageRuntimeConfig>,
    #[serde(rename = "empire-upkeep")]
    pub empire_upkeep: Option<EmpireUpkeepRuntimeConfig>,
    #[serde(rename = "fog-of-war")]
    pub fog_of_war: Option<FogOfWarRuntimeConfig>,
    #[serde(rename = "pve-spawning")]
    pub pve_spawning: Option<PveSpawningRuntimeConfig>,
    #[serde(rename = "resource-decay")]
    pub resource_decay: Option<ResourceDecayRuntimeConfig>,
    #[serde(rename = "special-attacks")]
    pub special_attacks: Option<SpecialAttacksRuntimeConfig>,
    #[serde(rename = "vanilla-boss")]
    pub vanilla_boss: Option<VanillaBossRuntimeConfig>,
}

impl WorldModConfigs {
    fn configured_plugin_ids(&self) -> Vec<&'static str> {
        let mut configured = Vec::new();
        if self.combat_core.is_some() {
            configured.push("combat-core");
        }
        if self.depot_storage.is_some() {
            configured.push("depot-storage");
        }
        if self.empire_upkeep.is_some() {
            configured.push("empire-upkeep");
        }
        if self.fog_of_war.is_some() {
            configured.push("fog-of-war");
        }
        if self.pve_spawning.is_some() {
            configured.push("pve-spawning");
        }
        if self.resource_decay.is_some() {
            configured.push("resource-decay");
        }
        if self.special_attacks.is_some() {
            configured.push("special-attacks");
        }
        if self.vanilla_boss.is_some() {
            configured.push("vanilla-boss");
        }
        configured
    }
}

impl PluginLock {
    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let lock = Self::parse_lock(&contents)?;
                lock.verify_artifacts(path)?;
                Ok(lock)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::vanilla()),
            Err(error) => Err(format!("failed to read {}: {error}", path.display())),
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        let lock = Self::parse_lock(&contents)?;
        lock.verify_artifacts(path)?;
        Ok(lock)
    }

    fn parse_lock(contents: &str) -> Result<Self, String> {
        let raw: toml::Value = toml::from_str(contents)
            .map_err(|error| format!("failed to parse mods.lock: {error}"))?;
        reject_lock_gameplay_config(&raw)?;
        let lock: Self = raw
            .try_into()
            .map_err(|error| format!("failed to parse mods.lock: {error}"))?;
        lock.validate_identity()?;
        Ok(lock)
    }

    pub fn vanilla() -> Self {
        let mut plugins = HashMap::new();
        for name in VANILLA_PLUGIN_NAMES {
            plugins.insert(
                (*name).to_string(),
                PluginEntry::trusted_local_build(name, *name != "resource-decay"),
            );
        }
        Self { plugins }
    }

    pub fn enabled_set(&self) -> HashSet<String> {
        self.plugins
            .iter()
            .filter(|(_, entry)| entry.enabled)
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn enabled_vanilla_plugins_in_dependency_order(&self) -> Result<Vec<&'static str>, String> {
        self.validate_known_plugins()?;
        self.validate_dependencies()?;
        Ok(VANILLA_PLUGIN_NAMES
            .iter()
            .copied()
            .filter(|name| self.enabled(name))
            .collect())
    }

    pub fn validate_enabled_features(&self) -> Result<(), String> {
        for name in self.enabled_vanilla_plugins_in_dependency_order()? {
            if !compiled_feature_enabled(name) {
                return Err(format!(
                    "mods.lock enables '{name}' but the engine binary was not compiled with feature '{}'",
                    feature_name(name)
                ));
            }
        }
        Ok(())
    }

    pub fn runtime_config(&self) -> Result<VanillaRuntimeConfig, String> {
        self.runtime_config_for_world(&WorldModConfigs::default())
    }

    fn enabled(&self, name: &str) -> bool {
        self.plugins
            .get(name)
            .map(|entry| entry.enabled)
            .unwrap_or(false)
    }

    fn validate_identity(&self) -> Result<(), String> {
        self.validate_known_plugins()?;
        for (name, entry) in &self.plugins {
            entry.validate_identity(name)?;
        }
        Ok(())
    }

    fn verify_artifacts(&self, lock_path: &Path) -> Result<(), String> {
        let base_dir = lock_path.parent().unwrap_or_else(|| Path::new("."));
        for (plugin_name, entry) in BTreeMap::from_iter(self.plugins.iter()) {
            if !entry.enabled || entry.source == PluginSource::LocalBuild {
                continue;
            }
            verify_plugin_artifacts(plugin_name, entry, base_dir)?;
        }
        Ok(())
    }

    fn validate_known_plugins(&self) -> Result<(), String> {
        for name in self.plugins.keys() {
            if !VANILLA_PLUGIN_NAMES.contains(&name.as_str()) {
                return Err(format!("mods.lock contains unknown plugin '{name}'"));
            }
        }
        Ok(())
    }

    fn validate_dependencies(&self) -> Result<(), String> {
        for (plugin, dependencies) in VANILLA_PLUGIN_DEPENDENCIES {
            if !self.enabled(plugin) {
                continue;
            }
            for dependency in *dependencies {
                if !self.enabled(dependency) {
                    return Err(format!(
                        "mods.lock enables '{plugin}' but dependency '{dependency}' is disabled"
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn runtime_config_for_world(
        &self,
        mods: &WorldModConfigs,
    ) -> Result<VanillaRuntimeConfig, String> {
        self.validate_identity()?;
        self.validate_dependencies()?;
        for plugin in mods.configured_plugin_ids() {
            if !self.enabled(plugin) {
                return Err(format!(
                    "world.toml config for '{plugin}' requires the plugin to be enabled in mods.lock"
                ));
            }
        }
        Ok(VanillaRuntimeConfig {
            combat_core: self.config_for_enabled("combat-core", mods.combat_core.clone())?,
            depot_storage: self.config_for_enabled("depot-storage", mods.depot_storage.clone())?,
            empire_upkeep: self.config_for_enabled("empire-upkeep", mods.empire_upkeep.clone())?,
            fog_of_war: self.config_for_enabled("fog-of-war", mods.fog_of_war.clone())?,
            pve_spawning: self.config_for_enabled("pve-spawning", mods.pve_spawning.clone())?,
            resource_decay: self
                .config_for_enabled("resource-decay", mods.resource_decay.clone())?,
            special_attacks: self
                .config_for_enabled("special-attacks", mods.special_attacks.clone())?,
            vanilla_boss: self.config_for_enabled("vanilla-boss", mods.vanilla_boss.clone())?,
        })
    }

    fn config_for_enabled<T>(&self, name: &str, config: Option<T>) -> Result<Option<T>, String>
    where
        T: Default + ValidateRuntimeConfig,
    {
        if !self.enabled(name) {
            return Ok(None);
        }
        let config = config.unwrap_or_default();
        config.validate(name)?;
        Ok(Some(config))
    }
}

fn reject_lock_gameplay_config(value: &toml::Value) -> Result<(), String> {
    let Some(plugins) = value.get("plugins").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (plugin, entry) in plugins {
        if entry.get("config").is_some() {
            return Err(format!(
                "mods.lock must not contain gameplay config for plugin '{plugin}'"
            ));
        }
    }
    Ok(())
}

fn validate_identity_hash(plugin: &str, key: &str, value: &Option<String>) -> Result<(), String> {
    let Some(value) = value else {
        return Err(format!(
            "mods.lock plugin '{plugin}' missing required identity {key}"
        ));
    };
    if value.trim().is_empty() {
        return Err(format!(
            "mods.lock plugin '{plugin}' missing required identity {key}"
        ));
    }
    Ok(())
}

fn validate_identity_path(plugin: &str, key: &str, value: &Option<PathBuf>) -> Result<(), String> {
    let Some(value) = value else {
        return Err(format!(
            "mods.lock plugin '{plugin}' missing required identity {key}"
        ));
    };
    if value.as_os_str().is_empty() {
        return Err(format!(
            "mods.lock plugin '{plugin}' missing required identity {key}"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentHashAlgorithm {
    Blake3,
    Sha256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContentHash {
    algorithm: ContentHashAlgorithm,
    digest: [u8; 32],
}

fn parse_content_hash(plugin: &str, field: &str, value: &str) -> Result<ContentHash, String> {
    let (algorithm, digest) = value.split_once(':').ok_or_else(|| {
        format!("mods.lock plugin '{plugin}' {field} must use blake3:<hex> or sha256:<hex>")
    })?;
    let algorithm = match algorithm {
        "blake3" => ContentHashAlgorithm::Blake3,
        "sha256" => ContentHashAlgorithm::Sha256,
        _ => {
            return Err(format!(
                "mods.lock plugin '{plugin}' {field} uses unsupported hash algorithm '{algorithm}'"
            ));
        }
    };
    let digest = decode_hex_array::<32>(digest).map_err(|error| {
        format!("mods.lock plugin '{plugin}' {field} must contain a 32-byte hex digest: {error}")
    })?;
    Ok(ContentHash { algorithm, digest })
}

fn parse_trusted_signing_key(plugin: &str, value: &str) -> Result<VerifyingKey, String> {
    let encoded = value.strip_prefix("ed25519:").ok_or_else(|| {
        format!("mods.lock plugin '{plugin}' trusted_signing_key must use ed25519:<hex>")
    })?;
    let key = decode_hex_array::<32>(encoded).map_err(|error| {
        format!(
            "mods.lock plugin '{plugin}' trusted_signing_key must contain a 32-byte hex key: {error}"
        )
    })?;
    VerifyingKey::from_bytes(&key).map_err(|_| {
        format!("mods.lock plugin '{plugin}' trusted_signing_key is not a valid Ed25519 key")
    })
}

fn decode_hex_array<const N: usize>(value: &str) -> Result<[u8; N], String> {
    if value.len() != N * 2 {
        return Err(format!("expected {} hex characters", N * 2));
    }
    let mut output = [0_u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&value[offset..offset + 2], 16)
            .map_err(|_| format!("invalid hex at byte {index}"))?;
    }
    Ok(output)
}

fn encode_hex(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn resolve_artifact_path(base_dir: &Path, configured_path: &Path) -> PathBuf {
    if configured_path.is_absolute() {
        configured_path.to_path_buf()
    } else {
        base_dir.join(configured_path)
    }
}

fn verify_plugin_artifacts(
    plugin_name: &str,
    entry: &PluginEntry,
    base_dir: &Path,
) -> Result<(), String> {
    let expected_package = parse_content_hash(
        plugin_name,
        "package_hash",
        entry.package_hash.as_deref().expect("identity validated"),
    )?;
    let expected_signature = parse_content_hash(
        plugin_name,
        "signature_hash",
        entry.signature_hash.as_deref().expect("identity validated"),
    )?;
    let package_path = resolve_artifact_path(
        base_dir,
        entry.package_path.as_deref().expect("identity validated"),
    );
    let signature_path = resolve_artifact_path(
        base_dir,
        entry.signature_path.as_deref().expect("identity validated"),
    );

    let actual_package = match entry.source {
        PluginSource::Registry => hash_registry_package(&package_path, expected_package.algorithm)?,
        PluginSource::Git => hash_package_tree(&package_path, expected_package.algorithm)?,
        PluginSource::LocalBuild => unreachable!("local builds do not verify package artifacts"),
    };
    if actual_package != expected_package.digest {
        return Err(format!(
            "mods.lock plugin '{plugin_name}' package hash mismatch: expected {}, got {}",
            entry.package_hash.as_deref().expect("identity validated"),
            format_content_hash(expected_package.algorithm, &actual_package)
        ));
    }

    let signature_bytes = read_regular_file(&signature_path, "detached signature")?;
    let actual_signature_hash = hash_bytes(&signature_bytes, expected_signature.algorithm);
    if actual_signature_hash != expected_signature.digest {
        return Err(format!(
            "mods.lock plugin '{plugin_name}' signature hash mismatch: expected {}, got {}",
            entry.signature_hash.as_deref().expect("identity validated"),
            format_content_hash(expected_signature.algorithm, &actual_signature_hash)
        ));
    }
    let signature_bytes: [u8; 64] = signature_bytes.try_into().map_err(|signature: Vec<u8>| {
        format!(
            "mods.lock plugin '{plugin_name}' detached signature must be exactly 64 bytes, got {}",
            signature.len()
        )
    })?;
    let signature = Signature::from_bytes(&signature_bytes);
    let trusted_key = parse_trusted_signing_key(
        plugin_name,
        entry
            .trusted_signing_key
            .as_deref()
            .expect("identity validated"),
    )?;
    trusted_key
        .verify(&expected_package.digest, &signature)
        .map_err(|_| {
            format!(
                "mods.lock plugin '{plugin_name}' detached signature is not valid for the package hash and trusted signing key"
            )
        })
}

fn format_content_hash(algorithm: ContentHashAlgorithm, digest: &[u8; 32]) -> String {
    let prefix = match algorithm {
        ContentHashAlgorithm::Blake3 => "blake3",
        ContentHashAlgorithm::Sha256 => "sha256",
    };
    format!("{prefix}:{}", encode_hex(digest))
}

fn read_regular_file(path: &Path, description: &str) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        format!(
            "failed to inspect {description} {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "{description} {} must be a regular file",
            path.display()
        ));
    }
    std::fs::read(path)
        .map_err(|error| format!("failed to read {description} {}: {error}", path.display()))
}

fn hash_registry_package(
    package_path: &Path,
    algorithm: ContentHashAlgorithm,
) -> Result<[u8; 32], String> {
    let bytes = read_regular_file(package_path, "registry package")?;
    Ok(hash_bytes(&bytes, algorithm))
}

fn hash_package_tree(
    package_root: &Path,
    algorithm: ContentHashAlgorithm,
) -> Result<[u8; 32], String> {
    let metadata = std::fs::symlink_metadata(package_root).map_err(|error| {
        format!(
            "failed to inspect Git package tree {}: {error}",
            package_root.display()
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "Git package path {} must be a directory",
            package_root.display()
        ));
    }

    let mut files = Vec::new();
    collect_package_tree_files(package_root, package_root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = ContentHasher::new(algorithm);
    hasher.update(b"swarm-package-tree-v1\0");
    hasher.update(&(files.len() as u64).to_le_bytes());
    for (relative_path, absolute_path) in files {
        let path_bytes = relative_path.as_bytes();
        hasher.update(&(path_bytes.len() as u64).to_le_bytes());
        hasher.update(path_bytes);

        let metadata = std::fs::metadata(&absolute_path).map_err(|error| {
            format!(
                "failed to inspect Git package file {}: {error}",
                absolute_path.display()
            )
        })?;
        hasher.update(&metadata.len().to_le_bytes());
        let mut file = std::fs::File::open(&absolute_path).map_err(|error| {
            format!(
                "failed to read Git package file {}: {error}",
                absolute_path.display()
            )
        })?;
        let mut buffer = [0_u8; 16 * 1024];
        let mut bytes_read = 0_u64;
        loop {
            let count = file.read(&mut buffer).map_err(|error| {
                format!(
                    "failed to read Git package file {}: {error}",
                    absolute_path.display()
                )
            })?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            bytes_read = bytes_read.saturating_add(count as u64);
        }
        if bytes_read != metadata.len() {
            return Err(format!(
                "Git package file {} changed while it was being hashed",
                absolute_path.display()
            ));
        }
    }
    Ok(hasher.finalize())
}

fn collect_package_tree_files(
    package_root: &Path,
    directory: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(directory).map_err(|error| {
        format!(
            "failed to read Git package directory {}: {error}",
            directory.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read Git package directory entry in {}: {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        // Checkout metadata and local build output are not package contents.
        if directory == package_root
            && matches!(entry.file_name().to_str(), Some(".git" | "target"))
        {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "failed to inspect Git package path {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Git package tree must not contain symbolic link {}",
                path.display()
            ));
        }
        if metadata.file_type().is_dir() {
            collect_package_tree_files(package_root, &path, files)?;
        } else if metadata.file_type().is_file() {
            let relative = path.strip_prefix(package_root).map_err(|_| {
                format!(
                    "Git package path {} is outside package root {}",
                    path.display(),
                    package_root.display()
                )
            })?;
            let relative = relative.to_str().ok_or_else(|| {
                format!("Git package path {} is not valid UTF-8", relative.display())
            })?;
            files.push((relative.replace('\\', "/"), path));
        } else {
            return Err(format!(
                "Git package tree contains unsupported file type {}",
                path.display()
            ));
        }
    }
    Ok(())
}

enum ContentHasher {
    Blake3(blake3::Hasher),
    Sha256(Sha256),
}

impl ContentHasher {
    fn new(algorithm: ContentHashAlgorithm) -> Self {
        match algorithm {
            ContentHashAlgorithm::Blake3 => Self::Blake3(blake3::Hasher::new()),
            ContentHashAlgorithm::Sha256 => Self::Sha256(Sha256::new()),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Blake3(hasher) => {
                hasher.update(bytes);
            }
            Self::Sha256(hasher) => {
                hasher.update(bytes);
            }
        }
    }

    fn finalize(self) -> [u8; 32] {
        match self {
            Self::Blake3(hasher) => *hasher.finalize().as_bytes(),
            Self::Sha256(hasher) => hasher.finalize().into(),
        }
    }
}

fn hash_bytes(bytes: &[u8], algorithm: ContentHashAlgorithm) -> [u8; 32] {
    let mut hasher = ContentHasher::new(algorithm);
    hasher.update(bytes);
    hasher.finalize()
}

pub const VANILLA_PLUGIN_NAMES: &[&str] = &[
    "combat-core",
    "depot-storage",
    "empire-upkeep",
    "fog-of-war",
    "pve-spawning",
    "resource-decay",
    "special-attacks",
    "vanilla-boss",
];

pub const VANILLA_DEFAULT_ENABLED_PLUGIN_NAMES: &[&str] = &[
    "combat-core",
    "depot-storage",
    "empire-upkeep",
    "fog-of-war",
    "pve-spawning",
    "special-attacks",
    "vanilla-boss",
];

pub const VANILLA_PLUGIN_DEPENDENCIES: &[(&str, &[&str])] = &[
    ("combat-core", &[]),
    ("depot-storage", &[]),
    ("empire-upkeep", &[]),
    ("fog-of-war", &[]),
    ("pve-spawning", &[]),
    ("resource-decay", &[]),
    ("special-attacks", &["combat-core"]),
    ("vanilla-boss", &["pve-spawning", "combat-core"]),
];

pub const CANONICAL_PLUGIN_CONFIG_KEYS: &[(&str, &[&str])] = &[
    (
        "combat-core",
        &[
            "damage_multiplier",
            "repair_hp_per_work_part",
            "repair_energy_per_hp",
        ],
    ),
    (
        "depot-storage",
        &[
            "depot_capacity",
            "depot_hits",
            "repair_range",
            "repair_capacity",
        ],
    ),
    (
        "empire-upkeep",
        &[
            "base_upkeep",
            "room_soft_cap",
            "controller_passive_income",
            "controller_passive_income_rcl_bonus",
            "resource",
            "repair_cap",
            "distance_decay_bp",
            "recycle_refund_base",
            "recycle_refund_min",
            "tutorial_recycle_refund_full_ticks",
        ],
    ),
    ("fog-of-war", &["fog_of_war", "player_view"]),
    (
        "pve-spawning",
        &[
            "spawn_interval",
            "max_npcs_per_room",
            "npc_drone_body",
            "npc_drop_table",
        ],
    ),
    (
        "resource-decay",
        &["decay_rate_ppm", "per_resource_decay_rate_ppm"],
    ),
    (
        "special-attacks",
        &[
            "special_attacks_enabled",
            "enabled",
            "tutorial_enabled",
            "novice_enabled",
            "damage_multiplier",
            "fabricate_allowed_output_structures",
        ],
    ),
    (
        "vanilla-boss",
        &[
            "arena_bosses_enabled",
            "world_bosses_enabled",
            "boss_spawn_interval",
            "boss_templates",
        ],
    ),
];

pub const CANONICAL_PLUGIN_CONFIG_TYPES: &[(&str, &[(&str, &str)])] = &[
    (
        "combat-core",
        &[
            ("damage_multiplier", "fixed_bp"),
            ("repair_hp_per_work_part", "u32"),
            ("repair_energy_per_hp", "u32"),
        ],
    ),
    (
        "depot-storage",
        &[
            ("depot_capacity", "u32"),
            ("depot_hits", "u32"),
            ("repair_range", "u32"),
            ("repair_capacity", "u32"),
        ],
    ),
    (
        "empire-upkeep",
        &[
            ("base_upkeep", "u32"),
            ("room_soft_cap", "u32"),
            ("controller_passive_income", "u32"),
            ("controller_passive_income_rcl_bonus", "u32"),
            ("resource", "string"),
            ("repair_cap", "basis_points"),
            ("distance_decay_bp", "basis_points"),
            ("recycle_refund_base", "basis_points"),
            ("recycle_refund_min", "basis_points"),
            ("tutorial_recycle_refund_full_ticks", "u64"),
        ],
    ),
    (
        "fog-of-war",
        &[("fog_of_war", "bool"), ("player_view", "enum")],
    ),
    (
        "pve-spawning",
        &[
            ("spawn_interval", "u32"),
            ("max_npcs_per_room", "u32"),
            ("npc_drone_body", "array<BodyPart>"),
            ("npc_drop_table", "map<Resource,u32>"),
        ],
    ),
    (
        "resource-decay",
        &[
            ("decay_rate_ppm", "ppm"),
            ("per_resource_decay_rate_ppm", "map<Resource,ppm>"),
        ],
    ),
    (
        "special-attacks",
        &[
            ("special_attacks_enabled", "bool"),
            ("enabled", "array<SpecialAttack>"),
            ("tutorial_enabled", "array<SpecialAttack>"),
            ("novice_enabled", "array<SpecialAttack>"),
            ("damage_multiplier", "fixed_bp"),
            (
                "fabricate_allowed_output_structures",
                "array<StructureType>",
            ),
        ],
    ),
    (
        "vanilla-boss",
        &[
            ("arena_bosses_enabled", "bool"),
            ("world_bosses_enabled", "bool"),
            ("boss_spawn_interval", "u64"),
            ("boss_templates", "array<BossTemplate>"),
        ],
    ),
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VanillaRuntimeConfig {
    pub combat_core: Option<CombatCoreRuntimeConfig>,
    pub depot_storage: Option<DepotStorageRuntimeConfig>,
    pub empire_upkeep: Option<EmpireUpkeepRuntimeConfig>,
    pub fog_of_war: Option<FogOfWarRuntimeConfig>,
    pub pve_spawning: Option<PveSpawningRuntimeConfig>,
    pub resource_decay: Option<ResourceDecayRuntimeConfig>,
    pub special_attacks: Option<SpecialAttacksRuntimeConfig>,
    pub vanilla_boss: Option<VanillaBossRuntimeConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginIdentitySnapshot {
    pub plugin_id: String,
    pub version: String,
    pub source: PluginSource,
    pub package_hash: Option<String>,
    pub signature_hash: Option<String>,
    pub trust_class: Option<PluginTrustClass>,
    pub design_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolvedPluginConfigValue {
    Bool(bool),
    Integer(i64),
    String(String),
    Array(Vec<ResolvedPluginConfigValue>),
    Table(BTreeMap<String, ResolvedPluginConfigValue>),
}

pub type ResolvedPluginConfigSnapshot =
    BTreeMap<String, BTreeMap<String, ResolvedPluginConfigValue>>;

impl PluginLock {
    pub fn enabled_identity_snapshot(
        &self,
        design_profiles: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, PluginIdentitySnapshot>, String> {
        self.validate_identity()?;
        let mut snapshot = BTreeMap::new();
        for (name, entry) in BTreeMap::from_iter(self.plugins.iter()) {
            if !entry.enabled {
                continue;
            }
            let design_profile = design_profiles.get(name.as_str()).cloned().ok_or_else(|| {
                format!("enabled plugin '{name}' missing required source descriptor design profile")
            })?;
            snapshot.insert(
                name.clone(),
                PluginIdentitySnapshot {
                    plugin_id: entry.plugin_id.clone(),
                    version: entry.version.clone(),
                    source: entry.source.clone(),
                    package_hash: entry.package_hash.clone(),
                    signature_hash: entry.signature_hash.clone(),
                    trust_class: entry.trust_class.clone(),
                    design_profile,
                },
            );
        }
        Ok(snapshot)
    }
}

impl VanillaRuntimeConfig {
    pub fn resolved_snapshot(&self) -> Result<ResolvedPluginConfigSnapshot, String> {
        let mut snapshot = BTreeMap::new();
        insert_config_snapshot(&mut snapshot, "combat-core", &self.combat_core)?;
        insert_config_snapshot(&mut snapshot, "depot-storage", &self.depot_storage)?;
        insert_config_snapshot(&mut snapshot, "empire-upkeep", &self.empire_upkeep)?;
        insert_config_snapshot(&mut snapshot, "fog-of-war", &self.fog_of_war)?;
        insert_config_snapshot(&mut snapshot, "pve-spawning", &self.pve_spawning)?;
        insert_config_snapshot(&mut snapshot, "resource-decay", &self.resource_decay)?;
        insert_config_snapshot(&mut snapshot, "special-attacks", &self.special_attacks)?;
        insert_config_snapshot(&mut snapshot, "vanilla-boss", &self.vanilla_boss)?;
        Ok(snapshot)
    }
}

fn insert_config_snapshot<T: Serialize>(
    snapshot: &mut ResolvedPluginConfigSnapshot,
    plugin: &str,
    config: &Option<T>,
) -> Result<(), String> {
    let Some(config) = config else {
        return Ok(());
    };
    let value = toml::Value::try_from(config)
        .map_err(|error| format!("failed to snapshot resolved config for '{plugin}': {error}"))?;
    let table = value.as_table().ok_or_else(|| {
        format!("failed to snapshot resolved config for '{plugin}': expected table")
    })?;
    let mut params = BTreeMap::new();
    for (key, value) in table {
        params.insert(key.clone(), resolved_value_from_toml(plugin, key, value)?);
    }
    snapshot.insert(plugin.to_string(), params);
    Ok(())
}

fn resolved_value_from_toml(
    plugin: &str,
    key: &str,
    value: &toml::Value,
) -> Result<ResolvedPluginConfigValue, String> {
    Ok(match value {
        toml::Value::String(value) => ResolvedPluginConfigValue::String(value.clone()),
        toml::Value::Integer(value) => ResolvedPluginConfigValue::Integer(*value),
        toml::Value::Boolean(value) => ResolvedPluginConfigValue::Bool(*value),
        toml::Value::Array(values) => ResolvedPluginConfigValue::Array(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    resolved_value_from_toml(plugin, &format!("{key}[{index}]"), value)
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        toml::Value::Table(values) => {
            let mut table = BTreeMap::new();
            for (nested_key, value) in values {
                table.insert(
                    nested_key.clone(),
                    resolved_value_from_toml(plugin, &format!("{key}.{nested_key}"), value)?,
                );
            }
            ResolvedPluginConfigValue::Table(table)
        }
        toml::Value::Float(_) | toml::Value::Datetime(_) => {
            return Err(format!(
                "resolved config for '{plugin}.{key}' uses unsupported non-deterministic value type"
            ));
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CombatCoreRuntimeConfig {
    pub damage_multiplier: u32,
    pub repair_hp_per_work_part: u32,
    pub repair_energy_per_hp: u32,
}

impl Default for CombatCoreRuntimeConfig {
    fn default() -> Self {
        Self {
            damage_multiplier: 10_000,
            repair_hp_per_work_part: 5,
            repair_energy_per_hp: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DepotStorageRuntimeConfig {
    pub depot_capacity: u32,
    pub depot_hits: u32,
    pub repair_range: u32,
    pub repair_capacity: u32,
}

impl Default for DepotStorageRuntimeConfig {
    fn default() -> Self {
        Self {
            depot_capacity: 10_000,
            depot_hits: 5_000,
            repair_range: 1,
            repair_capacity: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmpireUpkeepRuntimeConfig {
    pub base_upkeep: u32,
    pub room_soft_cap: u32,
    pub controller_passive_income: u32,
    pub controller_passive_income_rcl_bonus: u32,
    pub resource: String,
    pub repair_cap: u32,
    pub distance_decay_bp: u32,
    pub recycle_refund_base: u32,
    pub recycle_refund_min: u32,
    pub tutorial_recycle_refund_full_ticks: u64,
}

impl Default for EmpireUpkeepRuntimeConfig {
    fn default() -> Self {
        let defaults = crate::world::EmpireUpkeepConfig::default();
        Self {
            base_upkeep: defaults.base_upkeep,
            room_soft_cap: defaults.room_soft_cap,
            controller_passive_income: defaults.controller_passive_income,
            controller_passive_income_rcl_bonus: defaults.controller_passive_income_rcl_bonus,
            resource: defaults.resource,
            repair_cap: defaults.repair_cap,
            distance_decay_bp: defaults.distance_decay_bp,
            recycle_refund_base: defaults.recycle_refund_base,
            recycle_refund_min: defaults.recycle_refund_min,
            tutorial_recycle_refund_full_ticks: defaults.tutorial_recycle_refund_full_ticks,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FogOfWarRuntimeConfig {
    pub fog_of_war: bool,
    pub player_view: PlayerViewMode,
}

impl Default for FogOfWarRuntimeConfig {
    fn default() -> Self {
        let defaults = crate::world::VisibilityConfig::default();
        Self {
            fog_of_war: defaults.fog_of_war,
            player_view: defaults.player_view,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PveSpawningRuntimeConfig {
    pub spawn_interval: u32,
    pub max_npcs_per_room: u32,
    pub npc_drone_body: Vec<BodyPart>,
    pub npc_drop_table: BTreeMap<String, u32>,
}

impl Default for PveSpawningRuntimeConfig {
    fn default() -> Self {
        Self {
            spawn_interval: 300,
            max_npcs_per_room: 50,
            npc_drone_body: vec![BodyPart::Attack, BodyPart::Move, BodyPart::Move],
            npc_drop_table: BTreeMap::from([("Energy".to_string(), 50)]),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourceDecayRuntimeConfig {
    pub decay_rate_ppm: u32,
    pub per_resource_decay_rate_ppm: BTreeMap<String, u32>,
}

impl Default for ResourceDecayRuntimeConfig {
    fn default() -> Self {
        Self {
            decay_rate_ppm: 1_000,
            per_resource_decay_rate_ppm: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SpecialAttackName {
    Hack,
    Drain,
    Overload,
    Debilitate,
    Disrupt,
    Fortify,
    Leech,
    Fabricate,
}

impl SpecialAttackName {
    pub fn runtime_kind(self) -> SpecialAttackKind {
        match self {
            Self::Hack => SpecialAttackKind::Hack,
            Self::Drain => SpecialAttackKind::Drain,
            Self::Overload => SpecialAttackKind::Overload,
            Self::Debilitate => SpecialAttackKind::Debilitate,
            Self::Disrupt => SpecialAttackKind::Disrupt,
            Self::Fortify => SpecialAttackKind::Fortify,
            Self::Leech => SpecialAttackKind::Leech,
            Self::Fabricate => SpecialAttackKind::Fabricate,
        }
    }

    pub fn action_name(self) -> &'static str {
        match self {
            Self::Hack => "Hack",
            Self::Drain => "Drain",
            Self::Overload => "Overload",
            Self::Debilitate => "Debilitate",
            Self::Disrupt => "Disrupt",
            Self::Fortify => "Fortify",
            Self::Leech => "Leech",
            Self::Fabricate => "Fabricate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpecialAttacksRuntimeConfig {
    pub special_attacks_enabled: bool,
    pub enabled: BTreeSet<SpecialAttackName>,
    pub tutorial_enabled: BTreeSet<SpecialAttackName>,
    pub novice_enabled: BTreeSet<SpecialAttackName>,
    pub damage_multiplier: u32,
    pub fabricate_allowed_output_structures: Vec<String>,
}

impl Default for SpecialAttacksRuntimeConfig {
    fn default() -> Self {
        Self {
            special_attacks_enabled: true,
            enabled: all_special_attack_names(),
            tutorial_enabled: [
                SpecialAttackName::Hack,
                SpecialAttackName::Drain,
                SpecialAttackName::Fortify,
            ]
            .into_iter()
            .collect(),
            novice_enabled: [
                SpecialAttackName::Hack,
                SpecialAttackName::Drain,
                SpecialAttackName::Overload,
                SpecialAttackName::Fortify,
            ]
            .into_iter()
            .collect(),
            damage_multiplier: 10_000,
            fabricate_allowed_output_structures: vec!["Tower".to_string()],
        }
    }
}

impl SpecialAttacksRuntimeConfig {
    pub fn runtime_kinds_for_mode(&self, mode: WorldMode) -> BTreeSet<SpecialAttackKind> {
        if !self.special_attacks_enabled {
            return BTreeSet::new();
        }
        let names = match mode {
            WorldMode::Tutorial => &self.tutorial_enabled,
            WorldMode::Novice => &self.novice_enabled,
            WorldMode::Default | WorldMode::Arena => &self.enabled,
        };
        names.iter().map(|name| name.runtime_kind()).collect()
    }

    fn action_names_for_mode(&self, mode: WorldMode) -> BTreeSet<&'static str> {
        if !self.special_attacks_enabled {
            return BTreeSet::new();
        }
        let names = match mode {
            WorldMode::Tutorial => &self.tutorial_enabled,
            WorldMode::Novice => &self.novice_enabled,
            WorldMode::Default | WorldMode::Arena => &self.enabled,
        };
        names.iter().map(|name| name.action_name()).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BossModeConfig {
    World,
    Arena,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BossTemplateConfig {
    pub name: String,
    pub mode: BossModeConfig,
    pub hits: u32,
    pub phases: Vec<u32>,
    pub drops: BTreeMap<String, u32>,
    pub spawn_position: Position,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VanillaBossRuntimeConfig {
    pub arena_bosses_enabled: bool,
    pub world_bosses_enabled: bool,
    pub boss_spawn_interval: u64,
    pub boss_templates: Vec<BossTemplateConfig>,
}

impl Default for VanillaBossRuntimeConfig {
    fn default() -> Self {
        Self {
            arena_bosses_enabled: true,
            world_bosses_enabled: true,
            boss_spawn_interval: 5_000,
            boss_templates: vec![
                BossTemplateConfig {
                    name: "world-alpha".to_string(),
                    mode: BossModeConfig::World,
                    hits: 100_000,
                    phases: vec![75, 50, 25],
                    drops: BTreeMap::from([
                        ("Energy".to_string(), 5_000),
                        ("Mineral".to_string(), 100),
                    ]),
                    spawn_position: Position {
                        x: 25,
                        y: 25,
                        room: RoomId(0),
                    },
                },
                BossTemplateConfig {
                    name: "arena-champion".to_string(),
                    mode: BossModeConfig::Arena,
                    hits: 50_000,
                    phases: vec![50, 20],
                    drops: BTreeMap::from([("ArenaToken".to_string(), 1)]),
                    spawn_position: Position {
                        x: 25,
                        y: 25,
                        room: RoomId(1),
                    },
                },
            ],
        }
    }
}

pub trait ValidateRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String>;
}

impl ValidateRuntimeConfig for CombatCoreRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_fixed_bp(plugin, "damage_multiplier", self.damage_multiplier)?;
        validate_positive(
            plugin,
            "repair_hp_per_work_part",
            self.repair_hp_per_work_part,
        )?;
        validate_positive(plugin, "repair_energy_per_hp", self.repair_energy_per_hp)
    }
}

impl ValidateRuntimeConfig for DepotStorageRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_positive(plugin, "depot_capacity", self.depot_capacity)?;
        validate_positive(plugin, "depot_hits", self.depot_hits)?;
        validate_positive(plugin, "repair_capacity", self.repair_capacity)?;
        validate_positive(plugin, "repair_range", self.repair_range)
    }
}

impl ValidateRuntimeConfig for EmpireUpkeepRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_positive(plugin, "room_soft_cap", self.room_soft_cap)?;
        validate_bp(plugin, "repair_cap", self.repair_cap)?;
        validate_bp(plugin, "distance_decay_bp", self.distance_decay_bp)?;
        validate_bp(plugin, "recycle_refund_base", self.recycle_refund_base)?;
        validate_bp(plugin, "recycle_refund_min", self.recycle_refund_min)?;
        if self.recycle_refund_min > self.recycle_refund_base {
            return Err(format!(
                "{plugin}.recycle_refund_min must be <= recycle_refund_base"
            ));
        }
        if self.resource.trim().is_empty() {
            return Err(format!("{plugin}.resource must not be empty"));
        }
        Ok(())
    }
}

impl ValidateRuntimeConfig for FogOfWarRuntimeConfig {
    fn validate(&self, _plugin: &str) -> Result<(), String> {
        Ok(())
    }
}

impl ValidateRuntimeConfig for PveSpawningRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_positive(plugin, "spawn_interval", self.spawn_interval)?;
        validate_positive(plugin, "max_npcs_per_room", self.max_npcs_per_room)?;
        if self.npc_drone_body.is_empty() {
            return Err(format!("{plugin}.npc_drone_body must not be empty"));
        }
        Ok(())
    }
}

impl ValidateRuntimeConfig for ResourceDecayRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_ppm(plugin, "decay_rate_ppm", self.decay_rate_ppm)?;
        for (resource, ppm) in &self.per_resource_decay_rate_ppm {
            if resource.trim().is_empty() {
                return Err(format!(
                    "{plugin}.per_resource_decay_rate_ppm contains an empty resource name"
                ));
            }
            validate_ppm(plugin, resource, *ppm)?;
        }
        Ok(())
    }
}

impl ValidateRuntimeConfig for SpecialAttacksRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_fixed_bp(plugin, "damage_multiplier", self.damage_multiplier)?;
        if self.fabricate_allowed_output_structures.is_empty() {
            return Err(format!(
                "{plugin}.fabricate_allowed_output_structures must not be empty"
            ));
        }
        for structure in &self.fabricate_allowed_output_structures {
            if !matches!(structure.as_str(), "Tower" | "Storage" | "Wall") {
                return Err(format!(
                    "{plugin}.fabricate_allowed_output_structures contains unsupported structure '{structure}'"
                ));
            }
        }
        Ok(())
    }
}

impl ValidateRuntimeConfig for VanillaBossRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_positive_u64(plugin, "boss_spawn_interval", self.boss_spawn_interval)?;
        if self.boss_templates.is_empty() {
            return Err(format!("{plugin}.boss_templates must not be empty"));
        }
        for template in &self.boss_templates {
            validate_positive(plugin, "boss_templates[].hits", template.hits)?;
            if template.name.trim().is_empty() {
                return Err(format!("{plugin}.boss_templates[].name must not be empty"));
            }
            if template.phases.is_empty() {
                return Err(format!(
                    "{plugin}.boss_templates[].phases must not be empty"
                ));
            }
        }
        Ok(())
    }
}

pub fn apply_lock_to_world_config(
    lock: &PluginLock,
    config: &mut WorldConfig,
    mode: WorldMode,
) -> Result<VanillaRuntimeConfig, String> {
    let runtime = lock.runtime_config_for_world(&config.mods)?;
    if let Some(combat) = &runtime.combat_core {
        if !config.explicit_fields.contains("combat.damage_multiplier") {
            config.combat.damage_multiplier = combat.damage_multiplier;
        }
    }
    if let Some(upkeep) = &runtime.empire_upkeep {
        apply_empire_upkeep_to_world_config(upkeep, config);
    }
    if let Some(fog) = &runtime.fog_of_war {
        if !config.explicit_fields.contains("visibility.fog_of_war") {
            config.visibility.fog_of_war = fog.fog_of_war;
        }
        if !config.explicit_fields.contains("visibility.player_view") {
            config.visibility.player_view = fog.player_view;
        }
    }
    if let Some(special) = &runtime.special_attacks {
        let allowed = special.action_names_for_mode(mode);
        config.custom_actions.retain(|action| {
            special_action_name(action.name.as_str()).is_none_or(|name| allowed.contains(name))
        });
    }
    Ok(runtime)
}

pub fn install_plugin_registry(app: &mut App, lock: PluginLock) {
    app.insert_resource(PluginRegistry {
        enabled: lock.enabled_set(),
        lock,
    });
}

pub fn register_mods(app: &mut App, lock: &PluginLock) {
    install_plugin_registry(app, lock.clone());
}

pub fn load_default_plugin_lock() -> Result<PluginLock, String> {
    PluginLock::load_or_default("mods.lock")
}

fn apply_empire_upkeep_to_world_config(
    upkeep: &EmpireUpkeepRuntimeConfig,
    config: &mut WorldConfig,
) {
    let explicit = &config.explicit_fields;
    if !explicit.contains("empire_upkeep.base_upkeep") {
        config.empire_upkeep.base_upkeep = upkeep.base_upkeep;
    }
    if !explicit.contains("empire_upkeep.room_soft_cap") {
        config.empire_upkeep.room_soft_cap = upkeep.room_soft_cap;
    }
    if !explicit.contains("empire_upkeep.controller_passive_income") {
        config.empire_upkeep.controller_passive_income = upkeep.controller_passive_income;
    }
    if !explicit.contains("empire_upkeep.controller_passive_income_rcl_bonus") {
        config.empire_upkeep.controller_passive_income_rcl_bonus =
            upkeep.controller_passive_income_rcl_bonus;
    }
    if !explicit.contains("empire_upkeep.resource") {
        config.empire_upkeep.resource = upkeep.resource.clone();
    }
    if !explicit.contains("empire_upkeep.repair_cap") {
        config.empire_upkeep.repair_cap = upkeep.repair_cap;
    }
    if !explicit.contains("empire_upkeep.distance_decay_bp") {
        config.empire_upkeep.distance_decay_bp = upkeep.distance_decay_bp;
    }
    if !explicit.contains("empire_upkeep.recycle_refund_base") {
        config.empire_upkeep.recycle_refund_base = upkeep.recycle_refund_base;
    }
    if !explicit.contains("empire_upkeep.recycle_refund_min") {
        config.empire_upkeep.recycle_refund_min = upkeep.recycle_refund_min;
    }
    if !explicit.contains("empire_upkeep.tutorial_recycle_refund_full_ticks") {
        config.empire_upkeep.tutorial_recycle_refund_full_ticks =
            upkeep.tutorial_recycle_refund_full_ticks;
    }
}

fn validate_positive(plugin: &str, key: &str, value: u32) -> Result<(), String> {
    if value == 0 {
        return Err(format!("{plugin}.{key} must be greater than zero"));
    }
    Ok(())
}

fn validate_positive_u64(plugin: &str, key: &str, value: u64) -> Result<(), String> {
    if value == 0 {
        return Err(format!("{plugin}.{key} must be greater than zero"));
    }
    Ok(())
}

fn validate_bp(plugin: &str, key: &str, value: u32) -> Result<(), String> {
    if value > 10_000 {
        return Err(format!("{plugin}.{key} must be <= 10000 basis points"));
    }
    Ok(())
}

fn validate_fixed_bp(plugin: &str, key: &str, value: u32) -> Result<(), String> {
    validate_positive(plugin, key, value)?;
    if value > 1_000_000 {
        return Err(format!(
            "{plugin}.{key} must be <= 1000000 fixed basis points"
        ));
    }
    Ok(())
}

fn validate_ppm(plugin: &str, key: &str, value: u32) -> Result<(), String> {
    if value > 1_000_000 {
        return Err(format!("{plugin}.{key} must be <= 1000000 ppm"));
    }
    Ok(())
}

fn all_special_attack_names() -> BTreeSet<SpecialAttackName> {
    [
        SpecialAttackName::Hack,
        SpecialAttackName::Drain,
        SpecialAttackName::Overload,
        SpecialAttackName::Debilitate,
        SpecialAttackName::Disrupt,
        SpecialAttackName::Fortify,
        SpecialAttackName::Leech,
        SpecialAttackName::Fabricate,
    ]
    .into_iter()
    .collect()
}

fn special_action_name(name: &str) -> Option<&'static str> {
    match name {
        "Hack" => Some("Hack"),
        "Drain" => Some("Drain"),
        "Overload" => Some("Overload"),
        "Debilitate" => Some("Debilitate"),
        "Disrupt" => Some("Disrupt"),
        "Fortify" => Some("Fortify"),
        "Leech" => Some("Leech"),
        "Fabricate" => Some("Fabricate"),
        _ => None,
    }
}

fn compiled_feature_enabled(plugin: &str) -> bool {
    match plugin {
        "combat-core" => cfg!(feature = "mod_combat_core"),
        "depot-storage" => cfg!(feature = "mod_depot_storage"),
        "empire-upkeep" => cfg!(feature = "mod_empire_upkeep"),
        "fog-of-war" => cfg!(feature = "mod_fog_of_war"),
        "pve-spawning" => cfg!(feature = "mod_pve_spawning"),
        "resource-decay" => cfg!(feature = "mod_resource_decay"),
        "special-attacks" => cfg!(feature = "mod_special_attacks"),
        "vanilla-boss" => cfg!(feature = "mod_vanilla_boss"),
        _ => false,
    }
}

fn feature_name(plugin: &str) -> &'static str {
    match plugin {
        "combat-core" => "mod_combat_core",
        "depot-storage" => "mod_depot_storage",
        "empire-upkeep" => "mod_empire_upkeep",
        "fog-of-war" => "mod_fog_of_war",
        "pve-spawning" => "mod_pve_spawning",
        "resource-decay" => "mod_resource_decay",
        "special-attacks" => "mod_special_attacks",
        "vanilla-boss" => "mod_vanilla_boss",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde::de::DeserializeOwned;

    fn write_remote_lock(
        directory: &Path,
        source: PluginSource,
        package_path: &str,
        signing_key: &SigningKey,
        trusted_key: &VerifyingKey,
    ) -> PathBuf {
        let source_name = match source {
            PluginSource::Registry => "registry",
            PluginSource::Git => "git",
            PluginSource::LocalBuild => panic!("test helper requires a remote source"),
        };
        let absolute_package_path = directory.join(package_path);
        let package_digest = match source {
            PluginSource::Registry => {
                hash_registry_package(&absolute_package_path, ContentHashAlgorithm::Blake3).unwrap()
            }
            PluginSource::Git => {
                hash_package_tree(&absolute_package_path, ContentHashAlgorithm::Blake3).unwrap()
            }
            PluginSource::LocalBuild => unreachable!(),
        };
        let signature = signing_key.sign(&package_digest).to_bytes();
        std::fs::write(directory.join("package.sig"), signature).unwrap();
        let signature_digest = hash_bytes(&signature, ContentHashAlgorithm::Sha256);
        let lock_path = directory.join("mods.lock");
        std::fs::write(
            &lock_path,
            format!(
                "[plugins.combat-core]\n\
                 plugin_id = \"combat-core\"\n\
                 version = \"0.1.0\"\n\
                 enabled = true\n\
                 source = \"{source_name}\"\n\
                 package_hash = \"{}\"\n\
                 signature_hash = \"{}\"\n\
                 package_path = \"{package_path}\"\n\
                 signature_path = \"package.sig\"\n\
                 trusted_signing_key = \"ed25519:{}\"\n",
                format_content_hash(ContentHashAlgorithm::Blake3, &package_digest),
                format_content_hash(ContentHashAlgorithm::Sha256, &signature_digest),
                encode_hex(&trusted_key.to_bytes()),
            ),
        )
        .unwrap();
        lock_path
    }

    #[test]
    fn plugin_entry_validates_typed_identity() {
        let entry = PluginEntry::trusted_local_build("combat-core", true);

        assert!(entry.validate_identity("combat-core").is_ok());
        assert!(
            entry
                .validate_identity("fog-of-war")
                .unwrap_err()
                .contains("mismatched plugin_id")
        );
    }

    #[test]
    fn registry_package_bytes_and_detached_signature_are_verified() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("package.swarm-mod"), b"package bytes").unwrap();
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let lock_path = write_remote_lock(
            directory.path(),
            PluginSource::Registry,
            "package.swarm-mod",
            &signing_key,
            &signing_key.verifying_key(),
        );

        PluginLock::load(&lock_path).unwrap();

        std::fs::write(
            directory.path().join("package.swarm-mod"),
            b"tampered package bytes",
        )
        .unwrap();
        let error = PluginLock::load(&lock_path).unwrap_err();
        assert!(error.contains("package hash mismatch"), "{error}");
    }

    #[test]
    fn git_package_tree_hash_is_sorted_and_tamper_evident() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir_all(first.join("src")).unwrap();
        std::fs::write(first.join("src/lib.rs"), b"pub fn plugin() {}\n").unwrap();
        std::fs::write(first.join("mod.toml"), b"[meta]\nname = \"combat-core\"\n").unwrap();
        std::fs::create_dir_all(second.join("src")).unwrap();
        std::fs::write(second.join("mod.toml"), b"[meta]\nname = \"combat-core\"\n").unwrap();
        std::fs::write(second.join("src/lib.rs"), b"pub fn plugin() {}\n").unwrap();
        std::fs::create_dir(first.join(".git")).unwrap();
        std::fs::write(first.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(second.join("target")).unwrap();
        std::fs::write(second.join("target/build-output"), b"local output").unwrap();

        assert_eq!(
            hash_package_tree(&first, ContentHashAlgorithm::Blake3).unwrap(),
            hash_package_tree(&second, ContentHashAlgorithm::Blake3).unwrap()
        );

        let signing_key = SigningKey::from_bytes(&[11_u8; 32]);
        let lock_path = write_remote_lock(
            directory.path(),
            PluginSource::Git,
            "first",
            &signing_key,
            &signing_key.verifying_key(),
        );
        PluginLock::load(&lock_path).unwrap();

        std::fs::write(first.join("src/lib.rs"), b"pub fn tampered() {}\n").unwrap();
        let error = PluginLock::load(&lock_path).unwrap_err();
        assert!(error.contains("package hash mismatch"), "{error}");
    }

    #[test]
    fn detached_signature_bytes_and_trusted_key_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("package.swarm-mod"), b"package bytes").unwrap();
        let signing_key = SigningKey::from_bytes(&[17_u8; 32]);
        let untrusted_key = SigningKey::from_bytes(&[19_u8; 32]);
        let lock_path = write_remote_lock(
            directory.path(),
            PluginSource::Registry,
            "package.swarm-mod",
            &signing_key,
            &untrusted_key.verifying_key(),
        );

        let error = PluginLock::load(&lock_path).unwrap_err();
        assert!(error.contains("trusted signing key"), "{error}");

        let lock_path = write_remote_lock(
            directory.path(),
            PluginSource::Registry,
            "package.swarm-mod",
            &signing_key,
            &signing_key.verifying_key(),
        );
        let mut signature = std::fs::read(directory.path().join("package.sig")).unwrap();
        signature[0] ^= 0x80;
        std::fs::write(directory.path().join("package.sig"), signature).unwrap();
        let error = PluginLock::load(&lock_path).unwrap_err();
        assert!(error.contains("signature hash mismatch"), "{error}");
    }

    #[test]
    fn strict_decode_rejects_unknown_obsolete_keys() {
        let error = WorldConfig::from_toml_str(
            "[mods.pve-spawning]\n\
             spawn_rate = 5\n\
             npc_types = \"basic\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn strict_decode_rejects_wrong_types_and_ranges() {
        let wrong_type = WorldConfig::from_toml_str(
            "[mods.combat-core]\n\
             damage_multiplier = \"fast\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(wrong_type.contains("invalid type"), "{wrong_type}");

        let mut lock = PluginLock::vanilla();
        lock.plugins.get_mut("resource-decay").unwrap().enabled = true;
        let mut invalid_range = WorldConfig::from_toml_str(
            "[mods.resource-decay]\n\
             decay_rate_ppm = 1000001\n",
        )
        .unwrap();
        assert!(
            apply_lock_to_world_config(&lock, &mut invalid_range, WorldMode::Default)
                .unwrap_err()
                .contains("<= 1000000 ppm")
        );
    }

    #[test]
    fn dependency_order_is_deterministic_and_validated() {
        let mut lock = PluginLock::vanilla();
        lock.plugins.get_mut("combat-core").unwrap().enabled = false;
        let error = lock
            .enabled_vanilla_plugins_in_dependency_order()
            .unwrap_err();
        assert!(error.contains("special-attacks"));

        let lock = PluginLock::vanilla();
        assert_eq!(
            lock.enabled_vanilla_plugins_in_dependency_order().unwrap(),
            VANILLA_DEFAULT_ENABLED_PLUGIN_NAMES
        );
    }

    #[test]
    fn lock_defaults_apply_but_explicit_world_fields_win() {
        let lock = PluginLock::vanilla();
        let mut config = WorldConfig::from_toml_str(
            "[empire_upkeep]\n\
             base_upkeep = 77\n\
             [mods.empire-upkeep]\n\
             base_upkeep = 99\n\
             room_soft_cap = 3\n",
        )
        .unwrap();

        apply_lock_to_world_config(&lock, &mut config, WorldMode::Default).unwrap();

        assert_eq!(config.empire_upkeep.base_upkeep, 77);
        assert_eq!(config.empire_upkeep.room_soft_cap, 3);
    }

    #[test]
    fn special_attack_allowlists_filter_actions_without_overriding_vanilla_defs() {
        let lock = PluginLock::vanilla();
        let expected = [
            (
                WorldMode::Tutorial,
                ["Attack", "RangedAttack", "Heal", "Hack"],
            ),
            (
                WorldMode::Novice,
                ["Attack", "RangedAttack", "Heal", "Overload"],
            ),
            (
                WorldMode::Default,
                ["Attack", "RangedAttack", "Heal", "Leech"],
            ),
            (
                WorldMode::Arena,
                ["Attack", "RangedAttack", "Heal", "Leech"],
            ),
        ];

        for (mode, expected_names) in expected {
            let mut config = WorldConfig::from_toml_str(
                "[mods.special-attacks]\n\
                 enabled = [\"Leech\"]\n\
                 tutorial_enabled = [\"Hack\"]\n\
                 novice_enabled = [\"Overload\"]\n",
            )
            .unwrap();
            apply_lock_to_world_config(&lock, &mut config, mode).unwrap();
            let names = config
                .custom_actions
                .iter()
                .map(|action| action.name.as_str())
                .collect::<BTreeSet<_>>();

            assert_eq!(
                names,
                expected_names.into_iter().collect::<BTreeSet<_>>(),
                "{mode:?} custom action allowlist differed"
            );
        }
    }

    #[test]
    fn special_attack_runtime_kinds_use_mode_specific_allowlists() {
        let config = SpecialAttacksRuntimeConfig {
            enabled: [SpecialAttackName::Fabricate].into_iter().collect(),
            tutorial_enabled: [SpecialAttackName::Hack].into_iter().collect(),
            novice_enabled: [SpecialAttackName::Overload].into_iter().collect(),
            ..Default::default()
        };

        assert_eq!(
            config.runtime_kinds_for_mode(WorldMode::Tutorial),
            [SpecialAttackKind::Hack].into_iter().collect()
        );
        assert_eq!(
            config.runtime_kinds_for_mode(WorldMode::Novice),
            [SpecialAttackKind::Overload].into_iter().collect()
        );
        assert_eq!(
            config.runtime_kinds_for_mode(WorldMode::Default),
            [SpecialAttackKind::Fabricate].into_iter().collect()
        );
        assert_eq!(
            config.runtime_kinds_for_mode(WorldMode::Arena),
            [SpecialAttackKind::Fabricate].into_iter().collect()
        );
    }

    #[test]
    fn public_spectate_and_custom_actions_are_rejected_by_strict_contracts() {
        let public_spectate = WorldConfig::from_toml_str(
            "[mods.fog-of-war]\n\
             public_spectate = true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(public_spectate.contains("unknown field"));

        let custom_actions = WorldConfig::from_toml_str(
            "[mods.special-attacks]\n\
             custom_actions = []\n",
        )
        .unwrap_err()
        .to_string();
        assert!(custom_actions.contains("unknown field"));
    }

    #[test]
    fn enabled_plugins_require_compiled_features() {
        let lock = PluginLock::vanilla();

        let result = lock.validate_enabled_features();
        if cfg!(all(
            feature = "mod_combat_core",
            feature = "mod_depot_storage",
            feature = "mod_empire_upkeep",
            feature = "mod_fog_of_war",
            feature = "mod_pve_spawning",
            feature = "mod_special_attacks",
            feature = "mod_vanilla_boss"
        )) {
            assert!(result.is_ok());
        } else {
            assert!(
                result
                    .unwrap_err()
                    .contains("was not compiled with feature")
            );
        }
    }

    #[test]
    fn boss_templates_decode_as_typed_runtime_config() {
        let lock = PluginLock::vanilla();
        let config = WorldConfig::from_toml_str(
            "[[mods.vanilla-boss.boss_templates]]\n\
             name = \"omega\"\n\
             mode = \"world\"\n\
             hits = 42\n\
             phases = [75]\n\
             drops = { Energy = 1 }\n\
             spawn_position = { x = 1, y = 2, room = 0 }\n",
        )
        .unwrap();
        let config = lock
            .runtime_config_for_world(&config.mods)
            .unwrap()
            .vanilla_boss
            .unwrap();

        assert_eq!(config.boss_templates[0].name, "omega");
        assert_eq!(config.boss_templates[0].mode, BossModeConfig::World);
    }

    #[test]
    fn manifests_match_canonical_runtime_schema_keys() {
        for (plugin, keys) in CANONICAL_PLUGIN_CONFIG_KEYS {
            let path = plugin_manifest_path(plugin);
            let manifest = std::fs::read_to_string(&path).unwrap();
            let manifest: toml::Value = toml::from_str(&manifest).unwrap();
            let config = manifest
                .get("config")
                .and_then(toml::Value::as_table)
                .unwrap_or_else(|| panic!("missing [config] in {}", path.display()));
            let actual = config.keys().map(String::as_str).collect::<BTreeSet<_>>();
            let expected = keys.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(actual, expected, "{plugin} manifest keys differ");
        }
    }

    #[test]
    fn manifests_match_canonical_runtime_schema_types() {
        for (plugin, types) in CANONICAL_PLUGIN_CONFIG_TYPES {
            let path = plugin_manifest_path(plugin);
            let manifest = std::fs::read_to_string(&path).unwrap();
            let manifest: toml::Value = toml::from_str(&manifest).unwrap();
            let config = manifest
                .get("config")
                .and_then(toml::Value::as_table)
                .unwrap_or_else(|| panic!("missing [config] in {}", path.display()));
            for (key, expected_type) in *types {
                let actual_type = config
                    .get(*key)
                    .and_then(toml::Value::as_table)
                    .and_then(|metadata| metadata.get("type"))
                    .and_then(toml::Value::as_str)
                    .unwrap_or_else(|| panic!("missing type for {plugin}.{key}"));
                assert_eq!(actual_type, *expected_type, "{plugin}.{key} type differs");
            }
        }
    }

    #[test]
    fn manifest_defaults_decode_as_typed_runtime_configs() {
        let lock = lock_with_all_plugins_enabled();
        let mods = WorldModConfigs {
            combat_core: Some(manifest_default_config("combat-core")),
            depot_storage: Some(manifest_default_config("depot-storage")),
            empire_upkeep: Some(manifest_default_config("empire-upkeep")),
            fog_of_war: Some(manifest_default_config("fog-of-war")),
            pve_spawning: Some(manifest_default_config("pve-spawning")),
            resource_decay: Some(manifest_default_config("resource-decay")),
            special_attacks: Some(manifest_default_config("special-attacks")),
            vanilla_boss: Some(manifest_default_config("vanilla-boss")),
        };

        let runtime = lock.runtime_config_for_world(&mods).unwrap();
        assert_eq!(
            runtime.combat_core,
            Some(CombatCoreRuntimeConfig::default())
        );
        assert_eq!(
            runtime.depot_storage,
            Some(DepotStorageRuntimeConfig::default())
        );
        assert_eq!(
            runtime.empire_upkeep,
            Some(EmpireUpkeepRuntimeConfig::default())
        );
        assert_eq!(runtime.fog_of_war, Some(FogOfWarRuntimeConfig::default()));
        assert_eq!(
            runtime.pve_spawning,
            Some(PveSpawningRuntimeConfig::default())
        );
        assert_eq!(
            runtime.resource_decay,
            Some(ResourceDecayRuntimeConfig::default())
        );
        assert_eq!(
            runtime.special_attacks,
            Some(SpecialAttacksRuntimeConfig::default())
        );
        assert_eq!(
            runtime.vanilla_boss,
            Some(VanillaBossRuntimeConfig::default())
        );
    }

    #[test]
    fn load_or_default_only_falls_back_when_lock_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-mods.lock");
        assert_eq!(
            PluginLock::load_or_default(&missing)
                .unwrap()
                .enabled_vanilla_plugins_in_dependency_order()
                .unwrap(),
            VANILLA_DEFAULT_ENABLED_PLUGIN_NAMES
        );

        let malformed = directory.path().join("mods.lock");
        std::fs::write(&malformed, "[plugins.combat-core\n").unwrap();
        assert!(
            PluginLock::load_or_default(&malformed)
                .unwrap_err()
                .contains("failed to parse mods.lock")
        );

        let unsafe_lock = directory.path().join("unsafe-mods.lock");
        std::fs::write(
            &unsafe_lock,
            "[plugins.fog-of-war]\nplugin_id = \"fog-of-war\"\nversion = \"0.1.0\"\nenabled = true\nsource = \"local-build\"\ntrust_class = \"trusted-local-build\"\nconfig = { public_spectate = true }\n",
        )
        .unwrap();
        assert!(
            PluginLock::load_or_default(&unsafe_lock)
                .unwrap_err()
                .contains("mods.lock must not contain gameplay config")
        );
    }

    #[test]
    fn mods_lock_is_identity_only_and_fails_closed_without_provenance() {
        let gameplay_config = PluginLock::parse_lock(
            "[plugins.combat-core]\n\
             version = \"0.1.0\"\n\
             enabled = true\n\
             source = \"registry\"\n\
             package_hash = \"sha256:combat\"\n\
             signature_hash = \"sha256:combat.sig\"\n\
             config = { damage_multiplier = 12500 }\n",
        )
        .unwrap_err();
        assert!(gameplay_config.contains("mods.lock must not contain gameplay config"));

        let missing_identity = PluginLock::parse_lock(
            "[plugins.combat-core]\n\
             plugin_id = \"combat-core\"\n\
             version = \"0.1.0\"\n\
             enabled = true\n\
             source = \"registry\"\n",
        )
        .unwrap_err();
        assert!(missing_identity.contains("missing required identity"));

        let missing_signature = PluginLock::parse_lock(
            "[plugins.combat-core]\n\
             plugin_id = \"combat-core\"\n\
             version = \"0.1.0\"\n\
             enabled = true\n\
             source = \"registry\"\n\
             package_hash = \"sha256:combat\"\n",
        )
        .unwrap_err();
        assert!(missing_signature.contains("missing required identity signature_hash"));

        let missing_trusted_local_class = PluginLock::parse_lock(
            "[plugins.combat-core]\n\
             plugin_id = \"combat-core\"\n\
             version = \"0.1.0\"\n\
             enabled = true\n\
             source = \"local-build\"\n",
        )
        .unwrap_err();
        assert!(missing_trusted_local_class.contains("trusted local-build class"));
    }

    #[test]
    fn world_mod_config_supplies_typed_runtime_values() {
        let lock = PluginLock::vanilla();
        let mut config = WorldConfig::from_toml_str(
            "[mods.combat-core]\n\
             damage_multiplier = 12500\n",
        )
        .unwrap();

        apply_lock_to_world_config(&lock, &mut config, WorldMode::Default).unwrap();

        assert_eq!(config.combat.damage_multiplier, 12_500);
    }

    #[test]
    fn resolved_snapshot_contains_enabled_defaults_after_world_overrides() {
        let lock = PluginLock::vanilla();
        let world_config = WorldConfig::from_toml_str(
            "[mods.combat-core]\n\
             damage_multiplier = 12500\n\
             repair_hp_per_work_part = 7\n",
        )
        .unwrap();

        let runtime = lock.runtime_config_for_world(&world_config.mods).unwrap();
        let snapshot = runtime.resolved_snapshot().unwrap();
        let combat = snapshot.get("combat-core").unwrap();
        assert_eq!(
            combat.get("damage_multiplier"),
            Some(&ResolvedPluginConfigValue::Integer(12_500))
        );
        assert_eq!(
            combat.get("repair_hp_per_work_part"),
            Some(&ResolvedPluginConfigValue::Integer(7))
        );
        assert_eq!(
            combat.get("repair_energy_per_hp"),
            Some(&ResolvedPluginConfigValue::Integer(1))
        );

        let special = snapshot.get("special-attacks").unwrap();
        assert_eq!(
            special.get("fabricate_allowed_output_structures"),
            Some(&ResolvedPluginConfigValue::Array(vec![
                ResolvedPluginConfigValue::String("Tower".to_string())
            ]))
        );
    }

    fn lock_with_all_plugins_enabled() -> PluginLock {
        let mut lock = PluginLock::vanilla();
        for entry in lock.plugins.values_mut() {
            entry.enabled = true;
        }
        lock
    }

    fn manifest_default_config<T>(plugin: &str) -> T
    where
        T: DeserializeOwned,
    {
        let path = plugin_manifest_path(plugin);
        let manifest = std::fs::read_to_string(&path).unwrap();
        let manifest: toml::Value = toml::from_str(&manifest).unwrap();
        let defaults = manifest
            .get("config")
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| panic!("missing [config] in {}", path.display()))
            .iter()
            .map(|(key, metadata)| {
                let default = metadata
                    .get("default")
                    .unwrap_or_else(|| panic!("missing default for {plugin}.{key}"));
                (key.clone(), default.clone())
            })
            .collect();
        toml::Value::Table(defaults)
            .try_into()
            .unwrap_or_else(|error| {
                panic!("failed to decode manifest defaults for {plugin}: {error}")
            })
    }

    fn plugin_manifest_path(plugin: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("engine manifest must have a workspace parent")
            .join("mods")
            .join(plugin)
            .join("mod.toml")
    }
}
