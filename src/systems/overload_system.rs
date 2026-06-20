use bevy::prelude::*;

use crate::components::{OverloadState, Owner};
use crate::resources::PlayerLocalStorage;

/// S18 overload_system — per-tick fuel drain from OverloadState targets.
///
/// Reads OverloadState (written by S22 status_advance_system) and drains
/// fuel from the target player's local storage each tick, down to a floor.
pub fn overload_system(
    mut storage: ResMut<PlayerLocalStorage>,
    targets: Query<(&OverloadState, &Owner)>,
) {
    for (state, owner) in targets.iter() {
        if state.remaining_ticks == 0 {
            continue;
        }
        let player_storage = storage.0.entry(owner.0).or_default();
        let current: u32 = player_storage
            .get("energy")
            .or_else(|| player_storage.get("fuel"))
            .copied()
            .unwrap_or(0);

        if current <= state.fuel_floor {
            continue;
        }
        let drain = state
            .fuel_drain_per_tick
            .min(current.saturating_sub(state.fuel_floor));
        if let Some(energy) = player_storage.get_mut("energy") {
            *energy = energy.saturating_sub(drain);
        } else if let Some(fuel) = player_storage.get_mut("fuel") {
            *fuel = fuel.saturating_sub(drain);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::Position;
    use indexmap::IndexMap;

    #[test]
    fn overload_drains_fuel_to_floor() {
        let mut app = App::new();
        let mut storage = PlayerLocalStorage(IndexMap::new());
        storage.0.entry(1).or_default().insert("energy".to_string(), 500);
        app.insert_resource(storage);

        app.world_mut().spawn((
            OverloadState { fuel_drain_per_tick: 100, fuel_floor: 200, remaining_ticks: 3 },
            Owner(1),
            Position { x: 0, y: 0, room: crate::components::RoomId(0) },
        ));

        app.add_systems(Update, overload_system);
        app.update();

        let storage = app.world().resource::<PlayerLocalStorage>();
        let player = storage.0.get(&1).unwrap();
        assert_eq!(player.get("energy").copied().unwrap_or(0), 400, "should drain 100 fuel");
    }

    #[test]
    fn overload_stops_at_fuel_floor() {
        let mut app = App::new();
        let mut storage = PlayerLocalStorage(IndexMap::new());
        storage.0.entry(1).or_default().insert("energy".to_string(), 250);
        app.insert_resource(storage);

        app.world_mut().spawn((
            OverloadState { fuel_drain_per_tick: 100, fuel_floor: 200, remaining_ticks: 3 },
            Owner(1),
            Position { x: 0, y: 0, room: crate::components::RoomId(0) },
        ));

        app.add_systems(Update, overload_system);
        app.update();

        let storage = app.world().resource::<PlayerLocalStorage>();
        let player = storage.0.get(&1).unwrap();
        assert_eq!(player.get("energy").copied().unwrap_or(0), 200, "should stop at floor 200");
    }
}
