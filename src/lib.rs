pub mod arena;
pub mod clickhouse;
pub mod command;
pub mod components;
pub mod dragonfly;
pub mod fdb;
pub mod hot_cache;
pub mod idl;
pub mod mcp;
pub mod mod_cli;
pub mod npc;
pub mod onboarding;
pub mod pve;
pub mod ranking;
pub mod realtime;
pub mod replay_storage;
pub mod resources;
pub mod rule_module;
pub mod sdk_gen;
pub mod security;
pub mod sim;
pub mod systems;
pub mod tick;
pub mod tutorial;
pub mod visibility;
pub mod world;

pub use arena::*;
pub use clickhouse::*;
pub use command::*;
pub use components::*;
pub use dragonfly::*;
pub use fdb::*;
pub use hot_cache::*;
pub use mcp::{
    DeployParams, DeployResult, JsonRpcRequest, JsonRpcResponse, McpContext, McpError, McpServer,
    StoredModule, TournamentLockedModule, TournamentPrecommitParams, TournamentPrecommitResult,
    TournamentStatusResult, VisibleController, VisibleDrone, VisibleEntity, VisiblePosition,
    VisibleResource, VisibleSource, VisibleStructure, VisibleTile, VisibleWorldSnapshot,
    WorldRuleMod, WorldRules, swarm_get_snapshot, swarm_get_world_rules,
    visible_entities_for_player,
};
pub use npc::components::*;
pub use npc::events::*;
pub use npc::loot::*;
pub use onboarding::*;
pub use pve::*;
pub use ranking::*;
pub use realtime::*;
pub use replay_storage::*;
pub use resources::*;
pub use rule_module::*;
pub use tick::*;
pub use visibility::*;
pub use world::{SwarmWorld, create_world, create_world_with_mode};

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use crate::{
        command::*, components::*, create_world, create_world_with_mode, onboarding::*,
        resources::*, systems::*,
    };

    fn drone_count(world: &mut crate::SwarmWorld) -> usize {
        world
            .app
            .world_mut()
            .query::<&Drone>()
            .iter(world.app.world())
            .count()
    }

    fn drone_hits(world: &mut crate::SwarmWorld) -> u32 {
        *world
            .app
            .world_mut()
            .query::<&Drone>()
            .iter(world.app.world())
            .map(|drone| &drone.hits)
            .next()
            .expect("expected one drone")
    }

    fn default_structure(cooldown: u32) -> Structure {
        Structure {
            structure_type: StructureType::Spawn,
            owner: Some(1),
            hits: 5_000,
            hits_max: 5_000,
            energy: Some(0),
            energy_capacity: Some(300),
            cooldown,
        }
    }

    fn spawn_structure(
        world: &mut crate::SwarmWorld,
        owner: Option<PlayerId>,
        x: i32,
        y: i32,
        energy: u32,
        capacity: u32,
        cooldown: u32,
    ) -> bevy::prelude::Entity {
        world
            .app
            .world_mut()
            .spawn((
                Position {
                    x,
                    y,
                    room: RoomId(0),
                },
                Structure {
                    structure_type: StructureType::Spawn,
                    owner,
                    hits: 5_000,
                    hits_max: 5_000,
                    energy: Some(energy),
                    energy_capacity: Some(capacity),
                    cooldown,
                },
            ))
            .id()
    }

    fn spawn_terminal(world: &mut crate::SwarmWorld, owner: PlayerId, x: i32, y: i32) {
        world.app.world_mut().spawn((
            Position {
                x,
                y,
                room: RoomId(0),
            },
            Structure {
                structure_type: StructureType::Terminal,
                owner: Some(owner),
                hits: 5_000,
                hits_max: 5_000,
                energy: None,
                energy_capacity: None,
                cooldown: 0,
            },
        ));
    }

    fn first_source_id(world: &mut crate::SwarmWorld) -> ObjectId {
        object_id(
            world
                .app
                .world_mut()
                .query::<(bevy::prelude::Entity, &Source)>()
                .iter(world.app.world())
                .map(|(entity, _)| entity)
                .next()
                .expect("expected source"),
        )
    }

    fn submit(
        world: &mut crate::SwarmWorld,
        player_id: PlayerId,
        sequence: u32,
        action: CommandAction,
    ) -> CommandResult {
        world.submit_intent(
            player_id,
            1,
            CommandSource::Wasm,
            CommandIntent { sequence, action },
        )
    }

    fn create_tutorial_world() -> crate::SwarmWorld {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<GlobalStorageConfig>()
            .intercept_enabled = false;
        world
    }

    fn recycle_refund_for_mode(mode: WorldMode) -> (u32, u32) {
        let mut world = create_world_with_mode(mode);
        let body = vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry];
        let body_cost = world
            .app
            .world()
            .resource::<ResourceRegistry>()
            .body_energy_cost(&body);
        let spawn = spawn_structure(&mut world, Some(1), 10, 10, 0, 300, 0);
        let drone = world.spawn_drone(1, 11, 10, body);

        submit(
            &mut world,
            1,
            1,
            CommandAction::Recycle {
                object_id: object_id(drone),
                spawn_id: object_id(spawn),
            },
        )
        .unwrap();

        let refund = world
            .app
            .world()
            .get::<Structure>(spawn)
            .and_then(|structure| structure.energy)
            .expect("spawn energy after recycle");
        (body_cost, refund)
    }

    #[test]
    fn recycle_refunds_half_body_cost_in_default_world() {
        let (body_cost, refund) = recycle_refund_for_mode(WorldMode::Default);
        assert_eq!(refund, body_cost / 2);
    }

    #[test]
    fn recycle_refunds_full_body_cost_in_tutorial_world_after_500_ticks() {
        let (body_cost, refund) = recycle_refund_for_mode(WorldMode::Tutorial);
        assert_eq!(refund, body_cost);
    }

    #[test]
    fn room_id_parses_formats_and_checks_adjacency() {
        let room = RoomId::from_room_name("A12N34W").unwrap();
        assert_eq!(room.room_name(), "A12N34W");
        assert_eq!(RoomId(0).room_name(), "A0N0E");
        assert!(RoomId(0).is_same_or_adjacent(RoomId::from_room_name("A0N1E").unwrap()));
        assert!(!RoomId(0).is_same_or_adjacent(RoomId::from_room_name("A0N2E").unwrap()));
        assert!(RoomId::from_room_name("A0N0E").is_ok());
        assert!(RoomId::from_room_name("A0E0N").is_err());
    }

    #[test]
    fn move_crosses_room_boundary_and_wraps_coordinates() {
        let mut world = create_world();
        let north = RoomId::from_room_name("A1S0E").unwrap();
        world.ensure_room(north);
        let drone = world.spawn_drone(1, 10, 0, vec![BodyPart::Move]);

        submit(
            &mut world,
            1,
            1,
            CommandAction::Move {
                object_id: object_id(drone),
                direction: Direction::Top,
            },
        )
        .unwrap();

        let position = *world.app.world().get::<Position>(drone).unwrap();
        assert_eq!(
            position,
            Position {
                x: 10,
                y: 49,
                room: north
            }
        );
    }

    #[test]
    fn move_rejects_boundary_crossing_into_missing_room() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 0, vec![BodyPart::Move]);

        assert_eq!(
            submit(
                &mut world,
                1,
                1,
                CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            ),
            Err(RejectionReason::InvalidDirection)
        );
    }
    #[test]
    fn default_world_has_plain_terrain_and_source() {
        let mut world = create_world();

        assert_eq!(world.get_terrain(RoomId(0), 0, 0), Some(TerrainType::Plain));
        assert_eq!(
            world.get_terrain(RoomId(0), 49, 49),
            Some(TerrainType::Plain)
        );
        assert_eq!(world.get_terrain(RoomId(0), 50, 50), None);
        assert!(world.is_passable(RoomId(0), 25, 25));

        let sources = world
            .app
            .world_mut()
            .query::<(&Position, &Source)>()
            .iter(world.app.world())
            .map(|(position, source)| (*position, source.clone()))
            .collect::<Vec<_>>();

        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].0,
            Position {
                x: 25,
                y: 25,
                room: RoomId(0)
            }
        );
        assert_eq!(sources[0].1.capacity, 3_000);
        assert_eq!(sources[0].1.produces.get("Energy"), Some(&1));
    }

    #[test]
    fn terrain_set_get_and_passable_track_each_other() {
        let mut world = create_world();

        assert!(world.set_terrain(RoomId(0), 3, 4, TerrainType::Swamp));
        assert_eq!(world.get_terrain(RoomId(0), 3, 4), Some(TerrainType::Swamp));
        assert!(world.is_passable(RoomId(0), 3, 4));

        assert!(world.set_terrain(RoomId(0), 3, 4, TerrainType::Wall));
        assert_eq!(world.get_terrain(RoomId(0), 3, 4), Some(TerrainType::Wall));
        assert!(!world.is_passable(RoomId(0), 3, 4));
        assert!(!world.set_terrain(RoomId(0), 50, 0, TerrainType::Plain));
        assert_eq!(world.get_terrain(RoomId(0), 50, 0), None);
    }

    #[test]
    fn queue_spawn_spawns_on_run_tick() {
        let mut world = create_world();

        world.queue_spawn(7, 10, 10, vec![BodyPart::Move, BodyPart::Work]);

        assert_eq!(drone_count(&mut world), 0);
        assert_eq!(world.app.world().resource::<PendingSpawnQueue>().0.len(), 1);

        world.run_tick();

        assert_eq!(drone_count(&mut world), 1);
        assert_eq!(world.app.world().resource::<PendingSpawnQueue>().0.len(), 0);
        assert_eq!(
            world
                .app
                .world()
                .resource::<RoomDroneCounts>()
                .0
                .get(&(RoomId(0), 7)),
            Some(&1)
        );
    }

    #[test]
    fn spawn_system_rejects_wall_tile_spawns() {
        let mut world = create_world();

        assert!(world.set_terrain(RoomId(0), 12, 12, TerrainType::Wall));
        world.queue_spawn(7, 12, 12, vec![BodyPart::Move]);
        world.run_tick();

        assert_eq!(drone_count(&mut world), 0);
        assert_eq!(
            world
                .app
                .world()
                .resource::<RoomDroneCounts>()
                .0
                .get(&(RoomId(0), 7))
                .copied()
                .unwrap_or_default(),
            0
        );
    }

    #[test]
    fn phase2b_death_mark_frees_count_before_spawn_then_cleanup_removes_dead() {
        let mut world = create_world();
        let old_drone = world.spawn_drone(9, 13, 13, vec![BodyPart::Move]);
        world
            .app
            .world_mut()
            .entity_mut(old_drone)
            .get_mut::<Drone>()
            .unwrap()
            .hits = 0;

        world.queue_spawn(9, 14, 14, vec![BodyPart::Work]);
        world.run_tick();

        assert!(world.app.world().get_entity(old_drone).is_err());
        assert_eq!(drone_count(&mut world), 1);
        assert_eq!(
            world
                .app
                .world()
                .resource::<RoomDroneCounts>()
                .0
                .get(&(RoomId(0), 9)),
            Some(&1)
        );
    }

    #[test]
    fn combat_applies_damage_before_heal() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .remove::<SpawningGrace>();

        {
            let mut combat = world.app.world_mut().resource_mut::<PendingCombat>();
            combat.damage.push((drone.to_bits(), 60));
            combat.heal.push((drone.to_bits(), 30));
        }

        world.run_tick();

        assert_eq!(drone_hits(&mut world), 70);
    }

    #[test]
    fn spawning_grace_skips_first_tick_damage_then_expires() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);

        {
            let mut combat = world.app.world_mut().resource_mut::<PendingCombat>();
            combat.queue_damage(drone, 60);
        }

        world.run_tick();

        let entity = world.app.world().entity(drone);
        assert_eq!(entity.get::<Drone>().unwrap().hits, 100);
        assert!(entity.get::<SpawningGrace>().is_none());
    }

    #[test]
    fn decay_reduces_fatigue_and_cooldown_and_increments_age() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let structure = world
            .app
            .world_mut()
            .spawn((
                Position {
                    x: 11,
                    y: 10,
                    room: RoomId(0),
                },
                default_structure(3),
            ))
            .id();

        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .fatigue = 2;
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .age = 7;

        world.run_tick();

        let drone_ref = world.app.world().entity(drone).get::<Drone>().unwrap();
        let structure_ref = world
            .app
            .world()
            .entity(structure)
            .get::<Structure>()
            .unwrap();
        assert_eq!(drone_ref.fatigue, 1);
        assert_eq!(drone_ref.age, 8);
        assert_eq!(structure_ref.cooldown, 2);
    }

    #[test]
    fn death_cleanup_removes_dead_drone_and_decrements_room_count() {
        let mut world = create_world();
        let drone = world.spawn_drone(42, 10, 10, vec![BodyPart::Move]);
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .hits = 0;

        world.run_tick();

        assert_eq!(drone_count(&mut world), 0);
        assert_eq!(
            world
                .app
                .world()
                .resource::<RoomDroneCounts>()
                .0
                .get(&(RoomId(0), 42)),
            Some(&0)
        );
    }

    #[test]
    fn state_checksum_is_deterministic_and_changes_with_state() {
        let mut first = create_world();
        let mut second = create_world();

        let first_checksum = first.state_checksum();
        assert_eq!(first_checksum, first.state_checksum());
        assert_eq!(first_checksum, second.state_checksum());

        first.spawn_drone(
            1,
            10,
            10,
            vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
        );
        assert_ne!(first_checksum, first.state_checksum());

        let terrain_checksum = second.state_checksum();
        second.set_terrain(RoomId(0), 0, 0, TerrainType::Wall);
        assert_ne!(terrain_checksum, second.state_checksum());

        let structure_checksum = second.state_checksum();
        second.app.world_mut().spawn((
            Position {
                x: 1,
                y: 1,
                room: RoomId(0),
            },
            default_structure(0),
        ));
        assert_ne!(structure_checksum, second.state_checksum());

        let controller_checksum = second.state_checksum();
        second.app.world_mut().spawn((
            Position {
                x: 2,
                y: 2,
                room: RoomId(0),
            },
            Controller {
                owner: Some(1),
                level: 2,
                progress: 10,
                progress_total: 200,
                downgrade_timer: 1_000,
                safe_mode: 0,
                safe_mode_available: 1,
                safe_mode_cooldown: 0,
                repair_capacity: 0,
                repair_range: 0,
                repair_per_drone: 0,
            },
        ));
        assert_ne!(controller_checksum, second.state_checksum());

        let resource_checksum = second.state_checksum();
        let mut amounts = IndexMap::new();
        amounts.insert("Energy".to_string(), 42);
        second.app.world_mut().spawn((
            Position {
                x: 3,
                y: 3,
                room: RoomId(0),
            },
            Resource { amounts },
        ));
        assert_ne!(resource_checksum, second.state_checksum());
    }

    #[test]
    fn source_gate_injects_envelope_fields() {
        let intent = CommandIntent {
            sequence: 7,
            action: CommandAction::Move {
                object_id: 12,
                direction: Direction::Top,
            },
        };

        let raw = source_gate(42, 99, CommandSource::Wasm, intent).unwrap();

        assert_eq!(raw.player_id, 42);
        assert_eq!(raw.tick, 99);
        assert_eq!(raw.source, CommandSource::Wasm);
        assert_eq!(raw.auth.source, CommandSource::Wasm);
        assert_eq!(raw.auth.player_id, 42);
        assert_eq!(raw.auth.tick_submitted, 99);
        assert_eq!(raw.auth.tick_target, 99);
        assert_eq!(raw.sequence, 7);
    }

    #[test]
    fn tick_output_schema_accepts_intents_and_injects_wasm_envelope() {
        let json = br#"[
            {"sequence":3,"action":{"type":"Move","object_id":1001,"direction":"TopRight"}}
        ]"#;

        let commands = parse_tick_output(42, 4521, json).unwrap();

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].player_id, 42);
        assert_eq!(commands[0].tick, 4521);
        assert_eq!(commands[0].source, CommandSource::Wasm);
        assert_eq!(commands[0].auth.source, CommandSource::Wasm);
        assert_eq!(commands[0].auth.player_id, 42);
        assert_eq!(commands[0].auth.tick_submitted, 4521);
        assert_eq!(commands[0].auth.tick_target, 4521);
        assert_eq!(commands[0].sequence, 3);
        assert_eq!(
            commands[0].action,
            CommandAction::Move {
                object_id: 1001,
                direction: Direction::TopRight
            }
        );
    }

    #[test]
    fn tick_output_schema_rejects_non_arrays_extra_fields_depth_and_size() {
        assert_eq!(
            parse_tick_output(1, 1, br#"{"sequence":1,"action":{"type":"Move"}}"#),
            Err(TickValidationError::NotArray)
        );
        assert_eq!(
            parse_tick_output(
                1,
                1,
                br#"[{"player_id":99,"sequence":1,"action":{"type":"Move","object_id":1,"direction":"Top"}}]"#
            ),
            Err(TickValidationError::SchemaViolation)
        );
        assert_eq!(
            parse_tick_output(
                1,
                1,
                br#"[{"sequence":1,"action":{"type":"Move","object_id":1,"direction":"Top","extra":true}}]"#
            ),
            Err(TickValidationError::SchemaViolation)
        );
        assert_eq!(
            parse_tick_output(1, 1, br#"[[[[[[[[[[[0]]]]]]]]]]]"#),
            Err(TickValidationError::TooDeep)
        );
        assert_eq!(
            parse_tick_output(1, 1, &vec![b' '; MAX_TICK_OUTPUT_BYTES + 1]),
            Err(TickValidationError::TooLarge)
        );
    }

    #[test]
    fn tick_output_schema_rejects_too_many_commands() {
        let command = r#"{"sequence":1,"action":{"type":"Move","object_id":1,"direction":"Top"}}"#;
        let json = format!("[{}]", vec![command; MAX_COMMANDS_PER_PLAYER + 1].join(","));

        assert_eq!(
            parse_tick_output(1, 1, json.as_bytes()),
            Err(TickValidationError::TooManyCommands)
        );
    }

    #[test]
    fn source_gate_enforces_p0_9_gameplay_source_matrix() {
        let move_intent = CommandIntent {
            sequence: 1,
            action: CommandAction::Move {
                object_id: 1,
                direction: Direction::Top,
            },
        };
        let attack_intent = CommandIntent {
            sequence: 1,
            action: CommandAction::Attack {
                object_id: 1,
                target_id: 2,
            },
        };

        let cases = [
            (CommandSource::Wasm, true, true),
            (CommandSource::McpDeploy, false, false),
            (CommandSource::McpQuery, false, false),
            (CommandSource::Admin, true, true),
            (CommandSource::Replay, false, false),
            (CommandSource::TestHarness, true, true),
            (CommandSource::Tutorial, true, true),
            (CommandSource::Deploy, false, false),
            (CommandSource::Rollback, false, false),
            (CommandSource::RuleMod, false, false),
            (CommandSource::Simulate, false, false),
            (CommandSource::DryRun, false, false),
        ];

        for (source, allows_move, allows_attack) in cases {
            assert_eq!(
                source_gate(1, 1, source, move_intent.clone()).is_ok(),
                allows_move,
                "{source:?} move capability"
            );
            assert_eq!(
                source_gate(1, 1, source, attack_intent.clone()).is_ok(),
                allows_attack,
                "{source:?} combat capability"
            );
        }
    }

    #[test]
    fn source_capabilities_match_p0_9_matrix() {
        let cases = [
            (CommandSource::Wasm, (true, true, false, true, true)),
            (CommandSource::McpDeploy, (false, false, true, false, false)),
            (CommandSource::McpQuery, (false, false, false, true, false)),
            (CommandSource::Admin, (true, true, true, true, true)),
            (CommandSource::Replay, (false, false, false, true, false)),
            (CommandSource::TestHarness, (true, true, true, true, true)),
            (CommandSource::Tutorial, (true, false, false, true, true)),
            (CommandSource::Deploy, (false, false, true, false, false)),
            (CommandSource::Rollback, (true, true, true, true, false)),
            (CommandSource::RuleMod, (true, false, false, false, false)),
            (CommandSource::Simulate, (false, false, false, true, false)),
            (CommandSource::DryRun, (false, false, false, false, false)),
        ];

        for (source, expected) in cases {
            let capabilities = source_capabilities(source);
            assert_eq!(
                (
                    capabilities.write_world,
                    capabilities.global_storage,
                    capabilities.deploy_code,
                    capabilities.query_world,
                    capabilities.trigger_combat,
                ),
                expected,
                "{source:?} capabilities"
            );
        }
    }

    #[test]
    fn tutorial_world_uses_tutorial_settings_and_accelerated_single_room() {
        let default_world = create_world();
        let tutorial_world = create_world_with_mode(WorldMode::Tutorial);

        let default_settings = default_world.app.world().resource::<WorldSettings>();
        assert_eq!(default_settings.mode, WorldMode::Default);
        assert_eq!(default_settings.tick_interval_ms, DEFAULT_TICK_INTERVAL_MS);
        assert_eq!(default_settings.namespace, "default");
        assert!(
            !default_world
                .app
                .world()
                .resource::<OnboardingConfig>()
                .enabled
        );

        let tutorial_settings = tutorial_world.app.world().resource::<WorldSettings>();
        assert_eq!(tutorial_settings.mode, WorldMode::Tutorial);
        assert_eq!(
            tutorial_settings.tick_interval_ms,
            TUTORIAL_TICK_INTERVAL_MS
        );
        assert!(tutorial_settings.namespace.starts_with("tutorial_"));
        assert!(
            tutorial_world
                .app
                .world()
                .resource::<OnboardingConfig>()
                .enabled
        );

        let tutorial_rooms = &tutorial_world.app.world().resource::<RoomTerrains>().0;
        assert_eq!(tutorial_rooms.len(), 1);
        assert!(tutorial_rooms.contains_key(&RoomId(0)));

        let default_source = default_world
            .app
            .world()
            .resource::<ResourceRegistry>()
            .source("EnergyField")
            .unwrap();
        let tutorial_source = tutorial_world
            .app
            .world()
            .resource::<ResourceRegistry>()
            .source("EnergyField")
            .unwrap();
        assert!(tutorial_source.capacity > default_source.capacity);
        assert!(tutorial_source.regeneration < default_source.regeneration);
    }

    #[test]
    fn tutorial_world_namespaces_are_isolated() {
        let first = create_world_with_mode(WorldMode::Tutorial);
        let second = create_world_with_mode(WorldMode::Tutorial);

        let first_namespace = first
            .app
            .world()
            .resource::<WorldSettings>()
            .namespace
            .clone();
        let second_namespace = second
            .app
            .world()
            .resource::<WorldSettings>()
            .namespace
            .clone();

        assert_ne!(first_namespace, second_namespace);
        assert_eq!(
            first
                .app
                .world()
                .resource::<GlobalStorageConfig>()
                .namespace,
            first_namespace
        );
        assert_eq!(
            second
                .app
                .world()
                .resource::<GlobalStorageConfig>()
                .namespace,
            second_namespace
        );
    }

    #[test]
    fn tutorial_source_is_only_validated_in_tutorial_worlds() {
        let mut default_world = create_world();
        let default_entity = default_world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let default_intent = CommandIntent {
            sequence: 1,
            action: CommandAction::Move {
                object_id: object_id(default_entity),
                direction: Direction::Top,
            },
        };
        let default_raw = source_gate(1, 1, CommandSource::Tutorial, default_intent).unwrap();
        assert_eq!(
            validate_command(default_world.app.world_mut(), default_raw),
            Err(RejectionReason::SourceNotAllowed)
        );

        let mut tutorial_world = create_world_with_mode(WorldMode::Tutorial);
        let tutorial_entity = tutorial_world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let tutorial_intent = CommandIntent {
            sequence: 1,
            action: CommandAction::Move {
                object_id: object_id(tutorial_entity),
                direction: Direction::Top,
            },
        };
        let tutorial_raw = source_gate(1, 1, CommandSource::Tutorial, tutorial_intent).unwrap();
        assert!(validate_command(tutorial_world.app.world_mut(), tutorial_raw).is_ok());
    }

    #[test]
    fn tutorial_onboarding_records_successful_gameplay_events() {
        let mut world = create_world_with_mode(WorldMode::Tutorial);
        let drone = world.spawn_drone(1, 24, 25, vec![BodyPart::Work, BodyPart::Carry]);
        let source_id = first_source_id(&mut world);

        submit(
            &mut world,
            1,
            1,
            CommandAction::Harvest {
                object_id: object_id(drone),
                target_id: source_id,
                resource: None,
            },
        )
        .unwrap();
        submit(
            &mut world,
            1,
            2,
            CommandAction::Build {
                object_id: object_id(drone),
                x: 23,
                y: 25,
                structure: StructureType::Extension,
            },
        )
        .unwrap();
        world.run_tick();

        let progress = world.app.world().resource::<OnboardingProgress>();
        assert!(progress.is_unlocked(OnboardingAchievementId::FirstSpawn));
        assert!(progress.is_unlocked(OnboardingAchievementId::FirstHarvestOrCollection));
        assert!(progress.is_unlocked(OnboardingAchievementId::FirstBuild));
        assert_eq!(progress.completed_count(), 3);
        assert_eq!(
            world
                .app
                .world()
                .resource::<bevy::prelude::Events<OnboardingSwarmEvent>>()
                .len(),
            3
        );
    }

    #[test]
    fn tutorial_onboarding_records_insufficient_resource_rejection() {
        let mut world = create_world_with_mode(WorldMode::Tutorial);
        let spawn = spawn_structure(&mut world, Some(1), 10, 10, 0, 300, 0);

        assert_eq!(
            submit(
                &mut world,
                1,
                1,
                CommandAction::Spawn {
                    spawn_id: object_id(spawn),
                    body: vec![BodyPart::Move],
                },
            ),
            Err(RejectionReason::InsufficientResource {
                resource: "Energy".to_string(),
                required: 50,
                available: 0,
            })
        );
        world.run_tick();

        let progress = world.app.world().resource::<OnboardingProgress>();
        assert!(progress.is_unlocked(OnboardingAchievementId::ResourceBottleneckExplanation));
        assert_eq!(
            world
                .app
                .world()
                .resource::<bevy::prelude::Events<OnboardingSwarmEvent>>()
                .len(),
            1
        );
    }

    #[test]
    fn onboarding_records_replay_and_arena_completion() {
        let mut world = create_world_with_mode(WorldMode::Tutorial);

        world.record_replay_completed();
        world.record_arena_completed();
        world.run_tick();

        let progress = world.app.world().resource::<OnboardingProgress>();
        assert!(progress.is_unlocked(OnboardingAchievementId::Replay));
        assert!(progress.is_unlocked(OnboardingAchievementId::Arena));
        assert_eq!(progress.completed_count(), 2);
    }

    #[test]
    fn raw_command_auth_context_must_match_source_gate_envelope() {
        let intent = CommandIntent {
            sequence: 1,
            action: CommandAction::Move {
                object_id: 1,
                direction: Direction::Top,
            },
        };
        let raw = source_gate(7, 11, CommandSource::Wasm, intent).unwrap();
        let mut world = create_world();

        assert_eq!(
            validate_command(
                world.app.world_mut(),
                RawCommand {
                    player_id: 8,
                    ..raw.clone()
                }
            ),
            Err(RejectionReason::AuthContextInvalid)
        );
        assert_eq!(
            validate_command(
                world.app.world_mut(),
                RawCommand {
                    tick: 12,
                    ..raw.clone()
                }
            ),
            Err(RejectionReason::AuthContextInvalid)
        );
        assert_eq!(
            validate_command(
                world.app.world_mut(),
                RawCommand {
                    source: CommandSource::Admin,
                    ..raw
                }
            ),
            Err(RejectionReason::AuthContextInvalid)
        );
    }

    #[test]
    fn refund_policy_only_refunds_contention_once_and_caps_credit() {
        let raw = RawCommand {
            player_id: 7,
            tick: 9,
            source: CommandSource::Wasm,
            auth: CommandAuth {
                source: CommandSource::Wasm,
                player_id: 7,
                tick_submitted: 9,
                tick_target: 9,
            },
            sequence: 1,
            action: CommandAction::Harvest {
                object_id: 1,
                target_id: 2,
                resource: None,
            },
        };
        let mut refunds = RefundAccumulator::default();

        assert_eq!(
            refunds.record_rejection(&raw, &RejectionReason::SourceEmpty, 10_000),
            5_000
        );
        assert_eq!(
            refunds.record_rejection(&raw, &RejectionReason::SourceEmpty, 10_000),
            0
        );
        assert_eq!(
            refunds.record_rejection(
                &RawCommand {
                    sequence: 2,
                    ..raw.clone()
                },
                &RejectionReason::OutOfRange {
                    distance: 2,
                    max: 1
                },
                10_000
            ),
            0
        );
        assert_eq!(
            refunds.record_rejection(
                &RawCommand {
                    sequence: 3,
                    ..raw.clone()
                },
                &RejectionReason::TargetFull,
                MAX_FUEL * 4
            ),
            MAX_REFUND_PER_TICK - 5_000
        );
        assert_eq!(refunds.next_tick_fuel_credit, MAX_REFUND_PER_TICK);
        assert_eq!(next_tick_fuel_budget(u64::MAX), MAX_NEXT_TICK_FUEL_BUDGET);

        refunds.clear_for_deploy();
        assert_eq!(refunds.next_tick_fuel_credit, 0);
    }

    #[test]
    fn rule_module_budget_runs_in_world_tick_and_auto_disables() {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<crate::rule_module::RhaiRuleModules>()
            .add_module(crate::rule_module::RhaiRuleModule::with_ast_nodes(
                "oversized",
                crate::rule_module::DEFAULT_RHAI_AST_NODES_PER_TICK + 1,
                |_: &mut crate::rule_module::RhaiActions<'_>| {
                    panic!("AST over-budget module should be skipped");
                },
            ));

        for _ in 0..crate::rule_module::DEFAULT_RHAI_MAX_CONSECUTIVE_OVER_BUDGET_TICKS {
            world.run_tick();
        }

        let modules = world
            .app
            .world()
            .resource::<crate::rule_module::RhaiRuleModules>();
        assert!(!modules.modules()[0].is_enabled());
        assert_eq!(modules.last_tick_reports()[0].module_name, "oversized");
        assert!(modules.last_tick_reports()[0].disabled);
    }

    #[test]
    fn move_rejects_owner_fatigue_spawning_body_and_terrain_then_succeeds() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let id = object_id(drone);

        assert_eq!(
            submit(
                &mut world,
                2,
                1,
                CommandAction::Move {
                    object_id: id,
                    direction: Direction::Top
                }
            ),
            Err(RejectionReason::NotOwner)
        );

        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .fatigue = 1;
        assert_eq!(
            submit(
                &mut world,
                1,
                2,
                CommandAction::Move {
                    object_id: id,
                    direction: Direction::Top
                }
            ),
            Err(RejectionReason::Fatigued)
        );

        {
            let mut entity = world.app.world_mut().entity_mut(drone);
            let mut drone_ref = entity.get_mut::<Drone>().unwrap();
            drone_ref.fatigue = 0;
            drone_ref.spawning = true;
        }
        assert_eq!(
            submit(
                &mut world,
                1,
                3,
                CommandAction::Move {
                    object_id: id,
                    direction: Direction::Top
                }
            ),
            Err(RejectionReason::StillSpawning)
        );

        {
            let mut entity = world.app.world_mut().entity_mut(drone);
            let mut drone_ref = entity.get_mut::<Drone>().unwrap();
            drone_ref.spawning = false;
            drone_ref.body.clear();
        }
        assert_eq!(
            submit(
                &mut world,
                1,
                4,
                CommandAction::Move {
                    object_id: id,
                    direction: Direction::Top
                }
            ),
            Err(RejectionReason::MissingBodyPart {
                part: BodyPart::Move
            })
        );

        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .body = vec![BodyPart::Move];
        assert!(world.set_terrain(RoomId(0), 10, 9, TerrainType::Wall));
        assert_eq!(
            submit(
                &mut world,
                1,
                5,
                CommandAction::Move {
                    object_id: id,
                    direction: Direction::Top
                }
            ),
            Err(RejectionReason::TileBlocked)
        );

        assert!(world.set_terrain(RoomId(0), 10, 9, TerrainType::Plain));
        assert_eq!(
            submit(
                &mut world,
                1,
                6,
                CommandAction::Move {
                    object_id: id,
                    direction: Direction::Top
                }
            ),
            Ok(())
        );
        assert_eq!(
            *world.app.world().entity(drone).get::<Position>().unwrap(),
            Position {
                x: 10,
                y: 9,
                room: RoomId(0)
            }
        );
    }

    #[test]
    fn harvest_rejects_missing_body_range_and_empty_source_then_adds_energy() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 24, 25, vec![BodyPart::Carry]);
        let drone_id = object_id(drone);
        let source_id = first_source_id(&mut world);

        assert_eq!(
            submit(
                &mut world,
                1,
                1,
                CommandAction::Harvest {
                    object_id: drone_id,
                    target_id: source_id,
                    resource: None
                }
            ),
            Err(RejectionReason::MissingBodyPart {
                part: BodyPart::Work
            })
        );

        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .body = vec![BodyPart::Work, BodyPart::Carry];
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Position>()
            .unwrap()
            .x = 20;
        assert_eq!(
            submit(
                &mut world,
                1,
                2,
                CommandAction::Harvest {
                    object_id: drone_id,
                    target_id: source_id,
                    resource: None
                }
            ),
            Err(RejectionReason::OutOfRange {
                distance: 5,
                max: 1
            })
        );

        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Position>()
            .unwrap()
            .x = 24;
        let source_entity = bevy::prelude::Entity::from_bits(source_id);
        world
            .app
            .world_mut()
            .entity_mut(source_entity)
            .get_mut::<crate::components::Source>()
            .unwrap()
            .capacity = 0;
        assert_eq!(
            submit(
                &mut world,
                1,
                3,
                CommandAction::Harvest {
                    object_id: drone_id,
                    target_id: source_id,
                    resource: None
                }
            ),
            Err(RejectionReason::SourceEmpty)
        );

        world
            .app
            .world_mut()
            .entity_mut(source_entity)
            .get_mut::<crate::components::Source>()
            .unwrap()
            .capacity = 10;
        assert_eq!(
            submit(
                &mut world,
                1,
                4,
                CommandAction::Harvest {
                    object_id: drone_id,
                    target_id: source_id,
                    resource: None
                }
            ),
            Ok(())
        );
        assert_eq!(
            world
                .app
                .world()
                .entity(drone)
                .get::<Drone>()
                .unwrap()
                .carry
                .get("Energy"),
            Some(&2)
        );
    }

    #[test]
    fn transfer_and_withdraw_update_resources() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Carry]);
        let store = spawn_structure(&mut world, Some(1), 11, 10, 25, 100, 0);
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .carry
            .insert("Energy".to_string(), 30);

        assert_eq!(
            submit(
                &mut world,
                1,
                1,
                CommandAction::Transfer {
                    object_id: object_id(drone),
                    target_id: object_id(store),
                    resource: "Energy".to_string(),
                    amount: 20
                }
            ),
            Ok(())
        );
        assert_eq!(
            world
                .app
                .world()
                .entity(store)
                .get::<Structure>()
                .unwrap()
                .energy,
            Some(45)
        );

        assert_eq!(
            submit(
                &mut world,
                1,
                2,
                CommandAction::Withdraw {
                    object_id: object_id(drone),
                    target_id: object_id(store),
                    resource: "Energy".to_string(),
                    amount: 15
                }
            ),
            Ok(())
        );
        assert_eq!(
            world
                .app
                .world()
                .entity(drone)
                .get::<Drone>()
                .unwrap()
                .carry
                .get("Energy"),
            Some(&25)
        );
    }

    #[test]
    fn transfer_to_global_delivers_after_ten_ticks_and_is_visible_in_snapshot() {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<PlayerLocalStorage>()
            .0
            .entry(1)
            .or_default()
            .insert("Energy".to_string(), 1_000);

        submit(
            &mut world,
            1,
            1,
            CommandAction::TransferToGlobal {
                resource: "Energy".to_string(),
                amount: 1_000,
            },
        )
        .unwrap();

        let snapshot = crate::swarm_get_snapshot(
            &mut world,
            crate::McpContext {
                player_id: 1,
                tick: 1,
            },
        );
        assert_eq!(snapshot.pending_global_transfers.len(), 1);
        assert_eq!(snapshot.pending_global_transfers[0].remaining_ticks, 10);
        assert_eq!(snapshot.global_storage.get("Energy"), None);

        for _ in 0..9 {
            world.run_tick();
        }
        assert_eq!(
            world
                .app
                .world()
                .resource::<PlayerGlobalStorage>()
                .0
                .get(&1)
                .and_then(|storage| storage.get("Energy")),
            None
        );
        assert_eq!(
            world.app.world().resource::<PendingGlobalTransfers>().0[0].remaining_ticks,
            1
        );

        world.run_tick();
        assert_eq!(
            world
                .app
                .world()
                .resource::<PlayerGlobalStorage>()
                .0
                .get(&1)
                .and_then(|storage| storage.get("Energy")),
            Some(&990)
        );
        assert!(
            world
                .app
                .world()
                .resource::<PendingGlobalTransfers>()
                .0
                .is_empty()
        );
    }

    #[test]
    fn transfer_from_global_delivers_after_five_ticks() {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .entry(1)
            .or_default()
            .insert("Energy".to_string(), 1_000);

        submit(
            &mut world,
            1,
            1,
            CommandAction::TransferFromGlobal {
                resource: "Energy".to_string(),
                amount: 1_000,
            },
        )
        .unwrap();

        for _ in 0..4 {
            world.run_tick();
        }
        assert_eq!(
            world
                .app
                .world()
                .resource::<PlayerLocalStorage>()
                .0
                .get(&1)
                .and_then(|storage| storage.get("Energy")),
            None
        );
        assert_eq!(
            world.app.world().resource::<PendingGlobalTransfers>().0[0].remaining_ticks,
            1
        );

        world.run_tick();
        assert_eq!(
            world
                .app
                .world()
                .resource::<PlayerLocalStorage>()
                .0
                .get(&1)
                .and_then(|storage| storage.get("Energy")),
            Some(&950)
        );
    }

    #[test]
    fn global_transfer_is_intercepted_by_enemy_on_path() {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<PlayerLocalStorage>()
            .0
            .entry(1)
            .or_default()
            .insert("Energy".to_string(), 1_000);
        world.spawn_drone(2, 1, 25, vec![BodyPart::Move]);

        submit(
            &mut world,
            1,
            1,
            CommandAction::TransferToGlobal {
                resource: "Energy".to_string(),
                amount: 1_000,
            },
        )
        .unwrap();

        world.run_tick();

        assert!(
            world
                .app
                .world()
                .resource::<PendingGlobalTransfers>()
                .0
                .is_empty()
        );
        assert_eq!(
            world
                .app
                .world()
                .resource::<PlayerGlobalStorage>()
                .0
                .get(&1)
                .and_then(|storage| storage.get("Energy")),
            None
        );
    }

    #[test]
    fn global_transfer_interception_respects_range_boundary() {
        let mut inside = create_world();
        inside
            .app
            .world_mut()
            .resource_mut::<GlobalStorageConfig>()
            .intercept_range = 3;
        inside
            .app
            .world_mut()
            .resource_mut::<PlayerLocalStorage>()
            .0
            .entry(1)
            .or_default()
            .insert("Energy".to_string(), 1_000);
        inside.spawn_drone(2, 4, 25, vec![BodyPart::Move]);

        submit(
            &mut inside,
            1,
            1,
            CommandAction::TransferToGlobal {
                resource: "Energy".to_string(),
                amount: 1_000,
            },
        )
        .unwrap();
        inside.run_tick();
        assert!(
            inside
                .app
                .world()
                .resource::<PendingGlobalTransfers>()
                .0
                .is_empty()
        );

        let mut outside = create_world();
        outside
            .app
            .world_mut()
            .resource_mut::<GlobalStorageConfig>()
            .intercept_range = 3;
        outside
            .app
            .world_mut()
            .resource_mut::<PlayerLocalStorage>()
            .0
            .entry(1)
            .or_default()
            .insert("Energy".to_string(), 1_000);
        outside.spawn_drone(2, 5, 25, vec![BodyPart::Move]);

        submit(
            &mut outside,
            1,
            1,
            CommandAction::TransferToGlobal {
                resource: "Energy".to_string(),
                amount: 1_000,
            },
        )
        .unwrap();
        outside.run_tick();
        assert_eq!(
            outside.app.world().resource::<PendingGlobalTransfers>().0[0].remaining_ticks,
            9
        );
    }

    #[test]
    fn tutorial_world_disables_global_transfer_interception() {
        let mut world = create_tutorial_world();
        world
            .app
            .world_mut()
            .resource_mut::<PlayerLocalStorage>()
            .0
            .entry(1)
            .or_default()
            .insert("Energy".to_string(), 1_000);
        world.spawn_drone(2, 1, 25, vec![BodyPart::Move]);

        submit(
            &mut world,
            1,
            1,
            CommandAction::TransferToGlobal {
                resource: "Energy".to_string(),
                amount: 1_000,
            },
        )
        .unwrap();
        world.run_tick();

        assert_eq!(
            world.app.world().resource::<PendingGlobalTransfers>().0[0].remaining_ticks,
            9
        );
    }

    #[test]
    fn global_storage_tax_is_progressive() {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .entry(1)
            .or_default()
            .insert("Energy".to_string(), 100_000);

        world.run_tick();

        assert_eq!(
            world
                .app
                .world()
                .resource::<PlayerGlobalStorage>()
                .0
                .get(&1)
                .and_then(|storage| storage.get("Energy")),
            Some(&99_955)
        );
    }

    #[test]
    fn command_action_serde_accepts_global_storage_variants() {
        let action: CommandAction = serde_json::from_str(
            r#"{"type":"TransferToGlobal","resource":"Energy","amount":1000}"#,
        )
        .unwrap();
        assert_eq!(
            action,
            CommandAction::TransferToGlobal {
                resource: "Energy".to_string(),
                amount: 1000
            }
        );

        let action: CommandAction = serde_json::from_str(
            r#"{"type":"TransferFromGlobal","resource":"Energy","amount":1000}"#,
        )
        .unwrap();
        assert_eq!(
            action,
            CommandAction::TransferFromGlobal {
                resource: "Energy".to_string(),
                amount: 1000
            }
        );
    }

    #[test]
    fn attack_and_heal_update_hits() {
        let mut world = create_world();
        let attacker = world.spawn_drone(1, 10, 10, vec![BodyPart::Attack]);
        let target = world.spawn_drone(2, 11, 10, vec![BodyPart::Move]);

        assert_eq!(
            submit(
                &mut world,
                1,
                1,
                CommandAction::Attack {
                    object_id: object_id(attacker),
                    target_id: object_id(target)
                }
            ),
            Ok(())
        );
        assert_eq!(
            world
                .app
                .world()
                .entity(target)
                .get::<Drone>()
                .unwrap()
                .hits,
            70
        );

        let healer = world.spawn_drone(2, 12, 10, vec![BodyPart::Heal]);
        assert_eq!(
            submit(
                &mut world,
                2,
                2,
                CommandAction::Heal {
                    object_id: object_id(healer),
                    target_id: object_id(target)
                }
            ),
            Ok(())
        );
        assert_eq!(
            world
                .app
                .world()
                .entity(target)
                .get::<Drone>()
                .unwrap()
                .hits,
            82
        );
    }

    #[test]
    fn attack_rejects_friendly_and_heal_rejects_full_or_enemy() {
        let mut world = create_world();
        let attacker = world.spawn_drone(1, 10, 10, vec![BodyPart::Attack]);
        let friendly = world.spawn_drone(1, 11, 10, vec![BodyPart::Move]);
        assert_eq!(
            submit(
                &mut world,
                1,
                1,
                CommandAction::Attack {
                    object_id: object_id(attacker),
                    target_id: object_id(friendly)
                }
            ),
            Err(RejectionReason::FriendlyTarget)
        );

        let healer = world.spawn_drone(1, 12, 10, vec![BodyPart::Heal]);
        assert_eq!(
            submit(
                &mut world,
                1,
                2,
                CommandAction::Heal {
                    object_id: object_id(healer),
                    target_id: object_id(friendly)
                }
            ),
            Err(RejectionReason::AlreadyFullHealth)
        );

        let enemy = world.spawn_drone(2, 11, 11, vec![BodyPart::Move]);
        world
            .app
            .world_mut()
            .entity_mut(enemy)
            .get_mut::<Drone>()
            .unwrap()
            .hits = 50;
        assert_eq!(
            submit(
                &mut world,
                1,
                3,
                CommandAction::Heal {
                    object_id: object_id(healer),
                    target_id: object_id(enemy)
                }
            ),
            Err(RejectionReason::NotFriendly)
        );
    }

    #[test]
    fn spawn_drone_rejects_spawn_constraints_then_queues_spawn() {
        let mut world = create_world();
        let spawn = spawn_structure(&mut world, Some(1), 10, 10, 300, 300, 1);

        assert_eq!(
            submit(
                &mut world,
                1,
                1,
                CommandAction::Spawn {
                    spawn_id: object_id(spawn),
                    body: vec![BodyPart::Move]
                }
            ),
            Err(RejectionReason::SpawnOnCooldown)
        );

        world
            .app
            .world_mut()
            .entity_mut(spawn)
            .get_mut::<Structure>()
            .unwrap()
            .cooldown = 0;
        assert_eq!(
            submit(
                &mut world,
                2,
                2,
                CommandAction::Spawn {
                    spawn_id: object_id(spawn),
                    body: vec![BodyPart::Move]
                }
            ),
            Err(RejectionReason::NotYourSpawn)
        );

        assert_eq!(
            submit(
                &mut world,
                1,
                3,
                CommandAction::Spawn {
                    spawn_id: object_id(spawn),
                    body: vec![BodyPart::Tough; 51]
                }
            ),
            Err(RejectionReason::BodyTooLarge)
        );

        assert!(world.set_terrain(RoomId(0), 11, 10, TerrainType::Wall));
        assert_eq!(
            submit(
                &mut world,
                1,
                4,
                CommandAction::Spawn {
                    spawn_id: object_id(spawn),
                    body: vec![BodyPart::Move]
                }
            ),
            Err(RejectionReason::InvalidTerrain)
        );

        assert!(world.set_terrain(RoomId(0), 11, 10, TerrainType::Plain));
        assert_eq!(
            submit(
                &mut world,
                1,
                5,
                CommandAction::Spawn {
                    spawn_id: object_id(spawn),
                    body: vec![BodyPart::Move, BodyPart::Carry]
                }
            ),
            Ok(())
        );
        assert_eq!(world.app.world().resource::<PendingSpawnQueue>().0.len(), 1);
        assert_eq!(
            world
                .app
                .world()
                .entity(spawn)
                .get::<Structure>()
                .unwrap()
                .energy,
            Some(200)
        );
    }

    #[test]
    fn build_rejects_constraints_then_places_structure() {
        let mut world = create_world();
        let builder = world.spawn_drone(1, 10, 10, vec![BodyPart::Work]);
        let mover = world.spawn_drone(1, 12, 10, vec![BodyPart::Move]);

        assert_eq!(
            submit(
                &mut world,
                1,
                1,
                CommandAction::Build {
                    object_id: object_id(mover),
                    x: 13,
                    y: 10,
                    structure: StructureType::Extension,
                }
            ),
            Err(RejectionReason::MissingBodyPart {
                part: BodyPart::Work
            })
        );

        assert!(world.set_terrain(RoomId(0), 10, 11, TerrainType::Wall));
        assert_eq!(
            submit(
                &mut world,
                1,
                2,
                CommandAction::Build {
                    object_id: object_id(builder),
                    x: 10,
                    y: 11,
                    structure: StructureType::Extension,
                }
            ),
            Err(RejectionReason::InvalidTerrain)
        );

        assert_eq!(
            submit(
                &mut world,
                1,
                3,
                CommandAction::Build {
                    object_id: object_id(builder),
                    x: 25,
                    y: 25,
                    structure: StructureType::Extension,
                }
            ),
            Err(RejectionReason::TileOccupied)
        );

        assert_eq!(
            submit(
                &mut world,
                1,
                4,
                CommandAction::Build {
                    object_id: object_id(builder),
                    x: 11,
                    y: 10,
                    structure: StructureType::Extension,
                }
            ),
            Ok(())
        );

        let built = world
            .app
            .world_mut()
            .query::<(&Position, &Structure)>()
            .iter(world.app.world())
            .find(|(position, structure)| {
                **position
                    == Position {
                        x: 11,
                        y: 10,
                        room: RoomId(0),
                    }
                    && structure.structure_type == StructureType::Extension
            })
            .map(|(_, structure)| structure.clone());

        assert_eq!(
            built,
            Some(Structure {
                structure_type: StructureType::Extension,
                owner: Some(1),
                hits: 1,
                hits_max: 5_000,
                energy: Some(0),
                energy_capacity: Some(50),
                cooldown: 0,
            })
        );
    }
}
