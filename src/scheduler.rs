use bevy::prelude::Resource;

pub const SYSTEM_MANIFEST_VERSION: &str = "2.0.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemPhase {
    Phase2aInline,
    Phase2bDeferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParallelSet {
    Combat,
    StatusEffects,
    WorldMaintenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionKind {
    Serial,
    Parallel(ParallelSet),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemManifestEntry {
    pub sequence: u8,
    pub name: &'static str,
    pub system_id: &'static str,
    pub version: &'static str,
    pub phase: SystemPhase,
    pub parallel_set: Option<ParallelSet>,
    pub reads: &'static [&'static str],
    pub writes: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemExecutionStep {
    pub kind: ExecutionKind,
    pub system_ids: &'static [&'static str],
}

#[derive(Resource, Debug, Clone, Copy)]
pub struct SystemSchedulerManifest {
    pub systems: &'static [SystemManifestEntry],
    pub execution_steps: &'static [SystemExecutionStep],
}

impl Default for SystemSchedulerManifest {
    fn default() -> Self {
        Self {
            systems: SYSTEM_MANIFEST,
            execution_steps: SYSTEM_EXECUTION_STEPS,
        }
    }
}

impl SystemSchedulerManifest {
    pub fn ordered_system_ids(&self) -> Vec<&'static str> {
        self.systems.iter().map(|system| system.system_id).collect()
    }

    pub fn manifest_hash(&self) -> blake3::Hash {
        manifest_hash(self.systems)
    }

    pub fn manifest_hash_hex(&self) -> String {
        self.manifest_hash().to_hex().to_string()
    }
}

pub fn manifest_hash(systems: &[SystemManifestEntry]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    for system in systems {
        hasher.update(system.system_id.as_bytes());
        hasher.update(system.version.as_bytes());
    }
    hasher.finalize()
}

pub fn world_config_registration_order() -> Vec<&'static str> {
    SYSTEM_MANIFEST
        .iter()
        .map(|system| system.system_id)
        .collect()
}

pub const SYSTEM_MANIFEST: &[SystemManifestEntry] = &[
    system(
        1,
        "command_executor",
        "cmd_exec",
        SystemPhase::Phase2aInline,
        None,
        &[
            "CommandQueue",
            "WorldConfig",
            "PlayerState",
            "Drone",
            "Room",
            "Entity",
            "Owner",
        ],
        &["Drone", "Entity", "ResourceAmount", "EventLog"],
    ),
    system(
        2,
        "controller_system",
        "ctrl_2a",
        SystemPhase::Phase2aInline,
        None,
        &["Controller", "PlayerState", "Room"],
        &["Controller", "Room"],
    ),
    system(
        3,
        "build_system",
        "build",
        SystemPhase::Phase2aInline,
        None,
        &["ConstructionSite", "Room", "ResourceAmount", "WorldConfig"],
        &["Structure", "ConstructionSite", "ResourceAmount"],
    ),
    system(
        4,
        "recycle_system",
        "recycle",
        SystemPhase::Phase2aInline,
        None,
        &["Entity", "ResourceAmount", "Owner"],
        &["ResourceAmount", "DeathMark"],
    ),
    system(
        5,
        "transfer_system",
        "transfer",
        SystemPhase::Phase2aInline,
        None,
        &["ResourceAmount", "Room", "WorldConfig", "ResourceLedger"],
        &["ResourceAmount", "ResourceLedger"],
    ),
    system(
        6,
        "spawn_validator",
        "spawn_val",
        SystemPhase::Phase2aInline,
        None,
        &[
            "Spawn",
            "DroneTemplate",
            "Room",
            "ResourceAmount",
            "PlayerState",
            "RoomCap",
        ],
        &["Spawn", "ResourceAmount", "PendingSpawn"],
    ),
    system(
        7,
        "death_marker",
        "death_mark",
        SystemPhase::Phase2bDeferred,
        None,
        &["Entity", "Drone", "Recycle DeathMark"],
        &["DeathMark", "RoomCap"],
    ),
    system(
        8,
        "spawn_system",
        "spawn",
        SystemPhase::Phase2bDeferred,
        None,
        &["PendingSpawn", "DroneTemplate", "Room", "RoomCap"],
        &["Drone", "Position", "ResourceAmount", "RoomCap"],
    ),
    system(
        9,
        "spawning_grace_system",
        "spawn_grace",
        SystemPhase::Phase2bDeferred,
        None,
        &["SpawningGrace"],
        &["SpawningGrace"],
    ),
    system(
        10,
        "regeneration_system",
        "regen",
        SystemPhase::Phase2bDeferred,
        None,
        &["Drone"],
        &["Drone"],
    ),
    system(
        11,
        "attack_system",
        "atk",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::Combat),
        &[
            "Drone",
            "Position",
            "BodyPart",
            "Entity",
            "Owner",
            "SpawningGrace",
        ],
        &["PendingDamage"],
    ),
    system(
        12,
        "ranged_attack_system",
        "rng_atk",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::Combat),
        &[
            "Drone",
            "Position",
            "BodyPart",
            "Entity",
            "Owner",
            "SpawningGrace",
        ],
        &["PendingDamage"],
    ),
    system(
        13,
        "heal_system",
        "heal",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::Combat),
        &["Drone", "Position", "Entity", "SpawningGrace"],
        &["PendingHeal"],
    ),
    system(
        14,
        "special_attack_reducer",
        "spec_atk_red",
        SystemPhase::Phase2bDeferred,
        None,
        &["PendingSpecialAttack", "Entity", "Owner"],
        &["PendingIntents", "StatusState"],
    ),
    system(
        15,
        "damage_application",
        "dmg_apply",
        SystemPhase::Phase2bDeferred,
        None,
        &["PendingDamage", "Entity", "SpawningGrace"],
        &["HitPoints", "DeathMark"],
    ),
    system(
        16,
        "hack_system",
        "hack",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::StatusEffects),
        &["HackState", "Entity", "Owner"],
        &["HackState"],
    ),
    system(
        17,
        "drain_system",
        "drain",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::StatusEffects),
        &["DrainState", "ResourceAmount"],
        &["ResourceAmount", "DrainState"],
    ),
    system(
        18,
        "overload_system",
        "overload",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::StatusEffects),
        &["OverloadState", "FuelBudget"],
        &["FuelBudget", "OverloadState"],
    ),
    system(
        19,
        "debilitate_system",
        "debuff",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::StatusEffects),
        &["DebilitateState", "Entity"],
        &["DebilitateState"],
    ),
    system(
        20,
        "disrupt_system",
        "disrupt",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::StatusEffects),
        &["DisruptState", "Entity", "BodyPart"],
        &["DisruptState", "Interrupted"],
    ),
    system(
        21,
        "fortify_system",
        "fort",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::StatusEffects),
        &["FortifyState", "Entity"],
        &["FortifyState", "Armor"],
    ),
    system(
        22,
        "status_advance_system",
        "status_adv",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::StatusEffects),
        &["StatusState", "PendingIntents"],
        &["StatusState"],
    ),
    system(
        23,
        "aging_system",
        "aging",
        SystemPhase::Phase2bDeferred,
        None,
        &["Drone"],
        &["Drone", "DeathMark"],
    ),
    system(
        24,
        "decay_system",
        "decay",
        SystemPhase::Phase2bDeferred,
        Some(ParallelSet::WorldMaintenance),
        &["Structure", "Drone"],
        &["Fatigue", "Cooldown"],
    ),
    system(
        25,
        "death_cleanup",
        "death_cln",
        SystemPhase::Phase2bDeferred,
        None,
        &["DeathMark"],
        &["Entity", "ResourceAmount"],
    ),
    system(
        26,
        "pvp_block_system",
        "pvp_block",
        SystemPhase::Phase2bDeferred,
        None,
        &["WorldConfig", "Room"],
        &["PendingCombat"],
    ),
    system(
        27,
        "room_state_system",
        "room_state",
        SystemPhase::Phase2bDeferred,
        None,
        &["Room", "Entity", "Controller"],
        &["Room", "EventLog"],
    ),
    system(
        28,
        "controller_system",
        "ctrl_p2b",
        SystemPhase::Phase2bDeferred,
        None,
        &["Controller", "Room"],
        &["Controller", "PlayerState"],
    ),
    system(
        29,
        "resource_ledger",
        "res_ledger",
        SystemPhase::Phase2bDeferred,
        None,
        &["ResourceAmount"],
        &["ResourceLedger"],
    ),
];

pub const SYSTEM_EXECUTION_STEPS: &[SystemExecutionStep] = &[
    serial(&["cmd_exec"]),
    serial(&["ctrl_2a"]),
    serial(&["build"]),
    serial(&["recycle"]),
    serial(&["transfer"]),
    serial(&["spawn_val"]),
    serial(&["death_mark"]),
    serial(&["spawn"]),
    serial(&["spawn_grace"]),
    serial(&["regen"]),
    parallel(ParallelSet::Combat, &["atk", "rng_atk", "heal"]),
    serial(&["spec_atk_red"]),
    serial(&["dmg_apply"]),
    parallel(
        ParallelSet::StatusEffects,
        &[
            "hack",
            "drain",
            "overload",
            "debuff",
            "disrupt",
            "fort",
            "status_adv",
        ],
    ),
    serial(&["aging"]),
    parallel(ParallelSet::WorldMaintenance, &["decay"]),
    serial(&["death_cln"]),
    serial(&["pvp_block"]),
    serial(&["room_state"]),
    serial(&["ctrl_p2b"]),
    serial(&["res_ledger"]),
];

const fn system(
    sequence: u8,
    name: &'static str,
    system_id: &'static str,
    phase: SystemPhase,
    parallel_set: Option<ParallelSet>,
    reads: &'static [&'static str],
    writes: &'static [&'static str],
) -> SystemManifestEntry {
    SystemManifestEntry {
        sequence,
        name,
        system_id,
        version: SYSTEM_MANIFEST_VERSION,
        phase,
        parallel_set,
        reads,
        writes,
    }
}

const fn serial(system_ids: &'static [&'static str]) -> SystemExecutionStep {
    SystemExecutionStep {
        kind: ExecutionKind::Serial,
        system_ids,
    }
}

const fn parallel(
    parallel_set: ParallelSet,
    system_ids: &'static [&'static str],
) -> SystemExecutionStep {
    SystemExecutionStep {
        kind: ExecutionKind::Parallel(parallel_set),
        system_ids,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[test]
    fn manifest_defines_29_ordered_systems() {
        assert_eq!(SYSTEM_MANIFEST.len(), 29);
        assert_eq!(SYSTEM_MANIFEST.first().unwrap().system_id, "cmd_exec");
        assert_eq!(SYSTEM_MANIFEST.last().unwrap().system_id, "res_ledger");
        for (index, system) in SYSTEM_MANIFEST.iter().enumerate() {
            assert_eq!(system.sequence as usize, index + 1);
            assert_eq!(system.version, SYSTEM_MANIFEST_VERSION);
        }
    }

    #[test]
    fn execution_steps_flatten_to_manifest_order() {
        let flattened = SYSTEM_EXECUTION_STEPS
            .iter()
            .flat_map(|step| step.system_ids.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(flattened, world_config_registration_order());
    }

    #[test]
    fn parallel_sets_match_authoritative_manifest() {
        let mut by_set = BTreeMap::<ParallelSet, Vec<&str>>::new();
        for system in SYSTEM_MANIFEST {
            if let Some(set) = system.parallel_set {
                by_set.entry(set).or_default().push(system.system_id);
            }
        }

        assert_eq!(by_set[&ParallelSet::Combat], ["atk", "rng_atk", "heal"]);
        assert_eq!(
            by_set[&ParallelSet::StatusEffects],
            [
                "hack",
                "drain",
                "overload",
                "debuff",
                "disrupt",
                "fort",
                "status_adv"
            ]
        );
        assert_eq!(by_set[&ParallelSet::WorldMaintenance], ["decay"]);
    }

    #[test]
    fn every_system_declares_reads_and_writes() {
        for system in SYSTEM_MANIFEST {
            assert!(
                !system.reads.is_empty(),
                "{} has no reads",
                system.system_id
            );
            assert!(
                !system.writes.is_empty(),
                "{} has no writes",
                system.system_id
            );
        }

        let damage_application = SYSTEM_MANIFEST
            .iter()
            .find(|system| system.system_id == "dmg_apply")
            .unwrap();
        assert!(damage_application.reads.contains(&"SpawningGrace"));
        assert!(damage_application.writes.contains(&"DeathMark"));
    }

    #[test]
    fn manifest_ids_are_unique_and_hash_is_stable() {
        let unique = SYSTEM_MANIFEST
            .iter()
            .map(|system| system.system_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), SYSTEM_MANIFEST.len());
        assert_eq!(
            manifest_hash(SYSTEM_MANIFEST),
            manifest_hash(SYSTEM_MANIFEST)
        );
        assert_eq!(
            SystemSchedulerManifest::default().manifest_hash_hex().len(),
            64
        );
    }

    #[test]
    fn create_world_installs_scheduler_manifest_resource() {
        let world = crate::create_world();
        let manifest = world.app.world().resource::<SystemSchedulerManifest>();
        assert_eq!(manifest.systems.len(), 29);
        assert_eq!(
            manifest.ordered_system_ids(),
            world_config_registration_order()
        );
    }
}
