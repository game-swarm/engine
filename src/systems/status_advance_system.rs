use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use std::collections::{HashMap, HashSet};
use swarm_engine_api::ids::PlayerId;
use swarm_engine_plugin_sdk::buffers::{PendingSpecialAttack, SpecialAttackKind};
use swarm_engine_plugin_sdk::components::{
    DeathMark, Drone, Owner, Position, SpawningGrace, Structure, StructureType,
};

use crate::command::CustomActionCooldowns;
use crate::components::{
    Attributes, DebilitateState, DisruptBuffer, DisruptState, DrainState, EntityFlags,
    FabricateBuffer, FabricateState, FortifyState, HackState, OverloadState, PendingEntityCreation,
    PendingEntityCreationEntry, PendingEntityKind, StableEntityIdAllocator, StructureTypeRegistry,
};
use crate::plugins::PluginRegistry;
use crate::resources::CurrentTick;
use crate::systems::PendingIntents;
use crate::world::WorldConfig;

/// S22 status_advance_system — UNIQUE WRITER for all StatusState components.
///
/// This system is the single authority that writes HackState, DrainState,
/// OverloadState, DebilitateState, DisruptState, FortifyState,
/// and FabricateState (R22 B3).
/// It reads canonical sorted intents from S14's PendingIntents buffer,
/// resets/extends existing statuses, advances remaining_ticks, and
/// removes expired statuses. Command validation only queues PendingSpecialAttack;
/// status components are advanced here from reduced intents.
#[derive(SystemParam)]
pub struct StatusQueries<'w, 's> {
    hack: Query<'w, 's, (Entity, &'static mut HackState)>,
    drain: Query<'w, 's, (Entity, &'static mut DrainState)>,
    overload: Query<'w, 's, (Entity, &'static mut OverloadState)>,
    debilitate: Query<'w, 's, (Entity, &'static mut DebilitateState)>,
    disrupt: Query<'w, 's, (Entity, &'static mut DisruptState)>,
    fortify: Query<'w, 's, (Entity, &'static mut FortifyState)>,
    fabricate: Query<'w, 's, (Entity, &'static mut FabricateState)>,
    fabricate_buffers: Query<'w, 's, &'static FabricateBuffer>,
    disrupt_buffers: Query<'w, 's, Entity, With<DisruptBuffer>>,
    fabricate_entities: Query<
        'w,
        's,
        (
            Option<&'static Position>,
            Option<&'static Owner>,
            Option<&'static Drone>,
            Has<DeathMark>,
            Has<SpawningGrace>,
        ),
    >,
}

pub fn status_advance_system(
    mut commands: Commands,
    intents: Option<Res<PendingIntents>>,
    mut statuses: StatusQueries,
    mut legacy_q: Query<(Option<&mut Attributes>, Option<&mut EntityFlags>)>,
    mut cooldowns: Option<ResMut<CustomActionCooldowns>>,
    mut raw_intents: Option<ResMut<PendingSpecialAttack>>,
    mut pending_entities: Option<ResMut<PendingEntityCreation>>,
    mut stable_ids: Option<ResMut<StableEntityIdAllocator>>,
    structure_registry: Option<Res<StructureTypeRegistry>>,
    current_tick: Option<Res<CurrentTick>>,
    world_config: Option<Res<WorldConfig>>,
    plugin_registry: Option<Res<PluginRegistry>>,
) {
    let mut newly_started_fabrications = HashSet::new();
    let resolved_fabricate_structure =
        resolved_fabricate_structure(world_config.as_deref(), plugin_registry.as_deref());
    if let Some(intents) = intents {
        for intent in &intents.intents {
            match intent.kind {
                SpecialAttackKind::Hack => {
                    if let Ok((_, mut state)) = statuses.hack.get_mut(intent.target) {
                        state.remaining_ticks = 5;
                        state.stage = 0;
                    } else {
                        commands.entity(intent.target).insert(HackState::default());
                    }
                }
                SpecialAttackKind::Drain => {
                    let amount = intent.amount;
                    if let Ok((_, mut state)) = statuses.drain.get_mut(intent.target) {
                        state.remaining_ticks = 3;
                        state.amount_per_tick = amount / 3;
                        state.resource = "energy".into();
                    } else {
                        commands.entity(intent.target).insert(DrainState {
                            resource: "energy".into(),
                            amount_per_tick: amount / 3,
                            remaining_ticks: 3,
                        });
                    }
                }
                SpecialAttackKind::Overload => {
                    if let Ok((_, mut state)) = statuses.overload.get_mut(intent.target) {
                        state.remaining_ticks = 3;
                        state.fuel_drain_per_tick = 100;
                        state.fuel_floor = 200;
                    } else {
                        commands.entity(intent.target).insert(OverloadState {
                            fuel_drain_per_tick: 100,
                            fuel_floor: 200,
                            remaining_ticks: 3,
                        });
                    }
                }
                SpecialAttackKind::Debilitate => {
                    if let Ok((_, mut state)) = statuses.debilitate.get_mut(intent.target) {
                        state.remaining_ticks = 50;
                        state.damage_type = "Corrosive".into();
                    } else {
                        commands.entity(intent.target).insert(DebilitateState {
                            damage_type: "Corrosive".into(),
                            remaining_ticks: 50,
                        });
                    }
                }
                SpecialAttackKind::Disrupt => {
                    if let Ok((_, mut state)) = statuses.disrupt.get_mut(intent.target) {
                        state.remaining_ticks = 1;
                    } else {
                        commands.entity(intent.target).insert(DisruptState {
                            body_parts: Vec::new(),
                            remaining_ticks: 1,
                        });
                    }
                    if let Ok((attributes, flags)) = legacy_q.get_mut(intent.target) {
                        if let Some(mut attributes) = attributes {
                            attributes.0.retain(|attribute| {
                                attribute != "Hacking" && !attribute.starts_with("CurrentAction:")
                            });
                            add_attribute(&mut attributes.0, "Disrupted");
                            add_attribute(&mut attributes.0, "Disrupted:duration=1");
                        }
                        if let Some(mut flags) = flags {
                            flags.0.remove("Hacking");
                            flags.0.insert("Disrupted".to_string(), true);
                        } else {
                            commands
                                .entity(intent.target)
                                .insert(EntityFlags(HashMap::from([(
                                    "Disrupted".to_string(),
                                    true,
                                )])));
                        }
                    }
                    if let Some(cooldowns) = cooldowns.as_deref_mut() {
                        let target_id = intent.target.to_bits();
                        cooldowns
                            .0
                            .retain(|(object_id, _), _| *object_id != target_id);
                    }
                }
                SpecialAttackKind::Fortify => {
                    if let Ok((_, mut state)) = statuses.fortify.get_mut(intent.target) {
                        state.remaining_ticks = 3;
                    } else {
                        commands
                            .entity(intent.target)
                            .insert(FortifyState { remaining_ticks: 3 });
                    }
                    commands
                        .entity(intent.target)
                        .remove::<HackState>()
                        .remove::<DebilitateState>();
                    if let Ok((attributes, flags)) = legacy_q.get_mut(intent.target) {
                        if let Some(mut attributes) = attributes {
                            attributes.0.retain(|attribute| {
                                attribute != "Debilitated"
                                    && attribute != "Hacking"
                                    && !attribute.starts_with("Debilitate:")
                                    && !attribute.starts_with("CurrentAction:")
                            });
                            add_attribute(&mut attributes.0, "Fortified");
                            add_attribute(&mut attributes.0, "Fortified:duration=3");
                        }
                        if let Some(mut flags) = flags {
                            flags.0.remove("Debilitated");
                            flags.0.remove("Hacking");
                            flags.0.insert("Fortified".to_string(), true);
                        }
                    }
                }
                SpecialAttackKind::Leech => {
                    // Routed by S14 to S15; Leech has no persistent StatusState.
                }
                SpecialAttackKind::Fabricate => {
                    if statuses
                        .fabricate_entities
                        .get(intent.target)
                        .is_ok_and(|(_, _, _, _, spawning_grace)| !spawning_grace)
                    {
                        let next = FabricateState {
                            source: intent.source,
                            target: intent.target,
                            resolved_structure_type: resolved_fabricate_structure,
                            channel_remaining: 5,
                            started_at_tick: current_tick.as_deref().map_or(0, |tick| tick.0),
                        };
                        if let Ok((_, mut state)) = statuses.fabricate.get_mut(intent.source) {
                            *state = next;
                        } else {
                            commands.entity(intent.source).insert(next);
                        }
                        newly_started_fabrications.insert(intent.source);
                    }
                }
            }
        }
    }

    // Advance all existing statuses: decrement ticks, expire at 0
    for (entity, mut state) in statuses.hack.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<HackState>();
        }
    }
    for (entity, mut state) in statuses.drain.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<DrainState>();
        }
    }
    for (entity, mut state) in statuses.overload.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<OverloadState>();
        }
    }
    for (entity, mut state) in statuses.debilitate.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<DebilitateState>();
        }
    }
    for (entity, mut state) in statuses.disrupt.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<DisruptState>();
        }
    }
    for (entity, mut state) in statuses.fortify.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<FortifyState>();
        }
    }
    let mut fabrication_sources = statuses
        .fabricate
        .iter_mut()
        .map(|(entity, _)| entity)
        .collect::<Vec<_>>();
    fabrication_sources.sort_by_key(|entity| entity.to_bits());
    for source in fabrication_sources {
        if newly_started_fabrications.contains(&source) {
            continue;
        }
        if statuses.disrupt_buffers.contains(source) {
            commands
                .entity(source)
                .remove::<(FabricateState, FabricateBuffer)>();
            continue;
        }
        let Ok(buffer) = statuses.fabricate_buffers.get(source).cloned() else {
            continue;
        };
        let Ok((_, mut state)) = statuses.fabricate.get_mut(source) else {
            continue;
        };
        if !fabricate_channel_is_valid(&state, &buffer, &statuses.fabricate_entities) {
            commands
                .entity(source)
                .remove::<(FabricateState, FabricateBuffer)>();
            continue;
        }
        if buffer.complete {
            state.channel_remaining = state.channel_remaining.saturating_sub(buffer.channel_delta);
            let (_, source_owner, _, _, _) = statuses
                .fabricate_entities
                .get(state.source)
                .expect("validated Fabricate source must still exist");
            let (target_position, _, _, _, _) = statuses
                .fabricate_entities
                .get(state.target)
                .expect("validated Fabricate target must still exist");
            if let (Some(pending_entities), Some(stable_ids), Some(owner), Some(position)) = (
                pending_entities.as_deref_mut(),
                stable_ids.as_deref_mut(),
                source_owner,
                target_position,
            ) {
                pending_entities.entries.push(PendingEntityCreationEntry {
                    stable_id: stable_ids.allocate(),
                    kind: PendingEntityKind::Structure {
                        position: *position,
                        structure: fabricated_structure(
                            state.resolved_structure_type,
                            owner.0,
                            structure_registry.as_deref(),
                        ),
                    },
                });
                commands.entity(state.target).insert(DeathMark);
            }
            commands
                .entity(source)
                .remove::<(FabricateState, FabricateBuffer)>();
        } else {
            state.channel_remaining = state.channel_remaining.saturating_sub(buffer.channel_delta);
            commands.entity(source).remove::<FabricateBuffer>();
        }
    }
    for entity in statuses.disrupt_buffers.iter() {
        commands.entity(entity).remove::<DisruptBuffer>();
    }
    if let Some(raw_intents) = raw_intents.as_deref_mut() {
        raw_intents.intents.clear();
    }
}

fn fabricate_channel_is_valid(
    state: &FabricateState,
    buffer: &FabricateBuffer,
    entities: &Query<(
        Option<&Position>,
        Option<&Owner>,
        Option<&Drone>,
        Has<DeathMark>,
        Has<SpawningGrace>,
    )>,
) -> bool {
    if state.source != buffer.source
        || state.target != buffer.target
        || state.resolved_structure_type != buffer.resolved_structure_type
    {
        return false;
    }
    let Ok((source_position, source_owner, source_drone, source_dead, source_grace)) =
        entities.get(state.source)
    else {
        return false;
    };
    let Ok((target_position, _, target_drone, target_dead, target_grace)) =
        entities.get(state.target)
    else {
        return false;
    };
    let (
        Some(source_position),
        Some(source_owner),
        Some(source_drone),
        Some(target_position),
        Some(target_drone),
    ) = (
        source_position,
        source_owner,
        source_drone,
        target_position,
        target_drone,
    )
    else {
        return false;
    };
    !source_dead
        && !source_grace
        && !target_dead
        && !target_grace
        && source_drone.owner == source_owner.0
        && target_drone.owner != source_owner.0
        && hex_distance(*source_position, *target_position) == 1
}

fn hex_distance(from: Position, to: Position) -> u32 {
    if from.room != to.room {
        return u32::MAX;
    }
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    dx.abs().max(dy.abs()).max((dx + dy).abs()) as u32
}

fn fabricated_structure(
    structure_type: StructureType,
    owner: PlayerId,
    registry: Option<&StructureTypeRegistry>,
) -> Structure {
    let definition = registry.and_then(|registry| registry.get(structure_type));
    let capacity = definition.and_then(|definition| definition.capacity);
    let energy_capacity = matches!(
        structure_type,
        StructureType::SPAWN | StructureType::EXTENSION | StructureType::TOWER
    )
    .then_some(capacity)
    .flatten();
    Structure {
        structure_type,
        owner: Some(owner),
        hits: 1,
        hits_max: definition.map_or(5_000, |definition| definition.hits),
        energy: energy_capacity.map(|_| 0),
        energy_capacity,
        cooldown: 0,
    }
}

fn resolved_fabricate_structure(
    world_config: Option<&WorldConfig>,
    plugin_registry: Option<&PluginRegistry>,
) -> StructureType {
    let configured = world_config
        .zip(plugin_registry)
        .and_then(|(world_config, registry)| {
            registry
                .lock
                .runtime_config_for_world(&world_config.mods)
                .ok()
        })
        .and_then(|runtime| runtime.special_attacks)
        .and_then(|config| {
            config
                .fabricate_allowed_output_structures
                .into_iter()
                .next()
        });
    match configured.as_deref() {
        Some("Storage") => StructureType::STORAGE,
        Some("Wall") => StructureType("Wall"),
        Some("Tower") | None => StructureType::TOWER,
        Some(_) => StructureType::TOWER,
    }
}

fn add_attribute(attributes: &mut Vec<String>, value: &str) {
    if !attributes.iter().any(|attribute| attribute == value) {
        attributes.push(value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{PendingEntityCreation, PendingEntityKind, StableEntityIdAllocator};
    use crate::systems::{ResolvedIntent, fabricate_buffer_system, special_attack_reducer};
    use swarm_engine_api::ids::{BodyPart, RoomId};
    use swarm_engine_plugin_sdk::buffers::{
        PendingDamage, PendingSpecialAttack, StatusActionIntent,
    };
    use swarm_engine_plugin_sdk::components::{
        BodyPartRegistry, DeathMark, Drone, Owner, Position, StructureType,
    };

    #[test]
    fn status_advance_resets_hack_intent() {
        let mut app = App::new();
        let e = app
            .world_mut()
            .spawn(HackState {
                stage: 1,
                remaining_ticks: 2,
            })
            .id();
        app.insert_resource(PendingIntents {
            intents: vec![ResolvedIntent {
                kind: SpecialAttackKind::Hack,
                source: e,
                target: e,
                amount: 0,
            }],
        });
        app.insert_resource(PendingDamage { entries: vec![] });

        app.add_systems(Update, status_advance_system);
        app.update();

        let hack = app.world().entity(e).get::<HackState>().unwrap();
        // Reset to 5, then advance: 5-1=4
        assert_eq!(hack.remaining_ticks, 4);
        assert_eq!(hack.stage, 0);
    }

    #[test]
    fn status_advance_expires_when_ticks_hit_zero() {
        let mut app = App::new();
        app.world_mut().spawn(FortifyState { remaining_ticks: 1 });
        app.insert_resource(PendingIntents { intents: vec![] });
        app.insert_resource(PendingDamage { entries: vec![] });

        app.add_systems(Update, status_advance_system);
        app.update();

        let mut query = app.world_mut().query::<&FortifyState>();
        assert_eq!(query.iter(app.world()).count(), 0);
    }

    #[test]
    fn status_advance_extends_existing_status() {
        let mut app = App::new();
        let e = app
            .world_mut()
            .spawn(DebilitateState {
                damage_type: "Kinetic".into(),
                remaining_ticks: 10,
            })
            .id();
        app.insert_resource(PendingIntents {
            intents: vec![ResolvedIntent {
                kind: SpecialAttackKind::Debilitate,
                source: e,
                target: e,
                amount: 0,
            }],
        });
        app.insert_resource(PendingDamage { entries: vec![] });

        app.add_systems(Update, status_advance_system);
        app.update();

        let state = app.world().entity(e).get::<DebilitateState>().unwrap();
        // Reset to 50, then advance: 50-1=49
        assert_eq!(state.remaining_ticks, 49);
    }

    #[test]
    fn fabricate_starts_next_tick_and_completes_after_five_decrements() {
        let mut app = App::new();
        let body_registry = BodyPartRegistry::default();
        let source = app
            .world_mut()
            .spawn((
                Position {
                    x: 10,
                    y: 10,
                    room: RoomId(0),
                },
                Owner(1),
                Drone::new(1, vec![BodyPart::Work, BodyPart::Carry], &body_registry),
            ))
            .id();
        let target_position = Position {
            x: 11,
            y: 10,
            room: RoomId(0),
        };
        let target = app
            .world_mut()
            .spawn((
                target_position,
                Owner(2),
                Drone::new(2, vec![BodyPart::Move], &body_registry),
            ))
            .id();
        app.insert_resource(PendingSpecialAttack {
            intents: vec![StatusActionIntent {
                kind: SpecialAttackKind::Fabricate,
                source,
                target,
                owner: 1,
                amount: 0,
            }],
        });
        app.insert_resource(PendingIntents::default());
        app.insert_resource(crate::systems::PendingLeechCombat::default());
        app.insert_resource(PendingEntityCreation::default());
        app.insert_resource(StableEntityIdAllocator { next: 41 });
        app.add_systems(
            Update,
            (
                special_attack_reducer,
                fabricate_buffer_system,
                status_advance_system,
            )
                .chain(),
        );

        app.update();

        let state = app.world().entity(source).get::<FabricateState>().unwrap();
        assert_eq!(state.source, source);
        assert_eq!(state.target, target);
        assert_eq!(state.resolved_structure_type, StructureType::TOWER);
        assert_eq!(state.channel_remaining, 5);
        assert!(app.world().entity(target).get::<DeathMark>().is_none());

        for expected_remaining in (1..=4).rev() {
            app.update();
            let state = app.world().entity(source).get::<FabricateState>().unwrap();
            assert_eq!(state.channel_remaining, expected_remaining);
            assert!(app.world().entity(target).get::<DeathMark>().is_none());
            assert!(
                app.world()
                    .resource::<PendingEntityCreation>()
                    .entries
                    .is_empty()
            );
        }

        app.update();

        assert!(app.world().entity(source).get::<FabricateState>().is_none());
        assert!(app.world().entity(target).get::<DeathMark>().is_some());
        let pending = app.world().resource::<PendingEntityCreation>();
        assert_eq!(pending.entries.len(), 1);
        assert_eq!(pending.entries[0].stable_id.0, 41);
        let PendingEntityKind::Structure {
            position,
            structure,
        } = &pending.entries[0].kind
        else {
            panic!("Fabricate must queue a structure replacement");
        };
        assert_eq!(*position, target_position);
        assert_eq!(structure.structure_type, StructureType::TOWER);
        assert_eq!(structure.owner, Some(1));
    }

    #[test]
    fn disrupt_cancels_fabricate_before_completion() {
        let mut app = App::new();
        let body_registry = BodyPartRegistry::default();
        let source = app
            .world_mut()
            .spawn((
                Position {
                    x: 10,
                    y: 10,
                    room: RoomId(0),
                },
                Owner(1),
                Drone::new(1, vec![BodyPart::Work, BodyPart::Carry], &body_registry),
            ))
            .id();
        let target = app
            .world_mut()
            .spawn((
                Position {
                    x: 11,
                    y: 10,
                    room: RoomId(0),
                },
                Owner(2),
                Drone::new(2, vec![BodyPart::Move], &body_registry),
            ))
            .id();
        app.insert_resource(PendingSpecialAttack {
            intents: vec![StatusActionIntent {
                kind: SpecialAttackKind::Fabricate,
                source,
                target,
                owner: 1,
                amount: 0,
            }],
        });
        app.insert_resource(PendingIntents::default());
        app.insert_resource(crate::systems::PendingLeechCombat::default());
        app.insert_resource(PendingEntityCreation::default());
        app.insert_resource(StableEntityIdAllocator::default());
        app.add_systems(
            Update,
            (
                special_attack_reducer,
                crate::systems::disrupt_buffer_system,
                fabricate_buffer_system,
                status_advance_system,
            )
                .chain(),
        );

        app.update();
        app.world_mut()
            .resource_mut::<PendingSpecialAttack>()
            .intents
            .push(StatusActionIntent {
                kind: SpecialAttackKind::Disrupt,
                source: target,
                target: source,
                owner: 2,
                amount: 0,
            });
        app.update();

        assert!(app.world().entity(source).get::<FabricateState>().is_none());
        assert!(app.world().entity(target).get::<DeathMark>().is_none());
        assert!(
            app.world()
                .resource::<PendingEntityCreation>()
                .entries
                .is_empty()
        );
    }
}
