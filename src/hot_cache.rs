use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::command::Tick;
use crate::components::PlayerId;
use crate::mcp::VisibleWorldSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SnapshotKey {
    pub player_id: PlayerId,
    pub tick: Tick,
}

impl SnapshotKey {
    pub fn new(player_id: PlayerId, tick: Tick) -> Self {
        Self { player_id, tick }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSnapshot {
    pub snapshot: VisibleWorldSnapshot,
    pub fingerprint: [u8; 32],
}

impl CachedSnapshot {
    pub fn new(snapshot: VisibleWorldSnapshot) -> Self {
        let fingerprint = snapshot_fingerprint(&snapshot);
        Self {
            snapshot,
            fingerprint,
        }
    }

    fn matches_authoritative(&self, authoritative: &CachedSnapshot) -> bool {
        self.fingerprint == authoritative.fingerprint && self.snapshot == authoritative.snapshot
    }
}

pub trait RedbSnapshotStore {
    fn get_snapshot(&self, key: SnapshotKey) -> Option<CachedSnapshot>;
    fn put_snapshot(&mut self, key: SnapshotKey, snapshot: CachedSnapshot);
}

pub trait SnapshotCache {
    fn get_snapshot(&mut self, key: SnapshotKey) -> Option<CachedSnapshot>;
    fn put_snapshot(&mut self, key: SnapshotKey, snapshot: CachedSnapshot);
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub refreshes: u64,
}

#[derive(bevy::prelude::Resource, Debug, Clone, Default)]
pub struct InMemorySnapshotCache {
    snapshots: BTreeMap<SnapshotKey, CachedSnapshot>,
    stats: SnapshotCacheStats,
}

impl InMemorySnapshotCache {
    pub fn in_process() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> SnapshotCacheStats {
        self.stats
    }
}

impl SnapshotCache for InMemorySnapshotCache {
    fn get_snapshot(&mut self, key: SnapshotKey) -> Option<CachedSnapshot> {
        let snapshot = self.snapshots.get(&key).cloned();
        if snapshot.is_some() {
            self.stats.hits += 1;
        } else {
            self.stats.misses += 1;
        }
        snapshot
    }

    fn put_snapshot(&mut self, key: SnapshotKey, snapshot: CachedSnapshot) {
        self.snapshots.insert(key, snapshot);
        self.stats.refreshes += 1;
    }
}

pub fn read_through_snapshot_cache<C, S>(
    cache: &mut C,
    key: SnapshotKey,
    store: &S,
) -> Option<VisibleWorldSnapshot>
where
    C: SnapshotCache,
    S: RedbSnapshotStore,
{
    let authoritative = store.get_snapshot(key)?;
    if let Some(cached) = cache.get_snapshot(key)
        && cached.matches_authoritative(&authoritative)
    {
        return Some(cached.snapshot);
    }

    cache.put_snapshot(key, authoritative.clone());
    Some(authoritative.snapshot)
}

fn snapshot_fingerprint(snapshot: &VisibleWorldSnapshot) -> [u8; 32] {
    let bytes = serde_json::to_vec(snapshot).expect("visible snapshots must serialize");
    *blake3::hash(&bytes).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hot_cache::InMemorySnapshotCache;
    use crate::mcp::VisibleWorldSnapshot;
    use crate::realtime::{RealtimeDelta, RealtimeEnvelope};
    use crate::redb_store::RedbStore;

    fn snapshot(tick: Tick, player_id: PlayerId, room_id: u32) -> VisibleWorldSnapshot {
        VisibleWorldSnapshot {
            tick,
            player_id,
            room_id,
            state_checksum: 0,
            recovery_envelope: RealtimeEnvelope {
                schema: "swarm.realtime.v1".to_string(),
                payload: RealtimeDelta {
                    tick,
                    last_tick: tick.saturating_sub(1),
                    player_id,
                    full_snapshot: true,
                    changed_entities: Vec::new(),
                    removed_entities: Vec::new(),
                    state_checksum: 0,
                },
            },
            visibility_radius: 5,
            visible_tiles: Vec::new(),
            entities: Vec::new(),
            local_storage: Default::default(),
            global_storage: Default::default(),
            pending_global_transfers: Vec::new(),
        }
    }

    #[test]
    fn snapshot_cache_hit_returns_cached_snapshot_when_consistent() {
        let key = SnapshotKey::new(1, 7);
        let authoritative = CachedSnapshot::new(snapshot(7, 1, 0));
        let mut store = RedbStore::unavailable("test degraded mode");
        store.put_snapshot(key, authoritative.clone());
        let mut cache = InMemorySnapshotCache::in_process();
        cache.put_snapshot(key, authoritative.clone());

        let result = read_through_snapshot_cache(&mut cache, key, &store).unwrap();

        assert_eq!(result, authoritative.snapshot);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 0);
        assert_eq!(cache.stats().refreshes, 1);
    }

    #[test]
    fn snapshot_cache_miss_reads_redb_and_backfills_cache() {
        let key = SnapshotKey::new(1, 7);
        let authoritative = CachedSnapshot::new(snapshot(7, 1, 0));
        let mut store = RedbStore::unavailable("test degraded mode");
        store.put_snapshot(key, authoritative.clone());
        let mut cache = InMemorySnapshotCache::in_process();

        let result = read_through_snapshot_cache(&mut cache, key, &store).unwrap();

        assert_eq!(result, authoritative.snapshot);
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().refreshes, 1);
        assert_eq!(cache.get_snapshot(key), Some(authoritative));
    }

    #[test]
    fn snapshot_cache_stale_or_inconsistent_entry_is_replaced_by_redb() {
        let key = SnapshotKey::new(1, 7);
        let authoritative = CachedSnapshot::new(snapshot(7, 1, 0));
        let stale = CachedSnapshot::new(snapshot(7, 1, 99));
        let mut store = RedbStore::unavailable("test degraded mode");
        store.put_snapshot(key, authoritative.clone());
        let mut cache = InMemorySnapshotCache::in_process();
        cache.put_snapshot(key, stale);

        let result = read_through_snapshot_cache(&mut cache, key, &store).unwrap();

        assert_eq!(result, authoritative.snapshot);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().refreshes, 2);
        assert_eq!(cache.get_snapshot(key), Some(authoritative));
    }
}
