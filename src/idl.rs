//! IDL (Interface Definition Language) extraction and SDK code generation.
//!
//! The engine can export its full type schema — core types from source +
//! mod types from world.toml runtime registries — as a machine-readable JSON IDL.
//! SDK code for both Rust and TypeScript is generated from this IDL.

use crate::command::{CANONICAL_REJECTION_REASONS, command_schema_branches};
use crate::components::{CustomActionRegistry, SpecialEffectRegistry, StructureTypeRegistry};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use swarm_engine_plugin_sdk::components::BodyPartRegistry;

// ── IDL data types ──────────────────────────────────────────────────

/// Top-level IDL document — serialized to JSON for SDK generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdlDoc {
    pub idl_version: String,
    /// SHA-256 of the world.toml used to generate this IDL.
    pub world_hash: String,
    /// Engine version.
    pub engine_version: String,
    pub core: CoreIdl,
    pub mods: ModIdl,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreIdl {
    pub commands: Vec<CommandDef>,
    pub enums: EnumDefs,
    pub structure_types: Vec<String>,
    pub constants: BTreeMap<String, serde_json::Value>,
    pub body_part_costs: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDef {
    pub name: String,
    /// field_name → type_name ("ObjectId", "ResourceName?", "BodyPart[]", "u32", etc.)
    pub fields: IndexMap<String, String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumDefs {
    pub direction: Vec<String>,
    pub body_part: Vec<String>,
    pub damage_type: Vec<String>,
    pub rejection_reason: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModIdl {
    pub custom_actions: Vec<ModCustomAction>,
    pub special_effects: Vec<ModSpecialEffect>,
    pub body_parts: Vec<ModBodyPart>,
    pub structure_types: Vec<ModStructureType>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModCustomAction {
    pub name: String,
    pub description: String,
    pub damage_type: Option<String>,
    pub base_damage: Option<u32>,
    pub range: u32,
    pub special_effect: Option<String>,
    pub special_param_micro: Option<u64>,
    pub cooldown: Option<u32>,
    pub cost: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModSpecialEffect {
    pub name: String,
    pub description: String,
    pub handler: String,
    pub target: String,
    pub duration: u32,
    pub resistance: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModBodyPart {
    pub name: String,
    pub damage_type: Option<String>,
    pub base_damage: Option<u32>,
    pub heal_amount: Option<u32>,
    pub age_modifier: i32,
    pub resistances_bps: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModStructureType {
    pub name: String,
    pub description: String,
    pub category: String,
    pub hits: u32,
    pub rcl_required: u8,
    pub cost: BTreeMap<String, u32>,
}

// ── IDL extraction ──────────────────────────────────────────────────

/// Extract the full IDL from engine source + runtime registries.
pub fn extract_idl(
    world_toml_hash: &str,
    body_parts: &BodyPartRegistry,
    structures: &StructureTypeRegistry,
    special_effects: &SpecialEffectRegistry,
    custom_actions: &CustomActionRegistry,
) -> IdlDoc {
    IdlDoc {
        idl_version: "1.0.0".into(),
        world_hash: world_toml_hash.into(),
        engine_version: env!("CARGO_PKG_VERSION").into(),
        core: extract_core(body_parts),
        mods: extract_mods(body_parts, structures, special_effects, custom_actions),
    }
}

fn extract_core(_body_parts: &BodyPartRegistry) -> CoreIdl {
    CoreIdl {
        commands: core_commands(),
        enums: core_enums(),
        structure_types: core_structure_types(),
        constants: core_constants(),
        body_part_costs: core_body_part_costs(),
    }
}

fn core_commands() -> Vec<CommandDef> {
    command_schema_branches()
        .into_iter()
        .filter(|branch| !branch.custom_wildcard)
        .map(|branch| {
            let mut metadata = BTreeMap::new();
            if let Some(kind) = branch.metadata.settlement_kind {
                metadata.insert("settlement_kind".to_string(), serde_json::json!(kind));
            }
            if let Some(phase) = branch.metadata.settlement_phase {
                metadata.insert("settlement_phase".to_string(), serde_json::json!(phase));
            }
            CommandDef {
                name: branch.name.to_string(),
                fields: branch
                    .fields
                    .into_iter()
                    .map(|field| {
                        let ty = if field.required {
                            field.type_name.to_string()
                        } else {
                            format!("{}?", field.type_name)
                        };
                        (field.name.to_string(), ty)
                    })
                    .collect(),
                metadata,
            }
        })
        .collect()
}

fn core_enums() -> EnumDefs {
    EnumDefs {
        direction: vec![
            "Top",
            "TopRight",
            "BottomRight",
            "Bottom",
            "BottomLeft",
            "TopLeft",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        body_part: vec![
            "Move",
            "Work",
            "Carry",
            "Attack",
            "RangedAttack",
            "Heal",
            "Claim",
            "Tough",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        damage_type: vec!["Kinetic", "Thermal", "EMP", "Sonic", "Corrosive", "Psionic"]
            .into_iter()
            .map(String::from)
            .collect(),
        rejection_reason: CANONICAL_REJECTION_REASONS
            .iter()
            .map(|reason| reason.to_string())
            .collect(),
    }
}

fn core_structure_types() -> Vec<String> {
    vec![
        "Spawn",
        "Extension",
        "Tower",
        "Storage",
        "Link",
        "Extractor",
        "Lab",
        "Terminal",
        "Nuker",
        "Observer",
        "PowerSpawn",
        "Factory",
        "Depot",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn core_constants() -> BTreeMap<String, serde_json::Value> {
    use crate::command;
    let mut m = BTreeMap::new();
    m.insert("MAX_FUEL".into(), serde_json::json!(command::MAX_FUEL));
    m.insert(
        "MAX_COMMANDS_PER_PLAYER".into(),
        serde_json::json!(command::MAX_COMMANDS_PER_PLAYER),
    );
    m.insert(
        "MAX_BODY_PARTS".into(),
        serde_json::json!(command::MAX_BODY_PARTS),
    );
    m.insert(
        "MAX_DRONES_PER_PLAYER".into(),
        serde_json::json!(command::MAX_DRONES_PER_PLAYER),
    );
    m.insert(
        "MAX_TICK_OUTPUT_BYTES".into(),
        serde_json::json!(command::MAX_TICK_OUTPUT_BYTES),
    );
    m.insert(
        "MAX_JSON_DEPTH".into(),
        serde_json::json!(command::MAX_JSON_DEPTH),
    );
    m.insert(
        "MAX_RANGED_ATTACK_RANGE".into(),
        serde_json::json!(command::MAX_RANGED_ATTACK_RANGE),
    );
    m.insert(
        "MAX_REFUND_PER_TICK".into(),
        serde_json::json!(command::MAX_REFUND_PER_TICK),
    );
    m.insert(
        "MAX_NEXT_TICK_FUEL_BUDGET".into(),
        serde_json::json!(command::MAX_NEXT_TICK_FUEL_BUDGET),
    );
    m
}

fn core_body_part_costs() -> BTreeMap<String, u32> {
    BTreeMap::from([
        ("Move".into(), 50),
        ("Work".into(), 100),
        ("Carry".into(), 50),
        ("Attack".into(), 80),
        ("Heal".into(), 250),
        ("Tough".into(), 10),
        ("RangedAttack".into(), 100),
        ("Claim".into(), 600),
    ])
}

fn extract_mods(
    body_parts: &BodyPartRegistry,
    structures: &StructureTypeRegistry,
    special_effects: &SpecialEffectRegistry,
    custom_actions: &CustomActionRegistry,
) -> ModIdl {
    let core_struct_list = core_structure_types();
    let core_structures: std::collections::HashSet<&str> =
        core_struct_list.iter().map(|s| s.as_str()).collect();
    let core_body_parts: std::collections::HashSet<&str> = [
        "Move",
        "Work",
        "Carry",
        "Attack",
        "RangedAttack",
        "Heal",
        "Claim",
        "Tough",
    ]
    .into_iter()
    .collect();

    ModIdl {
        custom_actions: custom_actions
            .actions
            .values()
            .map(|a| ModCustomAction {
                name: a.name.clone(),
                description: a.description.clone(),
                damage_type: a.damage_type.clone(),
                base_damage: a.base_damage,
                range: a.range,
                special_effect: a.special_effect.clone(),
                special_param_micro: a.special_param_micro,
                cooldown: a.cooldown,
                cost: a.cost.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            })
            .collect(),
        special_effects: special_effects
            .effects
            .values()
            .map(|e| ModSpecialEffect {
                name: e.name.clone(),
                description: e.description.clone(),
                handler: e.handler.clone(),
                target: e.target.clone(),
                duration: e.duration,
                resistance: e.resistance.clone(),
            })
            .collect(),
        body_parts: body_parts
            .parts
            .values()
            .filter(|p| !core_body_parts.contains(p.name.to_string().as_str()))
            .map(|p| ModBodyPart {
                name: p.name.to_string(),
                damage_type: p.damage_type.clone(),
                base_damage: p.base_damage,
                heal_amount: p.heal_amount,
                age_modifier: p.age_modifier,
                resistances_bps: p
                    .resistances_bps
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
            })
            .collect(),
        structure_types: structures
            .structure_types
            .values()
            .filter(|s| !core_structures.contains(s.name.as_str()))
            .map(|s| ModStructureType {
                name: s.name.as_str().to_string(),
                description: s.description.clone(),
                category: s.category.clone(),
                hits: s.hits,
                rcl_required: s.rcl_required,
                cost: s.cost.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            })
            .collect(),
    }
}
