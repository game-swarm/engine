use std::collections::BTreeMap;

use bevy::prelude::Resource as BevyResource;
use serde::{Deserialize, Serialize};
use swarm_engine_api::ids::PlayerId;

use crate::command::{CommandIntent, CommandSource, RawCommand, Tick, source_gate};
use crate::realtime::{NatsPublisher, RealtimeError};
use crate::world::{SwarmWorld, create_world_with_shard_config};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ShardId(pub u32);

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardConfig {
    pub shard_count: u32,
    pub shard_id: ShardId,
}

impl ShardConfig {
    pub fn new(shard_count: u32, shard_id: ShardId) -> Result<Self, ShardConfigError> {
        if shard_count == 0 {
            return Err(ShardConfigError::ZeroShardCount);
        }
        if shard_id.0 >= shard_count {
            return Err(ShardConfigError::ShardIdOutOfRange {
                shard_id,
                shard_count,
            });
        }
        Ok(Self {
            shard_count,
            shard_id,
        })
    }

    pub fn single() -> Self {
        Self {
            shard_count: 1,
            shard_id: ShardId(0),
        }
    }

    pub fn shard_for_player(&self, player_id: PlayerId) -> ShardId {
        shard_for_player(player_id, self.shard_count)
    }

    pub fn owns_player(&self, player_id: PlayerId) -> bool {
        self.shard_for_player(player_id) == self.shard_id
    }
}

impl Default for ShardConfig {
    fn default() -> Self {
        Self::single()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardConfigError {
    ZeroShardCount,
    ShardIdOutOfRange { shard_id: ShardId, shard_count: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardKey {
    pub player_id: PlayerId,
    pub shard_id: ShardId,
}

impl ShardKey {
    pub fn for_player(player_id: PlayerId, shard_count: u32) -> Self {
        Self {
            player_id,
            shard_id: shard_for_player(player_id, shard_count),
        }
    }
}

pub fn shard_for_player(player_id: PlayerId, shard_count: u32) -> ShardId {
    assert!(shard_count > 0, "shard_count must be greater than zero");
    let digest = blake3::hash(&player_id.to_le_bytes());
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest.as_bytes()[..8]);
    ShardId((u64::from_le_bytes(bytes) % shard_count as u64) as u32)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardEndpoint {
    pub shard_id: ShardId,
    pub command_subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardDiscovery {
    endpoints: BTreeMap<ShardId, ShardEndpoint>,
}

impl ShardDiscovery {
    pub fn for_count(shard_count: u32) -> Result<Self, ShardConfigError> {
        if shard_count == 0 {
            return Err(ShardConfigError::ZeroShardCount);
        }
        let endpoints = (0..shard_count)
            .map(|id| {
                let shard_id = ShardId(id);
                (
                    shard_id,
                    ShardEndpoint {
                        shard_id,
                        command_subject: shard_command_subject(shard_id),
                    },
                )
            })
            .collect();
        Ok(Self { endpoints })
    }

    pub fn endpoint(&self, shard_id: ShardId) -> Option<&ShardEndpoint> {
        self.endpoints.get(&shard_id)
    }

    pub fn command_subject(&self, shard_id: ShardId) -> Option<&str> {
        self.endpoint(shard_id)
            .map(|endpoint| endpoint.command_subject.as_str())
    }

    pub fn endpoints(&self) -> impl Iterator<Item = &ShardEndpoint> {
        self.endpoints.values()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardRegistry {
    pub local: ShardConfig,
    pub discovery: ShardDiscovery,
}

impl ShardRegistry {
    pub fn new(local: ShardConfig) -> Result<Self, ShardConfigError> {
        Ok(Self {
            discovery: ShardDiscovery::for_count(local.shard_count)?,
            local,
        })
    }

    pub fn route_player(&self, player_id: PlayerId) -> ShardKey {
        ShardKey {
            player_id,
            shard_id: self.local.shard_for_player(player_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardEnvelope {
    pub source_shard: ShardId,
    pub target_shard: ShardId,
    pub command: RawCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardRouteError {
    UnknownShard(ShardId),
    Serialize(String),
    Publish(RealtimeError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutedCommand {
    Local(Box<RawCommand>),
    Published { subject: String },
}

#[derive(Debug)]
pub struct ShardRoutingCapability {
    _private: (),
}

#[cfg(test)]
pub(crate) const fn shard_routing_capability() -> ShardRoutingCapability {
    ShardRoutingCapability { _private: () }
}

pub struct ShardRouter<P> {
    registry: ShardRegistry,
    publisher: P,
}

impl<P> ShardRouter<P>
where
    P: NatsPublisher,
{
    pub fn new(registry: ShardRegistry, publisher: P) -> Self {
        Self {
            registry,
            publisher,
        }
    }

    pub fn registry(&self) -> &ShardRegistry {
        &self.registry
    }

    pub fn into_publisher(self) -> P {
        self.publisher
    }

    pub fn route_raw_command(
        &mut self,
        _capability: &ShardRoutingCapability,
        raw: RawCommand,
    ) -> Result<RoutedCommand, ShardRouteError> {
        let target_shard = self.registry.local.shard_for_player(raw.player_id);
        if target_shard == self.registry.local.shard_id {
            return Ok(RoutedCommand::Local(Box::new(raw)));
        }

        let subject = self
            .registry
            .discovery
            .command_subject(target_shard)
            .ok_or(ShardRouteError::UnknownShard(target_shard))?
            .to_string();
        let envelope = ShardEnvelope {
            source_shard: self.registry.local.shard_id,
            target_shard,
            command: raw,
        };
        let payload = serde_json::to_vec(&envelope)
            .map_err(|error| ShardRouteError::Serialize(error.to_string()))?;
        self.publisher
            .publish(&subject, payload)
            .map_err(ShardRouteError::Publish)?;
        Ok(RoutedCommand::Published { subject })
    }

    pub fn route_intent(
        &mut self,
        capability: &ShardRoutingCapability,
        player_id: PlayerId,
        tick: Tick,
        source: CommandSource,
        intent: CommandIntent,
    ) -> Result<RoutedCommand, ShardRouteError> {
        let raw = source_gate(player_id, tick, source, intent).map_err(|error| {
            ShardRouteError::Serialize(format!("source gate rejected: {error:?}"))
        })?;
        self.route_raw_command(capability, raw)
    }
}

pub fn shard_command_subject(shard_id: ShardId) -> String {
    format!("swarm.shard.{}.commands", shard_id.0)
}

pub struct MultiShardWorld {
    shard_count: u32,
    shards: BTreeMap<ShardId, SwarmWorld>,
}

impl MultiShardWorld {
    pub fn new(shard_count: u32) -> Result<Self, ShardConfigError> {
        if shard_count == 0 {
            return Err(ShardConfigError::ZeroShardCount);
        }
        let mut shards = BTreeMap::new();
        for id in 0..shard_count {
            let shard_id = ShardId(id);
            let config = ShardConfig::new(shard_count, shard_id)?;
            shards.insert(shard_id, create_world_with_shard_config(config));
        }
        Ok(Self {
            shard_count,
            shards,
        })
    }

    pub fn shard_count(&self) -> u32 {
        self.shard_count
    }

    pub fn shard_for_player(&self, player_id: PlayerId) -> ShardId {
        shard_for_player(player_id, self.shard_count)
    }

    pub fn shard(&self, shard_id: ShardId) -> Option<&SwarmWorld> {
        self.shards.get(&shard_id)
    }

    pub fn shard_mut(&mut self, shard_id: ShardId) -> Option<&mut SwarmWorld> {
        self.shards.get_mut(&shard_id)
    }

    pub fn shards(&self) -> impl Iterator<Item = (ShardId, &SwarmWorld)> {
        self.shards.iter().map(|(id, world)| (*id, world))
    }

    pub fn run_tick(&mut self) {
        for world in self.shards.values_mut() {
            world.run_tick();
        }
    }

    pub fn state_checksum(&mut self) -> u64 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.shard_count.to_le_bytes());
        for (shard_id, world) in &mut self.shards {
            hasher.update(&shard_id.0.to_le_bytes());
            hasher.update(&world.state_checksum().to_le_bytes());
        }
        let digest = hasher.finalize();
        u64::from_le_bytes(
            digest.as_bytes()[..8]
                .try_into()
                .expect("BLAKE3 digest has 32 bytes"),
        )
    }
}
