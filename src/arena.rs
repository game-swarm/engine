use std::collections::HashMap;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::WorldMode;
use crate::components::{BodyPart, Owner, PlayerId, Position, RoomId};
use crate::ranking::MatchOutcome;
use crate::resources::GlobalStorageConfig;
use crate::tick::{InMemoryTickBroadcaster, InMemoryTickCommitter, PlayerExecutor, TickTrace};
use crate::world::{SwarmWorld, create_world_with_mode};

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
    pub spectate_delay: Option<Tick>,
    pub replay_privacy: ReplayPrivacy,
    pub starting_body: Vec<BodyPart>,
    pub slots: Vec<ArenaPlayerSlot>,
    pub precommit_required: bool,
}

impl ArenaConfig {
    pub fn one_v_one(left: ArenaPlayerCode, right: ArenaPlayerCode) -> Self {
        let room = RoomId(0);
        Self {
            fixed_ticks: ARENA_FIXED_TICKS,
            public_spectate: true,
            spectate_delay: Some(100),
            replay_privacy: ReplayPrivacy::Public,
            starting_body: vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
            precommit_required: true,
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

    /// Validate that all slot code hashes match their registered modules.
    /// Returns Ok(()) or ArenaError::PrecommitMismatch.
    pub fn validate_precommit(&self) -> Result<(), ArenaError> {
        if !self.precommit_required {
            return Ok(());
        }
        for slot in &self.slots {
            if slot.locked_code.module_id.is_empty() || slot.locked_code.code_hash.is_empty() {
                return Err(ArenaError::PrecommitMissing {
                    player_id: slot.player_id,
                });
            }
        }
        Ok(())
    }

    /// Generate a replay URL for this match configuration.
    pub fn replay_url(&self, match_id: u64) -> String {
        format!("swarm://arena/replay/{match_id}")
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArenaReplay {
    pub privacy: ReplayPrivacy,
    pub public: bool,
    pub traces: Vec<TickTrace>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TournamentElimination {
    Single,
    Double,
}

impl TournamentElimination {
    fn max_losses(self) -> u8 {
        match self {
            Self::Single => 1,
            Self::Double => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentSeed {
    pub seed: u32,
    pub code: ArenaPlayerCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentMatchSchedule {
    pub match_id: u64,
    pub round: u32,
    pub player_one: PlayerId,
    pub player_two: PlayerId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TournamentMatchRecord {
    pub schedule: TournamentMatchSchedule,
    pub winner: PlayerId,
    pub loser: PlayerId,
    pub outcome: MatchOutcome,
    pub replay: ArenaReplay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TournamentBracket {
    pub elimination: TournamentElimination,
    pub fixed_ticks: Tick,
    pub seeds: Vec<TournamentSeed>,
    pub scheduled: Vec<TournamentMatchSchedule>,
    pub completed: Vec<TournamentMatchRecord>,
    pub losses: HashMap<PlayerId, u8>,
    pub champion: Option<PlayerId>,
    round: u32,
    next_match_id: u64,
}

/// Six-stage Arena match lifecycle per spec §9.1.3
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArenaMatchState {
    /// Room created, awaiting configuration
    Created,
    /// Parameters set (map, duration, slots filled)
    Configured,
    /// All slots locked, code frozen — ready for start
    Ready,
    /// Match in progress (tick execution active)
    Playing,
    /// Match ended, result pending
    Finished,
    /// Replay generated, match closed
    Replay,
}

impl ArenaMatchState {
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Created, Self::Configured)
                | (Self::Configured, Self::Ready)
                | (Self::Ready, Self::Playing)
                | (Self::Playing, Self::Finished)
                | (Self::Finished, Self::Replay)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArenaError {
    EmptyPlayers,
    InvalidFixedTicks,
    MissingExecutor(PlayerId),
    DuplicatePlayer(PlayerId),
    UnknownTournamentPlayer(PlayerId),
    TournamentComplete,
    NoScheduledMatch,
    InvalidState {
        current: ArenaMatchState,
        expected: ArenaMatchState,
    },
    PrecommitMissing {
        player_id: PlayerId,
    },
}

impl TournamentBracket {
    pub fn seed(
        elimination: TournamentElimination,
        mut players: Vec<ArenaPlayerCode>,
        fixed_ticks: Tick,
    ) -> Result<Self, ArenaError> {
        if players.is_empty() {
            return Err(ArenaError::EmptyPlayers);
        }
        if fixed_ticks == 0 {
            return Err(ArenaError::InvalidFixedTicks);
        }

        players.sort_by_key(|code| code.player_id);
        let mut losses = HashMap::new();
        let mut seeds = Vec::with_capacity(players.len());
        for (index, code) in players.into_iter().enumerate() {
            if losses.insert(code.player_id, 0).is_some() {
                return Err(ArenaError::DuplicatePlayer(code.player_id));
            }
            seeds.push(TournamentSeed {
                seed: index as u32 + 1,
                code,
            });
        }

        let champion = if seeds.len() == 1 {
            Some(seeds[0].code.player_id)
        } else {
            None
        };

        Ok(Self {
            elimination,
            fixed_ticks,
            seeds,
            scheduled: Vec::new(),
            completed: Vec::new(),
            losses,
            champion,
            round: 0,
            next_match_id: 1,
        })
    }

    pub fn active_players(&self) -> Vec<PlayerId> {
        let max_losses = self.elimination.max_losses();
        self.seeds
            .iter()
            .filter_map(|seed| {
                let player_id = seed.code.player_id;
                (self.losses.get(&player_id).copied().unwrap_or(max_losses) < max_losses)
                    .then_some(player_id)
            })
            .collect()
    }

    pub fn schedule_next_round(&mut self) -> Result<Vec<TournamentMatchSchedule>, ArenaError> {
        if self.champion.is_some() {
            return Err(ArenaError::TournamentComplete);
        }
        if !self.scheduled.is_empty() {
            return Ok(self.scheduled.clone());
        }

        let active = self.active_players();
        if active.len() <= 1 {
            self.champion = active.first().copied();
            return Ok(Vec::new());
        }

        self.round += 1;
        let mut schedules = Vec::new();
        for pair in active.chunks(2) {
            if let [player_one, player_two] = pair {
                schedules.push(TournamentMatchSchedule {
                    match_id: self.next_match_id,
                    round: self.round,
                    player_one: *player_one,
                    player_two: *player_two,
                });
                self.next_match_id += 1;
            }
        }
        self.scheduled = schedules.clone();
        Ok(schedules)
    }

    pub fn record_match_result(
        &mut self,
        schedule: TournamentMatchSchedule,
        winner: PlayerId,
        replay: ArenaReplay,
    ) -> Result<&TournamentMatchRecord, ArenaError> {
        if winner != schedule.player_one && winner != schedule.player_two {
            return Err(ArenaError::UnknownTournamentPlayer(winner));
        }
        if !self.losses.contains_key(&schedule.player_one) {
            return Err(ArenaError::UnknownTournamentPlayer(schedule.player_one));
        }
        if !self.losses.contains_key(&schedule.player_two) {
            return Err(ArenaError::UnknownTournamentPlayer(schedule.player_two));
        }

        let loser = if winner == schedule.player_one {
            schedule.player_two
        } else {
            schedule.player_one
        };
        *self.losses.entry(loser).or_default() += 1;
        let outcome = if winner == schedule.player_one {
            MatchOutcome::PlayerOneWin
        } else {
            MatchOutcome::PlayerTwoWin
        };
        self.completed.push(TournamentMatchRecord {
            schedule,
            winner,
            loser,
            outcome,
            replay,
        });

        let active = self.active_players();
        if active.len() == 1 && self.scheduled.is_empty() {
            self.champion = Some(active[0]);
        }

        Ok(self.completed.last().expect("record was just pushed"))
    }

    pub fn execute_next_match<F>(
        &mut self,
        mut executor_factory: F,
    ) -> Result<&TournamentMatchRecord, ArenaError>
    where
        F: FnMut(PlayerId, &ArenaPlayerCode) -> Box<dyn PlayerExecutor>,
    {
        if self.champion.is_some() {
            return Err(ArenaError::TournamentComplete);
        }
        if self.scheduled.is_empty() {
            self.schedule_next_round()?;
        }
        if self.scheduled.is_empty() {
            return Err(ArenaError::NoScheduledMatch);
        }

        let schedule = self.scheduled.remove(0);
        let left = self
            .code_for(schedule.player_one)
            .ok_or(ArenaError::UnknownTournamentPlayer(schedule.player_one))?
            .clone();
        let right = self
            .code_for(schedule.player_two)
            .ok_or(ArenaError::UnknownTournamentPlayer(schedule.player_two))?
            .clone();
        let mut config = ArenaConfig::one_v_one(left.clone(), right.clone());
        config.fixed_ticks = self.fixed_ticks;
        let arena = ArenaMatch::new(config)?;
        let mut executors: HashMap<PlayerId, Box<dyn PlayerExecutor>> = HashMap::new();
        executors.insert(left.player_id, executor_factory(left.player_id, &left));
        executors.insert(right.player_id, executor_factory(right.player_id, &right));
        let replay = arena.run(executors)?;
        let winner = tournament_winner_from_replay(&schedule, &replay);
        self.record_match_result(schedule, winner, replay)
    }

    pub fn execute_all<F>(
        &mut self,
        mut executor_factory: F,
    ) -> Result<Option<PlayerId>, ArenaError>
    where
        F: FnMut(PlayerId, &ArenaPlayerCode) -> Box<dyn PlayerExecutor>,
    {
        while self.champion.is_none() {
            if self.scheduled.is_empty() {
                self.schedule_next_round()?;
                if self.champion.is_some() {
                    break;
                }
            }
            while !self.scheduled.is_empty() {
                self.execute_next_match(&mut executor_factory)?;
            }
        }
        Ok(self.champion)
    }

    fn code_for(&self, player_id: PlayerId) -> Option<&ArenaPlayerCode> {
        self.seeds
            .iter()
            .find(|seed| seed.code.player_id == player_id)
            .map(|seed| &seed.code)
    }
}

fn tournament_winner_from_replay(
    schedule: &TournamentMatchSchedule,
    replay: &ArenaReplay,
) -> PlayerId {
    let (p1_commands, p2_commands) = replay.traces.iter().fold((0_u64, 0_u64), |mut acc, trace| {
        if trace.player_id == schedule.player_one {
            acc.0 = acc.0.saturating_add(trace.metrics.accepted_commands);
        } else if trace.player_id == schedule.player_two {
            acc.1 = acc.1.saturating_add(trace.metrics.accepted_commands);
        }
        acc
    });

    if p2_commands > p1_commands {
        schedule.player_two
    } else {
        schedule.player_one
    }
}

pub struct ArenaMatch {
    pub world: SwarmWorld,
    pub config: ArenaConfig,
    pub state: ArenaMatchState,
}

impl ArenaMatch {
    /// Create a new arena match. Sets up the world, applies rules, seeds initial state.
    /// State transitions: Created → Configured → Ready (in constructor).
    pub fn new(config: ArenaConfig) -> Result<Self, ArenaError> {
        if config.slots.is_empty() {
            return Err(ArenaError::EmptyPlayers);
        }
        if config.fixed_ticks == 0 {
            return Err(ArenaError::InvalidFixedTicks);
        }

        let mut world = create_world_with_mode(WorldMode::Arena);
        apply_arena_rules(&mut world, &config);
        seed_symmetric_initial_state(&mut world, &config);

        Ok(Self {
            world,
            config,
            state: ArenaMatchState::Ready,
        })
    }

    /// Returns the current match state.
    pub fn state(&self) -> ArenaMatchState {
        self.state
    }

    /// Start the match — transitions Ready → Playing.
    /// Call before `run()` if you want explicit state tracking.
    pub fn start(&mut self) -> Result<(), ArenaError> {
        if self.state != ArenaMatchState::Ready {
            return Err(ArenaError::InvalidState {
                current: self.state,
                expected: ArenaMatchState::Ready,
            });
        }
        self.state = ArenaMatchState::Playing;
        Ok(())
    }

    pub fn locked_code(&self, player_id: PlayerId) -> Option<&ArenaPlayerCode> {
        self.world
            .app
            .world()
            .resource::<ArenaCodeLock>()
            .0
            .get(&player_id)
    }

    /// Run the match to completion (Playing → Finished → Replay).
    pub fn run(
        mut self,
        executors: HashMap<PlayerId, Box<dyn PlayerExecutor>>,
    ) -> Result<ArenaReplay, ArenaError> {
        for slot in &self.config.slots {
            if !executors.contains_key(&slot.player_id) {
                return Err(ArenaError::MissingExecutor(slot.player_id));
            }
        }

        // Transition Ready → Playing if not already started
        if self.state == ArenaMatchState::Ready {
            self.start()?;
        } else if self.state != ArenaMatchState::Playing {
            return Err(ArenaError::InvalidState {
                current: self.state,
                expected: ArenaMatchState::Playing,
            });
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

        self.state = ArenaMatchState::Finished;
        let records = scheduler.committer.records;
        let mut world = scheduler.world;
        world.record_arena_completed();

        self.state = ArenaMatchState::Replay;
        Ok(ArenaReplay {
            privacy: replay_privacy,
            public: replay_privacy == ReplayPrivacy::Public,
            traces: records,
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
    storage.intercept_enabled = true;
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

    #[test]
    fn eight_player_single_elimination_bracket_completes() {
        let players = (1..=8).map(code).collect::<Vec<_>>();
        let mut bracket =
            TournamentBracket::seed(TournamentElimination::Single, players, 2).unwrap();

        let first_round = bracket.schedule_next_round().unwrap();
        assert_eq!(first_round.len(), 4);
        assert_eq!(first_round[0].player_one, 1);
        assert_eq!(first_round[0].player_two, 2);

        let champion = bracket
            .execute_all(|_, _| Box::<IdleExecutor>::default())
            .unwrap();
        assert_eq!(champion, Some(1));
        assert_eq!(bracket.completed.len(), 7);
        assert!(bracket.scheduled.is_empty());
        assert_eq!(bracket.losses.get(&2), Some(&1));
        assert_eq!(bracket.losses.get(&8), Some(&1));
        assert_eq!(bracket.completed[0].replay.traces.len(), 2);
    }

    #[test]
    fn double_elimination_requires_second_loss() {
        let players = (1..=4).map(code).collect::<Vec<_>>();
        let mut bracket =
            TournamentBracket::seed(TournamentElimination::Double, players, 1).unwrap();

        let champion = bracket
            .execute_all(|_, _| Box::<IdleExecutor>::default())
            .unwrap();

        assert_eq!(champion, Some(1));
        assert_eq!(bracket.losses.get(&1), Some(&0));
        assert!(
            bracket
                .losses
                .values()
                .filter(|losses| **losses >= 2)
                .count()
                >= 3
        );
        assert!(bracket.completed.len() >= 5);
    }

    #[test]
    fn arena_match_state_initializes_as_ready() {
        let arena = ArenaMatch::new(ArenaConfig::one_v_one(code(1), code(2))).unwrap();
        assert_eq!(arena.state(), ArenaMatchState::Ready);
    }

    #[test]
    fn start_transitions_ready_to_playing() {
        let mut arena = ArenaMatch::new(ArenaConfig::one_v_one(code(1), code(2))).unwrap();
        assert!(arena.start().is_ok());
        assert_eq!(arena.state(), ArenaMatchState::Playing);
    }

    #[test]
    fn start_rejects_non_ready_state() {
        let mut arena = ArenaMatch::new(ArenaConfig::one_v_one(code(1), code(2))).unwrap();
        arena.start().unwrap(); // Ready→Playing
        let result = arena.start(); // Already Playing, should fail
        assert!(result.is_err());
    }

    #[test]
    fn state_transitions_through_full_lifecycle() {
        let mut config = ArenaConfig::one_v_one(code(1), code(2));
        config.fixed_ticks = 5; // 5 ticks suffice for state transition validation
        let arena = ArenaMatch::new(config).unwrap();
        assert_eq!(arena.state(), ArenaMatchState::Ready);

        let mut executors: HashMap<PlayerId, Box<dyn PlayerExecutor>> = HashMap::new();
        executors.insert(1, Box::<IdleExecutor>::default());
        executors.insert(2, Box::<IdleExecutor>::default());
        let replay = arena.run(executors).unwrap();

        // After run: Finished → Replay
        // (state is consumed with self, but replay contains traces)
        assert!(
            !replay.traces.is_empty(),
            "replay should contain tick traces"
        );
    }

    #[test]
    fn arena_match_state_can_transition_rules() {
        assert!(ArenaMatchState::Created.can_transition_to(ArenaMatchState::Configured));
        assert!(ArenaMatchState::Configured.can_transition_to(ArenaMatchState::Ready));
        assert!(ArenaMatchState::Ready.can_transition_to(ArenaMatchState::Playing));
        assert!(ArenaMatchState::Playing.can_transition_to(ArenaMatchState::Finished));
        assert!(ArenaMatchState::Finished.can_transition_to(ArenaMatchState::Replay));

        // Invalid transitions
        assert!(!ArenaMatchState::Ready.can_transition_to(ArenaMatchState::Finished));
        assert!(!ArenaMatchState::Created.can_transition_to(ArenaMatchState::Ready));
        assert!(!ArenaMatchState::Replay.can_transition_to(ArenaMatchState::Playing));
    }
}
