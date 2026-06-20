use bevy::prelude::*;

use crate::components::{
    DebilitateState, DisruptState, DrainState, FortifyState, HackState, OverloadState,
};
use crate::systems::{PendingDamage, PendingIntents, SpecialAttackKind};

/// S22 status_advance_system — UNIQUE WRITER for all StatusState components.
///
/// This system is the single authority that writes HackState, DrainState,
/// OverloadState, DebilitateState, DisruptState, and FortifyState (R22 B3).
/// It reads canonical sorted intents from S14's PendingIntents buffer,
/// resets/extends existing statuses, advances remaining_ticks, and
/// removes expired statuses. Initial component insertion happens via
/// command validation (apply_hack etc).
pub fn status_advance_system(
    mut commands: Commands,
    intents: Option<Res<PendingIntents>>,
    mut hack_q: Query<(Entity, &mut HackState)>,
    mut drain_q: Query<(Entity, &mut DrainState)>,
    mut overload_q: Query<(Entity, &mut OverloadState)>,
    mut debilitate_q: Query<(Entity, &mut DebilitateState)>,
    mut disrupt_q: Query<(Entity, &mut DisruptState)>,
    mut fortify_q: Query<(Entity, &mut FortifyState)>,
) {
    if let Some(intents) = intents {
        for intent in &intents.intents {
            match intent.kind {
                SpecialAttackKind::Hack => {
                    if let Ok((_, mut state)) = hack_q.get_mut(intent.target) {
                        state.remaining_ticks = 5;
                        state.stage = 0;
                    }
                }
                SpecialAttackKind::Drain => {
                    let amount = intent.amount;
                    if let Ok((_, mut state)) = drain_q.get_mut(intent.target) {
                        state.remaining_ticks = 3;
                        state.amount_per_tick = amount / 3;
                        state.resource = "energy".into();
                    }
                }
                SpecialAttackKind::Overload => {
                    if let Ok((_, mut state)) = overload_q.get_mut(intent.target) {
                        state.remaining_ticks = 3;
                        state.fuel_drain_per_tick = 100;
                        state.fuel_floor = 200;
                    }
                }
                SpecialAttackKind::Debilitate => {
                    if let Ok((_, mut state)) = debilitate_q.get_mut(intent.target) {
                        state.remaining_ticks = 50;
                        state.damage_type = "Corrosive".into();
                    }
                }
                SpecialAttackKind::Disrupt => {
                    if let Ok((_, mut state)) = disrupt_q.get_mut(intent.target) {
                        state.remaining_ticks = 1;
                    }
                }
                SpecialAttackKind::Fortify => {
                    if let Ok((_, mut state)) = fortify_q.get_mut(intent.target) {
                        state.remaining_ticks = 3;
                    }
                }
            }
        }
    }

    // Advance all existing statuses: decrement ticks, expire at 0
    for (entity, mut state) in hack_q.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<HackState>();
        }
    }
    for (entity, mut state) in drain_q.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<DrainState>();
        }
    }
    for (entity, mut state) in overload_q.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<OverloadState>();
        }
    }
    for (entity, mut state) in debilitate_q.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<DebilitateState>();
        }
    }
    for (entity, mut state) in disrupt_q.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<DisruptState>();
        }
    }
    for (entity, mut state) in fortify_q.iter_mut() {
        state.remaining_ticks = state.remaining_ticks.saturating_sub(1);
        if state.remaining_ticks == 0 {
            commands.entity(entity).remove::<FortifyState>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systems::ResolvedIntent;

    #[test]
    fn status_advance_resets_hack_intent() {
        let mut app = App::new();
        let e = app
            .world_mut()
            .spawn(HackState { stage: 1, remaining_ticks: 2 })
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
            .spawn(DebilitateState { damage_type: "Kinetic".into(), remaining_ticks: 10 })
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
