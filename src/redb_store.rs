use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use bevy::prelude::Resource;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::command::{ObjectId, Tick};
use crate::components::PlayerId;
use crate::hot_cache::{CachedSnapshot, RedbSnapshotStore, SnapshotKey};
use crate::mcp::VisibleWorldSnapshot;
use crate::tick::{
    AtomicTickStore, CommitError, DeployActivationDecision, TickCommitRecord, TickState,
};

const KV_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");
type RedbWrite = (Vec<u8>, Vec<u8>);
const KEYFRAME_MAGIC: [u8; 8] = *b"SWKFRM01";
const KEYFRAME_FORMAT_VERSION: u32 = 1;
const ARCHIVE_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
static NEXT_EPHEMERAL_STORE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedbError {
    Unavailable(String),
    Encode(String),
    Decode(String),
    Commit(String),
    Integrity(String),
    NotFound(String),
}

impl std::fmt::Display for RedbError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "redb unavailable: {message}"),
            Self::Encode(message) => write!(formatter, "redb encode failed: {message}"),
            Self::Decode(message) => write!(formatter, "redb decode failed: {message}"),
            Self::Commit(message) => write!(formatter, "redb commit failed: {message}"),
            Self::Integrity(message) => {
                write!(formatter, "redb integrity check failed: {message}")
            }
            Self::NotFound(message) => write!(formatter, "redb row not found: {message}"),
        }
    }
}

impl std::error::Error for RedbError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UploadStatus {
    Pending,
    Uploading,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TickTerminalState {
    Verified,
    AuditGap,
    Unreplayable,
    Reconstructable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickHeadRow {
    pub tick: Tick,
    pub state_checksum: u64,
    pub canonical_codec_version: u32,
    pub snapshot_hash: [u8; 32],
    pub commands_hash: [u8; 32],
    pub terminal_state: TickTerminalState,
    pub tick_head_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickManifestRow {
    pub tick: Tick,
    pub object_id: String,
    pub content_hash: [u8; 32],
    pub blob_size: u64,
    pub upload_status: UploadStatus,
    pub object_store_etag: Option<String>,
    pub manifest_hash: [u8; 32],
    pub system_manifest_hash: [u8; 32],
    pub world_config_hash: [u8; 32],
    pub mods_lock_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateArtifactRow {
    pub tick: Tick,
    pub object_id: String,
    pub content_hash: [u8; 32],
    pub blob_size: u64,
    pub upload_status: UploadStatus,
    pub object_store_etag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichTraceRead {
    pub blob: Option<Vec<u8>>,
    pub terminal_state: TickTerminalState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickHashChainRow {
    pub tick: Tick,
    pub previous_chain_hash: [u8; 32],
    pub chain_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRow {
    pub tick: Tick,
    pub state_checksum: u64,
    pub content_hash: [u8; 32],
    pub state: TickState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyframeHeader {
    pub magic: [u8; 8],
    pub format_version: u32,
    pub world_id: String,
    pub shard_id: String,
    pub tick: Tick,
    pub state_checksum: u64,
    pub payload_len: u64,
    pub payload_blake3: [u8; 32],
    pub header_crc32c: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyframePointerRow {
    pub tick: Tick,
    pub world_id: String,
    pub shard_id: String,
    pub primary_path: String,
    pub backup_path: String,
    pub header: KeyframeHeader,
    pub status: UploadStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct KeyframeFile {
    header: KeyframeHeader,
    state: TickState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployArtifactRow {
    pub deploy_id: String,
    pub wasm_module_hash: [u8; 32],
    pub module_object_id: String,
    pub module_len: u64,
    pub compiled_artifact_hash: [u8; 32],
    pub compiled_artifact_object_id: Option<String>,
    pub compiled_artifact_len: Option<u64>,
    pub status: UploadStatus,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployArtifactRead {
    pub row: DeployArtifactRow,
    pub wasm_bytes: Vec<u8>,
    pub compiled_artifact_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployManifestRow {
    pub schema_version: u32,
    pub deploy_id: String,
    pub player_id: PlayerId,
    pub world_id: String,
    pub module_slot: String,
    pub drone_id: ObjectId,
    pub wasm_module_hash: [u8; 32],
    pub metadata_hash: String,
    pub signed_payload_hash: String,
    pub compiled_artifact_hash: [u8; 32],
    pub client_version_counter: u64,
    pub redb_version_counter: u64,
    pub certificate_id: String,
    pub certificate_fingerprint: String,
    pub transport: String,
    pub signed_at: String,
    pub accepted_at_tick: Tick,
    pub activation_tick: Tick,
    pub status: String,
    pub archive: bool,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployCurrentRow {
    pub player_id: PlayerId,
    pub world_id: String,
    pub module_slot: String,
    pub deploy_id: String,
    pub drone_id: ObjectId,
    pub wasm_module_hash: [u8; 32],
    pub metadata_hash: String,
    pub client_version_counter: u64,
    pub redb_version_counter: u64,
    pub activation_tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployActivationIndexRow {
    pub tick: Tick,
    pub deploy_id: String,
    pub player_id: PlayerId,
    pub world_id: String,
    pub module_slot: String,
    pub drone_id: ObjectId,
    pub wasm_module_hash: [u8; 32],
    pub redb_version_counter: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DeployIdempotencyRow {
    deploy_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployManifestCommit {
    Accepted(DeployManifestRow),
    Idempotent(DeployManifestRow),
    AlreadyDeployed(DeployManifestRow),
}

impl DeployManifestCommit {
    pub fn manifest(&self) -> &DeployManifestRow {
        match self {
            Self::Accepted(manifest)
            | Self::Idempotent(manifest)
            | Self::AlreadyDeployed(manifest) => manifest,
        }
    }

    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TickCommitPayload {
    pub tick: Tick,
    pub commit_record: TickCommitRecord,
    pub tick_trace_blob: Vec<u8>,
    pub recovery_state_blob: Option<Vec<u8>>,
    pub object_id: String,
    pub terminal_state: TickTerminalState,
    pub system_manifest_hash: [u8; 32],
    pub mods_lock_hash: [u8; 32],
    pub keyframe: Option<SnapshotRow>,
    pub replay_critical_writes: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryPoint {
    pub tick: Tick,
    pub record: TickCommitRecord,
    pub head: TickHeadRow,
    pub manifest: TickManifestRow,
    pub chain: TickHashChainRow,
    pub snapshot: Option<SnapshotRow>,
    pub rich_terminal_state: TickTerminalState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TickArchiveWalEntry {
    tick: Tick,
    rich_object_id: String,
    rich_trace_blob: Vec<u8>,
    state_object_id: Option<String>,
    state_blob: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RichTraceBlob {
    schema: String,
    tick: Tick,
    rows: BTreeMap<String, Vec<u8>>,
}

#[derive(Resource, Debug, Clone)]
pub struct RedbStore {
    pub db: Option<Arc<Database>>,
    pub snapshots: Arc<Mutex<BTreeMap<SnapshotKey, CachedSnapshot>>>,
    backend: Arc<RedbBackend>,
    deploy_guard: Arc<Mutex<()>>,
    object_store_root: Arc<PathBuf>,
    wal_root: Arc<PathBuf>,
    keyframe_root: Arc<PathBuf>,
    keyframe_backup_root: Arc<PathBuf>,
    world_id: Arc<String>,
    shard_id: Arc<String>,
    archive_retry_delays: Arc<Vec<Duration>>,
    active_archive_workers: Arc<Mutex<BTreeSet<Tick>>>,
}

#[derive(Debug)]
pub enum RedbBackend {
    Unavailable(String),
    InMemory(Mutex<InMemoryRedb>),
}

impl Default for RedbStore {
    fn default() -> Self {
        Self::unavailable("not connected")
    }
}

impl RedbStore {
    pub fn open(path: &str) -> Result<Self, RedbError> {
        let object_store_root = std::env::var_os("SWARM_OBJECT_STORE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("{path}.objects")));
        let wal_root = std::env::var_os("SWARM_WAL_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("{path}.wal")));
        let keyframe_root = PathBuf::from(format!("{path}.keyframes"));
        let keyframe_backup_root = std::env::var_os("KEYFRAME_BACKUP_PATH")
            .map(PathBuf::from)
            .ok_or_else(|| {
                RedbError::Unavailable(
                    "KEYFRAME_BACKUP_PATH must be configured for production keyframe backups"
                        .to_string(),
                )
            })?;
        validate_backup_root_isolated(&keyframe_root, &keyframe_backup_root)?;
        Self::open_with_artifact_paths(
            path,
            object_store_root,
            wal_root,
            keyframe_root,
            keyframe_backup_root,
        )
    }

    fn open_with_artifact_paths(
        path: &str,
        object_store_root: PathBuf,
        wal_root: PathBuf,
        keyframe_root: PathBuf,
        keyframe_backup_root: PathBuf,
    ) -> Result<Self, RedbError> {
        let db =
            Database::create(path).map_err(|error| RedbError::Unavailable(error.to_string()))?;
        {
            let txn = db
                .begin_write()
                .map_err(|error| RedbError::Unavailable(error.to_string()))?;
            {
                txn.open_table(KV_TABLE)
                    .map_err(|error| RedbError::Unavailable(error.to_string()))?;
            }
            txn.commit()
                .map_err(|error| RedbError::Unavailable(error.to_string()))?;
        }
        fs::create_dir_all(&object_store_root).map_err(|error| {
            RedbError::Unavailable(format!(
                "create object store {}: {error}",
                object_store_root.display()
            ))
        })?;
        fs::create_dir_all(&wal_root).map_err(|error| {
            RedbError::Unavailable(format!("create WAL {}: {error}", wal_root.display()))
        })?;
        fs::create_dir_all(&keyframe_root).map_err(|error| {
            RedbError::Unavailable(format!(
                "create keyframe directory {}: {error}",
                keyframe_root.display()
            ))
        })?;
        let world_id = deployment_world_id();
        let shard_id = deployment_shard_id();
        fs::create_dir_all(keyframe_backup_root.join(&world_id).join(&shard_id)).map_err(
            |error| {
                RedbError::Unavailable(format!(
                    "create keyframe backup directory {}: {error}",
                    keyframe_backup_root.display()
                ))
            },
        )?;
        let store = Self {
            db: Some(Arc::new(db)),
            snapshots: Arc::new(Mutex::new(BTreeMap::new())),
            backend: Arc::new(RedbBackend::Unavailable(
                "redb database connected".to_string(),
            )),
            deploy_guard: Arc::new(Mutex::new(())),
            object_store_root: Arc::new(object_store_root),
            wal_root: Arc::new(wal_root),
            keyframe_root: Arc::new(keyframe_root),
            keyframe_backup_root: Arc::new(keyframe_backup_root),
            world_id: Arc::new(world_id),
            shard_id: Arc::new(shard_id),
            archive_retry_delays: Arc::new(ARCHIVE_RETRY_DELAYS.to_vec()),
            active_archive_workers: Arc::new(Mutex::new(BTreeSet::new())),
        };
        store.recover_archive_wal()?;
        Ok(store)
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        let root = ephemeral_store_root("unavailable");
        Self {
            db: None,
            snapshots: Arc::new(Mutex::new(BTreeMap::new())),
            backend: Arc::new(RedbBackend::Unavailable(reason.into())),
            deploy_guard: Arc::new(Mutex::new(())),
            object_store_root: Arc::new(root.join("objects")),
            wal_root: Arc::new(root.join("wal")),
            keyframe_root: Arc::new(root.join("keyframes")),
            keyframe_backup_root: Arc::new(root.join("keyframe-backup")),
            world_id: Arc::new(deployment_world_id()),
            shard_id: Arc::new(deployment_shard_id()),
            archive_retry_delays: Arc::new(ARCHIVE_RETRY_DELAYS.to_vec()),
            active_archive_workers: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        let root = ephemeral_store_root("memory");
        fs::create_dir_all(root.join("objects")).expect("create test object store");
        fs::create_dir_all(root.join("wal")).expect("create test WAL");
        fs::create_dir_all(root.join("keyframes")).expect("create test keyframes");
        fs::create_dir_all(root.join("keyframe-backup/default/default"))
            .expect("create test keyframe backup");
        Self {
            db: None,
            snapshots: Arc::new(Mutex::new(BTreeMap::new())),
            backend: Arc::new(RedbBackend::InMemory(Mutex::new(InMemoryRedb::default()))),
            deploy_guard: Arc::new(Mutex::new(())),
            object_store_root: Arc::new(root.join("objects")),
            wal_root: Arc::new(root.join("wal")),
            keyframe_root: Arc::new(root.join("keyframes")),
            keyframe_backup_root: Arc::new(root.join("keyframe-backup")),
            world_id: Arc::new("default".to_string()),
            shard_id: Arc::new("default".to_string()),
            archive_retry_delays: Arc::new(vec![Duration::ZERO; 3]),
            active_archive_workers: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    #[cfg(test)]
    pub fn in_memory_failing_commit() -> Self {
        let root = ephemeral_store_root("failing-memory");
        fs::create_dir_all(root.join("objects")).expect("create test object store");
        fs::create_dir_all(root.join("wal")).expect("create test WAL");
        fs::create_dir_all(root.join("keyframes")).expect("create test keyframes");
        fs::create_dir_all(root.join("keyframe-backup/default/default"))
            .expect("create test keyframe backup");
        Self {
            db: None,
            snapshots: Arc::new(Mutex::new(BTreeMap::new())),
            backend: Arc::new(RedbBackend::InMemory(Mutex::new(InMemoryRedb {
                fail_next_commit: true,
                ..Default::default()
            }))),
            deploy_guard: Arc::new(Mutex::new(())),
            object_store_root: Arc::new(root.join("objects")),
            wal_root: Arc::new(root.join("wal")),
            keyframe_root: Arc::new(root.join("keyframes")),
            keyframe_backup_root: Arc::new(root.join("keyframe-backup")),
            world_id: Arc::new("default".to_string()),
            shard_id: Arc::new("default".to_string()),
            archive_retry_delays: Arc::new(vec![Duration::ZERO; 3]),
            active_archive_workers: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub fn is_available(&self) -> bool {
        if self.db.is_some() {
            true
        } else {
            match self.backend.as_ref() {
                RedbBackend::Unavailable(_) => false,
                RedbBackend::InMemory(_) => true,
            }
        }
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        if self.db.is_some() {
            None
        } else {
            match self.backend.as_ref() {
                RedbBackend::Unavailable(reason) => Some(reason),
                RedbBackend::InMemory(_) => None,
            }
        }
    }

    pub fn write_visible_snapshot(&mut self, snapshot: VisibleWorldSnapshot) -> CachedSnapshot {
        let key = SnapshotKey::new(snapshot.player_id, snapshot.tick);
        let cached = CachedSnapshot::new(snapshot);
        self.put_snapshot(key, cached.clone());
        cached
    }

    pub fn commit_tick_writes(&self, writes: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), RedbError> {
        if let Some(db) = &self.db {
            return commit_writes(db, writes);
        }
        match self.backend.as_ref() {
            RedbBackend::Unavailable(reason) => Err(RedbError::Unavailable(reason.clone())),
            RedbBackend::InMemory(backend) => backend
                .lock()
                .map_err(|_| RedbError::Commit("in-memory redb lock poisoned".to_string()))?
                .commit(writes),
        }
    }

    pub fn commit_tick_payload(
        &mut self,
        payload: TickCommitPayload,
    ) -> Result<RecoveryPoint, RedbError> {
        let record = payload.commit_record.clone();
        let _deploy_guard =
            if record.deploy_activation_decision.is_empty() {
                None
            } else {
                Some(self.deploy_guard.lock().map_err(|_| {
                    RedbError::Commit("deploy transaction lock poisoned".to_string())
                })?)
            };
        let previous_chain_hash = self.previous_chain_hash(payload.tick)?;
        let head_hash = tick_head_hash(
            payload.tick,
            record.state_checksum,
            record.canonical_codec_version,
            record.snapshot_hash,
            record.commands_hash,
            payload.terminal_state,
        );
        let rich_content_hash = content_hash(&payload.tick_trace_blob);
        let head = TickHeadRow {
            tick: payload.tick,
            state_checksum: record.state_checksum,
            canonical_codec_version: record.canonical_codec_version,
            snapshot_hash: record.snapshot_hash,
            commands_hash: record.commands_hash,
            terminal_state: payload.terminal_state,
            tick_head_hash: head_hash,
        };
        let rich_object_id = payload.object_id.clone();
        let manifest = TickManifestRow {
            tick: payload.tick,
            object_id: rich_object_id.clone(),
            content_hash: rich_content_hash,
            blob_size: payload.tick_trace_blob.len() as u64,
            upload_status: UploadStatus::Pending,
            object_store_etag: None,
            manifest_hash: record.manifest_hash,
            system_manifest_hash: payload.system_manifest_hash,
            world_config_hash: record.world_config_hash,
            mods_lock_hash: payload.mods_lock_hash,
        };
        let state_object_id = payload
            .recovery_state_blob
            .as_ref()
            .map(|_| format!("tick/{}/state.json", payload.tick));
        let state_artifact = payload
            .recovery_state_blob
            .as_ref()
            .zip(state_object_id.as_ref())
            .map(|(state_blob, object_id)| StateArtifactRow {
                tick: payload.tick,
                object_id: object_id.clone(),
                content_hash: content_hash(state_blob),
                blob_size: state_blob.len() as u64,
                upload_status: UploadStatus::Pending,
                object_store_etag: None,
            });
        let wal_entry = TickArchiveWalEntry {
            tick: payload.tick,
            rich_object_id,
            rich_trace_blob: payload.tick_trace_blob,
            state_object_id,
            state_blob: payload.recovery_state_blob,
        };
        let chain = TickHashChainRow {
            tick: payload.tick,
            previous_chain_hash,
            chain_hash: chain_hash(previous_chain_hash, head.tick_head_hash),
        };

        let mut writes = vec![
            (
                tick_commit_record_key(payload.tick),
                encode(&record, "tick commit record")?,
            ),
            (tick_head_key(payload.tick), encode(&head, "tick head")?),
            (
                tick_manifest_key(payload.tick),
                encode(&manifest, "tick manifest")?,
            ),
            (
                tick_hash_chain_key(payload.tick),
                encode(&chain, "tick hash chain")?,
            ),
        ];
        if let Some(state_artifact) = &state_artifact {
            writes.push((
                state_artifact_key(payload.tick),
                encode(state_artifact, "state artifact")?,
            ));
        }
        let keyframe_pointer = payload
            .keyframe
            .as_ref()
            .map(|snapshot| self.keyframe_pointer(snapshot))
            .transpose()?;
        if let Some(snapshot) = &payload.keyframe {
            verify_snapshot_row(snapshot)?;
            writes.push((
                snapshot_state_key(snapshot.tick),
                encode(
                    keyframe_pointer
                        .as_ref()
                        .ok_or_else(|| RedbError::Encode("keyframe pointer missing".to_string()))?,
                    "keyframe pointer",
                )?,
            ));
        }
        for decision in &record.deploy_activation_decision {
            writes.extend(self.deploy_activation_writes(payload.tick, decision)?);
        }
        writes.extend(
            payload
                .replay_critical_writes
                .into_iter()
                .filter(|(key, _)| {
                    ![
                        "state",
                        "metrics",
                        "resource_ledger",
                        "security_alerts",
                        "delta",
                    ]
                    .iter()
                    .any(|suffix| key.as_slice() == tick_key_bytes(payload.tick, suffix).as_slice())
                }),
        );
        self.write_archive_wal(&wal_entry)?;
        if let Err(error) = self.commit_tick_writes(writes) {
            let _ = self.remove_archive_wal(payload.tick);
            return Err(error);
        }
        if let (Some(snapshot), Some(pointer)) = (&payload.keyframe, &keyframe_pointer)
            && let Err(error) = self.write_keyframe_files(snapshot, pointer)
        {
            eprintln!(
                "keyframe publish failed after redb commit tick={} error={error}",
                payload.tick
            );
            if let Err(mark_error) = self.update_keyframe_pointer_status(
                pointer,
                UploadStatus::Failed,
                "keyframe pointer failed",
            ) {
                eprintln!(
                    "keyframe failed-status update failed tick={} error={mark_error}",
                    payload.tick
                );
            }
        }
        self.spawn_archive_worker(wal_entry);

        Ok(RecoveryPoint {
            tick: payload.tick,
            record,
            head,
            manifest,
            chain,
            snapshot: None,
            rich_terminal_state: TickTerminalState::AuditGap,
        })
    }

    fn spawn_archive_worker(&self, entry: TickArchiveWalEntry) {
        if let Ok(mut active) = self.active_archive_workers.lock() {
            active.insert(entry.tick);
        }
        let store = self.clone();
        thread::spawn(move || {
            if let Err(error) = store.process_archive_entry(&entry) {
                eprintln!("tick archive failed tick={} error={error}", entry.tick);
            }
            if let Ok(mut active) = store.active_archive_workers.lock() {
                active.remove(&entry.tick);
            }
        });
    }

    fn process_archive_entry(&self, entry: &TickArchiveWalEntry) -> Result<(), RedbError> {
        self.update_manifest_upload(entry.tick, UploadStatus::Uploading, None)?;
        if let Some(state_blob) = &entry.state_blob {
            self.update_state_artifact_upload(entry.tick, UploadStatus::Uploading, None)?;
            match self.write_object_with_retry(
                entry
                    .state_object_id
                    .as_deref()
                    .ok_or_else(|| RedbError::Integrity("state object id missing".to_string()))?,
                state_blob,
            ) {
                Ok(etag) => self.update_state_artifact_upload(
                    entry.tick,
                    UploadStatus::Complete,
                    Some(etag),
                )?,
                Err(error) => {
                    self.update_state_artifact_upload(entry.tick, UploadStatus::Failed, None)?;
                    self.update_manifest_after_archive(
                        entry.tick,
                        &entry.rich_object_id,
                        &entry.rich_trace_blob,
                    )?;
                    return Err(error);
                }
            }
        }

        self.update_manifest_after_archive(
            entry.tick,
            &entry.rich_object_id,
            &entry.rich_trace_blob,
        )?;
        self.remove_archive_wal(entry.tick)
    }

    fn update_manifest_after_archive(
        &self,
        tick: Tick,
        object_id: &str,
        blob: &[u8],
    ) -> Result<(), RedbError> {
        match self.write_object_with_retry(object_id, blob) {
            Ok(etag) => self.update_manifest_upload(tick, UploadStatus::Complete, Some(etag)),
            Err(error) => {
                self.update_manifest_upload(tick, UploadStatus::Failed, None)?;
                Err(error)
            }
        }
    }

    fn update_manifest_upload(
        &self,
        tick: Tick,
        upload_status: UploadStatus,
        object_store_etag: Option<String>,
    ) -> Result<(), RedbError> {
        let mut manifest = self
            .read_tick_manifest(tick)?
            .ok_or_else(|| RedbError::NotFound(format!("tick_manifest {tick}")))?;
        manifest.upload_status = upload_status;
        manifest.object_store_etag = object_store_etag;
        self.commit_tick_writes(vec![(
            tick_manifest_key(tick),
            encode(&manifest, "tick manifest upload status")?,
        )])
    }

    fn update_state_artifact_upload(
        &self,
        tick: Tick,
        upload_status: UploadStatus,
        object_store_etag: Option<String>,
    ) -> Result<(), RedbError> {
        let mut artifact: StateArtifactRow = self
            .read_json(&state_artifact_key(tick))?
            .ok_or_else(|| RedbError::NotFound(format!("state artifact {tick}")))?;
        artifact.upload_status = upload_status;
        artifact.object_store_etag = object_store_etag;
        self.commit_tick_writes(vec![(
            state_artifact_key(tick),
            encode(&artifact, "state artifact upload status")?,
        )])
    }

    fn write_object_with_retry(&self, object_id: &str, bytes: &[u8]) -> Result<String, RedbError> {
        for attempt in 0..=self.archive_retry_delays.len() {
            match self.write_object(object_id, bytes) {
                Ok(etag) => return Ok(etag),
                Err(_error) if attempt < self.archive_retry_delays.len() => {
                    thread::sleep(self.archive_retry_delays[attempt]);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("archive retry loop always returns")
    }

    fn write_object(&self, object_id: &str, bytes: &[u8]) -> Result<String, RedbError> {
        let relative = safe_object_path(object_id)?;
        let path = self.object_store_root.join(relative);
        let parent = path.parent().ok_or_else(|| {
            RedbError::Unavailable(format!("object path has no parent: {}", path.display()))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            RedbError::Unavailable(format!(
                "create object directory {}: {error}",
                parent.display()
            ))
        })?;
        let temp = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            NEXT_EPHEMERAL_STORE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        durable_atomic_write(&temp, &path, bytes, "object")?;
        Ok(hex32(&content_hash(bytes)))
    }

    fn write_archive_wal(&self, entry: &TickArchiveWalEntry) -> Result<(), RedbError> {
        fs::create_dir_all(self.wal_root.as_ref()).map_err(|error| {
            RedbError::Unavailable(format!("create WAL {}: {error}", self.wal_root.display()))
        })?;
        let path = archive_wal_path(&self.wal_root, entry.tick);
        let temp = path.with_extension(format!(
            "json.tmp-{}-{}",
            std::process::id(),
            NEXT_EPHEMERAL_STORE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = encode(entry, "tick archive WAL")?;
        durable_atomic_write(&temp, &path, &bytes, "WAL")
    }

    fn remove_archive_wal(&self, tick: Tick) -> Result<(), RedbError> {
        let path = archive_wal_path(&self.wal_root, tick);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RedbError::Unavailable(format!(
                "remove WAL {}: {error}",
                path.display()
            ))),
        }
    }

    fn recover_archive_wal(&self) -> Result<(), RedbError> {
        let mut paths = fs::read_dir(self.wal_root.as_ref())
            .map_err(|error| {
                RedbError::Unavailable(format!("read WAL {}: {error}", self.wal_root.display()))
            })?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| RedbError::Unavailable(format!("read WAL entry: {error}")))?;
        paths.sort();
        for path in paths {
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path).map_err(|error| {
                RedbError::Unavailable(format!("read WAL {}: {error}", path.display()))
            })?;
            let entry: TickArchiveWalEntry = decode(&bytes, "tick archive WAL")?;
            if self.read_tick_manifest(entry.tick)?.is_some() {
                self.process_archive_entry(&entry)?;
            } else {
                self.remove_archive_wal(entry.tick)?;
            }
        }
        Ok(())
    }

    pub fn wait_for_archive(
        &self,
        tick: Tick,
        timeout: Duration,
    ) -> Result<UploadStatus, RedbError> {
        let started = Instant::now();
        loop {
            let status = self
                .read_tick_manifest(tick)?
                .ok_or_else(|| RedbError::NotFound(format!("tick_manifest {tick}")))?
                .upload_status;
            let worker_active = self
                .active_archive_workers
                .lock()
                .map_err(|_| RedbError::Commit("archive worker lock poisoned".to_string()))?
                .contains(&tick);
            let lifecycle_terminal = match status {
                UploadStatus::Failed => true,
                UploadStatus::Complete => !archive_wal_path(&self.wal_root, tick).exists(),
                UploadStatus::Pending | UploadStatus::Uploading => false,
            };
            if lifecycle_terminal && !worker_active {
                return Ok(status);
            }
            if started.elapsed() >= timeout {
                return Err(RedbError::Unavailable(format!(
                    "archive wait timed out at tick {tick}"
                )));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn write_snapshot(&mut self, row: SnapshotRow) -> Result<(), RedbError> {
        verify_snapshot_row(&row)?;
        let pointer = self.keyframe_pointer(&row)?;
        self.commit_tick_writes(vec![(
            snapshot_state_key(row.tick),
            encode(&pointer, "keyframe pointer")?,
        )])?;
        self.write_keyframe_files(&row, &pointer)
    }

    pub fn read_verified_snapshot(&self, tick: Tick) -> Result<SnapshotRow, RedbError> {
        let Some(bytes) = self.read_key(&snapshot_state_key(tick))? else {
            return Err(RedbError::NotFound(format!("snapshot tick {tick}")));
        };
        if let Ok(pointer) = decode::<KeyframePointerRow>(&bytes, "keyframe pointer") {
            return self.read_keyframe_from_pointer(&pointer);
        }
        let row: SnapshotRow = decode(&bytes, "legacy snapshot")?;
        verify_snapshot_row(&row)?;
        Ok(row)
    }

    pub fn verify_tick(&self, tick: Tick) -> Result<RecoveryPoint, RedbError> {
        let record = self
            .read_tick_commit_record(tick)?
            .ok_or_else(|| RedbError::NotFound(format!("tick_commit_record {tick}")))?;
        let head = self
            .read_tick_head(tick)?
            .ok_or_else(|| RedbError::NotFound(format!("tick_head {tick}")))?;
        let manifest = self
            .read_tick_manifest(tick)?
            .ok_or_else(|| RedbError::NotFound(format!("tick_manifest {tick}")))?;
        let chain = self
            .read_hash_chain(tick)?
            .ok_or_else(|| RedbError::NotFound(format!("tick_hash_chain {tick}")))?;

        if head.tick != tick || manifest.tick != tick || chain.tick != tick {
            return Err(RedbError::Integrity(format!(
                "tick row mismatch for {tick}"
            )));
        }
        let expected_head_hash = tick_head_hash(
            head.tick,
            head.state_checksum,
            head.canonical_codec_version,
            head.snapshot_hash,
            head.commands_hash,
            head.terminal_state,
        );
        if head.tick_head_hash != expected_head_hash {
            return Err(RedbError::Integrity(format!(
                "tick_head hash mismatch at tick {tick}"
            )));
        }
        if record.state_checksum != head.state_checksum
            || record.canonical_codec_version != head.canonical_codec_version
            || record.snapshot_hash != head.snapshot_hash
            || record.commands_hash != head.commands_hash
        {
            return Err(RedbError::Integrity(format!(
                "tick commit record does not match tick_head at tick {tick}"
            )));
        }
        if record.manifest_hash != manifest.manifest_hash
            || record.world_config_hash != manifest.world_config_hash
        {
            return Err(RedbError::Integrity(format!(
                "tick commit record does not match tick_manifest at tick {tick}"
            )));
        }
        let expected_previous = self.previous_chain_hash(tick)?;
        if chain.previous_chain_hash != expected_previous {
            return Err(RedbError::Integrity(format!(
                "hash chain previous mismatch at tick {tick}"
            )));
        }
        let expected_chain_hash = chain_hash(chain.previous_chain_hash, head.tick_head_hash);
        if chain.chain_hash != expected_chain_hash {
            return Err(RedbError::Integrity(format!(
                "hash chain mismatch at tick {tick}"
            )));
        }

        Ok(RecoveryPoint {
            tick,
            record,
            head,
            rich_terminal_state: rich_terminal_state(manifest.upload_status),
            manifest,
            chain,
            snapshot: self.read_verified_snapshot(tick).ok(),
        })
    }

    pub fn recover_latest(&self) -> Result<Option<RecoveryPoint>, RedbError> {
        let mut latest = None;
        for tick in self.committed_ticks()? {
            let mut point = self.verify_tick(tick)?;
            if let Ok(snapshot) = self.read_verified_snapshot(tick) {
                if snapshot.state_checksum != point.head.state_checksum {
                    return Err(RedbError::Integrity(format!(
                        "snapshot checksum does not match tick_head at tick {tick}"
                    )));
                }
                point.snapshot = Some(snapshot);
            }
            latest = Some(point);
        }
        Ok(latest)
    }

    pub fn read_tick_head(&self, tick: Tick) -> Result<Option<TickHeadRow>, RedbError> {
        self.read_json(&tick_head_key(tick))
    }

    pub fn read_tick_commit_record(
        &self,
        tick: Tick,
    ) -> Result<Option<TickCommitRecord>, RedbError> {
        self.read_key(&tick_commit_record_key(tick))?
            .map(|bytes| decode_tick_commit_record(&bytes))
            .transpose()
    }

    pub fn read_tick_state(&self, tick: Tick) -> Result<Option<TickState>, RedbError> {
        if let Some(state) = self.read_json(&tick_key_bytes(tick, "state"))? {
            return Ok(Some(state));
        }
        let Some(artifact) = self.read_state_artifact(tick)? else {
            return Ok(None);
        };
        if artifact.upload_status == UploadStatus::Failed {
            return Ok(None);
        }
        let Some(bytes) = self.read_object_if_present(&artifact.object_id)? else {
            return Ok(None);
        };
        verify_artifact_bytes(
            &bytes,
            artifact.content_hash,
            artifact.blob_size,
            "state artifact",
        )?;
        decode(&bytes, "state artifact").map(Some)
    }

    pub fn read_state_artifact(&self, tick: Tick) -> Result<Option<StateArtifactRow>, RedbError> {
        self.read_json(&state_artifact_key(tick))
    }

    fn keyframe_pointer(&self, row: &SnapshotRow) -> Result<KeyframePointerRow, RedbError> {
        verify_snapshot_row(row)?;
        let state_bytes = keyframe_state_bytes(&row.state)?;
        let mut header = KeyframeHeader {
            magic: KEYFRAME_MAGIC,
            format_version: KEYFRAME_FORMAT_VERSION,
            world_id: (*self.world_id).clone(),
            shard_id: (*self.shard_id).clone(),
            tick: row.tick,
            state_checksum: row.state_checksum,
            payload_len: state_bytes.len() as u64,
            payload_blake3: content_hash(&state_bytes),
            header_crc32c: 0,
        };
        header.header_crc32c = keyframe_header_crc32c(&header)?;
        let primary = self.keyframe_root.join(keyframe_file_name(row.tick));
        let backup = self
            .keyframe_backup_root
            .join(&*self.world_id)
            .join(&*self.shard_id)
            .join(keyframe_file_name(row.tick));
        Ok(KeyframePointerRow {
            tick: row.tick,
            world_id: (*self.world_id).clone(),
            shard_id: (*self.shard_id).clone(),
            primary_path: primary.to_string_lossy().into_owned(),
            backup_path: backup.to_string_lossy().into_owned(),
            header,
            status: UploadStatus::Pending,
        })
    }

    fn write_keyframe_files(
        &self,
        row: &SnapshotRow,
        pointer: &KeyframePointerRow,
    ) -> Result<(), RedbError> {
        let bytes = encode(
            &KeyframeFile {
                header: pointer.header.clone(),
                state: row.state.clone(),
            },
            "keyframe file",
        )?;
        let primary = PathBuf::from(&pointer.primary_path);
        let backup = PathBuf::from(&pointer.backup_path);
        write_keyframe_path(&primary, &bytes, "primary keyframe")?;
        write_keyframe_path(&backup, &bytes, "backup keyframe")?;
        let mut complete = pointer.clone();
        complete.status = UploadStatus::Complete;
        self.commit_tick_writes(vec![(
            snapshot_state_key(row.tick),
            encode(&complete, "keyframe pointer complete")?,
        )])
    }

    fn read_keyframe_from_pointer(
        &self,
        pointer: &KeyframePointerRow,
    ) -> Result<SnapshotRow, RedbError> {
        if pointer.status != UploadStatus::Complete {
            return Err(RedbError::Integrity(format!(
                "keyframe tick {} is not complete: {:?}",
                pointer.tick, pointer.status
            )));
        }
        let mut first_error = None;
        for path in [&pointer.primary_path, &pointer.backup_path] {
            match read_keyframe_path(Path::new(path), pointer) {
                Ok(row) => return Ok(row),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        Err(first_error
            .unwrap_or_else(|| RedbError::NotFound(format!("keyframe tick {}", pointer.tick))))
    }

    fn update_keyframe_pointer_status(
        &self,
        pointer: &KeyframePointerRow,
        status: UploadStatus,
        label: &str,
    ) -> Result<(), RedbError> {
        let mut next = pointer.clone();
        next.status = status;
        self.commit_tick_writes(vec![(
            snapshot_state_key(pointer.tick),
            encode(&next, label)?,
        )])
    }

    pub fn commit_deploy_artifact(
        &self,
        deploy_id: &str,
        wasm_module_hash: [u8; 32],
        wasm_bytes: &[u8],
        compiled_artifact_hash: [u8; 32],
        compiled_artifact_bytes: Option<&[u8]>,
    ) -> Result<DeployArtifactRow, RedbError> {
        verify_non_empty_hash(wasm_bytes, wasm_module_hash, "deploy module")?;
        if let Some(bytes) = compiled_artifact_bytes {
            verify_non_empty_hash(bytes, compiled_artifact_hash, "compiled deploy artifact")?;
        }
        let module_object_id = format!("deploy/{deploy_id}/module.wasm");
        let compiled_artifact_object_id = compiled_artifact_bytes
            .as_ref()
            .map(|_| format!("deploy/{deploy_id}/compiled_artifact.bin"));
        self.write_object_with_retry(&module_object_id, wasm_bytes)?;
        if let (Some(object_id), Some(bytes)) =
            (&compiled_artifact_object_id, compiled_artifact_bytes)
        {
            self.write_object_with_retry(object_id, bytes)?;
        }
        let row = DeployArtifactRow {
            deploy_id: deploy_id.to_string(),
            wasm_module_hash,
            module_object_id,
            module_len: wasm_bytes.len() as u64,
            compiled_artifact_hash,
            compiled_artifact_object_id,
            compiled_artifact_len: compiled_artifact_bytes.map(|bytes| bytes.len() as u64),
            status: UploadStatus::Complete,
            failure: None,
        };
        self.commit_tick_writes(vec![(
            deploy_artifact_key(deploy_id),
            encode(&row, "deploy artifact")?,
        )])?;
        Ok(row)
    }

    pub fn read_deploy_artifact(
        &self,
        deploy_id: &str,
    ) -> Result<Option<DeployArtifactRow>, RedbError> {
        self.read_json(&deploy_artifact_key(deploy_id))
    }

    pub fn read_verified_deploy_artifact(
        &self,
        manifest: &DeployManifestRow,
    ) -> Result<DeployArtifactRead, RedbError> {
        let row = self
            .read_deploy_artifact(&manifest.deploy_id)?
            .ok_or_else(|| {
                RedbError::NotFound(format!("deploy artifact {}", manifest.deploy_id))
            })?;
        if row.status != UploadStatus::Complete {
            return Err(RedbError::Integrity(format!(
                "deploy artifact {} not complete",
                manifest.deploy_id
            )));
        }
        if row.wasm_module_hash != manifest.wasm_module_hash
            || row.compiled_artifact_hash != manifest.compiled_artifact_hash
        {
            return Err(RedbError::Integrity(format!(
                "deploy artifact hash pointer mismatch deploy_id={}",
                manifest.deploy_id
            )));
        }
        let wasm_bytes = self
            .read_object_if_present(&row.module_object_id)?
            .ok_or_else(|| RedbError::NotFound(format!("deploy module {}", manifest.deploy_id)))?;
        verify_artifact_bytes(
            &wasm_bytes,
            row.wasm_module_hash,
            row.module_len,
            "deploy module",
        )?;
        if wasm_bytes.is_empty() {
            return Err(RedbError::Integrity(format!(
                "deploy module {} is empty",
                manifest.deploy_id
            )));
        }
        let compiled_artifact_bytes =
            match (&row.compiled_artifact_object_id, row.compiled_artifact_len) {
                (Some(object_id), Some(len)) => {
                    let bytes = self.read_object_if_present(object_id)?.ok_or_else(|| {
                        RedbError::NotFound(format!(
                            "compiled deploy artifact {}",
                            manifest.deploy_id
                        ))
                    })?;
                    verify_artifact_bytes(
                        &bytes,
                        row.compiled_artifact_hash,
                        len,
                        "compiled deploy artifact",
                    )?;
                    Some(bytes)
                }
                (None, None) => None,
                _ => {
                    return Err(RedbError::Integrity(format!(
                        "deploy artifact {} has incomplete compiled artifact pointer",
                        manifest.deploy_id
                    )));
                }
            };
        Ok(DeployArtifactRead {
            row,
            wasm_bytes,
            compiled_artifact_bytes,
        })
    }

    pub fn mark_deploy_recovery_failed(
        &self,
        deploy_id: &str,
        failure: impl Into<String>,
    ) -> Result<(), RedbError> {
        let Some(mut manifest) = self.read_deploy_manifest(deploy_id)? else {
            return Ok(());
        };
        manifest.status = "failed".to_string();
        manifest.failure = Some(failure.into());
        self.commit_tick_writes(vec![(
            deploy_manifest_key(deploy_id),
            encode(&manifest, "deploy recovery failure")?,
        )])
    }

    pub fn read_rich_trace(&self, tick: Tick) -> Result<RichTraceRead, RedbError> {
        let manifest = self
            .read_tick_manifest(tick)?
            .ok_or_else(|| RedbError::NotFound(format!("tick_manifest {tick}")))?;
        let terminal_state = rich_terminal_state(manifest.upload_status);
        if manifest.upload_status != UploadStatus::Complete {
            return Ok(RichTraceRead {
                blob: None,
                terminal_state,
            });
        }
        let bytes = self
            .read_object_if_present(&manifest.object_id)?
            .ok_or_else(|| {
                RedbError::Integrity(format!("rich trace object missing at tick {tick}"))
            })?;
        verify_artifact_bytes(
            &bytes,
            manifest.content_hash,
            manifest.blob_size,
            "rich trace",
        )?;
        Ok(RichTraceRead {
            blob: Some(bytes),
            terminal_state,
        })
    }

    fn read_object_if_present(&self, object_id: &str) -> Result<Option<Vec<u8>>, RedbError> {
        let path = self.object_store_root.join(safe_object_path(object_id)?);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(RedbError::Unavailable(format!(
                "read object {}: {error}",
                path.display()
            ))),
        }
    }

    pub fn read_tick_manifest(&self, tick: Tick) -> Result<Option<TickManifestRow>, RedbError> {
        self.read_json(&tick_manifest_key(tick))
    }

    pub fn commit_deploy_manifest(
        &self,
        mut manifest: DeployManifestRow,
    ) -> Result<DeployManifestCommit, RedbError> {
        let _guard = self
            .deploy_guard
            .lock()
            .map_err(|_| RedbError::Commit("deploy transaction lock poisoned".to_string()))?;
        let idempotency_key = deploy_idempotency_key_fields(
            manifest.player_id,
            &manifest.world_id,
            &manifest.module_slot,
            manifest.client_version_counter,
            &manifest.wasm_module_hash,
            &manifest.metadata_hash,
        );
        if let Some(existing) = self.read_json::<DeployIdempotencyRow>(&idempotency_key)? {
            let manifest = self
                .read_deploy_manifest(&existing.deploy_id)?
                .ok_or_else(|| {
                    RedbError::Integrity(format!(
                        "deploy idempotency row {} has no manifest",
                        existing.deploy_id
                    ))
                })?;
            return Ok(DeployManifestCommit::Idempotent(manifest));
        }

        let current_key = deploy_current_key(
            manifest.player_id,
            &manifest.world_id,
            &manifest.module_slot,
        );
        let previous_current = self.read_json::<DeployCurrentRow>(&current_key)?;
        if let Some(current) = &previous_current {
            if current.wasm_module_hash == manifest.wasm_module_hash
                && current.metadata_hash == manifest.metadata_hash
            {
                let existing = self
                    .read_deploy_manifest(&current.deploy_id)?
                    .ok_or_else(|| {
                        RedbError::Integrity(format!(
                            "deploy current {} has no manifest",
                            current.deploy_id
                        ))
                    })?;
                return Ok(DeployManifestCommit::AlreadyDeployed(existing));
            }
            if manifest.client_version_counter <= current.client_version_counter {
                return Err(RedbError::Integrity(format!(
                    "stale deploy counter for player {} world {} slot {}",
                    manifest.player_id, manifest.world_id, manifest.module_slot
                )));
            }
        }

        manifest.redb_version_counter = previous_current
            .as_ref()
            .map_or(1, |row| row.redb_version_counter.saturating_add(1));
        manifest.status = "activation_pending".to_string();
        let current = DeployCurrentRow {
            player_id: manifest.player_id,
            world_id: manifest.world_id.clone(),
            module_slot: manifest.module_slot.clone(),
            deploy_id: manifest.deploy_id.clone(),
            drone_id: manifest.drone_id,
            wasm_module_hash: manifest.wasm_module_hash,
            metadata_hash: manifest.metadata_hash.clone(),
            client_version_counter: manifest.client_version_counter,
            redb_version_counter: manifest.redb_version_counter,
            activation_tick: manifest.activation_tick,
        };
        let activation = DeployActivationIndexRow {
            tick: manifest.activation_tick,
            deploy_id: manifest.deploy_id.clone(),
            player_id: manifest.player_id,
            world_id: manifest.world_id.clone(),
            module_slot: manifest.module_slot.clone(),
            drone_id: manifest.drone_id,
            wasm_module_hash: manifest.wasm_module_hash,
            redb_version_counter: manifest.redb_version_counter,
        };
        let mut writes = Vec::new();
        if let Some(previous_current) = &previous_current {
            let mut previous = self
                .read_deploy_manifest(&previous_current.deploy_id)?
                .ok_or_else(|| {
                    RedbError::Integrity(format!(
                        "deploy current {} has no manifest",
                        previous_current.deploy_id
                    ))
                })?;
            if previous.status == "activation_pending" {
                previous.status = "superseded".to_string();
                writes.push((
                    deploy_manifest_key(&previous.deploy_id),
                    encode(&previous, "superseded deploy manifest")?,
                ));
            }
        }
        writes.extend([
            (
                deploy_manifest_key(&manifest.deploy_id),
                encode(&manifest, "deploy manifest")?,
            ),
            (current_key, encode(&current, "deploy current")?),
            (
                idempotency_key,
                encode(
                    &DeployIdempotencyRow {
                        deploy_id: manifest.deploy_id.clone(),
                    },
                    "deploy idempotency",
                )?,
            ),
            (
                deploy_activation_index_key(manifest.activation_tick, &manifest.deploy_id),
                encode(&activation, "deploy activation index")?,
            ),
        ]);
        self.commit_tick_writes(writes)?;
        Ok(DeployManifestCommit::Accepted(manifest))
    }

    pub fn read_deploy_current(
        &self,
        player_id: PlayerId,
        world_id: &str,
        module_slot: &str,
    ) -> Result<Option<DeployCurrentRow>, RedbError> {
        self.read_json(&deploy_current_key(player_id, world_id, module_slot))
    }

    pub fn read_deploy_manifest(
        &self,
        deploy_id: &str,
    ) -> Result<Option<DeployManifestRow>, RedbError> {
        self.read_json(&deploy_manifest_key(deploy_id))
    }

    pub fn read_deploy_activation_index(
        &self,
        tick: Tick,
        deploy_id: &str,
    ) -> Result<Option<DeployActivationIndexRow>, RedbError> {
        self.read_json(&deploy_activation_index_key(tick, deploy_id))
    }

    pub fn recover_deploy_manifests(&self) -> Result<Vec<DeployManifestRow>, RedbError> {
        let mut manifests = self
            .scan_json_prefix::<DeployManifestRow>(b"/deploy/manifest/")?
            .into_iter()
            .map(|(_, manifest)| manifest)
            .filter(|manifest| {
                manifest.status == "active" || manifest.status == "activation_pending"
            })
            .collect::<Vec<_>>();
        manifests.sort_by(|left, right| {
            (
                left.player_id,
                left.world_id.as_str(),
                left.module_slot.as_str(),
                left.redb_version_counter,
            )
                .cmp(&(
                    right.player_id,
                    right.world_id.as_str(),
                    right.module_slot.as_str(),
                    right.redb_version_counter,
                ))
        });
        Ok(manifests)
    }

    pub fn read_hash_chain(&self, tick: Tick) -> Result<Option<TickHashChainRow>, RedbError> {
        self.read_json(&tick_hash_chain_key(tick))
    }

    fn previous_chain_hash(&self, tick: Tick) -> Result<[u8; 32], RedbError> {
        if tick == 0 {
            return Ok([0; 32]);
        }
        Ok(self
            .read_hash_chain(tick - 1)?
            .map(|row| row.chain_hash)
            .unwrap_or([0; 32]))
    }

    fn deploy_activation_writes(
        &self,
        tick: Tick,
        decision: &DeployActivationDecision,
    ) -> Result<Vec<RedbWrite>, RedbError> {
        let current_key = deploy_current_key(
            decision.player_id,
            &decision.world_id,
            &decision.module_slot,
        );
        if let Some(current) = self.read_json::<DeployCurrentRow>(&current_key)? {
            if current.client_version_counter > decision.client_version_counter {
                return Err(RedbError::Integrity(format!(
                    "stale deploy counter for player {} world {} slot {}",
                    decision.player_id, decision.world_id, decision.module_slot
                )));
            }
            if current.client_version_counter == decision.client_version_counter
                && (current.wasm_module_hash != decision.wasm_module_hash
                    || current.redb_version_counter != decision.redb_version_counter)
            {
                return Err(RedbError::Integrity(format!(
                    "conflicting deploy replay for player {} world {} slot {} counter {}",
                    decision.player_id,
                    decision.world_id,
                    decision.module_slot,
                    decision.client_version_counter
                )));
            }
        }

        let manifest = DeployManifestRow {
            schema_version: decision.schema_version,
            deploy_id: decision.deploy_id.clone(),
            player_id: decision.player_id,
            world_id: decision.world_id.clone(),
            module_slot: decision.module_slot.clone(),
            drone_id: decision.drone_id,
            wasm_module_hash: decision.wasm_module_hash,
            metadata_hash: decision.metadata_hash.clone(),
            signed_payload_hash: decision.signed_payload_hash.clone(),
            compiled_artifact_hash: decision.compiled_artifact_hash,
            client_version_counter: decision.client_version_counter,
            redb_version_counter: decision.redb_version_counter,
            certificate_id: decision.certificate_id.clone(),
            certificate_fingerprint: decision.certificate_fingerprint.clone(),
            transport: decision.transport.clone(),
            signed_at: decision.signed_at.clone(),
            accepted_at_tick: decision.accepted_at_tick,
            activation_tick: decision.activation_tick,
            status: decision.status.clone(),
            archive: decision.archive,
            failure: decision.failure.clone(),
        };
        let current = DeployCurrentRow {
            player_id: decision.player_id,
            world_id: decision.world_id.clone(),
            module_slot: decision.module_slot.clone(),
            deploy_id: decision.deploy_id.clone(),
            drone_id: decision.drone_id,
            wasm_module_hash: decision.wasm_module_hash,
            metadata_hash: decision.metadata_hash.clone(),
            client_version_counter: decision.client_version_counter,
            redb_version_counter: decision.redb_version_counter,
            activation_tick: decision.activation_tick,
        };
        let index = DeployActivationIndexRow {
            tick,
            deploy_id: decision.deploy_id.clone(),
            player_id: decision.player_id,
            world_id: decision.world_id.clone(),
            module_slot: decision.module_slot.clone(),
            drone_id: decision.drone_id,
            wasm_module_hash: decision.wasm_module_hash,
            redb_version_counter: decision.redb_version_counter,
        };
        Ok(vec![
            (
                deploy_manifest_key(&decision.deploy_id),
                encode(&manifest, "deploy manifest")?,
            ),
            (current_key, encode(&current, "deploy current")?),
            (
                deploy_idempotency_key(decision),
                encode(
                    &DeployIdempotencyRow {
                        deploy_id: decision.deploy_id.clone(),
                    },
                    "deploy idempotency",
                )?,
            ),
            (
                deploy_activation_index_key(tick, &decision.deploy_id),
                encode(&index, "deploy activation index")?,
            ),
        ])
    }

    pub fn write_json<T: Serialize>(
        &mut self,
        key: &[u8],
        value: &T,
        label: &str,
    ) -> Result<(), RedbError> {
        self.commit_tick_writes(vec![(key.to_vec(), encode(value, label)?)])
    }

    pub fn write_json_batch(&mut self, writes: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), RedbError> {
        self.commit_tick_writes(writes)
    }

    pub fn encode_json<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>, RedbError> {
        encode(value, label)
    }

    pub fn read_json_value<T: DeserializeOwned>(&self, key: &[u8]) -> Result<Option<T>, RedbError> {
        self.read_json(key)
    }

    pub fn scan_json_prefix<T: DeserializeOwned>(
        &self,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, T)>, RedbError> {
        if let Some(db) = &self.db {
            let txn = db
                .begin_read()
                .map_err(|error| RedbError::Commit(error.to_string()))?;
            let table = txn
                .open_table(KV_TABLE)
                .map_err(|error| RedbError::Commit(error.to_string()))?;
            let mut rows = Vec::new();
            for entry in table
                .range(prefix..)
                .map_err(|error| RedbError::Commit(error.to_string()))?
            {
                let (key, value) = entry.map_err(|error| RedbError::Commit(error.to_string()))?;
                let key = key.value().to_vec();
                if !key.starts_with(prefix) {
                    break;
                }
                rows.push((
                    key,
                    decode(
                        value.value(),
                        std::str::from_utf8(prefix).unwrap_or("prefix scan"),
                    )?,
                ));
            }
            return Ok(rows);
        }
        match self.backend.as_ref() {
            RedbBackend::Unavailable(reason) => Err(RedbError::Unavailable(format!(
                "{reason}; cannot scan prefix"
            ))),
            RedbBackend::InMemory(backend) => backend
                .lock()
                .map_err(|_| RedbError::Commit("in-memory redb lock poisoned".to_string()))?
                .data
                .range(prefix.to_vec()..)
                .take_while(|(key, _)| key.starts_with(prefix))
                .map(|(key, value)| {
                    decode(value, std::str::from_utf8(prefix).unwrap_or("prefix scan"))
                        .map(|row| (key.clone(), row))
                })
                .collect(),
        }
    }

    fn read_json<T: DeserializeOwned>(&self, key: &[u8]) -> Result<Option<T>, RedbError> {
        self.read_key(key)?
            .map(|value| decode(&value, std::str::from_utf8(key).unwrap_or("key")))
            .transpose()
    }

    fn read_key(&self, key: &[u8]) -> Result<Option<Vec<u8>>, RedbError> {
        if let Some(db) = &self.db {
            return read_key(db, key);
        }
        match self.backend.as_ref() {
            RedbBackend::Unavailable(reason) => {
                Err(RedbError::Unavailable(format!("{reason}; cannot read key")))
            }
            RedbBackend::InMemory(backend) => Ok(backend
                .lock()
                .map_err(|_| RedbError::Commit("in-memory redb lock poisoned".to_string()))?
                .data
                .get(key)
                .cloned()),
        }
    }

    fn committed_ticks(&self) -> Result<Vec<Tick>, RedbError> {
        if let Some(db) = &self.db {
            return committed_ticks(db);
        }
        match self.backend.as_ref() {
            RedbBackend::Unavailable(reason) => Err(RedbError::Unavailable(format!(
                "{reason}; cannot scan ticks"
            ))),
            RedbBackend::InMemory(backend) => {
                let backend = backend
                    .lock()
                    .map_err(|_| RedbError::Commit("in-memory redb lock poisoned".to_string()))?;
                let mut ticks = backend
                    .data
                    .keys()
                    .filter_map(|key| parse_tick_head_key(key))
                    .collect::<Vec<_>>();
                ticks.sort_unstable();
                Ok(ticks)
            }
        }
    }
}

impl RedbSnapshotStore for RedbStore {
    fn get_snapshot(&self, key: SnapshotKey) -> Option<CachedSnapshot> {
        if let Some(snapshot) = self
            .snapshots
            .lock()
            .ok()
            .and_then(|snapshots| snapshots.get(&key).cloned())
        {
            return Some(snapshot);
        }
        let key_bytes = visible_snapshot_key(key);
        let value = self.read_key(&key_bytes).ok().flatten()?;
        decode::<CachedSnapshot>(&value, "visible_snapshot").ok()
    }

    fn put_snapshot(&mut self, key: SnapshotKey, snapshot: CachedSnapshot) {
        if let Ok(mut snapshots) = self.snapshots.lock() {
            snapshots.insert(key, snapshot.clone());
        }
        if self.is_available() {
            let key_bytes = visible_snapshot_key(key);
            match serde_json::to_vec(&snapshot) {
                Ok(value) => {
                    if let Err(error) = self.commit_tick_writes(vec![(key_bytes, value)]) {
                        eprintln!("redb snapshot write failed key={key:?} error={error}");
                    }
                }
                Err(error) => {
                    eprintln!("redb snapshot encode failed key={key:?} error={error}")
                }
            }
        }
    }
}

impl AtomicTickStore for RedbStore {
    fn atomic_commit(&mut self, writes: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), CommitError> {
        self.commit_tick_writes_or_payload(writes)
            .map_err(|error| CommitError::Failed(error.to_string()))
    }
}

impl RedbStore {
    fn commit_tick_writes_or_payload(
        &mut self,
        writes: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<(), RedbError> {
        let Some((tick, record_bytes)) = writes.iter().find_map(|(key, value)| {
            parse_tick_commit_record_key(key).map(|tick| (tick, value.clone()))
        }) else {
            return self.commit_tick_writes(writes);
        };

        let record = decode_tick_commit_record(&record_bytes)?;
        let recovery_state_blob = writes
            .iter()
            .find(|(key, _)| key.as_slice() == tick_key_bytes(tick, "state").as_slice())
            .map(|(_, value)| value.clone());
        let keyframe = writes
            .iter()
            .find(|(key, _)| key.as_slice() == tick_key_bytes(tick, "keyframe").as_slice())
            .map(|(_, value)| -> Result<SnapshotRow, RedbError> {
                let state: TickState = decode(value, "tick keyframe")?;
                Ok(SnapshotRow {
                    tick,
                    state_checksum: record.state_checksum,
                    content_hash: snapshot_content_hash(&state)?,
                    state,
                })
            })
            .transpose()?;

        let rich_trace_blob = encode(
            &RichTraceBlob {
                schema: "swarm.rich-trace.v1".to_string(),
                tick,
                rows: writes
                    .iter()
                    .map(|(key, value)| (String::from_utf8_lossy(key).into_owned(), value.clone()))
                    .collect(),
            },
            "rich trace blob",
        )?;

        let replay_critical_writes = writes
            .into_iter()
            .filter(|(key, _)| {
                key.as_slice() != tick_commit_record_key(tick).as_slice()
                    && key.as_slice() != tick_key_bytes(tick, "keyframe").as_slice()
                    && key.as_slice() != tick_key_bytes(tick, "state").as_slice()
                    && key.as_slice() != tick_key_bytes(tick, "metrics").as_slice()
                    && key.as_slice() != tick_key_bytes(tick, "resource_ledger").as_slice()
                    && key.as_slice() != tick_key_bytes(tick, "security_alerts").as_slice()
                    && key.as_slice() != tick_key_bytes(tick, "delta").as_slice()
            })
            .collect::<Vec<_>>();

        self.commit_tick_payload(TickCommitPayload {
            tick,
            commit_record: record,
            tick_trace_blob: rich_trace_blob,
            recovery_state_blob,
            object_id: format!("tick/{tick}/rich-trace.json"),
            terminal_state: TickTerminalState::Verified,
            system_manifest_hash: [0; 32],
            mods_lock_hash: [0; 32],
            keyframe,
            replay_critical_writes,
        })?;
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct InMemoryRedb {
    data: BTreeMap<Vec<u8>, Vec<u8>>,
    fail_next_commit: bool,
}

impl InMemoryRedb {
    fn commit(&mut self, writes: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), RedbError> {
        if self.fail_next_commit {
            self.fail_next_commit = false;
            return Err(RedbError::Commit("in-memory commit failed".to_string()));
        }
        let mut next = self.data.clone();
        for (key, value) in writes {
            next.insert(key, value);
        }
        self.data = next;
        Ok(())
    }
}

fn encode<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>, RedbError> {
    serde_json::to_vec(value).map_err(|error| RedbError::Encode(format!("{label}: {error}")))
}

fn decode<T: DeserializeOwned>(value: &[u8], label: &str) -> Result<T, RedbError> {
    serde_json::from_slice(value).map_err(|error| RedbError::Decode(format!("{label}: {error}")))
}

fn decode_tick_commit_record(value: &[u8]) -> Result<TickCommitRecord, RedbError> {
    match decode(value, "tick commit record") {
        Ok(record) => Ok(record),
        Err(first_error) => {
            let mut json: serde_json::Value = serde_json::from_slice(value).map_err(|error| {
                RedbError::Decode(format!("tick commit record legacy json: {error}"))
            })?;
            if migrate_legacy_action_commands(&mut json) {
                serde_json::from_value(json).map_err(|error| {
                    RedbError::Decode(format!(
                        "tick commit record legacy Action migration: {error}"
                    ))
                })
            } else {
                Err(first_error)
            }
        }
    }
}

fn migrate_legacy_action_commands(value: &mut serde_json::Value) -> bool {
    let mut changed = false;
    if let Some(commands) = value
        .get_mut("commands")
        .and_then(serde_json::Value::as_array_mut)
    {
        for command in commands {
            changed |= migrate_legacy_action_command(command);
        }
    }
    if let Some(rejections) = value
        .get_mut("rejections")
        .and_then(serde_json::Value::as_array_mut)
    {
        for rejection in rejections {
            if let Some(command) = rejection.get_mut("command") {
                changed |= migrate_legacy_action_command(command);
            }
        }
    }
    changed
}

fn migrate_legacy_action_command(command: &mut serde_json::Value) -> bool {
    let Some(action) = command.get_mut("action") else {
        return false;
    };
    let Some(fields) = action.as_object_mut() else {
        return false;
    };
    if fields.get("type").and_then(serde_json::Value::as_str) != Some("Action") {
        return false;
    }
    let Some(action_type) = fields
        .remove("action_type")
        .or_else(|| fields.remove("action_name"))
        .and_then(|value| value.as_str().map(str::to_string))
    else {
        return false;
    };
    fields.insert("type".to_string(), serde_json::Value::String(action_type));
    if let Some(serde_json::Value::Object(payload)) = fields.remove("payload") {
        for (key, value) in payload {
            fields.entry(key).or_insert(value);
        }
    }
    true
}

fn ephemeral_store_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "swarm-redb-{label}-{}-{}",
        std::process::id(),
        NEXT_EPHEMERAL_STORE_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

fn safe_object_path(object_id: &str) -> Result<PathBuf, RedbError> {
    let path = Path::new(object_id);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(RedbError::Integrity(format!(
            "invalid object id {object_id}"
        )));
    }
    Ok(path.to_path_buf())
}

fn archive_wal_path(wal_root: &Path, tick: Tick) -> PathBuf {
    wal_root.join(format!("tick-{tick:020}.json"))
}

fn durable_atomic_write(
    temp_path: &Path,
    final_path: &Path,
    bytes: &[u8],
    label: &str,
) -> Result<(), RedbError> {
    let mut file = File::create(temp_path).map_err(|error| {
        RedbError::Unavailable(format!("create {label} {}: {error}", temp_path.display()))
    })?;
    file.write_all(bytes).map_err(|error| {
        RedbError::Unavailable(format!("write {label} {}: {error}", temp_path.display()))
    })?;
    file.sync_all().map_err(|error| {
        RedbError::Unavailable(format!("sync {label} {}: {error}", temp_path.display()))
    })?;
    drop(file);
    fs::rename(temp_path, final_path).map_err(|error| {
        let _ = fs::remove_file(temp_path);
        RedbError::Unavailable(format!("publish {label} {}: {error}", final_path.display()))
    })?;
    if let Some(parent) = final_path.parent() {
        match File::open(parent).and_then(|directory| directory.sync_all()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {}
            Err(error) => {
                return Err(RedbError::Unavailable(format!(
                    "sync {label} directory {}: {error}",
                    parent.display()
                )));
            }
        }
    }
    Ok(())
}

fn rich_terminal_state(upload_status: UploadStatus) -> TickTerminalState {
    match upload_status {
        UploadStatus::Complete => TickTerminalState::Verified,
        UploadStatus::Pending | UploadStatus::Uploading | UploadStatus::Failed => {
            TickTerminalState::AuditGap
        }
    }
}

fn verify_artifact_bytes(
    bytes: &[u8],
    expected_hash: [u8; 32],
    expected_size: u64,
    label: &str,
) -> Result<(), RedbError> {
    if bytes.len() as u64 != expected_size {
        return Err(RedbError::Integrity(format!(
            "{label} size mismatch: expected {expected_size}, got {}",
            bytes.len()
        )));
    }
    if content_hash(bytes) != expected_hash {
        return Err(RedbError::Integrity(format!(
            "{label} content hash mismatch"
        )));
    }
    Ok(())
}

fn visible_snapshot_key(key: SnapshotKey) -> Vec<u8> {
    format!("/snapshot/{}/{}", key.player_id, key.tick).into_bytes()
}

fn tick_head_key(tick: Tick) -> Vec<u8> {
    format!("/tick/{tick}/tick_head").into_bytes()
}

fn tick_commit_record_key(tick: Tick) -> Vec<u8> {
    format!("/tick/{tick}/commit_record").into_bytes()
}

fn tick_key_bytes(tick: Tick, suffix: &str) -> Vec<u8> {
    format!("/tick/{tick}/{suffix}").into_bytes()
}

fn tick_manifest_key(tick: Tick) -> Vec<u8> {
    format!("/tick/{tick}/tick_manifest").into_bytes()
}

fn state_artifact_key(tick: Tick) -> Vec<u8> {
    format!("/tick/{tick}/state_artifact").into_bytes()
}

fn tick_hash_chain_key(tick: Tick) -> Vec<u8> {
    format!("/tick/{tick}/tick_hash_chain").into_bytes()
}

fn snapshot_state_key(tick: Tick) -> Vec<u8> {
    format!("/snapshot_state/{tick}").into_bytes()
}

fn deploy_manifest_key(deploy_id: &str) -> Vec<u8> {
    format!("/deploy/manifest/{deploy_id}").into_bytes()
}

fn deploy_artifact_key(deploy_id: &str) -> Vec<u8> {
    format!("/deploy/artifact/{deploy_id}").into_bytes()
}

fn deploy_current_key(player_id: PlayerId, world_id: &str, module_slot: &str) -> Vec<u8> {
    format!("/deploy/current/{player_id}/{world_id}/{module_slot}").into_bytes()
}

fn deploy_idempotency_key(decision: &DeployActivationDecision) -> Vec<u8> {
    deploy_idempotency_key_fields(
        decision.player_id,
        &decision.world_id,
        &decision.module_slot,
        decision.client_version_counter,
        &decision.wasm_module_hash,
        &decision.metadata_hash,
    )
}

fn deploy_idempotency_key_fields(
    player_id: PlayerId,
    world_id: &str,
    module_slot: &str,
    client_version_counter: u64,
    wasm_module_hash: &[u8; 32],
    metadata_hash: &str,
) -> Vec<u8> {
    format!(
        "/deploy/idempotency/{player_id}/{world_id}/{module_slot}/{client_version_counter}/{}/{}",
        hex32(wasm_module_hash),
        metadata_hash,
    )
    .into_bytes()
}

fn deploy_activation_index_key(tick: Tick, deploy_id: &str) -> Vec<u8> {
    format!("/deploy/activation/{tick}/{deploy_id}").into_bytes()
}

fn hex32(value: &[u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_tick_head_key(key: &[u8]) -> Option<Tick> {
    let text = std::str::from_utf8(key).ok()?;
    let tick = text.strip_prefix("/tick/")?.strip_suffix("/tick_head")?;
    tick.parse().ok()
}

fn parse_tick_commit_record_key(key: &[u8]) -> Option<Tick> {
    let text = std::str::from_utf8(key).ok()?;
    let tick = text
        .strip_prefix("/tick/")?
        .strip_suffix("/commit_record")?;
    tick.parse().ok()
}

fn content_hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

fn tick_head_hash(
    tick: Tick,
    state_checksum: u64,
    canonical_codec_version: u32,
    snapshot_hash: [u8; 32],
    commands_hash: [u8; 32],
    terminal_state: TickTerminalState,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&tick.to_le_bytes());
    hasher.update(&state_checksum.to_le_bytes());
    hasher.update(&canonical_codec_version.to_le_bytes());
    hasher.update(&snapshot_hash);
    hasher.update(&commands_hash);
    hasher.update(&[terminal_state as u8]);
    *hasher.finalize().as_bytes()
}

fn chain_hash(previous: [u8; 32], tick_head_hash: [u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&previous);
    hasher.update(&tick_head_hash);
    *hasher.finalize().as_bytes()
}

fn snapshot_content_hash(state: &TickState) -> Result<[u8; 32], RedbError> {
    let value = serde_json::to_value(state)
        .map_err(|error| RedbError::Encode(format!("snapshot state: {error}")))?;
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| RedbError::Encode(format!("snapshot state: {error}")))?;
    Ok(content_hash(&bytes))
}

fn keyframe_header_crc32c(header: &KeyframeHeader) -> Result<u32, RedbError> {
    let mut canonical = header.clone();
    canonical.header_crc32c = 0;
    let bytes = encode(&canonical, "keyframe header crc")?;
    Ok(crc32c::crc32c(&bytes))
}

fn keyframe_state_bytes(state: &TickState) -> Result<Vec<u8>, RedbError> {
    let value = serde_json::to_value(state)
        .map_err(|error| RedbError::Encode(format!("keyframe state: {error}")))?;
    serde_json::to_vec(&value)
        .map_err(|error| RedbError::Encode(format!("keyframe state: {error}")))
}

fn verify_snapshot_row(row: &SnapshotRow) -> Result<(), RedbError> {
    let expected = snapshot_content_hash(&row.state)?;
    if row.content_hash != expected {
        return Err(RedbError::Integrity(format!(
            "snapshot content hash mismatch at tick {}",
            row.tick
        )));
    }
    Ok(())
}

fn verify_keyframe_header(
    header: &KeyframeHeader,
    pointer: &KeyframePointerRow,
    state: &TickState,
) -> Result<(), RedbError> {
    if header.magic != KEYFRAME_MAGIC
        || header.format_version != KEYFRAME_FORMAT_VERSION
        || header.tick != pointer.tick
        || header.world_id != pointer.world_id
        || header.shard_id != pointer.shard_id
    {
        return Err(RedbError::Integrity(format!(
            "keyframe header identity mismatch at tick {}",
            pointer.tick
        )));
    }
    if keyframe_header_crc32c(header)? != header.header_crc32c {
        return Err(RedbError::Integrity(format!(
            "keyframe header crc mismatch at tick {}",
            pointer.tick
        )));
    }
    if header != &pointer.header {
        return Err(RedbError::Integrity(format!(
            "keyframe header pointer mismatch at tick {}",
            pointer.tick
        )));
    }
    let state_bytes = keyframe_state_bytes(state)?;
    if header.payload_len != state_bytes.len() as u64
        || header.payload_blake3 != content_hash(&state_bytes)
    {
        return Err(RedbError::Integrity(format!(
            "keyframe payload checksum mismatch at tick {}",
            pointer.tick
        )));
    }
    Ok(())
}

fn keyframe_file_name(tick: Tick) -> String {
    format!("{tick}.snap")
}

fn write_keyframe_path(path: &Path, bytes: &[u8], label: &str) -> Result<(), RedbError> {
    let parent = path.parent().ok_or_else(|| {
        RedbError::Unavailable(format!("{label} path has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        RedbError::Unavailable(format!(
            "create {label} directory {}: {error}",
            parent.display()
        ))
    })?;
    let temp = path.with_extension(format!(
        "snap.tmp-{}-{}",
        std::process::id(),
        NEXT_EPHEMERAL_STORE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    durable_atomic_write(&temp, path, bytes, label)
}

fn validate_backup_root_isolated(primary: &Path, backup: &Path) -> Result<(), RedbError> {
    if backup == primary || backup.starts_with(primary) || primary.starts_with(backup) {
        return Err(RedbError::Unavailable(format!(
            "KEYFRAME_BACKUP_PATH {} must be isolated from primary keyframe path {}",
            backup.display(),
            primary.display()
        )));
    }
    Ok(())
}

fn verify_non_empty_hash(
    bytes: &[u8],
    expected_hash: [u8; 32],
    label: &str,
) -> Result<(), RedbError> {
    if bytes.is_empty() {
        return Err(RedbError::Integrity(format!("{label} bytes are empty")));
    }
    if content_hash(bytes) != expected_hash {
        return Err(RedbError::Integrity(format!(
            "{label} BLAKE3 hash mismatch"
        )));
    }
    Ok(())
}

fn read_keyframe_path(path: &Path, pointer: &KeyframePointerRow) -> Result<SnapshotRow, RedbError> {
    let bytes = fs::read(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => RedbError::NotFound(format!(
            "keyframe {} missing at {}",
            pointer.tick,
            path.display()
        )),
        _ => RedbError::Unavailable(format!("read keyframe {}: {error}", path.display())),
    })?;
    let file: KeyframeFile = decode(&bytes, "keyframe file")?;
    verify_keyframe_header(&file.header, pointer, &file.state)?;
    let row = SnapshotRow {
        tick: file.header.tick,
        state_checksum: file.header.state_checksum,
        content_hash: snapshot_content_hash(&file.state)?,
        state: file.state,
    };
    verify_snapshot_row(&row)?;
    Ok(row)
}

fn deployment_world_id() -> String {
    std::env::var("WORLD_ID")
        .or_else(|_| std::env::var("SWARM_WORLD_ID"))
        .unwrap_or_else(|_| "default".to_string())
}

fn deployment_shard_id() -> String {
    std::env::var("SHARD_ID")
        .or_else(|_| std::env::var("SWARM_SHARD_ID"))
        .unwrap_or_else(|_| "default".to_string())
}

fn commit_writes(db: &Database, writes: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), RedbError> {
    let txn = db
        .begin_write()
        .map_err(|error| RedbError::Commit(error.to_string()))?;
    {
        let mut table = txn
            .open_table(KV_TABLE)
            .map_err(|error| RedbError::Commit(error.to_string()))?;
        for (key, value) in writes {
            table
                .insert(key.as_slice(), value.as_slice())
                .map_err(|error| RedbError::Commit(error.to_string()))?;
        }
    }
    txn.commit()
        .map_err(|error| RedbError::Commit(error.to_string()))
}

fn read_key(db: &Database, key: &[u8]) -> Result<Option<Vec<u8>>, RedbError> {
    let txn = db
        .begin_read()
        .map_err(|error| RedbError::Commit(error.to_string()))?;
    let table = txn
        .open_table(KV_TABLE)
        .map_err(|error| RedbError::Commit(error.to_string()))?;
    table
        .get(key)
        .map(|value| value.map(|bytes| bytes.value().to_vec()))
        .map_err(|error| RedbError::Commit(error.to_string()))
}

fn committed_ticks(db: &Database) -> Result<Vec<Tick>, RedbError> {
    let txn = db
        .begin_read()
        .map_err(|error| RedbError::Commit(error.to_string()))?;
    let table = txn
        .open_table(KV_TABLE)
        .map_err(|error| RedbError::Commit(error.to_string()))?;
    let mut ticks = Vec::new();
    for entry in table
        .iter()
        .map_err(|error| RedbError::Commit(error.to_string()))?
    {
        let (key, _) = entry.map_err(|error| RedbError::Commit(error.to_string()))?;
        if let Some(tick) = parse_tick_head_key(key.value()) {
            ticks.push(tick);
        }
    }
    ticks.sort_unstable();
    Ok(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandAction, CommandAuth, CommandSource, RawCommand};
    use crate::components::PlayerId;
    use crate::tick::{
        TickCommitRecord, TickFuelLedger, TickMetrics, TickTrace, WorldSnapshot, commands_hash,
        tick_trace_writes,
    };
    use crate::world::create_world;

    fn visible_snapshot(tick: Tick, player_id: PlayerId, room_id: u32) -> VisibleWorldSnapshot {
        VisibleWorldSnapshot {
            tick,
            player_id,
            room_id,
            visibility_radius: 5,
            visible_tiles: Vec::new(),
            entities: Vec::new(),
            local_storage: Default::default(),
            global_storage: Default::default(),
            pending_global_transfers: Vec::new(),
        }
    }

    fn payload(tick: Tick, checksum: u64) -> TickCommitPayload {
        let commands = Vec::new();
        let rejections = Vec::new();
        let record = TickCommitRecord {
            commands,
            rejections,
            fuel: TickFuelLedger::default(),
            deploy_activation_decision: Vec::new(),
            canonical_codec_version: 1,
            snapshot_hash: [4; 32],
            commands_hash: commands_hash(&Vec::new(), &Vec::new()),
            state_checksum: checksum,
            manifest_hash: [1; 32],
            world_config_hash: [2; 32],
        };
        TickCommitPayload {
            tick,
            commit_record: record,
            tick_trace_blob: format!("trace-{tick}").into_bytes(),
            recovery_state_blob: None,
            object_id: format!("tick-trace/{tick}.zst"),
            terminal_state: TickTerminalState::Verified,
            system_manifest_hash: [6; 32],
            mods_lock_hash: [3; 32],
            keyframe: None,
            replay_critical_writes: vec![(
                format!("/tick/{tick}/state").into_bytes(),
                b"state".to_vec(),
            )],
        }
    }

    fn atomic_trace(tick: Tick) -> TickTrace {
        let mut world = create_world();
        let state = WorldSnapshot::capture(world.app.world_mut());
        TickTrace {
            tick,
            player_id: 7,
            commands: Vec::new(),
            state,
            rejections: Vec::new(),
            metrics: TickMetrics::default(),
            state_checksum: world.state_checksum(),
            system_manifest_hash: [6; 32],
            action_manifest_hash: [7; 32],
            security_alerts: Vec::new(),
            trace_events: Vec::new(),
            resource_ledger: Default::default(),
        }
    }

    fn deploy_decision(
        deploy_id: &str,
        counter: u64,
        wasm_hash: [u8; 32],
    ) -> DeployActivationDecision {
        DeployActivationDecision {
            schema_version: 1,
            deploy_id: deploy_id.to_string(),
            player_id: 7,
            world_id: "world-alpha".to_string(),
            module_slot: "spawn:10:11".to_string(),
            drone_id: 99,
            wasm_module_hash: wasm_hash,
            metadata_hash: "blake3:metadata".to_string(),
            signed_payload_hash: "blake3:signed-payload".to_string(),
            compiled_artifact_hash: [8; 32],
            client_version_counter: counter,
            redb_version_counter: counter,
            certificate_id: "cert-1".to_string(),
            certificate_fingerprint: "fingerprint-1".to_string(),
            transport: "mcp".to_string(),
            signed_at: "1700000000".to_string(),
            accepted_at_tick: 4,
            activation_tick: 5,
            status: "active".to_string(),
            archive: false,
            failure: None,
        }
    }

    fn deploy_manifest(deploy_id: &str, counter: u64, wasm_hash: [u8; 32]) -> DeployManifestRow {
        let decision = deploy_decision(deploy_id, counter, wasm_hash);
        DeployManifestRow {
            schema_version: decision.schema_version,
            deploy_id: decision.deploy_id,
            player_id: decision.player_id,
            world_id: decision.world_id,
            module_slot: decision.module_slot,
            drone_id: decision.drone_id,
            wasm_module_hash: decision.wasm_module_hash,
            metadata_hash: decision.metadata_hash,
            signed_payload_hash: decision.signed_payload_hash,
            compiled_artifact_hash: decision.compiled_artifact_hash,
            client_version_counter: decision.client_version_counter,
            redb_version_counter: 0,
            certificate_id: decision.certificate_id,
            certificate_fingerprint: decision.certificate_fingerprint,
            transport: decision.transport,
            signed_at: decision.signed_at,
            accepted_at_tick: decision.accepted_at_tick,
            activation_tick: decision.activation_tick,
            status: "validated".to_string(),
            archive: decision.archive,
            failure: decision.failure,
        }
    }

    fn payload_with_deploy(
        tick: Tick,
        checksum: u64,
        decision: DeployActivationDecision,
    ) -> TickCommitPayload {
        let mut payload = payload(tick, checksum);
        payload.commit_record.deploy_activation_decision = vec![decision];
        payload
    }

    fn snapshot_row(tick: Tick, state_checksum: u64) -> SnapshotRow {
        let mut world = create_world();
        let state = WorldSnapshot::capture(world.app.world_mut());
        let content_hash = snapshot_content_hash(&state).unwrap();
        SnapshotRow {
            tick,
            state_checksum,
            content_hash,
            state,
        }
    }

    fn open_test_store(directory: &tempfile::TempDir, db_name: &str) -> RedbStore {
        RedbStore::open_with_artifact_paths(
            directory.path().join(db_name).to_str().unwrap(),
            directory.path().join(format!("{db_name}.objects")),
            directory.path().join(format!("{db_name}.wal")),
            directory.path().join(format!("{db_name}.keyframes")),
            directory.path().join(format!("{db_name}.keyframe-backup")),
        )
        .unwrap()
    }

    #[test]
    fn open_reports_database_errors() {
        let error = RedbStore::open("/tmp").unwrap_err();

        assert!(error.to_string().contains("redb unavailable"));
    }

    #[test]
    fn deploy_manifest_commit_is_atomic_idempotent_and_monotonic() {
        let store = RedbStore::in_memory();
        let first = deploy_manifest("deploy-1", 1, [7; 32]);

        let accepted = store.commit_deploy_manifest(first.clone()).unwrap();
        assert!(accepted.is_accepted());
        assert_eq!(accepted.manifest().redb_version_counter, 1);
        assert_eq!(accepted.manifest().status, "activation_pending");

        let retry = store.commit_deploy_manifest(first).unwrap();
        assert!(matches!(retry, DeployManifestCommit::Idempotent(_)));
        assert_eq!(retry.manifest().deploy_id, "deploy-1");

        let duplicate = store
            .commit_deploy_manifest(deploy_manifest("deploy-2", 2, [7; 32]))
            .unwrap();
        assert!(matches!(
            duplicate,
            DeployManifestCommit::AlreadyDeployed(_)
        ));

        let stale = store
            .commit_deploy_manifest(deploy_manifest("deploy-3", 1, [9; 32]))
            .unwrap_err();
        assert!(stale.to_string().contains("stale deploy counter"));
        let replacement = store
            .commit_deploy_manifest(deploy_manifest("deploy-4", 2, [9; 32]))
            .unwrap();
        assert!(replacement.is_accepted());
        assert_eq!(replacement.manifest().redb_version_counter, 2);
        assert_eq!(
            store
                .read_deploy_manifest("deploy-1")
                .unwrap()
                .unwrap()
                .status,
            "superseded"
        );
        let old_retry = store
            .commit_deploy_manifest(deploy_manifest("deploy-1", 1, [7; 32]))
            .unwrap();
        assert_eq!(old_retry.manifest().status, "superseded");
        let current = store
            .read_deploy_current(7, "world-alpha", "spawn:10:11")
            .unwrap()
            .unwrap();
        assert_eq!(current.deploy_id, "deploy-4");
        assert_eq!(current.redb_version_counter, 2);
    }

    #[test]
    fn failed_deploy_manifest_commit_leaves_no_partial_rows() {
        let store = RedbStore::in_memory_failing_commit();
        let manifest = deploy_manifest("deploy-failed", 1, [7; 32]);

        assert!(store.commit_deploy_manifest(manifest).is_err());
        assert!(
            store
                .read_deploy_manifest("deploy-failed")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .read_deploy_current(7, "world-alpha", "spawn:10:11")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .read_deploy_activation_index(5, "deploy-failed")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pending_deploy_manifest_survives_restart_for_activation_recovery() {
        let directory = tempfile::tempdir().unwrap();
        {
            let writer = open_test_store(&directory, "deploy-recovery.redb");
            writer
                .commit_deploy_manifest(deploy_manifest("deploy-pending", 1, [7; 32]))
                .unwrap();
        }

        let reader = open_test_store(&directory, "deploy-recovery.redb");
        let recovered = reader.recover_deploy_manifests().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].deploy_id, "deploy-pending");
        assert_eq!(recovered[0].status, "activation_pending");
    }

    #[test]
    fn redb_store_persists_atomic_tick_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = open_test_store(&directory, "swarm.redb");

        let point = store.commit_tick_payload(payload(1, 42)).unwrap();

        assert_eq!(point.head.state_checksum, 42);
        assert!(store.read_key(b"/tick/1/state").unwrap().is_none());
        assert_eq!(
            store.wait_for_archive(1, Duration::from_secs(1)).unwrap(),
            UploadStatus::Complete
        );
        assert_eq!(store.recover_latest().unwrap().unwrap().tick, 1);
    }

    #[test]
    fn degraded_store_keeps_visible_snapshots_available_in_process() {
        let mut store = RedbStore::unavailable("test degraded mode");
        let key = SnapshotKey::new(1, 7);
        let cached = store.write_visible_snapshot(visible_snapshot(7, 1, 0));

        assert_eq!(store.get_snapshot(key), Some(cached));
        assert!(!store.is_available());
    }

    #[test]
    fn degraded_atomic_commit_reports_unavailable_without_partial_success() {
        let mut store = RedbStore::unavailable("test degraded mode");

        let error = store
            .atomic_commit(vec![(b"/tick/1/state".to_vec(), b"{}".to_vec())])
            .unwrap_err();

        let CommitError::Failed(message) = error;
        assert!(message.contains("redb unavailable"));
    }

    #[test]
    fn tick_payload_commit_writes_head_manifest_and_hash_chain_atomically() {
        let mut store = RedbStore::in_memory();

        let point = store.commit_tick_payload(payload(1, 42)).unwrap();

        assert_eq!(point.head.state_checksum, 42);
        assert_eq!(point.record.state_checksum, 42);
        assert_eq!(point.manifest.upload_status, UploadStatus::Pending);
        assert_eq!(point.manifest.blob_size, b"trace-1".len() as u64);
        assert_eq!(point.manifest.system_manifest_hash, [6; 32]);
        assert!(store.read_key(b"/tick/1/commit_record").unwrap().is_some());
        assert!(store.read_key(b"/tick/1/state").unwrap().is_none());
        assert_eq!(store.verify_tick(1).unwrap().chain, point.chain);
    }

    #[test]
    fn atomic_tick_commit_archives_rich_trace_and_state_without_large_redb_rows() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("swarm.redb");
        let object_root = directory.path().join("objects");
        let wal_root = directory.path().join("wal");
        let keyframe_root = directory.path().join("keyframes");
        let keyframe_backup_root = directory.path().join("keyframe-backup");
        let mut store = RedbStore::open_with_artifact_paths(
            db_path.to_str().unwrap(),
            object_root,
            wal_root.clone(),
            keyframe_root,
            keyframe_backup_root,
        )
        .unwrap();
        store.archive_retry_delays = Arc::new(vec![Duration::ZERO; 3]);
        let trace = atomic_trace(1);

        store
            .atomic_commit(tick_trace_writes(&trace).unwrap())
            .unwrap();
        assert_eq!(
            store.wait_for_archive(1, Duration::from_secs(1)).unwrap(),
            UploadStatus::Complete
        );

        assert!(store.read_key(b"/tick/1/state").unwrap().is_none());
        assert!(store.read_key(b"/tick/1/metrics").unwrap().is_none());
        assert!(
            store
                .read_key(b"/tick/1/resource_ledger")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .read_key(b"/tick/1/security_alerts")
                .unwrap()
                .is_none()
        );
        assert!(store.read_key(b"/tick/1/delta").unwrap().is_none());
        assert_eq!(store.read_tick_state(1).unwrap(), Some(trace.state.clone()));
        let artifact = store.read_state_artifact(1).unwrap().unwrap();
        assert_eq!(artifact.upload_status, UploadStatus::Complete);
        assert!(artifact.object_store_etag.is_some());
        let rich = store.read_rich_trace(1).unwrap();
        assert_eq!(rich.terminal_state, TickTerminalState::Verified);
        let blob: RichTraceBlob = decode(rich.blob.as_deref().unwrap(), "rich trace").unwrap();
        assert_eq!(blob.tick, 1);
        assert!(blob.rows.contains_key("/tick/1/state"));
        assert!(blob.rows.contains_key("/tick/1/metrics"));
        assert!(!archive_wal_path(&wal_root, 1).exists());
    }

    #[test]
    fn wal_publish_is_atomic_durable_and_decodable_before_redb_commit() {
        let store = RedbStore::in_memory();
        let entry = TickArchiveWalEntry {
            tick: 41,
            rich_object_id: "tick/41/rich-trace.json".to_string(),
            rich_trace_blob: b"rich".to_vec(),
            state_object_id: Some("tick/41/state.json".to_string()),
            state_blob: Some(b"state".to_vec()),
        };

        store.write_archive_wal(&entry).unwrap();

        let path = archive_wal_path(&store.wal_root, 41);
        let persisted: TickArchiveWalEntry =
            decode(&fs::read(&path).unwrap(), "tick archive WAL").unwrap();
        assert_eq!(persisted.tick, 41);
        assert_eq!(persisted.rich_trace_blob, b"rich");
        assert_eq!(persisted.state_blob.as_deref(), Some(b"state".as_slice()));
        assert_eq!(
            fs::read_dir(store.wal_root.as_ref())
                .unwrap()
                .filter_map(Result::ok)
                .filter(
                    |entry| entry.path().extension().and_then(|value| value.to_str())
                        != Some("json")
                )
                .count(),
            0
        );
    }

    #[test]
    fn object_store_failure_preserves_redb_core_and_downgrades_rich_replay() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("swarm.redb");
        let object_root = directory.path().join("objects");
        let wal_root = directory.path().join("wal");
        let trace = atomic_trace(2);
        {
            let mut store = RedbStore::open_with_artifact_paths(
                db_path.to_str().unwrap(),
                object_root.clone(),
                wal_root.clone(),
                directory.path().join("keyframes"),
                directory.path().join("keyframe-backup"),
            )
            .unwrap();
            store.archive_retry_delays = Arc::new(vec![Duration::ZERO; 3]);
            fs::remove_dir_all(&object_root).unwrap();
            fs::write(&object_root, b"not-a-directory").unwrap();

            store
                .atomic_commit(tick_trace_writes(&trace).unwrap())
                .unwrap();
            assert_eq!(
                store.wait_for_archive(2, Duration::from_secs(1)).unwrap(),
                UploadStatus::Failed
            );

            assert!(store.read_tick_commit_record(2).unwrap().is_some());
            assert!(store.read_tick_head(2).unwrap().is_some());
            assert_eq!(
                store.read_state_artifact(2).unwrap().unwrap().upload_status,
                UploadStatus::Failed
            );
            assert_eq!(
                store.read_rich_trace(2).unwrap(),
                RichTraceRead {
                    blob: None,
                    terminal_state: TickTerminalState::AuditGap,
                }
            );
            assert_eq!(
                store.verify_tick(2).unwrap().rich_terminal_state,
                TickTerminalState::AuditGap
            );
            assert!(archive_wal_path(&wal_root, 2).exists());
        }

        fs::remove_file(&object_root).unwrap();
        fs::create_dir_all(&object_root).unwrap();
        let recovered = RedbStore::open_with_artifact_paths(
            db_path.to_str().unwrap(),
            object_root,
            wal_root.clone(),
            directory.path().join("keyframes"),
            directory.path().join("keyframe-backup"),
        )
        .unwrap();
        assert_eq!(
            recovered.read_tick_state(2).unwrap(),
            Some(trace.state.clone())
        );
        assert_eq!(recovered.recover_latest().unwrap().unwrap().tick, 2);
        assert_eq!(
            recovered
                .read_state_artifact(2)
                .unwrap()
                .unwrap()
                .upload_status,
            UploadStatus::Complete
        );
        assert_eq!(
            recovered.read_rich_trace(2).unwrap().terminal_state,
            TickTerminalState::Verified
        );
        assert!(!archive_wal_path(&wal_root, 2).exists());
    }

    #[test]
    fn rich_trace_failure_after_state_upload_preserves_wal_for_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("swarm.redb");
        let object_root = directory.path().join("objects");
        let wal_root = directory.path().join("wal");
        let trace = atomic_trace(4);
        {
            let mut store = RedbStore::open_with_artifact_paths(
                db_path.to_str().unwrap(),
                object_root.clone(),
                wal_root.clone(),
                directory.path().join("keyframes"),
                directory.path().join("keyframe-backup"),
            )
            .unwrap();
            store.archive_retry_delays = Arc::new(vec![Duration::ZERO; 3]);
            fs::create_dir_all(object_root.join("tick/4/rich-trace.json")).unwrap();

            store
                .atomic_commit(tick_trace_writes(&trace).unwrap())
                .unwrap();
            assert_eq!(
                store.wait_for_archive(4, Duration::from_secs(1)).unwrap(),
                UploadStatus::Failed
            );

            let state_artifact = store.read_state_artifact(4).unwrap().unwrap();
            assert_eq!(state_artifact.upload_status, UploadStatus::Complete);
            assert!(state_artifact.object_store_etag.is_some());
            assert_eq!(
                store.read_tick_manifest(4).unwrap().unwrap().upload_status,
                UploadStatus::Failed
            );
            assert_eq!(
                store.read_rich_trace(4).unwrap().terminal_state,
                TickTerminalState::AuditGap
            );
            assert!(archive_wal_path(&wal_root, 4).exists());
        }

        fs::remove_dir_all(object_root.join("tick/4/rich-trace.json")).unwrap();
        let recovered = RedbStore::open_with_artifact_paths(
            db_path.to_str().unwrap(),
            object_root,
            wal_root.clone(),
            directory.path().join("keyframes"),
            directory.path().join("keyframe-backup"),
        )
        .unwrap();
        assert_eq!(
            recovered
                .read_state_artifact(4)
                .unwrap()
                .unwrap()
                .upload_status,
            UploadStatus::Complete
        );
        assert_eq!(
            recovered
                .read_tick_manifest(4)
                .unwrap()
                .unwrap()
                .upload_status,
            UploadStatus::Complete
        );
        assert_eq!(recovered.read_tick_state(4).unwrap(), Some(trace.state));
        assert_eq!(
            recovered.read_rich_trace(4).unwrap().terminal_state,
            TickTerminalState::Verified
        );
        assert!(!archive_wal_path(&wal_root, 4).exists());
    }

    #[test]
    fn startup_wal_recovery_finishes_committed_artifacts_before_state_restore() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("swarm.redb");
        let object_root = directory.path().join("objects");
        let wal_root = directory.path().join("wal");
        let trace = atomic_trace(3);
        {
            let mut writer = RedbStore::open_with_artifact_paths(
                db_path.to_str().unwrap(),
                object_root.clone(),
                wal_root.clone(),
                directory.path().join("keyframes"),
                directory.path().join("keyframe-backup"),
            )
            .unwrap();
            writer.archive_retry_delays = Arc::new(vec![Duration::ZERO; 3]);
            writer
                .atomic_commit(tick_trace_writes(&trace).unwrap())
                .unwrap();
            writer.wait_for_archive(3, Duration::from_secs(1)).unwrap();
            let manifest = writer.read_tick_manifest(3).unwrap().unwrap();
            let state_artifact = writer.read_state_artifact(3).unwrap().unwrap();
            let rich_trace_blob = writer
                .read_object_if_present(&manifest.object_id)
                .unwrap()
                .unwrap();
            let state_blob = writer
                .read_object_if_present(&state_artifact.object_id)
                .unwrap()
                .unwrap();
            fs::remove_file(object_root.join(safe_object_path(&manifest.object_id).unwrap()))
                .unwrap();
            fs::remove_file(object_root.join(safe_object_path(&state_artifact.object_id).unwrap()))
                .unwrap();
            writer
                .update_manifest_upload(3, UploadStatus::Pending, None)
                .unwrap();
            writer
                .update_state_artifact_upload(3, UploadStatus::Pending, None)
                .unwrap();
            writer
                .write_archive_wal(&TickArchiveWalEntry {
                    tick: 3,
                    rich_object_id: manifest.object_id,
                    rich_trace_blob,
                    state_object_id: Some(state_artifact.object_id),
                    state_blob: Some(state_blob),
                })
                .unwrap();
        }

        let reader = RedbStore::open_with_artifact_paths(
            db_path.to_str().unwrap(),
            object_root,
            wal_root.clone(),
            directory.path().join("keyframes"),
            directory.path().join("keyframe-backup"),
        )
        .unwrap();
        assert_eq!(
            reader.read_tick_manifest(3).unwrap().unwrap().upload_status,
            UploadStatus::Complete
        );
        assert_eq!(reader.read_tick_state(3).unwrap(), Some(trace.state));
        assert_eq!(
            reader.read_rich_trace(3).unwrap().terminal_state,
            TickTerminalState::Verified
        );
        assert!(!archive_wal_path(&wal_root, 3).exists());
    }

    #[test]
    fn tick_payload_commit_writes_ten_field_record_and_keyframe_atomically() {
        let mut store = RedbStore::in_memory();
        let mut payload = payload(0, 77);
        payload.keyframe = Some(snapshot_row(0, 77));

        let point = store.commit_tick_payload(payload).unwrap();
        let record_json = serde_json::to_value(&point.record).unwrap();

        assert_eq!(record_json.as_object().unwrap().len(), 10);
        assert_eq!(point.head.snapshot_hash, point.record.snapshot_hash);
        assert_eq!(point.manifest.manifest_hash, point.record.manifest_hash);
        assert!(store.read_verified_snapshot(0).is_ok());
        assert!(store.verify_tick(0).unwrap().snapshot.is_some());
    }

    #[test]
    fn keyframe_redb_row_is_pointer_and_primary_corruption_falls_back_to_backup() {
        let mut store = RedbStore::in_memory();
        let mut payload = payload(4, 88);
        let snapshot = snapshot_row(4, 88);
        payload.keyframe = Some(snapshot.clone());

        store.commit_tick_payload(payload).unwrap();
        let pointer_bytes = store.read_key(&snapshot_state_key(4)).unwrap().unwrap();
        let pointer: KeyframePointerRow = decode(&pointer_bytes, "keyframe pointer").unwrap();

        assert!(decode::<SnapshotRow>(&pointer_bytes, "legacy snapshot").is_err());
        assert_eq!(pointer.header.magic, KEYFRAME_MAGIC);
        assert_eq!(pointer.header.format_version, KEYFRAME_FORMAT_VERSION);
        assert_eq!(
            keyframe_header_crc32c(&pointer.header).unwrap(),
            pointer.header.header_crc32c
        );
        assert!(Path::new(&pointer.primary_path).exists());
        assert!(Path::new(&pointer.backup_path).exists());
        fs::write(&pointer.primary_path, b"corrupt primary").unwrap();

        let recovered = store.read_verified_snapshot(4).unwrap();
        assert_eq!(recovered, snapshot);
    }

    #[test]
    fn keyframe_header_crc_mismatch_is_rejected_before_payload_hash() {
        let mut store = RedbStore::in_memory();
        let mut payload = payload(5, 89);
        payload.keyframe = Some(snapshot_row(5, 89));
        store.commit_tick_payload(payload).unwrap();
        let pointer_bytes = store.read_key(&snapshot_state_key(5)).unwrap().unwrap();
        let pointer: KeyframePointerRow = decode(&pointer_bytes, "keyframe pointer").unwrap();
        let mut file: KeyframeFile =
            decode(&fs::read(&pointer.primary_path).unwrap(), "keyframe file").unwrap();
        file.header.payload_blake3 = [9; 32];
        let bytes = encode(&file, "corrupt keyframe file").unwrap();
        fs::write(&pointer.primary_path, bytes).unwrap();

        let error = read_keyframe_path(Path::new(&pointer.primary_path), &pointer).unwrap_err();

        assert!(error.to_string().contains("header crc mismatch"));
    }

    #[test]
    fn pending_keyframe_pointer_is_not_recoverable_even_when_files_exist() {
        let mut store = RedbStore::in_memory();
        let mut payload = payload(8, 90);
        payload.keyframe = Some(snapshot_row(8, 90));
        store.commit_tick_payload(payload).unwrap();
        let pointer_bytes = store.read_key(&snapshot_state_key(8)).unwrap().unwrap();
        let mut pointer: KeyframePointerRow = decode(&pointer_bytes, "keyframe pointer").unwrap();
        pointer.status = UploadStatus::Pending;
        store
            .commit_tick_writes(vec![(
                snapshot_state_key(8),
                encode(&pointer, "pending keyframe pointer").unwrap(),
            )])
            .unwrap();

        let error = store.read_verified_snapshot(8).unwrap_err();

        assert!(error.to_string().contains("not complete"));
    }

    #[test]
    fn post_commit_keyframe_file_failure_keeps_tick_durable_and_marks_failed() {
        let directory = tempfile::tempdir().unwrap();
        let backup_file = directory.path().join("backup-file");
        fs::write(&backup_file, b"not a directory").unwrap();
        let mut store = RedbStore::in_memory();
        store.keyframe_backup_root = Arc::new(backup_file);
        let mut payload = payload(9, 91);
        payload.keyframe = Some(snapshot_row(9, 91));

        let point = store.commit_tick_payload(payload).unwrap();

        assert_eq!(point.tick, 9);
        assert!(store.read_tick_head(9).unwrap().is_some());
        assert!(store.read_tick_commit_record(9).unwrap().is_some());
        let pointer_bytes = store.read_key(&snapshot_state_key(9)).unwrap().unwrap();
        let pointer: KeyframePointerRow = decode(&pointer_bytes, "keyframe pointer").unwrap();
        assert_eq!(pointer.status, UploadStatus::Failed);
        assert!(
            store
                .read_verified_snapshot(9)
                .unwrap_err()
                .to_string()
                .contains("not complete")
        );
    }

    #[test]
    fn failed_redb_commit_does_not_publish_keyframe_files() {
        let mut store = RedbStore::in_memory_failing_commit();
        let mut payload = payload(6, 99);
        payload.keyframe = Some(snapshot_row(6, 99));
        let pointer = store
            .keyframe_pointer(payload.keyframe.as_ref().unwrap())
            .unwrap();

        let error = store.commit_tick_payload(payload).unwrap_err();

        assert!(matches!(error, RedbError::Commit(_)));
        assert!(!Path::new(&pointer.primary_path).exists());
        assert!(!Path::new(&pointer.backup_path).exists());
        assert!(store.read_key(&snapshot_state_key(6)).unwrap().is_none());
    }

    #[test]
    fn keyframe_pointer_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("swarm.redb");
        let object_root = directory.path().join("objects");
        let wal_root = directory.path().join("wal");
        let keyframe_root = directory.path().join("keyframes");
        let keyframe_backup_root = directory.path().join("keyframe-backup");
        let snapshot = snapshot_row(7, 101);
        {
            let mut writer = RedbStore::open_with_artifact_paths(
                db_path.to_str().unwrap(),
                object_root.clone(),
                wal_root.clone(),
                keyframe_root.clone(),
                keyframe_backup_root.clone(),
            )
            .unwrap();
            let mut payload = payload(7, 101);
            payload.keyframe = Some(snapshot.clone());
            writer.commit_tick_payload(payload).unwrap();
            assert_eq!(
                writer.wait_for_archive(7, Duration::from_secs(1)).unwrap(),
                UploadStatus::Complete
            );
        }

        let reader = RedbStore::open_with_artifact_paths(
            db_path.to_str().unwrap(),
            object_root,
            wal_root,
            keyframe_root,
            keyframe_backup_root,
        )
        .unwrap();

        assert_eq!(
            reader.recover_latest().unwrap().unwrap().snapshot.unwrap(),
            snapshot
        );
    }

    #[test]
    fn legacy_persisted_action_codec_migrates_without_live_wire_reopen() {
        let auth = CommandAuth::server_injected(CommandSource::Wasm, 7, 1, 1);
        let legacy = serde_json::json!({
            "commands": [{
                "player_id": 7,
                "tick": 1,
                "source": "Wasm",
                "auth": auth,
                "sequence": 1,
                "action": {
                    "type": "Action",
                    "action_type": "Hack",
                    "object_id": 10,
                    "target_id": 11,
                    "payload": {"power": 3}
                }
            }],
            "rejections": [],
            "fuel": TickFuelLedger::default(),
            "deploy_activation_decision": [],
            "canonical_codec_version": 1,
            "snapshot_hash": vec![0u8; 32],
            "commands_hash": vec![0u8; 32],
            "state_checksum": 123,
            "manifest_hash": vec![0u8; 32],
            "world_config_hash": vec![0u8; 32]
        });

        let record = decode_tick_commit_record(&serde_json::to_vec(&legacy).unwrap()).unwrap();

        assert!(matches!(
            record.commands[0].action,
            CommandAction::Action { ref action_type, .. } if action_type == "Hack"
        ));
        let live_wire = serde_json::json!({
            "player_id": 7,
            "tick": 1,
            "source": "Wasm",
            "auth": auth,
            "sequence": 1,
            "action": {"type": "Action", "action_type": "Hack", "object_id": 10}
        });
        assert!(serde_json::from_value::<RawCommand>(live_wire).is_err());
    }

    #[test]
    fn deploy_artifact_round_trips_verified_non_empty_bytes() {
        let store = RedbStore::in_memory();
        let module_bytes = b"\0asmdeploy-module".to_vec();
        let module_hash = content_hash(&module_bytes);
        let artifact_bytes = b"compiled-artifact".to_vec();
        let artifact_hash = content_hash(&artifact_bytes);
        let mut manifest = deploy_manifest("deploy-artifact", 1, module_hash);
        manifest.compiled_artifact_hash = artifact_hash;

        store
            .commit_deploy_artifact(
                &manifest.deploy_id,
                module_hash,
                &module_bytes,
                artifact_hash,
                Some(&artifact_bytes),
            )
            .unwrap();
        let recovered = store.read_verified_deploy_artifact(&manifest).unwrap();

        assert_eq!(recovered.wasm_bytes, module_bytes);
        assert_eq!(recovered.compiled_artifact_bytes, Some(artifact_bytes));
        assert_eq!(recovered.row.status, UploadStatus::Complete);
    }

    #[test]
    fn deploy_artifact_recovery_rejects_missing_or_empty_module_bytes() {
        let store = RedbStore::in_memory();
        let module_bytes = b"\0asmdeploy-module".to_vec();
        let module_hash = content_hash(&module_bytes);
        let artifact_hash = content_hash(b"compiled-artifact");
        let mut manifest = deploy_manifest("deploy-missing-artifact", 1, module_hash);
        manifest.compiled_artifact_hash = artifact_hash;
        let row = store
            .commit_deploy_artifact(
                &manifest.deploy_id,
                module_hash,
                &module_bytes,
                artifact_hash,
                None,
            )
            .unwrap();
        let module_path = store
            .object_store_root
            .join(safe_object_path(&row.module_object_id).unwrap());
        fs::write(&module_path, Vec::<u8>::new()).unwrap();

        let error = store.read_verified_deploy_artifact(&manifest).unwrap_err();

        assert!(error.to_string().contains("size mismatch"));
    }

    #[test]
    fn legacy_admin_command_auth_is_not_synthesized_during_persisted_decode() {
        let auth = serde_json::json!({
            "source": "Admin",
            "player_id": 7,
            "tick_submitted": 1,
            "tick_target": 1
        });
        let legacy = serde_json::json!({
            "commands": [{
                "player_id": 7,
                "tick": 1,
                "source": "Admin",
                "auth": auth,
                "sequence": 1,
                "action": {"type": "Move", "object_id": 10, "direction": "Top"}
            }],
            "rejections": [],
            "fuel": TickFuelLedger::default(),
            "deploy_activation_decision": [],
            "canonical_codec_version": 1,
            "snapshot_hash": vec![0u8; 32],
            "commands_hash": vec![0u8; 32],
            "state_checksum": 123,
            "manifest_hash": vec![0u8; 32],
            "world_config_hash": vec![0u8; 32]
        });

        let error = decode_tick_commit_record(&serde_json::to_vec(&legacy).unwrap()).unwrap_err();

        assert!(error.to_string().contains("Admin CommandAuth"));
    }

    #[test]
    fn failed_tick_payload_commit_rolls_back_every_row() {
        let mut store = RedbStore::in_memory_failing_commit();

        let error = store.commit_tick_payload(payload(2, 44)).unwrap_err();

        assert!(matches!(error, RedbError::Commit(_)));
        assert!(store.read_tick_head(2).unwrap().is_none());
        assert!(store.read_tick_manifest(2).unwrap().is_none());
        assert!(store.read_hash_chain(2).unwrap().is_none());
        assert!(store.read_key(b"/tick/2/commit_record").unwrap().is_none());
        assert!(store.read_key(b"/tick/2/state").unwrap().is_none());
    }

    #[test]
    fn deploy_activation_commit_writes_full_schema_rows_atomically() {
        let mut store = RedbStore::in_memory();
        let decision = deploy_decision("deploy-1", 7, [7; 32]);

        store
            .commit_tick_payload(payload_with_deploy(5, 55, decision.clone()))
            .unwrap();

        let manifest = store.read_deploy_manifest("deploy-1").unwrap().unwrap();
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(manifest.deploy_id, "deploy-1");
        assert_eq!(manifest.player_id, 7);
        assert_eq!(manifest.world_id, "world-alpha");
        assert_eq!(manifest.module_slot, "spawn:10:11");
        assert_eq!(manifest.drone_id, 99);
        assert_eq!(manifest.wasm_module_hash, [7; 32]);
        assert_eq!(manifest.metadata_hash, "blake3:metadata");
        assert_eq!(manifest.signed_payload_hash, "blake3:signed-payload");
        assert_eq!(manifest.compiled_artifact_hash, [8; 32]);
        assert_eq!(manifest.client_version_counter, 7);
        assert_eq!(manifest.redb_version_counter, 7);
        assert_eq!(manifest.certificate_id, "cert-1");
        assert_eq!(manifest.certificate_fingerprint, "fingerprint-1");
        assert_eq!(manifest.transport, "mcp");
        assert_eq!(manifest.accepted_at_tick, 4);
        assert_eq!(manifest.activation_tick, 5);
        assert_eq!(manifest.status, "active");
        assert!(!manifest.archive);
        assert!(manifest.failure.is_none());

        let current = store
            .read_deploy_current(7, "world-alpha", "spawn:10:11")
            .unwrap()
            .unwrap();
        assert_eq!(current.deploy_id, "deploy-1");
        assert_eq!(current.client_version_counter, 7);
        assert_eq!(current.redb_version_counter, 7);
        assert_eq!(current.activation_tick, 5);

        let index = store
            .read_deploy_activation_index(5, "deploy-1")
            .unwrap()
            .unwrap();
        assert_eq!(index.tick, 5);
        assert_eq!(index.deploy_id, "deploy-1");
        assert_eq!(index.redb_version_counter, 7);
        assert!(
            store
                .read_key(&deploy_idempotency_key(&decision))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn deploy_activation_rows_survive_restart_and_recover_commit_record() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = open_test_store(&directory, "swarm.redb");
        let decision = deploy_decision("deploy-restart", 3, [3; 32]);
        writer
            .commit_tick_payload(payload_with_deploy(3, 33, decision.clone()))
            .unwrap();
        assert_eq!(
            writer.wait_for_archive(3, Duration::from_secs(1)).unwrap(),
            UploadStatus::Complete
        );
        drop(writer);

        let reader = open_test_store(&directory, "swarm.redb");
        let recovered = reader.recover_latest().unwrap().unwrap();

        assert_eq!(recovered.tick, 3);
        assert_eq!(
            recovered.record.deploy_activation_decision,
            vec![decision.clone()]
        );
        assert_eq!(
            reader
                .read_deploy_manifest("deploy-restart")
                .unwrap()
                .unwrap()
                .redb_version_counter,
            3
        );
        assert_eq!(
            reader
                .read_deploy_current(7, "world-alpha", "spawn:10:11")
                .unwrap()
                .unwrap()
                .deploy_id,
            "deploy-restart"
        );
    }

    #[test]
    fn deploy_activation_exact_replay_is_idempotent_and_stale_counter_rejects() {
        let mut store = RedbStore::in_memory();
        let decision = deploy_decision("deploy-idem", 9, [9; 32]);
        store
            .commit_tick_payload(payload_with_deploy(9, 90, decision.clone()))
            .unwrap();

        store
            .commit_tick_payload(payload_with_deploy(10, 91, decision.clone()))
            .unwrap();

        let stale = deploy_decision("deploy-stale", 8, [4; 32]);
        let error = store
            .commit_tick_payload(payload_with_deploy(11, 92, stale))
            .unwrap_err();

        assert!(matches!(error, RedbError::Integrity(_)));
        assert!(store.read_tick_head(11).unwrap().is_none());
        assert!(
            store
                .read_deploy_manifest("deploy-stale")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .read_deploy_current(7, "world-alpha", "spawn:10:11")
                .unwrap()
                .unwrap()
                .deploy_id,
            "deploy-idem"
        );
    }

    #[test]
    fn failed_deploy_activation_commit_rolls_back_deploy_rows() {
        let mut store = RedbStore::in_memory_failing_commit();
        let decision = deploy_decision("deploy-rollback", 5, [5; 32]);

        let error = store
            .commit_tick_payload(payload_with_deploy(5, 50, decision.clone()))
            .unwrap_err();

        assert!(matches!(error, RedbError::Commit(_)));
        assert!(store.read_tick_head(5).unwrap().is_none());
        assert!(
            store
                .read_deploy_manifest("deploy-rollback")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .read_deploy_current(7, "world-alpha", "spawn:10:11")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .read_deploy_activation_index(5, "deploy-rollback")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .read_key(&deploy_idempotency_key(&decision))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn hash_chain_continuity_links_to_previous_tick() {
        let mut store = RedbStore::in_memory();
        let first = store.commit_tick_payload(payload(1, 11)).unwrap();
        let second = store.commit_tick_payload(payload(2, 22)).unwrap();

        assert_eq!(second.chain.previous_chain_hash, first.chain.chain_hash);
        assert_eq!(store.verify_tick(2).unwrap().chain, second.chain);
    }

    #[test]
    fn snapshot_read_rejects_content_hash_mismatch() {
        let mut store = RedbStore::in_memory();
        let mut row = snapshot_row(5, 55);
        row.content_hash = [9; 32];

        let error = store.write_snapshot(row).unwrap_err();

        assert!(matches!(error, RedbError::Integrity(_)));
    }

    #[test]
    fn recovery_uses_latest_verified_tick_and_snapshot() {
        let mut store = RedbStore::in_memory();
        store.commit_tick_payload(payload(1, 11)).unwrap();
        store.commit_tick_payload(payload(2, 22)).unwrap();
        store.write_snapshot(snapshot_row(2, 22)).unwrap();

        let recovered = store.recover_latest().unwrap().unwrap();

        assert_eq!(recovered.tick, 2);
        assert_eq!(recovered.head.state_checksum, 22);
        assert!(recovered.snapshot.is_some());
    }

    #[test]
    fn visible_snapshot_reads_back_from_redb_after_cache_miss() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = open_test_store(&directory, "swarm.redb");
        let key = SnapshotKey::new(1, 8);
        let cached = writer.write_visible_snapshot(visible_snapshot(8, 1, 0));
        drop(writer);

        let reader = open_test_store(&directory, "swarm.redb");

        assert_eq!(reader.get_snapshot(key), Some(cached));
    }
}
