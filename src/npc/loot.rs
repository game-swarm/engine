use bevy::prelude::Resource as BevyResource;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::resources::{ResourceAmount, ResourceCost};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NpcKind {
    Creep,
    Guardian,
    Merchant,
    Swarmling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LootType {
    Energy,
    Crystal,
    Blueprint,
    Wreckage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlueprintUnlock {
    BodyPart,
    Structure,
    Recipe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintDef {
    pub id: String,
    pub display_name: String,
    pub unlock: BlueprintUnlock,
    pub tier: u8,
    pub craft_cost: ResourceCost,
}

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintRegistry {
    pub definitions: IndexMap<String, BlueprintDef>,
    pub dropped: Vec<BlueprintDropRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintDropRecord {
    pub blueprint_id: String,
    pub npc: NpcKind,
    pub zone: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LootStack {
    Resource {
        resource: LootType,
        amount: ResourceAmount,
    },
    Blueprint {
        blueprint_id: String,
        quality: u8,
    },
    Wreckage {
        energy_value: ResourceAmount,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LootEntry {
    pub loot_type: LootType,
    pub weight: u32,
    pub min_amount: ResourceAmount,
    pub max_amount: ResourceAmount,
    pub blueprint_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LootTable {
    pub npc: NpcKind,
    pub entries: Vec<LootEntry>,
}

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpcLootTables {
    pub tables: IndexMap<NpcKind, LootTable>,
}

impl Default for NpcLootTables {
    fn default() -> Self {
        let mut tables = IndexMap::new();
        tables.insert(
            NpcKind::Creep,
            LootTable {
                npc: NpcKind::Creep,
                entries: vec![LootEntry::resource(LootType::Energy, 100, 10, 30)],
            },
        );
        tables.insert(
            NpcKind::Guardian,
            LootTable {
                npc: NpcKind::Guardian,
                entries: vec![
                    LootEntry::resource(LootType::Crystal, 100, 5, 15),
                    LootEntry::blueprint(5, "ancient_reinforced_alloy"),
                    LootEntry::resource(LootType::Wreckage, 100, 60, 60),
                ],
            },
        );
        tables.insert(
            NpcKind::Merchant,
            LootTable {
                npc: NpcKind::Merchant,
                entries: Vec::new(),
            },
        );
        tables.insert(
            NpcKind::Swarmling,
            LootTable {
                npc: NpcKind::Swarmling,
                entries: vec![LootEntry::resource(LootType::Energy, 100, 5, 10)],
            },
        );

        Self { tables }
    }
}

impl Default for BlueprintRegistry {
    fn default() -> Self {
        let mut definitions = IndexMap::new();
        definitions.insert(
            "ancient_reinforced_alloy".to_string(),
            BlueprintDef {
                id: "ancient_reinforced_alloy".to_string(),
                display_name: "Ancient Reinforced Alloy".to_string(),
                unlock: BlueprintUnlock::Recipe,
                tier: 1,
                craft_cost: resource_cost(&[("Energy", 500), ("Crystal", 25)]),
            },
        );
        definitions.insert(
            "guardian_core_frame".to_string(),
            BlueprintDef {
                id: "guardian_core_frame".to_string(),
                display_name: "Guardian Core Frame".to_string(),
                unlock: BlueprintUnlock::Structure,
                tier: 2,
                craft_cost: resource_cost(&[("Energy", 1200), ("Crystal", 75)]),
            },
        );

        Self {
            definitions,
            dropped: Vec::new(),
        }
    }
}

impl BlueprintRegistry {
    pub fn register(&mut self, blueprint: BlueprintDef) {
        self.definitions.insert(blueprint.id.clone(), blueprint);
    }

    pub fn get(&self, id: &str) -> Option<&BlueprintDef> {
        self.definitions.get(id)
    }

    pub fn register_drop(&mut self, blueprint_id: impl Into<String>, npc: NpcKind, zone: u8) {
        self.dropped.push(BlueprintDropRecord {
            blueprint_id: blueprint_id.into(),
            npc,
            zone,
        });
    }
}

impl LootEntry {
    pub fn resource(
        loot_type: LootType,
        weight: u32,
        min_amount: ResourceAmount,
        max_amount: ResourceAmount,
    ) -> Self {
        Self {
            loot_type,
            weight,
            min_amount,
            max_amount,
            blueprint_id: None,
        }
    }

    pub fn blueprint(weight: u32, blueprint_id: impl Into<String>) -> Self {
        Self {
            loot_type: LootType::Blueprint,
            weight,
            min_amount: 1,
            max_amount: 1,
            blueprint_id: Some(blueprint_id.into()),
        }
    }
}

impl NpcLootTables {
    pub fn table(&self, npc: NpcKind) -> Option<&LootTable> {
        self.tables.get(&npc)
    }

    pub fn roll_loot(&self, npc: NpcKind, zone: u8, seed: u64) -> Vec<LootStack> {
        let Some(table) = self.table(npc) else {
            return Vec::new();
        };

        let mut rng = LootRng::new(seed);
        let mut drops = Vec::new();
        for entry in &table.entries {
            if !entry.roll(&mut rng) {
                continue;
            }
            match entry.loot_type {
                LootType::Energy | LootType::Crystal => drops.push(LootStack::Resource {
                    resource: entry.loot_type,
                    amount: scaled_amount(entry.random_amount(&mut rng), zone),
                }),
                LootType::Blueprint => {
                    if let Some(blueprint_id) = &entry.blueprint_id {
                        drops.push(LootStack::Blueprint {
                            blueprint_id: blueprint_id.clone(),
                            quality: blueprint_quality(zone),
                        });
                    }
                }
                LootType::Wreckage => drops.push(LootStack::Wreckage {
                    energy_value: scaled_amount(entry.random_amount(&mut rng), zone),
                }),
            }
        }

        drops
    }

    pub fn roll_loot_and_register_blueprints(
        &self,
        npc: NpcKind,
        zone: u8,
        seed: u64,
        blueprints: &mut BlueprintRegistry,
    ) -> Vec<LootStack> {
        let drops = self.roll_loot(npc, zone, seed);
        for drop in &drops {
            if let LootStack::Blueprint { blueprint_id, .. } = drop {
                blueprints.register_drop(blueprint_id.clone(), npc, zone);
            }
        }
        drops
    }
}

impl LootEntry {
    fn roll(&self, rng: &mut LootRng) -> bool {
        self.weight >= 100 || rng.next_below(100) < self.weight
    }

    fn random_amount(&self, rng: &mut LootRng) -> ResourceAmount {
        if self.min_amount >= self.max_amount {
            return self.min_amount;
        }
        self.min_amount + rng.next_below(self.max_amount - self.min_amount + 1)
    }
}

fn scaled_amount(amount: ResourceAmount, zone: u8) -> ResourceAmount {
    let bonus_percent = u32::from(zone.saturating_sub(1)) * 25;
    amount.saturating_mul(100 + bonus_percent) / 100
}

fn blueprint_quality(zone: u8) -> u8 {
    zone.clamp(1, 4)
}

fn resource_cost(entries: &[(&str, ResourceAmount)]) -> ResourceCost {
    entries
        .iter()
        .map(|(name, amount)| ((*name).to_string(), *amount))
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct LootRng(u64);

impl LootRng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next(&mut self) -> u32 {
        let mut value = self.0;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        self.0 = value;
        ((value.wrapping_mul(0x2545_f491_4f6c_dd1d)) >> 32) as u32
    }

    fn next_below(&mut self, upper: u32) -> u32 {
        if upper == 0 { 0 } else { self.next() % upper }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guardian_blueprint_drop_rate_tracks_weight() {
        let tables = NpcLootTables::default();
        let drops = (0..10_000)
            .filter(|seed| {
                tables
                    .roll_loot(NpcKind::Guardian, 1, *seed)
                    .iter()
                    .any(|drop| matches!(drop, LootStack::Blueprint { .. }))
            })
            .count();

        assert!((430..=570).contains(&drops), "unexpected drops: {drops}");
    }

    #[test]
    fn zone_bonus_increases_amount_and_quality() {
        let tables = NpcLootTables::default();
        let zone_one = tables.roll_loot(NpcKind::Guardian, 1, 42);
        let zone_four = tables.roll_loot(NpcKind::Guardian, 4, 42);

        assert!(total_resource_amount(&zone_four) > total_resource_amount(&zone_one));
        assert!(wreckage_value(&zone_four) > wreckage_value(&zone_one));
    }

    #[test]
    fn blueprint_drops_are_registered_for_crafting() {
        let mut registry = BlueprintRegistry::default();
        let tables = NpcLootTables {
            tables: IndexMap::from([(
                NpcKind::Guardian,
                LootTable {
                    npc: NpcKind::Guardian,
                    entries: vec![LootEntry::blueprint(100, "guardian_core_frame")],
                },
            )]),
        };

        let drops =
            tables.roll_loot_and_register_blueprints(NpcKind::Guardian, 4, 7, &mut registry);

        assert_eq!(registry.dropped.len(), 1);
        assert_eq!(registry.dropped[0].blueprint_id, "guardian_core_frame");
        assert!(registry.get("guardian_core_frame").is_some());
        assert!(matches!(
            drops.as_slice(),
            [LootStack::Blueprint { quality: 4, .. }]
        ));
    }

    fn total_resource_amount(drops: &[LootStack]) -> ResourceAmount {
        drops
            .iter()
            .filter_map(|drop| match drop {
                LootStack::Resource { amount, .. } => Some(*amount),
                _ => None,
            })
            .sum()
    }

    fn wreckage_value(drops: &[LootStack]) -> ResourceAmount {
        drops
            .iter()
            .find_map(|drop| match drop {
                LootStack::Wreckage { energy_value } => Some(*energy_value),
                _ => None,
            })
            .unwrap_or_default()
    }
}
