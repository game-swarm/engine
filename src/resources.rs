use bevy::prelude::Resource as BevyResource;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::{BodyPart, PlayerId, StructureType};

pub type ResourceName = String;
pub type ResourceAmount = u32;
pub type ResourceCost = IndexMap<ResourceName, ResourceAmount>;

pub const TRANSFER_TO_GLOBAL_TICKS: Tick = 10;
pub const TRANSFER_FROM_GLOBAL_TICKS: Tick = 5;
pub const TRANSFER_TO_GLOBAL_FEE_PER_10_000: u32 = 100;
pub const TRANSFER_FROM_GLOBAL_FEE_PER_10_000: u32 = 500;
pub const GLOBAL_STORAGE_INTERCEPT_RANGE: u32 = 3;
pub const DEFAULT_MAX_PVE_OUTPUT_PER_TICK: ResourceAmount = ResourceAmount::MAX;
pub const ALLIED_TRANSFER_FEE_BP: u32 = 200;
pub const ALLIED_TRANSFER_DELAY: Tick = 200;
pub const ALLIED_TRANSFER_COOLDOWN: Tick = 500;
pub const ALLIED_DAILY_CAP: u32 = 10_000;

#[derive(BevyResource, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentTick(pub Tick);

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PveOutputTracker {
    pub max_pve_output_per_tick: ResourceAmount,
    pub tick: Tick,
    pub produced_this_tick: ResourceAmount,
    pub discarded_this_tick: ResourceAmount,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceDef {
    pub name: ResourceName,
    pub display_name: String,
    pub category: String,
    pub starting_amount: ResourceAmount,
    pub max_storage: ResourceAmount,
    pub decay_rate: f32,
    pub tradeable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ActionCosts {
    pub spawn: ResourceCost,
    pub build: IndexMap<StructureType, ResourceCost>,
    pub body_part: IndexMap<BodyPart, ResourceCost>,
    pub code_update: ResourceCost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDef {
    pub name: String,
    pub produces: ResourceCost,
    pub capacity: ResourceAmount,
    pub regeneration: u32,
}

#[derive(BevyResource, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceRegistry {
    pub resources: IndexMap<ResourceName, ResourceDef>,
    pub action_costs: ActionCosts,
    pub sources: IndexMap<String, SourceDef>,
}

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalStorageConfig {
    pub enabled: bool,
    pub namespace: String,
    pub intercept_enabled: bool,
    pub intercept_range: u32,
    pub capacity: ResourceAmount,
    pub transfer_to_global_ticks: Tick,
    pub transfer_from_global_ticks: Tick,
    pub transfer_to_global_fee_per_10_000: u32,
    pub transfer_from_global_fee_per_10_000: u32,
    pub tax_anchors: [StorageTaxAnchor; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageTaxAnchor {
    pub utilization_ppm: u32,
    pub marginal_rate_bp: u32,
}

#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerLocalStorage(pub IndexMap<PlayerId, ResourceCost>);

#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerGlobalStorage(pub IndexMap<PlayerId, ResourceCost>);

#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingGlobalTransfers(pub Vec<PendingGlobalTransfer>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingGlobalTransfer {
    pub player_id: PlayerId,
    pub direction: GlobalTransferDirection,
    pub resource: ResourceName,
    pub amount: ResourceAmount,
    pub deliver_amount: ResourceAmount,
    pub remaining_ticks: Tick,
    pub start: crate::components::Position,
    pub end: crate::components::Position,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GlobalTransferDirection {
    ToGlobal,
    FromGlobal,
}

// ── Allied Transfer tracking ──

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAlliedTransfer {
    pub from_player: PlayerId,
    pub to_player: PlayerId,
    pub resource: ResourceName,
    pub amount: ResourceAmount,
    pub deliver_amount: ResourceAmount,
    pub remaining_ticks: Tick,
}

#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAlliedTransfers(pub Vec<PendingAlliedTransfer>);

/// Cooldowns: (from_player, to_player) → next_allowed_tick
#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlliedTransferCooldowns(pub IndexMap<(PlayerId, PlayerId), Tick>);

/// Daily usage tracking per sender: sum of all allied transfers per "day" (1440 ticks = 24h at 1 tick/min)
#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlliedTransferDailyUsage(pub IndexMap<PlayerId, u32>);

/// Tracks which ticks have been counted for daily usage (avoids double-counting replays)
#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlliedTransferDailyTick(pub Tick);

impl Default for GlobalStorageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            namespace: "default".to_string(),
            intercept_enabled: true,
            intercept_range: GLOBAL_STORAGE_INTERCEPT_RANGE,
            capacity: 100_000,
            transfer_to_global_ticks: TRANSFER_TO_GLOBAL_TICKS,
            transfer_from_global_ticks: TRANSFER_FROM_GLOBAL_TICKS,
            transfer_to_global_fee_per_10_000: TRANSFER_TO_GLOBAL_FEE_PER_10_000,
            transfer_from_global_fee_per_10_000: TRANSFER_FROM_GLOBAL_FEE_PER_10_000,
            tax_anchors: [
                StorageTaxAnchor {
                    utilization_ppm: 300_000,
                    marginal_rate_bp: 0,
                },
                StorageTaxAnchor {
                    utilization_ppm: 600_000,
                    marginal_rate_bp: 1,
                },
                StorageTaxAnchor {
                    utilization_ppm: 850_000,
                    marginal_rate_bp: 5,
                },
                StorageTaxAnchor {
                    utilization_ppm: 1_000_000,
                    marginal_rate_bp: 20,
                },
            ],
        }
    }
}

impl Default for PveOutputTracker {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_PVE_OUTPUT_PER_TICK)
    }
}

impl PveOutputTracker {
    pub fn new(max_pve_output_per_tick: ResourceAmount) -> Self {
        Self {
            max_pve_output_per_tick,
            tick: 0,
            produced_this_tick: 0,
            discarded_this_tick: 0,
        }
    }

    pub fn reset_for_tick(&mut self, tick: Tick) {
        if self.tick != tick {
            self.tick = tick;
            self.produced_this_tick = 0;
            self.discarded_this_tick = 0;
        }
    }

    pub fn cap_output(&mut self, tick: Tick, output: &ResourceCost) -> ResourceCost {
        self.reset_for_tick(tick);

        let mut capped = ResourceCost::new();
        let mut discarded = ResourceCost::new();

        for (resource, amount) in output {
            if is_pve_output_capped_resource(resource) {
                let remaining = self
                    .max_pve_output_per_tick
                    .saturating_sub(self.produced_this_tick);
                let accepted = (*amount).min(remaining);
                if accepted > 0 {
                    capped.insert(resource.clone(), accepted);
                    self.produced_this_tick = self.produced_this_tick.saturating_add(accepted);
                }

                let discarded_amount = amount.saturating_sub(accepted);
                if discarded_amount > 0 {
                    discarded.insert(resource.clone(), discarded_amount);
                    self.discarded_this_tick =
                        self.discarded_this_tick.saturating_add(discarded_amount);
                }
            } else {
                capped.insert(resource.clone(), *amount);
            }
        }

        if !discarded.is_empty() {
            eprintln!(
                "pve output cap discarded tick={} max={} discarded={:?}",
                tick, self.max_pve_output_per_tick, discarded
            );
        }

        capped
    }
}

impl Default for ResourceRegistry {
    fn default() -> Self {
        let mut resources = IndexMap::new();
        resources.insert(
            "Energy".to_string(),
            ResourceDef {
                name: "Energy".to_string(),
                display_name: "Energy".to_string(),
                category: "energy".to_string(),
                starting_amount: 1000,
                max_storage: 100_000,
                decay_rate: 0.0,
                tradeable: true,
            },
        );

        let mut body_part = IndexMap::new();
        body_part.insert(BodyPart::Move, energy_cost(50));
        body_part.insert(BodyPart::Work, energy_cost(100));
        body_part.insert(BodyPart::Carry, energy_cost(50));
        body_part.insert(BodyPart::Attack, energy_cost(80));
        body_part.insert(BodyPart::RangedAttack, energy_cost(100));
        body_part.insert(BodyPart::Heal, energy_cost(250));
        body_part.insert(BodyPart::Claim, energy_cost(600));
        body_part.insert(BodyPart::Tough, energy_cost(10));

        let mut sources = IndexMap::new();
        sources.insert(
            "EnergyField".to_string(),
            SourceDef {
                name: "EnergyField".to_string(),
                produces: energy_cost(1),
                capacity: 3000,
                regeneration: 300,
            },
        );

        Self {
            resources,
            action_costs: ActionCosts {
                body_part,
                ..Default::default()
            },
            sources,
        }
    }
}

impl ResourceRegistry {
    pub fn from_defs(resource_types: Vec<ResourceDef>, source_types: Vec<SourceDef>) -> Self {
        let mut registry = Self::default();
        for resource in resource_types {
            registry.resources.insert(resource.name.clone(), resource);
        }
        for source in source_types {
            registry.sources.insert(source.name.clone(), source);
        }
        registry
    }

    pub fn resource(&self, name: &str) -> Option<&ResourceDef> {
        self.resources.get(name)
    }

    pub fn source(&self, name: &str) -> Option<&SourceDef> {
        self.sources.get(name)
    }

    pub fn body_cost(&self, body: &[BodyPart]) -> ResourceCost {
        let mut total = ResourceCost::new();
        for part in body {
            if let Some(cost) = self.action_costs.body_part.get(part) {
                add_cost(&mut total, cost);
            }
        }
        total
    }

    pub fn body_energy_cost(&self, body: &[BodyPart]) -> ResourceAmount {
        self.body_cost(body)
            .get("Energy")
            .copied()
            .unwrap_or_default()
    }
}

pub fn energy_cost(amount: ResourceAmount) -> ResourceCost {
    let mut cost = ResourceCost::new();
    cost.insert("Energy".to_string(), amount);
    cost
}

pub fn cap_pve_output(
    tracker: &mut PveOutputTracker,
    tick: Tick,
    output: &ResourceCost,
) -> ResourceCost {
    tracker.cap_output(tick, output)
}

pub fn is_pve_output_capped_resource(resource: &str) -> bool {
    matches!(resource, "Energy" | "Crystal")
}

fn add_cost(total: &mut ResourceCost, cost: &ResourceCost) {
    for (resource, amount) in cost {
        *total.entry(resource.clone()).or_default() += amount;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pve_output_tracker_caps_energy_and_crystal_per_tick() {
        let mut tracker = PveOutputTracker::new(100);
        let mut output = ResourceCost::new();
        output.insert("Energy".to_string(), 70);
        output.insert("Crystal".to_string(), 50);
        output.insert("Blueprint".to_string(), 1);

        let capped = cap_pve_output(&mut tracker, 7, &output);

        assert_eq!(capped.get("Energy"), Some(&70));
        assert_eq!(capped.get("Crystal"), Some(&30));
        assert_eq!(capped.get("Blueprint"), Some(&1));
        assert_eq!(tracker.produced_this_tick, 100);
        assert_eq!(tracker.discarded_this_tick, 20);
    }

    #[test]
    fn pve_output_tracker_resets_for_new_tick() {
        let mut tracker = PveOutputTracker::new(10);
        let mut output = ResourceCost::new();
        output.insert("Energy".to_string(), 10);

        assert_eq!(
            cap_pve_output(&mut tracker, 1, &output).get("Energy"),
            Some(&10)
        );
        assert!(
            cap_pve_output(&mut tracker, 1, &output)
                .get("Energy")
                .is_none()
        );
        assert_eq!(tracker.discarded_this_tick, 10);
        assert_eq!(
            cap_pve_output(&mut tracker, 2, &output).get("Energy"),
            Some(&10)
        );
        assert_eq!(tracker.produced_this_tick, 10);
        assert_eq!(tracker.discarded_this_tick, 0);
    }

    #[test]
    fn default_registry_defines_energy_costs_and_source() {
        let registry = ResourceRegistry::default();

        assert_eq!(
            registry.resource("Energy").map(|def| def.starting_amount),
            Some(1000)
        );
        assert_eq!(
            registry.body_energy_cost(&[BodyPart::Move, BodyPart::Work]),
            150
        );

        let source = registry.source("EnergyField").expect("EnergyField source");
        assert_eq!(source.produces.get("Energy"), Some(&1));
        assert_eq!(source.capacity, 3000);
        assert_eq!(source.regeneration, 300);
    }

    #[test]
    fn registry_extends_defaults_from_resource_and_source_defs() {
        let registry = ResourceRegistry::from_defs(
            vec![ResourceDef {
                name: "Mineral".to_string(),
                display_name: "Mineral".to_string(),
                category: "mineral".to_string(),
                starting_amount: 0,
                max_storage: 50_000,
                decay_rate: 0.0,
                tradeable: true,
            }],
            vec![SourceDef {
                name: "MineralVein".to_string(),
                produces: {
                    let mut cost = ResourceCost::new();
                    cost.insert("Mineral".to_string(), 2);
                    cost
                },
                capacity: 1_000,
                regeneration: 100,
            }],
        );

        assert!(registry.resource("Energy").is_some());
        assert_eq!(registry.resource("Mineral").unwrap().max_storage, 50_000);
        assert_eq!(registry.source("MineralVein").unwrap().capacity, 1_000);
    }
}
