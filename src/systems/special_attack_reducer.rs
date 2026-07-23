use bevy::prelude::*;
use swarm_engine_api::ids::DamageType;
use swarm_engine_plugin_sdk::buffers::{
    PendingSpecialAttack, SpecialAttackKind, StatusActionIntent,
};
use swarm_engine_plugin_sdk::components::SpawningGrace;

/// Resolved intent ready for S22 status_advance_system to process.
#[derive(Debug, Clone)]
pub struct ResolvedIntent {
    pub kind: SpecialAttackKind,
    pub source: Entity,
    pub target: Entity,
    pub amount: u32,
}

/// Canonically sorted and resolved intents, delivered to S22.
#[derive(Resource, Debug, Clone, Default)]
pub struct PendingIntents {
    pub intents: Vec<ResolvedIntent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeechCombatIntent {
    pub source: Entity,
    pub target: Entity,
    pub base_damage: u32,
    pub damage_type: DamageType,
    pub heal_bps: u32,
    pub sort_key: u64,
}

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingLeechCombat {
    pub intents: Vec<LeechCombatIntent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeechResolutionEntry {
    pub source: Entity,
    pub target: Entity,
    pub actual_damage: u32,
    pub self_heal: u32,
    pub sort_key: u64,
}

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct LeechResolution {
    pub entries: Vec<LeechResolutionEntry>,
}

/// S14: Special Attack Reducer
///
/// Pipeline:
/// 1. Collect: drain all per-system sub-buffers from PendingSpecialAttack
/// 2. Merge sort: canonical sort by (priority, source, target) — deterministic
/// 3. Reducer resolve: same-target conflicts → highest priority wins (Hack > ... > Fabricate)
/// 4. Deliver: write resolved intents to PendingIntents for S22 consumption
///
/// Does NOT directly modify entity state — only routes intents.
pub fn special_attack_reducer(
    pending: Res<PendingSpecialAttack>,
    mut intents: ResMut<PendingIntents>,
    mut pending_leech: ResMut<PendingLeechCombat>,
    spawning_grace: Query<(), With<SpawningGrace>>,
) {
    intents.intents.clear();
    pending_leech.intents.clear();
    if pending.intents.is_empty() {
        return;
    }

    // S16-S22b still consume the raw buffer later in this tick. S22 clears it
    // after all typed buffer producers have run.
    let mut raw: Vec<StatusActionIntent> = pending
        .intents
        .iter()
        .filter(|intent| !spawning_grace.contains(intent.target))
        .cloned()
        .collect();

    // 2. Canonical sort: (priority DESC, source identity, target identity)
    raw.sort_by(|a, b| {
        special_attack_priority(b.kind)
            .cmp(&special_attack_priority(a.kind))
            .then_with(|| entity_identity(a.source).cmp(&entity_identity(b.source)))
            .then_with(|| entity_identity(a.target).cmp(&entity_identity(b.target)))
    });

    // 3. Reducer resolve: same target → highest priority wins
    // Group by target, keep only the highest-priority intent per target
    let resolved: Vec<ResolvedIntent> = raw.into_iter().fold(Vec::new(), |mut acc, intent| {
        if let Some(_existing) = acc.iter_mut().find(|r| r.target == intent.target) {
            // Keep the one with higher priority (already sorted, so first in group wins)
            // The first in a target group has highest priority due to sort
        } else {
            acc.push(ResolvedIntent {
                kind: intent.kind,
                source: intent.source,
                target: intent.target,
                amount: intent.amount,
            });
        }
        acc
    });

    // 4. Deliver to S22
    for (sort_key, intent) in resolved.into_iter().enumerate() {
        if intent.kind == SpecialAttackKind::Leech {
            pending_leech.intents.push(LeechCombatIntent {
                source: intent.source,
                target: intent.target,
                base_damage: 15,
                damage_type: DamageType::Kinetic,
                heal_bps: 5_000,
                sort_key: sort_key as u64,
            });
        } else {
            intents.intents.push(intent);
        }
    }
}

fn entity_identity(entity: Entity) -> u64 {
    u64::from(entity.index_u32())
}

fn special_attack_priority(kind: SpecialAttackKind) -> u8 {
    match kind {
        SpecialAttackKind::Hack => 8,
        SpecialAttackKind::Drain => 7,
        SpecialAttackKind::Overload => 6,
        SpecialAttackKind::Debilitate => 5,
        SpecialAttackKind::Disrupt => 4,
        SpecialAttackKind::Fortify => 3,
        SpecialAttackKind::Leech => 2,
        SpecialAttackKind::Fabricate => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(index: u32) -> Entity {
        Entity::from_raw_u32(index).expect("test entity index must fit")
    }

    #[test]
    fn reducer_sorts_by_priority_desc() {
        let mut app = App::new();
        app.insert_resource(PendingSpecialAttack {
            intents: vec![
                StatusActionIntent {
                    kind: SpecialAttackKind::Fortify,
                    source: entity(1),
                    target: entity(10),
                    owner: 1,
                    amount: 5,
                },
                StatusActionIntent {
                    kind: SpecialAttackKind::Hack,
                    source: entity(2),
                    target: entity(10),
                    owner: 1,
                    amount: 10,
                },
            ],
        });
        app.insert_resource(PendingIntents::default());
        app.insert_resource(PendingLeechCombat::default());
        app.add_systems(Update, special_attack_reducer);

        app.update();

        let intents = app.world().resource::<PendingIntents>();
        assert_eq!(
            intents.intents.len(),
            1,
            "same target → only one intent survives"
        );
        assert_eq!(
            intents.intents[0].kind,
            SpecialAttackKind::Hack,
            "Hack > Fortify"
        );
        assert_eq!(intents.intents[0].amount, 10);
        assert_eq!(intents.intents[0].source, entity(2));
        assert_eq!(intents.intents[0].target, entity(10));
        assert_eq!(intents.intents[0].kind, SpecialAttackKind::Hack);
        assert_eq!(
            app.world().resource::<PendingSpecialAttack>().intents.len(),
            2,
            "S14 must not starve the downstream typed buffer systems"
        );
    }

    #[test]
    fn reducer_resolves_same_target_same_priority() {
        let mut app = App::new();
        app.insert_resource(PendingSpecialAttack {
            intents: vec![
                StatusActionIntent {
                    kind: SpecialAttackKind::Drain,
                    source: entity(1),
                    target: entity(10),
                    owner: 1,
                    amount: 3,
                },
                StatusActionIntent {
                    kind: SpecialAttackKind::Drain,
                    source: entity(2),
                    target: entity(10),
                    owner: 2,
                    amount: 7,
                },
            ],
        });
        app.insert_resource(PendingIntents::default());
        app.insert_resource(PendingLeechCombat::default());
        app.add_systems(Update, special_attack_reducer);

        app.update();

        let intents = app.world().resource::<PendingIntents>();
        assert_eq!(
            intents.intents.len(),
            1,
            "same priority same target → one intent"
        );
        // First in sort order wins (lower source entity)
        assert_eq!(intents.intents[0].amount, 3);
    }

    #[test]
    fn reducer_same_priority_uses_numeric_source_identity() {
        let mut app = App::new();
        app.insert_resource(PendingSpecialAttack {
            intents: vec![
                StatusActionIntent {
                    kind: SpecialAttackKind::Drain,
                    source: entity(2),
                    target: entity(10),
                    owner: 2,
                    amount: 7,
                },
                StatusActionIntent {
                    kind: SpecialAttackKind::Drain,
                    source: entity(1),
                    target: entity(10),
                    owner: 1,
                    amount: 3,
                },
            ],
        });
        app.insert_resource(PendingIntents::default());
        app.insert_resource(PendingLeechCombat::default());
        app.add_systems(Update, special_attack_reducer);

        app.update();

        let intents = app.world().resource::<PendingIntents>();
        assert_eq!(intents.intents.len(), 1);
        assert_eq!(intents.intents[0].amount, 3);
    }

    #[test]
    fn reducer_preserves_different_targets() {
        let mut app = App::new();
        app.insert_resource(PendingSpecialAttack {
            intents: vec![
                StatusActionIntent {
                    kind: SpecialAttackKind::Hack,
                    source: entity(1),
                    target: entity(10),
                    owner: 1,
                    amount: 5,
                },
                StatusActionIntent {
                    kind: SpecialAttackKind::Drain,
                    source: entity(2),
                    target: entity(20),
                    owner: 2,
                    amount: 3,
                },
            ],
        });
        app.insert_resource(PendingIntents::default());
        app.insert_resource(PendingLeechCombat::default());
        app.add_systems(Update, special_attack_reducer);

        app.update();

        let intents = app.world().resource::<PendingIntents>();
        assert_eq!(
            intents.intents.len(),
            2,
            "different targets → both preserved"
        );
    }

    #[test]
    fn reducer_uses_manifest_priority_for_fortify_and_fabricate() {
        let mut app = App::new();
        app.insert_resource(PendingSpecialAttack {
            intents: vec![
                StatusActionIntent {
                    kind: SpecialAttackKind::Fabricate,
                    source: entity(1),
                    target: entity(10),
                    owner: 1,
                    amount: 0,
                },
                StatusActionIntent {
                    kind: SpecialAttackKind::Fortify,
                    source: entity(2),
                    target: entity(10),
                    owner: 1,
                    amount: 0,
                },
            ],
        });
        app.insert_resource(PendingIntents::default());
        app.insert_resource(PendingLeechCombat::default());
        app.add_systems(Update, special_attack_reducer);

        app.update();

        let intents = app.world().resource::<PendingIntents>();
        assert_eq!(intents.intents.len(), 1);
        assert_eq!(intents.intents[0].kind, SpecialAttackKind::Fortify);
    }

    #[test]
    fn reducer_keeps_leech_out_of_persistent_status_delivery() {
        let mut app = App::new();
        app.insert_resource(PendingSpecialAttack {
            intents: vec![StatusActionIntent {
                kind: SpecialAttackKind::Leech,
                source: entity(1),
                target: entity(10),
                owner: 1,
                amount: 15,
            }],
        });
        app.insert_resource(PendingIntents::default());
        app.insert_resource(PendingLeechCombat::default());
        app.add_systems(Update, special_attack_reducer);

        app.update();

        assert!(app.world().resource::<PendingIntents>().intents.is_empty());
        let leech = app.world().resource::<PendingLeechCombat>();
        assert_eq!(
            leech.intents,
            vec![LeechCombatIntent {
                source: entity(1),
                target: entity(10),
                base_damage: 15,
                damage_type: DamageType::Kinetic,
                heal_bps: 5_000,
                sort_key: 0,
            }]
        );
        assert_eq!(
            app.world().resource::<PendingSpecialAttack>().intents.len(),
            1,
            "Leech remains available to its downstream combat/buffer route"
        );
    }

    #[test]
    fn reducer_routes_leech_over_fabricate_for_the_same_target() {
        let mut app = App::new();
        app.insert_resource(PendingSpecialAttack {
            intents: vec![
                StatusActionIntent {
                    kind: SpecialAttackKind::Fabricate,
                    source: entity(1),
                    target: entity(10),
                    owner: 1,
                    amount: 0,
                },
                StatusActionIntent {
                    kind: SpecialAttackKind::Leech,
                    source: entity(2),
                    target: entity(10),
                    owner: 1,
                    amount: 0,
                },
            ],
        });
        app.insert_resource(PendingIntents::default());
        app.insert_resource(PendingLeechCombat::default());
        app.add_systems(Update, special_attack_reducer);

        app.update();

        assert!(app.world().resource::<PendingIntents>().intents.is_empty());
        assert_eq!(
            app.world().resource::<PendingLeechCombat>().intents[0].source,
            entity(2)
        );
    }

    #[test]
    fn reducer_rejects_leech_against_spawning_grace() {
        let mut app = App::new();
        let source = app.world_mut().spawn_empty().id();
        let target = app.world_mut().spawn(SpawningGrace { remaining: 1 }).id();
        app.insert_resource(PendingSpecialAttack {
            intents: vec![StatusActionIntent {
                kind: SpecialAttackKind::Leech,
                source,
                target,
                owner: 1,
                amount: 0,
            }],
        });
        app.insert_resource(PendingIntents::default());
        app.insert_resource(PendingLeechCombat::default());
        app.add_systems(Update, special_attack_reducer);

        app.update();

        assert!(
            app.world()
                .resource::<PendingLeechCombat>()
                .intents
                .is_empty()
        );
    }

    #[test]
    fn reducer_empty_is_noop() {
        let mut app = App::new();
        app.insert_resource(PendingSpecialAttack::default());
        app.insert_resource(PendingIntents::default());
        app.insert_resource(PendingLeechCombat::default());
        app.add_systems(Update, special_attack_reducer);

        app.update();

        let intents = app.world().resource::<PendingIntents>();
        assert!(intents.intents.is_empty());
    }
}
