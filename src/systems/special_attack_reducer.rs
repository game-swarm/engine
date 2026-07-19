use bevy::prelude::*;
use swarm_engine_plugin_sdk::buffers::{
    PendingSpecialAttack, SpecialAttackKind, StatusActionIntent,
};

/// Resolved intent ready for S22 status_advance_system to process.
#[derive(Debug, Clone)]
pub struct ResolvedIntent {
    pub kind: SpecialAttackKind,
    pub target: Entity,
    pub amount: u32,
}

/// Canonically sorted and resolved intents, delivered to S22.
#[derive(Resource, Debug, Clone, Default)]
pub struct PendingIntents {
    pub intents: Vec<ResolvedIntent>,
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
    mut pending: ResMut<PendingSpecialAttack>,
    mut intents: ResMut<PendingIntents>,
) {
    if pending.intents.is_empty() {
        return;
    }

    // 1. Drain all intents
    let mut raw: Vec<StatusActionIntent> = std::mem::take(&mut pending.intents);

    // 2. Canonical sort: (priority DESC, source identity, target identity)
    raw.sort_by(|a, b| {
        b.kind
            .cmp(&a.kind)
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
                target: intent.target,
                amount: intent.amount,
            });
        }
        acc
    });

    // 4. Deliver to S22
    intents.intents = resolved;
}

fn entity_identity(entity: Entity) -> u64 {
    u64::from(entity.index_u32())
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
    fn reducer_empty_is_noop() {
        let mut app = App::new();
        app.insert_resource(PendingSpecialAttack::default());
        app.insert_resource(PendingIntents::default());
        app.add_systems(Update, special_attack_reducer);

        app.update();

        let intents = app.world().resource::<PendingIntents>();
        assert!(intents.intents.is_empty());
    }
}
