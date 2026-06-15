use std::collections::BTreeMap;

use bevy::prelude::Resource;
use redis::Commands;

use crate::hot_cache::{CachedSnapshot, DragonflySnapshotCache, DragonflyStats, SnapshotKey};
use crate::mcp::VisibleWorldSnapshot;

#[derive(Resource, Debug)]
pub struct DragonflyCache {
    backend: DragonflyBackend,
    fallback: BTreeMap<SnapshotKey, CachedSnapshot>,
    stats: DragonflyStats,
}

#[derive(Debug)]
enum DragonflyBackend {
    Redis { client: redis::Client, url: String },
    Unavailable(String),
}

impl Default for DragonflyCache {
    fn default() -> Self {
        std::env::var("DRAGONFLY_URL")
            .or_else(|_| std::env::var("REDIS_URL"))
            .ok()
            .and_then(|url| Self::connect(&url).ok())
            .unwrap_or_else(Self::in_process)
    }
}

impl DragonflyCache {
    pub fn connect(url: &str) -> Result<Self, redis::RedisError> {
        Ok(Self {
            backend: DragonflyBackend::Redis {
                client: redis::Client::open(url)?,
                url: url.to_string(),
            },
            fallback: BTreeMap::new(),
            stats: DragonflyStats::default(),
        })
    }

    pub fn in_process() -> Self {
        Self {
            backend: DragonflyBackend::Unavailable("not connected".to_string()),
            fallback: BTreeMap::new(),
            stats: DragonflyStats::default(),
        }
    }

    pub fn stats(&self) -> DragonflyStats {
        self.stats
    }

    pub fn is_available(&self) -> bool {
        matches!(self.backend, DragonflyBackend::Redis { .. })
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        match &self.backend {
            DragonflyBackend::Redis { .. } => None,
            DragonflyBackend::Unavailable(reason) => Some(reason),
        }
    }

    pub fn put_visible_snapshot(&mut self, snapshot: VisibleWorldSnapshot) {
        let key = SnapshotKey::new(snapshot.player_id, snapshot.tick);
        self.put_snapshot(key, CachedSnapshot::new(snapshot));
    }

    fn redis_get(&mut self, key: SnapshotKey) -> Option<CachedSnapshot> {
        let redis_key = snapshot_key(key);
        let result = match &self.backend {
            DragonflyBackend::Redis { client, .. } => client
                .get_connection()
                .and_then(|mut connection| connection.get::<_, Option<Vec<u8>>>(&redis_key)),
            DragonflyBackend::Unavailable(_) => return None,
        };

        match result {
            Ok(Some(bytes)) => match serde_json::from_slice(&bytes) {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    eprintln!("dragonfly snapshot decode failed key={redis_key} error={error}");
                    None
                }
            },
            Ok(None) => None,
            Err(error) => {
                self.mark_unavailable(error.to_string());
                None
            }
        }
    }

    fn redis_put(&mut self, key: SnapshotKey, snapshot: &CachedSnapshot) -> bool {
        let redis_key = snapshot_key(key);
        let value = match serde_json::to_vec(snapshot) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("dragonfly snapshot encode failed key={redis_key} error={error}");
                return false;
            }
        };
        let result = match &self.backend {
            DragonflyBackend::Redis { client, .. } => client
                .get_connection()
                .and_then(|mut connection| connection.set::<_, _, ()>(&redis_key, value)),
            DragonflyBackend::Unavailable(_) => return false,
        };

        match result {
            Ok(()) => true,
            Err(error) => {
                self.mark_unavailable(error.to_string());
                false
            }
        }
    }

    fn mark_unavailable(&mut self, reason: String) {
        let url = match &self.backend {
            DragonflyBackend::Redis { url, .. } => Some(url.clone()),
            DragonflyBackend::Unavailable(_) => None,
        };
        let message = match url {
            Some(url) => format!("url={url} error={reason}"),
            None => reason,
        };
        eprintln!("dragonfly unavailable: {message}; using in-process fallback");
        self.backend = DragonflyBackend::Unavailable(message);
    }
}

impl DragonflySnapshotCache for DragonflyCache {
    fn get_snapshot(&mut self, key: SnapshotKey) -> Option<CachedSnapshot> {
        let snapshot = if self.is_available() {
            self.redis_get(key)
        } else {
            self.fallback.get(&key).cloned()
        };

        match snapshot {
            Some(snapshot) => {
                self.stats.hits += 1;
                Some(snapshot)
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }

    fn put_snapshot(&mut self, key: SnapshotKey, snapshot: CachedSnapshot) {
        self.stats.refreshes += 1;
        if !self.redis_put(key, &snapshot) {
            self.fallback.insert(key, snapshot);
        }
    }
}

fn snapshot_key(key: SnapshotKey) -> String {
    format!("swarm:snapshot:{}:{}", key.player_id, key.tick)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(tick: u64, player_id: u32, room_id: u32) -> VisibleWorldSnapshot {
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
            market_orders: Vec::new(),
        }
    }

    #[test]
    fn in_process_fallback_keeps_snapshots_without_dragonfly() {
        let key = SnapshotKey::new(1, 7);
        let cached = CachedSnapshot::new(snapshot(7, 1, 0));
        let mut cache = DragonflyCache::in_process();

        cache.put_snapshot(key, cached.clone());

        assert_eq!(cache.get_snapshot(key), Some(cached));
        assert!(!cache.is_available());
    }

    #[test]
    fn invalid_url_is_reported_without_network_dependency() {
        let error = DragonflyCache::connect("not a redis url").unwrap_err();

        assert!(!error.to_string().is_empty());
    }
}
