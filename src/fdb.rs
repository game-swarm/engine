use std::collections::BTreeMap;

use bevy::prelude::Resource;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::command::Tick;
use crate::hot_cache::{CachedSnapshot, FoundationDbSnapshotStore, SnapshotKey};
use crate::mcp::VisibleWorldSnapshot;
use crate::tick::{AtomicTickStore, CommitError, TickState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundationDbError {
    Unavailable(String),
    Encode(String),
    Decode(String),
    Commit(String),
    Integrity(String),
    NotFound(String),
}

impl std::fmt::Display for FoundationDbError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "foundationdb unavailable: {message}"),
            Self::Encode(message) => write!(formatter, "foundationdb encode failed: {message}"),
            Self::Decode(message) => write!(formatter, "foundationdb decode failed: {message}"),
            Self::Commit(message) => write!(formatter, "foundationdb commit failed: {message}"),
            Self::Integrity(message) => {
                write!(formatter, "foundationdb integrity check failed: {message}")
            }
            Self::NotFound(message) => write!(formatter, "foundationdb row not found: {message}"),
        }
    }
}

impl std::error::Error for FoundationDbError {}

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
    pub canonical_codec_version: u16,
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
    pub system_manifest_hash: [u8; 32],
    pub world_config_hash: [u8; 32],
    pub mods_lock_hash: [u8; 32],
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickCommitPayload {
    pub tick: Tick,
    pub state_checksum: u64,
    pub tick_trace_blob: Vec<u8>,
    pub object_id: String,
    pub canonical_codec_version: u16,
    pub terminal_state: TickTerminalState,
    pub system_manifest_hash: [u8; 32],
    pub world_config_hash: [u8; 32],
    pub mods_lock_hash: [u8; 32],
    pub replay_critical_writes: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryPoint {
    pub tick: Tick,
    pub head: TickHeadRow,
    pub manifest: TickManifestRow,
    pub chain: TickHashChainRow,
    pub snapshot: Option<SnapshotRow>,
}

#[derive(Resource, Debug)]
pub struct FoundationDbStore {
    backend: FoundationDbBackend,
    snapshots: BTreeMap<SnapshotKey, CachedSnapshot>,
}

#[derive(Debug)]
enum FoundationDbBackend {
    Unavailable(String),
    #[cfg(test)]
    InMemory(InMemoryFoundationDb),
    #[cfg(feature = "fdb")]
    Connected(foundationdb::Database),
}

impl Default for FoundationDbStore {
    fn default() -> Self {
        Self::unavailable("not connected")
    }
}

impl FoundationDbStore {
    pub fn connect(cluster_file: Option<&str>) -> Result<Self, FoundationDbError> {
        connect_backend(cluster_file).map(|backend| Self {
            backend,
            snapshots: BTreeMap::new(),
        })
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            backend: FoundationDbBackend::Unavailable(reason.into()),
            snapshots: BTreeMap::new(),
        }
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self {
            backend: FoundationDbBackend::InMemory(InMemoryFoundationDb::default()),
            snapshots: BTreeMap::new(),
        }
    }

    #[cfg(test)]
    pub fn in_memory_failing_commit() -> Self {
        Self {
            backend: FoundationDbBackend::InMemory(InMemoryFoundationDb {
                fail_next_commit: true,
                ..Default::default()
            }),
            snapshots: BTreeMap::new(),
        }
    }

    pub fn is_available(&self) -> bool {
        match self.backend {
            FoundationDbBackend::Unavailable(_) => false,
            #[cfg(test)]
            FoundationDbBackend::InMemory(_) => true,
            #[cfg(feature = "fdb")]
            FoundationDbBackend::Connected(_) => true,
        }
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        match &self.backend {
            FoundationDbBackend::Unavailable(reason) => Some(reason),
            #[cfg(test)]
            FoundationDbBackend::InMemory(_) => None,
            #[cfg(feature = "fdb")]
            FoundationDbBackend::Connected(_) => None,
        }
    }

    pub fn write_visible_snapshot(&mut self, snapshot: VisibleWorldSnapshot) -> CachedSnapshot {
        let key = SnapshotKey::new(snapshot.player_id, snapshot.tick);
        let cached = CachedSnapshot::new(snapshot);
        self.put_snapshot(key, cached.clone());
        cached
    }

    pub fn commit_tick_writes(
        &mut self,
        writes: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<(), FoundationDbError> {
        #[cfg(not(any(feature = "fdb", test)))]
        let _ = &writes;

        match &mut self.backend {
            FoundationDbBackend::Unavailable(reason) => {
                Err(FoundationDbError::Unavailable(format!(
                    "{reason}; enable the fdb Cargo feature and install FoundationDB client libraries"
                )))
            }
            #[cfg(test)]
            FoundationDbBackend::InMemory(backend) => backend.commit(writes),
            #[cfg(feature = "fdb")]
            FoundationDbBackend::Connected(database) => commit_writes(database, writes),
        }
    }

    pub fn commit_tick_payload(
        &mut self,
        payload: TickCommitPayload,
    ) -> Result<RecoveryPoint, FoundationDbError> {
        let previous_chain_hash = self
            .read_hash_chain(payload.tick.saturating_sub(1))?
            .map(|row| row.chain_hash)
            .unwrap_or([0; 32]);
        let head_hash = tick_head_hash(
            payload.tick,
            payload.state_checksum,
            payload.canonical_codec_version,
            payload.terminal_state,
        );
        let content_hash = content_hash(&payload.tick_trace_blob);
        let head = TickHeadRow {
            tick: payload.tick,
            state_checksum: payload.state_checksum,
            canonical_codec_version: payload.canonical_codec_version,
            terminal_state: payload.terminal_state,
            tick_head_hash: head_hash,
        };
        let manifest = TickManifestRow {
            tick: payload.tick,
            object_id: payload.object_id,
            content_hash,
            blob_size: payload.tick_trace_blob.len() as u64,
            upload_status: UploadStatus::Pending,
            object_store_etag: None,
            system_manifest_hash: payload.system_manifest_hash,
            world_config_hash: payload.world_config_hash,
            mods_lock_hash: payload.mods_lock_hash,
        };
        let chain = TickHashChainRow {
            tick: payload.tick,
            previous_chain_hash,
            chain_hash: chain_hash(previous_chain_hash, head.tick_head_hash),
        };

        let mut writes = vec![
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
        writes.extend(payload.replay_critical_writes);
        self.commit_tick_writes(writes)?;

        Ok(RecoveryPoint {
            tick: payload.tick,
            head,
            manifest,
            chain,
            snapshot: None,
        })
    }

    pub fn write_snapshot(&mut self, row: SnapshotRow) -> Result<(), FoundationDbError> {
        verify_snapshot_row(&row)?;
        self.commit_tick_writes(vec![(
            snapshot_state_key(row.tick),
            encode(&row, "snapshot")?,
        )])
    }

    pub fn read_verified_snapshot(&self, tick: Tick) -> Result<SnapshotRow, FoundationDbError> {
        let row: SnapshotRow = self
            .read_json(&snapshot_state_key(tick))?
            .ok_or_else(|| FoundationDbError::NotFound(format!("snapshot tick {tick}")))?;
        verify_snapshot_row(&row)?;
        Ok(row)
    }

    pub fn verify_tick(&self, tick: Tick) -> Result<RecoveryPoint, FoundationDbError> {
        let head = self
            .read_tick_head(tick)?
            .ok_or_else(|| FoundationDbError::NotFound(format!("tick_head {tick}")))?;
        let manifest = self
            .read_tick_manifest(tick)?
            .ok_or_else(|| FoundationDbError::NotFound(format!("tick_manifest {tick}")))?;
        let chain = self
            .read_hash_chain(tick)?
            .ok_or_else(|| FoundationDbError::NotFound(format!("tick_hash_chain {tick}")))?;

        if head.tick != tick || manifest.tick != tick || chain.tick != tick {
            return Err(FoundationDbError::Integrity(format!(
                "tick row mismatch for {tick}"
            )));
        }
        let expected_head_hash = tick_head_hash(
            head.tick,
            head.state_checksum,
            head.canonical_codec_version,
            head.terminal_state,
        );
        if head.tick_head_hash != expected_head_hash {
            return Err(FoundationDbError::Integrity(format!(
                "tick_head hash mismatch at tick {tick}"
            )));
        }
        let expected_previous = self
            .read_hash_chain(tick.saturating_sub(1))?
            .map(|row| row.chain_hash)
            .unwrap_or([0; 32]);
        if chain.previous_chain_hash != expected_previous {
            return Err(FoundationDbError::Integrity(format!(
                "hash chain previous mismatch at tick {tick}"
            )));
        }
        let expected_chain_hash = chain_hash(chain.previous_chain_hash, head.tick_head_hash);
        if chain.chain_hash != expected_chain_hash {
            return Err(FoundationDbError::Integrity(format!(
                "hash chain mismatch at tick {tick}"
            )));
        }

        Ok(RecoveryPoint {
            tick,
            head,
            manifest,
            chain,
            snapshot: self.read_verified_snapshot(tick).ok(),
        })
    }

    pub fn recover_latest(&self) -> Result<Option<RecoveryPoint>, FoundationDbError> {
        let mut latest = None;
        for tick in self.committed_ticks()? {
            let mut point = self.verify_tick(tick)?;
            if let Ok(snapshot) = self.read_verified_snapshot(tick) {
                if snapshot.state_checksum != point.head.state_checksum {
                    return Err(FoundationDbError::Integrity(format!(
                        "snapshot checksum does not match tick_head at tick {tick}"
                    )));
                }
                point.snapshot = Some(snapshot);
            }
            latest = Some(point);
        }
        Ok(latest)
    }

    pub fn read_tick_head(&self, tick: Tick) -> Result<Option<TickHeadRow>, FoundationDbError> {
        self.read_json(&tick_head_key(tick))
    }

    pub fn read_tick_manifest(
        &self,
        tick: Tick,
    ) -> Result<Option<TickManifestRow>, FoundationDbError> {
        self.read_json(&tick_manifest_key(tick))
    }

    pub fn read_hash_chain(
        &self,
        tick: Tick,
    ) -> Result<Option<TickHashChainRow>, FoundationDbError> {
        self.read_json(&tick_hash_chain_key(tick))
    }

    fn read_json<T: DeserializeOwned>(&self, key: &[u8]) -> Result<Option<T>, FoundationDbError> {
        self.read_key(key)?
            .map(|value| decode(&value, std::str::from_utf8(key).unwrap_or("key")))
            .transpose()
    }

    fn read_key(&self, key: &[u8]) -> Result<Option<Vec<u8>>, FoundationDbError> {
        match &self.backend {
            FoundationDbBackend::Unavailable(reason) => Err(FoundationDbError::Unavailable(
                format!("{reason}; cannot read key"),
            )),
            #[cfg(test)]
            FoundationDbBackend::InMemory(backend) => Ok(backend.data.get(key).cloned()),
            #[cfg(feature = "fdb")]
            FoundationDbBackend::Connected(database) => read_key(database, key),
        }
    }

    fn committed_ticks(&self) -> Result<Vec<Tick>, FoundationDbError> {
        match &self.backend {
            FoundationDbBackend::Unavailable(reason) => Err(FoundationDbError::Unavailable(
                format!("{reason}; cannot scan ticks"),
            )),
            #[cfg(test)]
            FoundationDbBackend::InMemory(backend) => {
                let mut ticks = backend
                    .data
                    .keys()
                    .filter_map(|key| parse_tick_head_key(key))
                    .collect::<Vec<_>>();
                ticks.sort_unstable();
                Ok(ticks)
            }
            #[cfg(feature = "fdb")]
            FoundationDbBackend::Connected(_) => Err(FoundationDbError::Unavailable(
                "tick scan requires an index in production FDB backend".to_string(),
            )),
        }
    }
}

impl FoundationDbSnapshotStore for FoundationDbStore {
    fn get_snapshot(&self, key: SnapshotKey) -> Option<CachedSnapshot> {
        self.snapshots.get(&key).cloned()
    }

    fn put_snapshot(&mut self, key: SnapshotKey, snapshot: CachedSnapshot) {
        self.snapshots.insert(key, snapshot.clone());
        if self.is_available() {
            let key_bytes = visible_snapshot_key(key);
            match serde_json::to_vec(&snapshot) {
                Ok(value) => {
                    if let Err(error) = self.commit_tick_writes(vec![(key_bytes, value)]) {
                        eprintln!("foundationdb snapshot write failed key={key:?} error={error}");
                    }
                }
                Err(error) => {
                    eprintln!("foundationdb snapshot encode failed key={key:?} error={error}")
                }
            }
        }
    }
}

impl AtomicTickStore for FoundationDbStore {
    fn atomic_commit(&mut self, writes: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), CommitError> {
        self.commit_tick_writes(writes)
            .map_err(|error| CommitError::Failed(error.to_string()))
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct InMemoryFoundationDb {
    data: BTreeMap<Vec<u8>, Vec<u8>>,
    fail_next_commit: bool,
}

#[cfg(test)]
impl InMemoryFoundationDb {
    fn commit(&mut self, writes: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), FoundationDbError> {
        if self.fail_next_commit {
            self.fail_next_commit = false;
            return Err(FoundationDbError::Commit(
                "in-memory commit failed".to_string(),
            ));
        }
        let mut next = self.data.clone();
        for (key, value) in writes {
            next.insert(key, value);
        }
        self.data = next;
        Ok(())
    }
}

fn encode<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>, FoundationDbError> {
    serde_json::to_vec(value)
        .map_err(|error| FoundationDbError::Encode(format!("{label}: {error}")))
}

fn decode<T: DeserializeOwned>(value: &[u8], label: &str) -> Result<T, FoundationDbError> {
    serde_json::from_slice(value)
        .map_err(|error| FoundationDbError::Decode(format!("{label}: {error}")))
}

fn visible_snapshot_key(key: SnapshotKey) -> Vec<u8> {
    format!("/snapshot/{}/{}", key.player_id, key.tick).into_bytes()
}

fn tick_head_key(tick: Tick) -> Vec<u8> {
    format!("/tick/{tick}/tick_head").into_bytes()
}

fn tick_manifest_key(tick: Tick) -> Vec<u8> {
    format!("/tick/{tick}/tick_manifest").into_bytes()
}

fn tick_hash_chain_key(tick: Tick) -> Vec<u8> {
    format!("/tick/{tick}/tick_hash_chain").into_bytes()
}

fn snapshot_state_key(tick: Tick) -> Vec<u8> {
    format!("/snapshot_state/{tick}").into_bytes()
}

fn parse_tick_head_key(key: &[u8]) -> Option<Tick> {
    let text = std::str::from_utf8(key).ok()?;
    let tick = text.strip_prefix("/tick/")?.strip_suffix("/tick_head")?;
    tick.parse().ok()
}

fn content_hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

fn tick_head_hash(
    tick: Tick,
    state_checksum: u64,
    canonical_codec_version: u16,
    terminal_state: TickTerminalState,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&tick.to_le_bytes());
    hasher.update(&state_checksum.to_le_bytes());
    hasher.update(&canonical_codec_version.to_le_bytes());
    hasher.update(&[terminal_state as u8]);
    *hasher.finalize().as_bytes()
}

fn chain_hash(previous: [u8; 32], tick_head_hash: [u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&previous);
    hasher.update(&tick_head_hash);
    *hasher.finalize().as_bytes()
}

fn snapshot_content_hash(state: &TickState) -> Result<[u8; 32], FoundationDbError> {
    let value = serde_json::to_value(state)
        .map_err(|error| FoundationDbError::Encode(format!("snapshot state: {error}")))?;
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| FoundationDbError::Encode(format!("snapshot state: {error}")))?;
    Ok(content_hash(&bytes))
}

fn verify_snapshot_row(row: &SnapshotRow) -> Result<(), FoundationDbError> {
    let expected = snapshot_content_hash(&row.state)?;
    if row.content_hash != expected {
        return Err(FoundationDbError::Integrity(format!(
            "snapshot content hash mismatch at tick {}",
            row.tick
        )));
    }
    Ok(())
}

#[cfg(not(feature = "fdb"))]
fn connect_backend(_cluster_file: Option<&str>) -> Result<FoundationDbBackend, FoundationDbError> {
    Err(FoundationDbError::Unavailable(
        "compiled without the fdb Cargo feature".to_string(),
    ))
}

#[cfg(feature = "fdb")]
fn connect_backend(cluster_file: Option<&str>) -> Result<FoundationDbBackend, FoundationDbError> {
    boot_network_once();
    let database = match cluster_file {
        Some(path) => foundationdb::Database::from_path(path),
        None => foundationdb::Database::default(),
    }
    .map_err(|error| FoundationDbError::Unavailable(error.to_string()))?;
    Ok(FoundationDbBackend::Connected(database))
}

#[cfg(feature = "fdb")]
fn boot_network_once() {
    static BOOT: std::sync::Once = std::sync::Once::new();
    BOOT.call_once(|| {
        let _network = Box::leak(Box::new(unsafe { foundationdb::boot() }));
    });
}

#[cfg(feature = "fdb")]
fn commit_writes(
    database: &foundationdb::Database,
    writes: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<(), FoundationDbError> {
    futures::executor::block_on(async {
        let transaction = database
            .create_trx()
            .map_err(|error| FoundationDbError::Commit(error.to_string()))?;
        for (key, value) in writes {
            transaction.set(&key, &value);
        }
        transaction
            .commit()
            .await
            .map_err(|error| FoundationDbError::Commit(error.to_string()))?;
        Ok(())
    })
}

#[cfg(feature = "fdb")]
fn read_key(
    database: &foundationdb::Database,
    key: &[u8],
) -> Result<Option<Vec<u8>>, FoundationDbError> {
    futures::executor::block_on(async {
        let transaction = database
            .create_trx()
            .map_err(|error| FoundationDbError::Commit(error.to_string()))?;
        transaction
            .get(key, false)
            .await
            .map(|value| value.map(|bytes| bytes.to_vec()))
            .map_err(|error| FoundationDbError::Commit(error.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::PlayerId;
    use crate::tick::WorldSnapshot;
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
        TickCommitPayload {
            tick,
            state_checksum: checksum,
            tick_trace_blob: format!("trace-{tick}").into_bytes(),
            object_id: format!("tick-trace/{tick}.zst"),
            canonical_codec_version: 1,
            terminal_state: TickTerminalState::Verified,
            system_manifest_hash: [1; 32],
            world_config_hash: [2; 32],
            mods_lock_hash: [3; 32],
            replay_critical_writes: vec![(
                format!("/tick/{tick}/state").into_bytes(),
                b"state".to_vec(),
            )],
        }
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

    #[test]
    fn unavailable_connector_reports_runtime_requirement() {
        let error = FoundationDbStore::connect(Some("/missing/fdb.cluster")).unwrap_err();

        assert!(error.to_string().contains("foundationdb unavailable"));
    }

    #[test]
    fn degraded_store_keeps_visible_snapshots_available_in_process() {
        let mut store = FoundationDbStore::unavailable("test degraded mode");
        let key = SnapshotKey::new(1, 7);
        let cached = store.write_visible_snapshot(visible_snapshot(7, 1, 0));

        assert_eq!(store.get_snapshot(key), Some(cached));
        assert!(!store.is_available());
    }

    #[test]
    fn degraded_atomic_commit_reports_unavailable_without_partial_success() {
        let mut store = FoundationDbStore::unavailable("test degraded mode");

        let error = store
            .atomic_commit(vec![(b"/tick/1/state".to_vec(), b"{}".to_vec())])
            .unwrap_err();

        let CommitError::Failed(message) = error;
        assert!(message.contains("foundationdb unavailable"));
    }

    #[test]
    fn tick_payload_commit_writes_head_manifest_and_hash_chain_atomically() {
        let mut store = FoundationDbStore::in_memory();

        let point = store.commit_tick_payload(payload(1, 42)).unwrap();

        assert_eq!(point.head.state_checksum, 42);
        assert_eq!(point.manifest.upload_status, UploadStatus::Pending);
        assert_eq!(point.manifest.blob_size, b"trace-1".len() as u64);
        assert!(store.read_key(b"/tick/1/state").unwrap().is_some());
        assert_eq!(store.verify_tick(1).unwrap().chain, point.chain);
    }

    #[test]
    fn failed_tick_payload_commit_rolls_back_every_row() {
        let mut store = FoundationDbStore::in_memory_failing_commit();

        let error = store.commit_tick_payload(payload(2, 44)).unwrap_err();

        assert!(matches!(error, FoundationDbError::Commit(_)));
        assert!(store.read_tick_head(2).unwrap().is_none());
        assert!(store.read_tick_manifest(2).unwrap().is_none());
        assert!(store.read_hash_chain(2).unwrap().is_none());
        assert!(store.read_key(b"/tick/2/state").unwrap().is_none());
    }

    #[test]
    fn hash_chain_continuity_links_to_previous_tick() {
        let mut store = FoundationDbStore::in_memory();
        let first = store.commit_tick_payload(payload(1, 11)).unwrap();
        let second = store.commit_tick_payload(payload(2, 22)).unwrap();

        assert_eq!(second.chain.previous_chain_hash, first.chain.chain_hash);
        assert_eq!(store.verify_tick(2).unwrap().chain, second.chain);
    }

    #[test]
    fn snapshot_read_rejects_content_hash_mismatch() {
        let mut store = FoundationDbStore::in_memory();
        let mut row = snapshot_row(5, 55);
        row.content_hash = [9; 32];

        let error = store.write_snapshot(row).unwrap_err();

        assert!(matches!(error, FoundationDbError::Integrity(_)));
    }

    #[test]
    fn recovery_uses_latest_verified_tick_and_snapshot() {
        let mut store = FoundationDbStore::in_memory();
        store.commit_tick_payload(payload(1, 11)).unwrap();
        store.commit_tick_payload(payload(2, 22)).unwrap();
        store.write_snapshot(snapshot_row(2, 22)).unwrap();

        let recovered = store.recover_latest().unwrap().unwrap();

        assert_eq!(recovered.tick, 2);
        assert_eq!(recovered.head.state_checksum, 22);
        assert!(recovered.snapshot.is_some());
    }
}
