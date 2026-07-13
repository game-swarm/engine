use bevy::prelude::*;

use crate::components::{Controller, StructureType};
use crate::resources::PlayerGlobalStorage;
use crate::world::WorldConfig;

pub const DEFAULT_CONTROLLER_DOWNGRADE_TIMER: u32 = 5_000;

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingControllerUpgrade(pub Vec<(u64, u32)>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RclLevel {
    pub level: u8,
    pub progress_total: u32,
    pub unlocked_buildings: &'static [StructureType],
    pub max_drones: u32,
}

const RCL1: &[StructureType] = &[StructureType::Spawn];
const RCL2: &[StructureType] = &[StructureType::Spawn, StructureType::Extension];
const RCL3: &[StructureType] = &[
    StructureType::Spawn,
    StructureType::Extension,
    StructureType::Tower,
    StructureType::Storage,
];
const RCL4: &[StructureType] = &[
    StructureType::Spawn,
    StructureType::Extension,
    StructureType::Tower,
    StructureType::Storage,
    StructureType::Link,
];
const RCL5: &[StructureType] = &[
    StructureType::Spawn,
    StructureType::Extension,
    StructureType::Tower,
    StructureType::Storage,
    StructureType::Link,
    StructureType::Terminal,
    StructureType::Observer,
];
const RCL6: &[StructureType] = &[
    StructureType::Spawn,
    StructureType::Extension,
    StructureType::Tower,
    StructureType::Storage,
    StructureType::Link,
    StructureType::Terminal,
    StructureType::Observer,
    StructureType::Extractor,
    StructureType::Lab,
    StructureType::Factory,
];
const RCL7: &[StructureType] = &[
    StructureType::Spawn,
    StructureType::Extension,
    StructureType::Tower,
    StructureType::Storage,
    StructureType::Link,
    StructureType::Terminal,
    StructureType::Observer,
    StructureType::Extractor,
    StructureType::Lab,
    StructureType::Factory,
    StructureType::PowerSpawn,
];
const RCL8: &[StructureType] = &[
    StructureType::Spawn,
    StructureType::Extension,
    StructureType::Tower,
    StructureType::Storage,
    StructureType::Link,
    StructureType::Terminal,
    StructureType::Observer,
    StructureType::Extractor,
    StructureType::Lab,
    StructureType::Factory,
    StructureType::PowerSpawn,
    StructureType::Nuker,
];

pub const RCL_TABLE: [RclLevel; 8] = [
    RclLevel {
        level: 1,
        progress_total: 0,
        unlocked_buildings: RCL1,
        max_drones: 50,
    },
    RclLevel {
        level: 2,
        progress_total: 200,
        unlocked_buildings: RCL2,
        max_drones: 100,
    },
    RclLevel {
        level: 3,
        progress_total: 500,
        unlocked_buildings: RCL3,
        max_drones: 200,
    },
    RclLevel {
        level: 4,
        progress_total: 1_500,
        unlocked_buildings: RCL4,
        max_drones: 300,
    },
    RclLevel {
        level: 5,
        progress_total: 5_000,
        unlocked_buildings: RCL5,
        max_drones: 400,
    },
    RclLevel {
        level: 6,
        progress_total: 15_000,
        unlocked_buildings: RCL6,
        max_drones: 500,
    },
    RclLevel {
        level: 7,
        progress_total: 50_000,
        unlocked_buildings: RCL7,
        max_drones: 500,
    },
    RclLevel {
        level: 8,
        progress_total: 150_000,
        unlocked_buildings: RCL8,
        max_drones: 500,
    },
];

pub fn rcl_level(level: u8) -> &'static RclLevel {
    &RCL_TABLE[(level.clamp(1, 8) - 1) as usize]
}

pub fn rcl_progress_total(level: u8) -> u32 {
    rcl_level(level).progress_total
}

pub fn controller_reserve_to_rcl_progress(controller_reserve: u32, ticks: u32) -> u32 {
    u64::from(controller_reserve)
        .saturating_mul(u64::from(ticks))
        .min(u64::from(u32::MAX)) as u32
}

pub fn controller_system(
    mut pending: ResMut<PendingControllerUpgrade>,
    mut controllers: Query<&mut Controller>,
    config: Res<WorldConfig>,
    mut global_storage: ResMut<PlayerGlobalStorage>,
) {
    for (entity_bits, amount) in pending.0.drain(..) {
        if let Ok(mut controller) = controllers.get_mut(Entity::from_bits(entity_bits))
            && controller.owner.is_some()
            && controller.level < 8
        {
            controller.progress = controller.progress.saturating_add(amount);
            while controller.level < 8
                && controller.progress >= rcl_progress_total(controller.level + 1)
            {
                controller.level += 1;
                controller.progress_total = rcl_progress_total((controller.level + 1).min(8));
                controller.downgrade_timer = DEFAULT_CONTROLLER_DOWNGRADE_TIMER;
            }
        }
    }

    for mut controller in &mut controllers {
        if controller.level == 0 {
            controller.level = 1;
        }
        controller.progress_total = rcl_progress_total((controller.level + 1).min(8));
        if controller.owner.is_none() {
            if controller.downgrade_timer > 0 {
                controller.downgrade_timer -= 1;
            } else if controller.level > 1 {
                controller.level -= 1;
                controller.progress = 0;
                controller.progress_total = rcl_progress_total((controller.level + 1).min(8));
                controller.downgrade_timer = DEFAULT_CONTROLLER_DOWNGRADE_TIMER;
            }
        } else if let Some(owner) = controller.owner
            && config.empire_upkeep.controller_passive_income > 0
        {
            *global_storage
                .0
                .entry(owner)
                .or_default()
                .entry(config.empire_upkeep.resource.clone())
                .or_default() += config.empire_upkeep.controller_passive_income;
        }
    }
}
