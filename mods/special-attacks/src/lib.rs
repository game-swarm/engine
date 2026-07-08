use bevy::prelude::*;
use std::collections::BTreeSet;
use swarm_engine::{
    ActionRegistry,
    systems::{PendingSpecialAttack, SpecialAttackKind, StatusActionIntent},
};

#[derive(Resource, Debug, Clone)]
pub struct SpecialAttacksConfig {
    pub enabled: BTreeSet<SpecialAttackKind>,
    pub damage_multiplier: u32,
}

impl Default for SpecialAttacksConfig {
    fn default() -> Self {
        Self {
            enabled: [
                SpecialAttackKind::Hack,
                SpecialAttackKind::Drain,
                SpecialAttackKind::Overload,
                SpecialAttackKind::Debilitate,
                SpecialAttackKind::Disrupt,
                SpecialAttackKind::Fortify,
                SpecialAttackKind::Leech,
                SpecialAttackKind::Fabricate,
            ]
            .into_iter()
            .collect(),
            damage_multiplier: 10_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SpecialAttacksModPlugin;

impl Plugin for SpecialAttacksModPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SpecialAttacksConfig>()
            .init_resource::<PendingSpecialAttack>()
            .init_resource::<ActionRegistry>()
            .add_systems(Startup, register_special_actions);
    }
}

pub fn register_special_actions(
    config: Res<SpecialAttacksConfig>,
    mut registry: ResMut<ActionRegistry>,
) {
    for (kind, action_type, handler) in [
        (SpecialAttackKind::Hack, "Hack", "hack"),
        (SpecialAttackKind::Drain, "Drain", "drain"),
        (SpecialAttackKind::Overload, "Overload", "overload"),
        (SpecialAttackKind::Debilitate, "Debilitate", "debilitate"),
        (SpecialAttackKind::Disrupt, "Disrupt", "disrupt"),
        (SpecialAttackKind::Fortify, "Fortify", "fortify"),
        (SpecialAttackKind::Leech, "Leech", "leech"),
        (SpecialAttackKind::Fabricate, "Fabricate", "fabricate"),
    ] {
        if config.enabled.contains(&kind) {
            register_action_handler(&mut registry, action_type, handler);
        }
    }
}

pub fn enqueue_special_attack(
    pending: &mut PendingSpecialAttack,
    kind: SpecialAttackKind,
    source: Entity,
    target: Entity,
    owner: u32,
    amount: u32,
) {
    pending.intents.push(StatusActionIntent {
        kind,
        source,
        target,
        owner,
        amount,
    });
}

fn register_action_handler(registry: &mut ActionRegistry, action_type: &str, handler: &str) {
    registry
        .handlers
        .insert(action_type.to_string(), handler.to_string());
}
