use bevy::prelude::*;

use crate::components::PlayerId;

/// Priority chain for status actions — Hack > Drain > Overload > Debilitate > Disrupt > Fortify > Leech > Fabricate.
/// Higher discriminant = higher priority. This is the single authority definition per manifest §S14.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpecialAttackKind {
    Fortify = 1,
    Leech = 2,
    Fabricate = 3,
    Disrupt = 4,
    Debilitate = 5,
    Overload = 6,
    Drain = 7,
    Hack = 8,
}

/// Raw incoming status action intent, populated by command processing.
#[derive(Debug, Clone)]
pub struct StatusActionIntent {
    pub kind: SpecialAttackKind,
    pub source: Entity,
    pub target: Entity,
    pub owner: PlayerId,
    pub amount: u32,
}

/// Buffer of pending special attack intents before reduction.
#[derive(Resource, Debug, Clone, Default)]
pub struct PendingSpecialAttack {
    pub intents: Vec<StatusActionIntent>,
}

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

/// Damage buffer filled by special attacks, consumed by S15 damage_application.
#[derive(Resource, Debug, Clone, Default)]
pub struct PendingDamage {
    pub entries: Vec<PendingDamageEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDamageEntry {
    pub target: Entity,
    pub amount: u32,
    pub damage_type: String,
}

impl From<(Entity, u32, String)> for PendingDamageEntry {
    fn from((target, amount, damage_type): (Entity, u32, String)) -> Self {
        Self {
            target,
            amount,
            damage_type,
        }
    }
}

impl PendingDamage {
    pub fn push(&mut self, target: Entity, amount: u32, damage_type: impl Into<String>) {
        self.entries.push(PendingDamageEntry {
            target,
            amount,
            damage_type: damage_type.into(),
        });
    }
}

#[derive(Resource, Debug, Clone, Default)]
pub struct PendingHeal {
    pub entries: Vec<PendingHealEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHealEntry {
    pub target: Entity,
    pub amount: u32,
}

impl PendingHeal {
    pub fn push(&mut self, target: Entity, amount: u32) {
        self.entries.push(PendingHealEntry { target, amount });
    }
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

    // 2. Canonical sort: (priority DESC, source, target)
    raw.sort_by(|a, b| {
        b.kind
            .cmp(&a.kind)
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.target.cmp(&b.target))
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
