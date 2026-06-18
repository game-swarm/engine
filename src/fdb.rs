use std::collections::BTreeMap;

use bevy::prelude::Resource;

use crate::hot_cache::{CachedSnapshot, FoundationDbSnapshotStore, SnapshotKey};
use crate::mcp::VisibleWorldSnapshot;
use crate::tick::{AtomicTickStore, CommitError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundationDbError {
    Unavailable(String),
    Encode(String),
    Decode(String),
    Commit(String),
}

impl std::fmt::Display for FoundationDbError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "foundationdb unavailable: {message}"),
            Self::Encode(message) => write!(formatter, "foundationdb encode failed: {message}"),
            Self::Decode(message) => write!(formatter, "foundationdb decode failed: {message}"),
            Self::Commit(message) => write!(formatter, "foundationdb commit failed: {message}"),
        }
    }
}

impl std::error::Error for FoundationDbError {}

#[derive(Resource, Debug)]
pub struct FoundationDbStore {
    backend: FoundationDbBackend,
    snapshots: BTreeMap<SnapshotKey, CachedSnapshot>,
}

#[derive(Debug)]
enum FoundationDbBackend {
    Unavailable(String),
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

    pub fn is_available(&self) -> bool {
        match self.backend {
            FoundationDbBackend::Unavailable(_) => false,
            #[cfg(feature = "fdb")]
            FoundationDbBackend::Connected(_) => true,
        }
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        match &self.backend {
            FoundationDbBackend::Unavailable(reason) => Some(reason),
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
        #[cfg(not(feature = "fdb"))]
        let _ = &writes;

        match &self.backend {
            FoundationDbBackend::Unavailable(reason) => {
                Err(FoundationDbError::Unavailable(format!(
                    "{reason}; enable the fdb Cargo feature and install FoundationDB client libraries"
                )))
            }
            #[cfg(feature = "fdb")]
            FoundationDbBackend::Connected(database) => commit_writes(database, writes),
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
            let key_bytes = snapshot_key(key);
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

fn snapshot_key(key: SnapshotKey) -> Vec<u8> {
    format!("/snapshot/{}/{}", key.player_id, key.tick).into_bytes()
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
        // Runtime requirement: libfdb_c must be installed and loadable. The leaked
        // network handle keeps FoundationDB's single process-wide network alive.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Tick;
    use crate::components::PlayerId;

    fn snapshot(tick: Tick, player_id: PlayerId, room_id: u32) -> VisibleWorldSnapshot {
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

    #[test]
    fn unavailable_connector_reports_runtime_requirement() {
        let error = FoundationDbStore::connect(Some("/missing/fdb.cluster")).unwrap_err();

        assert!(error.to_string().contains("foundationdb unavailable"));
    }

    #[test]
    fn degraded_store_keeps_visible_snapshots_available_in_process() {
        let mut store = FoundationDbStore::unavailable("test degraded mode");
        let key = SnapshotKey::new(1, 7);
        let cached = store.write_visible_snapshot(snapshot(7, 1, 0));

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
}
