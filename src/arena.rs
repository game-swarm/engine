use std::collections::HashMap;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::{BodyPart, Owner, PlayerId, Position, RoomId};
use crate::resources::GlobalStorageConfig;
use crate::tick::{InMemoryTickBroadcaster, InMemoryTickCommitter, PlayerExecutor, TickTrace};
use crate::world::{SwarmWorld, create_world};

pub const ARENA_FIXED_TICKS: Tick = 5_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaPlayerCode {
    pub player_id: PlayerId,
    pub module_id: String,
    pub code_hash: String,
}

impl ArenaPlayerCode {
    pub fn new(
        player_id: PlayerId,
        module_id: impl Into<String>,
        code_hash: impl Into<String>,
    ) -> Self {
        Self {
            player_id,
            module_id: module_id.into(),
            code_hash: code_hash.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaPlayerSlot {
    pub player_id: PlayerId,
    pub spawn: ArenaSpawn,
    pub locked_code: ArenaPlayerCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaSpawn {
    pub room: RoomId,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayPrivacy {
    Private,
    Allies,
    World,
    Public,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaConfig {
    pub fixed_ticks: Tick,
    pub public_spectate: bool,
    pub replay_privacy: ReplayPrivacy,
    pub starting_body: Vec<BodyPart>,
    pub slots: Vec<ArenaPlayerSlot>,
}

impl ArenaConfig {
    pub fn one_v_one(left: ArenaPlayerCode, right: ArenaPlayerCode) -> Self {
        let room = RoomId(0);
        Self {
            fixed_ticks: ARENA_FIXED_TICKS,
            public_spectate: true,
            replay_privacy: ReplayPrivacy::Public,
            starting_body: vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
            slots: vec![
                ArenaPlayerSlot {
                    player_id: left.player_id,
                    spawn: ArenaSpawn { room, x: 10, y: 25 },
                    locked_code: left,
                },
                ArenaPlayerSlot {
                    player_id: right.player_id,
                    spawn: ArenaSpawn { room, x: 39, y: 25 },
                    locked_code: right,
                },
            ],
        }
    }
}

#[derive(Resource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaRules {
    pub fixed_ticks: Tick,
    pub public_spectate: bool,
    pub replay_privacy: ReplayPrivacy,
}

#[derive(Resource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaCodeLock(pub HashMap<PlayerId, ArenaPlayerCode>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaReplay {
    pub privacy: ReplayPrivacy,
    pub public: bool,
    pub traces: Vec<TickTrace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArenaError {
    EmptyPlayers,
    InvalidFixedTicks,
    MissingExecutor(PlayerId),
}

pub struct ArenaMatch {
    pub world: SwarmWorld,
    pub config: ArenaConfig,
}

impl ArenaMatch {
    pub fn new(config: ArenaConfig) -> Result<Self, ArenaError> {
        if config.slots.is_empty() {
            return Err(ArenaError::EmptyPlayers);
        }
        if config.fixed_ticks == 0 {
            return Err(ArenaError::InvalidFixedTicks);
        }

        let mut world = create_world();
        apply_arena_rules(&mut world, &config);
        seed_symmetric_initial_state(&mut world, &config);

        Ok(Self { world, config })
    }

    pub fn locked_code(&self, player_id: PlayerId) -> Option<&ArenaPlayerCode> {
        self.world
            .app
            .world()
            .resource::<ArenaCodeLock>()
            .0
            .get(&player_id)
    }

    pub fn run(
        self,
        executors: HashMap<PlayerId, Box<dyn PlayerExecutor>>,
    ) -> Result<ArenaReplay, ArenaError> {
        for slot in &self.config.slots {
            if !executors.contains_key(&slot.player_id) {
                return Err(ArenaError::MissingExecutor(slot.player_id));
            }
        }

        let fixed_ticks = self.config.fixed_ticks;
        let replay_privacy = self.config.replay_privacy;
        let mut scheduler = crate::tick::MultiPlayerTickScheduler::new(
            self.world,
            executors,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        for _ in 0..fixed_ticks {
            scheduler.tick();
        }

        Ok(ArenaReplay {
            privacy: replay_privacy,
            public: replay_privacy == ReplayPrivacy::Public,
            traces: scheduler.committer.records,
        })
    }
}

fn apply_arena_rules(world: &mut SwarmWorld, config: &ArenaConfig) {
    world.app.insert_resource(ArenaRules {
        fixed_ticks: config.fixed_ticks,
        public_spectate: config.public_spectate,
        replay_privacy: config.replay_privacy,
    });
    world.app.insert_resource(ArenaCodeLock(
        config
            .slots
            .iter()
            .map(|slot| (slot.player_id, slot.locked_code.clone()))
            .collect(),
    ));

    let mut storage = world.app.world_mut().resource_mut::<GlobalStorageConfig>();
    storage.enabled = true;
    storage.transfer_to_global_fee_per_10_000 = 0;
    storage.transfer_from_global_fee_per_10_000 = 0;
    storage.tax_tiers.clear();
}

fn seed_symmetric_initial_state(world: &mut SwarmWorld, config: &ArenaConfig) {
    for slot in &config.slots {
        world.ensure_room(slot.spawn.room);
        let entity = world.spawn_drone_in_room(
            slot.player_id,
            slot.spawn.room,
            slot.spawn.x,
            slot.spawn.y,
            config.starting_body.clone(),
        );
        world
            .app
            .world_mut()
            .entity_mut(entity)
            .insert(Owner(slot.player_id));
    }
}

pub fn arena_owned_positions(world: &mut SwarmWorld) -> Vec<(PlayerId, Position)> {
    let mut positions = world
        .app
        .world_mut()
        .query::<(&Position, &Owner)>()
        .iter(world.app.world())
        .map(|(position, owner)| (owner.0, *position))
        .collect::<Vec<_>>();
    positions
        .sort_by_key(|(player_id, position)| (*player_id, position.room.0, position.x, position.y));
    positions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::{ExecutorError, TickSnapshot};

    #[derive(Default)]
    struct IdleExecutor;

    impl PlayerExecutor for IdleExecutor {
        fn collect(
            &mut self,
            _snapshot: TickSnapshot,
        ) -> Result<Vec<crate::command::CommandIntent>, ExecutorError> {
            Ok(Vec::new())
        }
    }

    fn code(player_id: PlayerId) -> ArenaPlayerCode {
        ArenaPlayerCode::new(
            player_id,
            format!("module-{player_id}"),
            format!("hash-{player_id}"),
        )
    }

    #[test]
    fn one_v_one_defaults_are_fixed_public_and_symmetric() {
        let mut arena = ArenaMatch::new(ArenaConfig::one_v_one(code(1), code(2))).unwrap();
        let rules = arena.world.app.world().resource::<ArenaRules>();
        assert_eq!(rules.fixed_ticks, ARENA_FIXED_TICKS);
        assert!(rules.public_spectate);
        assert_eq!(rules.replay_privacy, ReplayPrivacy::Public);

        assert_eq!(arena.locked_code(1).unwrap().code_hash, "hash-1");
        assert_eq!(arena.locked_code(2).unwrap().module_id, "module-2");

        let positions = arena_owned_positions(&mut arena.world);
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].1.x, 10);
        assert_eq!(positions[1].1.x, 39);
        assert_eq!(positions[0].1.y, positions[1].1.y);
        assert_eq!(positions[0].1.room, positions[1].1.room);
        assert_eq!(positions[0].1.x + positions[1].1.x, 49);
    }

    #[test]
    fn arena_runs_exactly_fixed_tick_count_and_publishes_replay() {
        let mut config = ArenaConfig::one_v_one(code(1), code(2));
        config.fixed_ticks = 3;
        let arena = ArenaMatch::new(config).unwrap();
        let mut executors: HashMap<PlayerId, Box<dyn PlayerExecutor>> = HashMap::new();
        executors.insert(1, Box::<IdleExecutor>::default());
        executors.insert(2, Box::<IdleExecutor>::default());

        let replay = arena.run(executors).unwrap();
        assert!(replay.public);
        assert_eq!(replay.privacy, ReplayPrivacy::Public);
        assert_eq!(replay.traces.len(), 3);
        assert_eq!(replay.traces[0].tick, 0);
        assert_eq!(replay.traces[2].tick, 2);
    }

    #[test]
    fn arena_requires_locked_executor_for_each_slot() {
        let mut config = ArenaConfig::one_v_one(code(1), code(2));
        config.fixed_ticks = 1;
        let arena = ArenaMatch::new(config).unwrap();
        let mut executors: HashMap<PlayerId, Box<dyn PlayerExecutor>> = HashMap::new();
        executors.insert(1, Box::<IdleExecutor>::default());

        assert_eq!(arena.run(executors), Err(ArenaError::MissingExecutor(2)));
    }
}
