use bevy::prelude::*;
use swarm_engine_plugin_sdk::components::Owner;

use crate::components::DrainState;
use crate::resource_ledger::{ResourceLedger, ResourceOperation};
use crate::resources::CurrentTick;
use crate::resources::PlayerLocalStorage;

/// S17 drain_system — per-tick resource drain from DrainState targets.
///
/// Reads DrainState (written by S22 status_advance_system) and drains
/// resources from the target's local storage each tick.
pub fn drain_system(
    mut storage: ResMut<PlayerLocalStorage>,
    current_tick: Option<Res<CurrentTick>>,
    mut ledger: Option<ResMut<ResourceLedger>>,
    targets: Query<(&DrainState, &Owner)>,
) {
    let tick = current_tick
        .as_deref()
        .map(|tick| tick.0)
        .unwrap_or_default();
    for (state, owner) in targets.iter() {
        if state.remaining_ticks == 0 || state.amount_per_tick == 0 {
            continue;
        }
        let player_storage = storage.0.entry(owner.0).or_default();
        let amount = state.amount_per_tick;
        if let Some(current) = player_storage.get_mut(&state.resource) {
            let drained = amount.min(*current);
            *current = current.saturating_sub(amount);
            if drained > 0
                && let Some(ledger) = ledger.as_mut()
            {
                ledger.record(
                    tick,
                    Some(owner.0),
                    None,
                    &state.resource,
                    i64::from(drained),
                    ResourceOperation::UpkeepDeduction,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use swarm_engine_api::ids::RoomId;
    use swarm_engine_plugin_sdk::components::Position;

    #[test]
    fn drain_reduces_player_local_storage() {
        let mut app = App::new();
        let mut storage = PlayerLocalStorage(IndexMap::new());
        storage
            .0
            .entry(1)
            .or_default()
            .insert("energy".to_string(), 500);
        app.insert_resource(storage);

        app.world_mut().spawn((
            DrainState {
                resource: "energy".into(),
                amount_per_tick: 50,
                remaining_ticks: 3,
            },
            Owner(1),
            Position {
                x: 0,
                y: 0,
                room: RoomId(0),
            },
        ));

        app.add_systems(Update, drain_system);
        app.update();

        let storage = app.world().resource::<PlayerLocalStorage>();
        let player = storage.0.get(&1).unwrap();
        assert_eq!(
            player.get("energy").copied().unwrap_or(0),
            450,
            "should drain 50 energy"
        );
    }

    #[test]
    fn drain_stops_when_remaining_ticks_zero() {
        let mut app = App::new();
        let mut storage = PlayerLocalStorage(IndexMap::new());
        storage
            .0
            .entry(1)
            .or_default()
            .insert("energy".to_string(), 500);
        app.insert_resource(storage);

        app.world_mut().spawn((
            DrainState {
                resource: "energy".into(),
                amount_per_tick: 50,
                remaining_ticks: 0,
            },
            Owner(1),
            Position {
                x: 0,
                y: 0,
                room: RoomId(0),
            },
        ));

        app.add_systems(Update, drain_system);
        app.update();

        let storage = app.world().resource::<PlayerLocalStorage>();
        let player = storage.0.get(&1).unwrap();
        assert_eq!(
            player.get("energy").copied().unwrap_or(0),
            500,
            "should not drain when ticks=0"
        );
    }
}
