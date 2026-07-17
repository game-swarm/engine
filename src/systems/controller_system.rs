use bevy::prelude::*;

use crate::components::{Controller, StructureType};
use crate::resource_ledger::{ResourceLedger, ResourceOperation};
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
    current_tick: Res<crate::resources::CurrentTick>,
    mut global_storage: ResMut<PlayerGlobalStorage>,
    mut ledger: ResMut<ResourceLedger>,
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
        } else if let Some(owner) = controller.owner {
            let passive_income = config
                .empire_upkeep
                .controller_passive_income_for_rcl(controller.level);
            if passive_income == 0 {
                continue;
            }

            let resource = config.empire_upkeep.resource.clone();
            let balance = global_storage
                .0
                .entry(owner)
                .or_default()
                .entry(resource.clone())
                .or_default();
            let previous = *balance;
            *balance = balance.saturating_add(passive_income);
            let delivered = balance.saturating_sub(previous);
            if delivered > 0 {
                ledger.record(
                    current_tick.0,
                    None,
                    Some(owner),
                    &resource,
                    i64::from(delivered),
                    ResourceOperation::ControllerPassiveIncome,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{Controller, PlayerId};
    use crate::resources::CurrentTick;
    use bevy::prelude::{App, Update};

    fn test_controller(level: u8, owner: Option<PlayerId>) -> Controller {
        Controller {
            owner,
            level,
            progress: 0,
            progress_total: rcl_progress_total((level + 1).min(8)),
            downgrade_timer: DEFAULT_CONTROLLER_DOWNGRADE_TIMER,
            safe_mode: 0,
            safe_mode_available: 0,
            safe_mode_cooldown: 0,
            repair_capacity: 0,
            repair_range: 0,
            repair_per_drone: 0,
        }
    }

    fn app_with_controller(config: WorldConfig, controller: Controller) -> App {
        let mut app = App::new();
        app.insert_resource(PendingControllerUpgrade::default());
        app.insert_resource(config);
        app.insert_resource(CurrentTick(77));
        app.insert_resource(PlayerGlobalStorage::default());
        app.insert_resource(ResourceLedger::default());
        app.world_mut().spawn(controller);
        app.add_systems(Update, controller_system);
        app
    }

    #[test]
    fn controller_income_uses_base_plus_rcl_bonus_and_records_ledger() {
        let mut app = app_with_controller(WorldConfig::default(), test_controller(3, Some(7)));

        app.update();

        let storage = app.world().resource::<PlayerGlobalStorage>();
        assert_eq!(storage.0.get(&7).unwrap().get("Energy"), Some(&55));

        let ledger = app.world().resource::<ResourceLedger>();
        assert_eq!(ledger.ops.len(), 1);
        let entry = &ledger.ops[0];
        assert_eq!(entry.tick, 77);
        assert_eq!(entry.source_player, None);
        assert_eq!(entry.target_player, Some(7));
        assert_eq!(entry.resource, "Energy");
        assert_eq!(entry.amount, 55);
        assert_eq!(entry.amount_requested, 55);
        assert_eq!(entry.amount_delivered, 55);
        assert_eq!(entry.operation, ResourceOperation::ControllerPassiveIncome);
        assert_eq!(
            ledger.balance_delta.get(&7).unwrap().get("Energy"),
            Some(&55)
        );
    }

    #[test]
    fn controller_income_saturates_storage_and_ledgers_delivered_amount() {
        let mut app = app_with_controller(WorldConfig::default(), test_controller(1, Some(7)));
        app.world_mut()
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .entry(7)
            .or_default()
            .insert("Energy".to_string(), u32::MAX - 2);

        app.update();

        let storage = app.world().resource::<PlayerGlobalStorage>();
        assert_eq!(storage.0.get(&7).unwrap().get("Energy"), Some(&u32::MAX));

        let ledger = app.world().resource::<ResourceLedger>();
        assert_eq!(ledger.ops.len(), 1);
        assert_eq!(ledger.ops[0].amount, 2);
        assert_eq!(ledger.ops[0].amount_delivered, 2);
        assert_eq!(
            ledger.balance_delta.get(&7).unwrap().get("Energy"),
            Some(&2)
        );
    }
}
