use bevy::prelude::*;

use crate::components::{Drone, Structure};

pub fn decay_system(mut drones: Query<&mut Drone>, mut structures: Query<&mut Structure>) {
    for mut drone in drones.iter_mut() {
        drone.fatigue = drone.fatigue.saturating_sub(1);
        drone.age = drone.age.saturating_add(1);
    }

    for mut structure in structures.iter_mut() {
        structure.cooldown = structure.cooldown.saturating_sub(1);
    }
}
