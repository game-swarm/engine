use std::collections::BTreeMap;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use swarm_engine_api::ids::{BodyPart, PlayerId, RoomId};
use swarm_engine_plugin_sdk::components::{
    BodyPartRegistry, Drone, Owner, Position, SpawningGrace, Structure,
};

use crate::command::{MAX_DRONES_PER_PLAYER, ObjectId};
use crate::components::{
    PendingEntityCreation, PendingEntityCreationEntry, PendingEntityKind, RoomTerrains,
    StableEntityIdAllocator,
};
use crate::onboarding::OnboardingEvent;
use crate::resource_ledger::{LedgerAccount, ResourceLedger, ResourceOperation};
use crate::resources::{CurrentTick, PlayerLocalStorage, ResourceCost};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSpawn {
    pub owner: PlayerId,
    pub spawn_id: ObjectId,
    pub body: Vec<BodyPart>,
    pub position: Position,
    pub cost: ResourceCost,
}

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSpawnQueue(pub Vec<PendingSpawn>);

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomDroneCounts(pub BTreeMap<(RoomId, PlayerId), u32>);

pub fn spawn_system(
    mut queue: ResMut<PendingSpawnQueue>,
    mut room_counts: ResMut<RoomDroneCounts>,
    mut pending_entities: ResMut<PendingEntityCreation>,
    mut stable_ids: ResMut<StableEntityIdAllocator>,
    terrains: Res<RoomTerrains>,
    mut structures: Query<&mut Structure>,
    drones: Query<&Position, With<Drone>>,
    mut local_storage: ResMut<PlayerLocalStorage>,
    mut ledger: ResMut<ResourceLedger>,
    current_tick: Res<CurrentTick>,
    mut onboarding_events: MessageWriter<OnboardingEvent>,
) {
    let pending = std::mem::take(&mut queue.0);
    for spawn in pending {
        if !spawn_admitted(&spawn, &terrains, &room_counts, &drones) {
            refund_spawn_request(
                &spawn,
                &mut structures,
                &mut local_storage,
                &mut ledger,
                current_tick.0,
            );
            continue;
        }
        if let Some(spawn_entity) = Entity::try_from_bits(spawn.spawn_id)
            && let Ok(mut structure) = structures.get_mut(spawn_entity)
        {
            structure.cooldown = 1;
        }

        let stable_id = stable_ids.allocate();
        pending_entities.entries.push(PendingEntityCreationEntry {
            stable_id,
            kind: PendingEntityKind::Drone {
                owner: spawn.owner,
                body: spawn.body,
                position: spawn.position,
                spawning_grace: 1,
            },
        });
        *room_counts
            .0
            .entry((spawn.position.room, spawn.owner))
            .or_default() += 1;
        onboarding_events.write(OnboardingEvent::DroneSpawned);
    }
}

fn spawn_admitted(
    spawn: &PendingSpawn,
    terrains: &RoomTerrains,
    room_counts: &RoomDroneCounts,
    drones: &Query<&Position, With<Drone>>,
) -> bool {
    terrains.is_passable(spawn.position)
        && room_counts
            .0
            .get(&(spawn.position.room, spawn.owner))
            .copied()
            .unwrap_or_default()
            < MAX_DRONES_PER_PLAYER
        && drones.iter().all(|position| *position != spawn.position)
}

fn refund_spawn_request(
    spawn: &PendingSpawn,
    structures: &mut Query<&mut Structure>,
    local_storage: &mut PlayerLocalStorage,
    ledger: &mut ResourceLedger,
    tick: u64,
) {
    for (resource, amount) in &spawn.cost {
        if *amount == 0 {
            continue;
        }
        if resource == "Energy" {
            if let Some(spawn_entity) = Entity::try_from_bits(spawn.spawn_id)
                && let Ok(mut structure) = structures.get_mut(spawn_entity)
                && let Some(energy) = &mut structure.energy
            {
                *energy = energy.saturating_add(*amount);
            }
        } else {
            *local_storage
                .0
                .entry(spawn.owner)
                .or_default()
                .entry(resource.clone())
                .or_default() += *amount;
        }
        ledger.record_account_transfer(
            tick,
            LedgerAccount::system("spawn_refund"),
            LedgerAccount::player(spawn.owner),
            resource,
            *amount,
            ResourceOperation::SpawnCost,
        );
    }
}

pub fn flush_pending_entity_creation_system(
    mut commands: Commands,
    mut pending_entities: ResMut<PendingEntityCreation>,
    body_registry: Res<BodyPartRegistry>,
) {
    let mut entries = std::mem::take(&mut pending_entities.entries);
    entries.sort_by_key(|entry| entry.stable_id);
    for entry in entries {
        match entry.kind {
            PendingEntityKind::Drone {
                owner,
                body,
                position,
                spawning_grace,
            } => {
                let mut entity = commands.spawn((
                    entry.stable_id,
                    position,
                    Owner(owner),
                    Drone::new(owner, body, &body_registry),
                ));
                if spawning_grace > 0 {
                    entity.insert(SpawningGrace {
                        remaining: spawning_grace,
                    });
                }
            }
            PendingEntityKind::Structure {
                position,
                structure,
            } => {
                commands.spawn((entry.stable_id, position, structure));
            }
        }
    }
}
