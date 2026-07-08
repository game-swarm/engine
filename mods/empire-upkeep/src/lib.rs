use bevy::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use swarm_engine::components::{
    Controller, DeathMark, Drone, PlayerId, Position, RoomId, Structure,
};

#[derive(Resource, Debug, Clone, Default)]
pub struct PlayerEnergyLedger {
    pub balances: BTreeMap<PlayerId, u32>,
}

#[derive(Resource, Debug, Clone, Default)]
pub struct UpkeepShortfalls {
    pub deficits: BTreeMap<PlayerId, u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortfallPolicy {
    Degrade,
    Damage,
    Despawn,
}

#[derive(Resource, Debug, Clone)]
pub struct EmpireUpkeepConfig {
    pub drone_cost: u32,
    pub room_base: u32,
    pub room_superlinear: u32,
    pub onshortfall: ShortfallPolicy,
}

impl Default for EmpireUpkeepConfig {
    fn default() -> Self {
        Self {
            drone_cost: 2,
            room_base: 10,
            room_superlinear: 1,
            onshortfall: ShortfallPolicy::Degrade,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EmpireUpkeepModPlugin;

impl Plugin for EmpireUpkeepModPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EmpireUpkeepConfig>()
            .init_resource::<PlayerEnergyLedger>()
            .init_resource::<UpkeepShortfalls>()
            .add_systems(Update, empire_upkeep_system);
    }
}

pub fn empire_upkeep_system(
    mut commands: Commands,
    config: Res<EmpireUpkeepConfig>,
    mut ledger: ResMut<PlayerEnergyLedger>,
    mut shortfalls: ResMut<UpkeepShortfalls>,
    drones: Query<(Entity, &Drone, &Position)>,
    rooms_from_structures: Query<(&Structure, &Position)>,
    mut controllers: Query<&mut Controller>,
) {
    let mut drone_count: BTreeMap<PlayerId, u32> = BTreeMap::new();
    let mut rooms: BTreeMap<PlayerId, BTreeSet<RoomId>> = BTreeMap::new();

    for (_, drone, position) in &drones {
        *drone_count.entry(drone.owner).or_default() += 1;
        rooms.entry(drone.owner).or_default().insert(position.room);
    }
    for (structure, position) in &rooms_from_structures {
        if let Some(owner) = structure.owner {
            rooms.entry(owner).or_default().insert(position.room);
        }
    }
    for controller in &controllers {
        if let Some(owner) = controller.owner {
            rooms.entry(owner).or_default();
        }
    }

    let mut players: BTreeSet<PlayerId> = drone_count.keys().copied().collect();
    players.extend(rooms.keys().copied());
    for player in players {
        let drones_owned = drone_count.get(&player).copied().unwrap_or(0);
        let rooms_owned = rooms.get(&player).map(|set| set.len() as u32).unwrap_or(0);
        let cost = drones_owned
            .saturating_mul(config.drone_cost)
            .saturating_add(rooms_owned.saturating_mul(config.room_base))
            .saturating_add(
                rooms_owned
                    .saturating_mul(rooms_owned)
                    .saturating_mul(config.room_superlinear),
            );
        let balance = ledger.balances.entry(player).or_default();
        if *balance >= cost {
            *balance -= cost;
            shortfalls.deficits.remove(&player);
            continue;
        }

        let deficit = cost.saturating_sub(*balance);
        *balance = 0;
        shortfalls.deficits.insert(player, deficit);
        match config.onshortfall {
            ShortfallPolicy::Degrade => {
                for mut controller in &mut controllers {
                    if controller.owner == Some(player) {
                        controller.downgrade_timer =
                            controller.downgrade_timer.saturating_sub(deficit);
                        if controller.downgrade_timer == 0 && controller.level > 0 {
                            controller.level -= 1;
                        }
                    }
                }
            }
            ShortfallPolicy::Damage => {
                for (entity, drone, _) in &drones {
                    if drone.owner == player {
                        let new_hits = drone.hits.saturating_sub(deficit.max(1));
                        if new_hits == 0 {
                            commands.entity(entity).insert(DeathMark);
                        }
                    }
                }
            }
            ShortfallPolicy::Despawn => {
                let mut owned: Vec<_> = drones
                    .iter()
                    .filter(|(_, drone, _)| drone.owner == player)
                    .map(|(entity, drone, _)| (drone.age, entity))
                    .collect();
                owned.sort_by_key(|(age, entity)| (std::cmp::Reverse(*age), entity.to_bits()));
                if let Some((_, entity)) = owned.first() {
                    commands.entity(*entity).insert(DeathMark);
                }
            }
        }
    }
}
