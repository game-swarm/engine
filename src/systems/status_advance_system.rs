use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use std::collections::HashMap;

use crate::command::CustomActionCooldowns;
use crate::components::{
    Attributes, DebilitateState, DisruptState, DrainState, EntityFlags, FabricateState,
    FortifyState, HackState, LeechState, OverloadState,
};
use crate::systems::{PendingIntents, SpecialAttackKind};

/// S22 status_advance_system — UNIQUE WRITER for all StatusState components.
///
/// This system is the single authority that writes HackState, DrainState,
/// OverloadState, DebilitateState, DisruptState, FortifyState, LeechState,
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
    leech: Query<'w, 's, (Entity, &'static mut LeechState)>,
    fabricate: Query<'w, 's, (Entity, &'static mut FabricateState)>,
}

pub fn status_advance_system(
    mut commands: Commands,
    intents: Option<Res<PendingIntents>>,
    mut statuses: StatusQueries,
    mut legacy_q: Query<(Option<&mut Attributes>, Option<&mut EntityFlags>)>,
    mut cooldowns: Option<ResMut<CustomActionCooldowns>>,
) {
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
                    if let Ok((_, mut state)) = statuses.leech.get_mut(intent.target) {
                        state.remaining_ticks = 3;
                        state.amount_per_tick = intent.amount / 3;
                        state.age_acceleration = state.age_acceleration.max(1);
                    } else {
                        commands.entity(intent.target).insert(LeechState {
                            amount_per_tick: intent.amount / 3,
                            age_acceleration: 1,
                            remaining_ticks: 3,
                            ..Default::default()
                        });
                    }
                }
                SpecialAttackKind::Fabricate => {
                    if let Ok((_, mut state)) = statuses.fabricate.get_mut(intent.target) {
                        state.remaining_ticks = 1;
                    } else {
                        commands.entity(intent.target).insert(FabricateState {
                            remaining_ticks: 1,
                            ..Default::default()
                        });
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
    for (entity, mut state) in statuses.leech.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks > 0 {
            state.age_acceleration = state.age_acceleration.saturating_add(1);
        } else {
            commands.entity(entity).remove::<LeechState>();
        }
    }
    for (entity, mut state) in statuses.fabricate.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<FabricateState>();
        }
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
    use crate::systems::PendingDamage;
    use crate::systems::ResolvedIntent;

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
}
