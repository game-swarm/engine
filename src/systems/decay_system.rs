use bevy::prelude::*;

use crate::components::{Drone, Structure};

/// Decay system — handles cooldown/fatigue reduction, drone aging, and structure maintenance.
/// Active drones (that executed a command this tick) age at 1.1x rate.
pub fn decay_system(mut drones: Query<&mut Drone>, mut structures: Query<&mut Structure>) {
    for mut drone in drones.iter_mut() {
        drone.fatigue = drone.fatigue.saturating_sub(1);
        // Active aging: 110% if drone executed a command this tick
        if drone.executed_command_this_tick {
            drone.aging_remainder = drone.aging_remainder.wrapping_add(1); // +0.1
        }
        // Age increment: 1.0 base + aging_remainder carry
        let mut age_inc: u32 = 1;
        if drone.aging_remainder >= 10 {
            age_inc += 1;
            drone.aging_remainder -= 10;
        }
        drone.age = drone.age.saturating_add(age_inc);
        // Reset for next tick
        drone.executed_command_this_tick = false;
    }

    for mut structure in structures.iter_mut() {
        structure.cooldown = structure.cooldown.saturating_sub(1);
    }
}
