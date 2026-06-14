use bevy::prelude::Resource as BevyResource;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::components::{BodyPart, StructureType};

pub type ResourceName = String;
pub type ResourceAmount = u32;
pub type ResourceCost = IndexMap<ResourceName, ResourceAmount>;

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

fn add_cost(total: &mut ResourceCost, cost: &ResourceCost) {
    for (resource, amount) in cost {
        *total.entry(resource.clone()).or_default() += amount;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
