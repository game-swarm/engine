use bevy::prelude::*;

use std::collections::{BTreeMap, BTreeSet};
use swarm_engine_plugin_sdk::buffers::{PendingDamage, PendingHeal};
use swarm_engine_plugin_sdk::components::{
    BodyPartRegistry, DeathMark, Drone, SpawningGrace, Structure,
};

use crate::components::{Attributes, DamageTypeRegistry, EntityFlags, ResistanceRegistry};
use crate::systems::combat_system::{CombatRules, final_damage_multiplier_bps, leech_self_heal};
use crate::systems::{LeechResolution, LeechResolutionEntry, PendingLeechCombat};

/// S15: Damage Application System
///
/// Applies pending damage from special attacks to entities.
/// Accounts for SpawningGrace (newborn invincibility) and marks
/// entities for death when hits reach zero.
///
/// Filter: `Without<SpawningGrace>` per manifest §S15.
pub fn damage_application_system(
    mut damage: ResMut<PendingDamage>,
    mut heal: ResMut<PendingHeal>,
    mut leech: Option<ResMut<PendingLeechCombat>>,
    mut resolutions: Option<ResMut<LeechResolution>>,
    rules: Option<Res<CombatRules>>,
    body_registry: Option<Res<BodyPartRegistry>>,
    damage_registry: Option<Res<DamageTypeRegistry>>,
    resistance_registry: Option<Res<ResistanceRegistry>>,
    mut entities: ParamSet<(
        Query<(
            Option<&Drone>,
            Option<&Structure>,
            Option<&Attributes>,
            Option<&EntityFlags>,
            Has<SpawningGrace>,
            Has<DeathMark>,
        )>,
        Query<(Option<&mut Drone>, Option<&mut Structure>)>,
    )>,
    mut commands: Commands,
) {
    if let Some(resolutions) = resolutions.as_deref_mut() {
        resolutions.entries.clear();
    }

    let mut damage_by_target = BTreeMap::<Entity, u32>::new();
    for entry in std::mem::take(&mut damage.entries) {
        let total = damage_by_target.entry(entry.target).or_default();
        *total = total.saturating_add(entry.amount);
    }

    let mut heal_by_target = BTreeMap::<Entity, u32>::new();
    for entry in std::mem::take(&mut heal.entries) {
        let total = heal_by_target.entry(entry.target).or_default();
        *total = total.saturating_add(entry.amount);
    }
    let leech_intents = leech
        .as_deref_mut()
        .map(|pending| std::mem::take(&mut pending.intents))
        .unwrap_or_default();

    let mut relevant = BTreeSet::new();
    relevant.extend(damage_by_target.keys().copied());
    relevant.extend(heal_by_target.keys().copied());
    for intent in &leech_intents {
        relevant.insert(intent.source);
        relevant.insert(intent.target);
    }

    let default_body_registry = BodyPartRegistry::default();
    let default_damage_registry = DamageTypeRegistry::default();
    let default_resistance_registry = ResistanceRegistry::default();
    let body_registry = body_registry.as_deref().unwrap_or(&default_body_registry);
    let damage_registry = damage_registry
        .as_deref()
        .unwrap_or(&default_damage_registry);
    let resistance_registry = resistance_registry
        .as_deref()
        .unwrap_or(&default_resistance_registry);
    let rules = rules.as_deref().copied().unwrap_or_default();

    let mut virtual_hits = BTreeMap::<Entity, (u32, u32, u32)>::new();
    {
        let read_entities = entities.p0();
        for entity in relevant {
            let Ok((drone, structure, attrs, flags, grace, dead)) = read_entities.get(entity)
            else {
                continue;
            };
            if grace || dead {
                continue;
            }
            let Some((hits, hits_max, body)) = drone
                .map(|drone| (drone.hits, drone.hits_max, Some(drone.body.as_slice())))
                .or_else(|| structure.map(|structure| (structure.hits, structure.hits_max, None)))
            else {
                continue;
            };
            let multiplier = final_damage_multiplier_bps(
                body,
                attrs,
                flags,
                "Kinetic",
                body_registry,
                damage_registry,
                resistance_registry,
            );
            virtual_hits.insert(entity, (hits, hits_max, multiplier));
        }
    }

    enum Contribution {
        Regular {
            target: Entity,
            damage: u32,
            heal: u32,
        },
        Leech(crate::systems::LeechCombatIntent),
    }

    let mut regular_targets = damage_by_target
        .keys()
        .chain(heal_by_target.keys())
        .copied()
        .collect::<Vec<_>>();
    regular_targets.sort_by_key(|entity| entity.to_bits());
    regular_targets.dedup();
    let mut contributions = regular_targets
        .into_iter()
        .map(|target| Contribution::Regular {
            target,
            damage: damage_by_target.get(&target).copied().unwrap_or_default(),
            heal: heal_by_target.get(&target).copied().unwrap_or_default(),
        })
        .chain(leech_intents.into_iter().map(Contribution::Leech))
        .collect::<Vec<_>>();
    contributions.sort_by_key(|contribution| match contribution {
        Contribution::Regular { target, .. } => (target.to_bits(), 0, 0),
        Contribution::Leech(intent) => (
            intent.target.to_bits(),
            intent.source.to_bits(),
            intent.sort_key,
        ),
    });

    for contribution in contributions {
        let Contribution::Leech(intent) = contribution else {
            let Contribution::Regular {
                target,
                damage,
                heal,
            } = contribution
            else {
                unreachable!()
            };
            let Some((hits, hits_max, _)) = virtual_hits.get_mut(&target) else {
                continue;
            };
            if damage >= heal {
                *hits = hits.saturating_sub(damage - heal);
            } else {
                *hits = hits.saturating_add(heal - damage).min(*hits_max);
            }
            continue;
        };
        let Some((target_hits, _, target_multiplier)) = virtual_hits.get(&intent.target).copied()
        else {
            continue;
        };
        let mitigated = scale_bps(intent.base_damage, target_multiplier);
        let actual_damage = rules.scale_damage(mitigated).min(target_hits);
        if let Some((hits, _, _)) = virtual_hits.get_mut(&intent.target) {
            *hits = hits.saturating_sub(actual_damage);
        }
        let requested_heal = leech_self_heal(actual_damage, intent.heal_bps);
        let self_heal = virtual_hits
            .get_mut(&intent.source)
            .map(|(hits, hits_max, _)| {
                let applied = requested_heal.min(hits_max.saturating_sub(*hits));
                *hits = hits.saturating_add(applied);
                applied
            })
            .unwrap_or_default();
        if let Some(resolutions) = resolutions.as_deref_mut() {
            resolutions.entries.push(LeechResolutionEntry {
                source: intent.source,
                target: intent.target,
                actual_damage,
                self_heal,
                sort_key: intent.sort_key,
            });
        }
    }

    for (entity, (hits, _, _)) in virtual_hits {
        if let Ok((drone, structure)) = entities.p1().get_mut(entity) {
            if let Some(mut drone) = drone {
                drone.hits = hits;
            } else if let Some(mut structure) = structure {
                structure.hits = hits;
            }
        }
        if hits == 0 {
            commands.entity(entity).insert(DeathMark);
        }
    }
}

fn scale_bps(amount: u32, multiplier_bps: u32) -> u32 {
    ((amount as u64 * multiplier_bps as u64) / 10_000).min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::DEFAULT_DRONE_LIFESPAN;
    use indexmap::IndexMap;
    use swarm_engine_api::ids::RoomId;
    use swarm_engine_plugin_sdk::buffers::PendingDamageEntry;
    use swarm_engine_plugin_sdk::components::Position;

    fn spawn_test_drone(app: &mut App, owner: u32, hits: u32, hits_max: u32) -> Entity {
        app.world_mut()
            .spawn((
                Drone {
                    owner,
                    body: vec![],
                    carry: IndexMap::new(),
                    carry_capacity: 0,
                    fatigue: 0,
                    hits,
                    hits_max,
                    spawning: false,
                    age: 0,
                    last_action_tick: u64::MAX,
                    lifespan: DEFAULT_DRONE_LIFESPAN,
                },
                Position {
                    x: 0,
                    y: 0,
                    room: RoomId(0),
                },
            ))
            .id()
    }

    #[test]
    fn damage_reduces_hits() {
        let mut app = App::new();
        app.add_systems(Update, damage_application_system);
        let drone = spawn_test_drone(&mut app, 1, 100, 100);
        app.insert_resource(PendingDamage {
            entries: vec![PendingDamageEntry {
                target: drone,
                amount: 30,
                damage_type: "Kinetic".into(),
            }],
        });
        app.insert_resource(PendingHeal::default());

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 70);
    }

    #[test]
    fn damage_at_zero_marks_for_death() {
        let mut app = App::new();
        app.add_systems(Update, damage_application_system);
        let drone = spawn_test_drone(&mut app, 1, 10, 100);
        app.insert_resource(PendingDamage {
            entries: vec![PendingDamageEntry {
                target: drone,
                amount: 10,
                damage_type: "Kinetic".into(),
            }],
        });
        app.insert_resource(PendingHeal::default());

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 0);
        assert!(
            app.world().entity(drone).contains::<DeathMark>(),
            "should be marked for death"
        );
    }

    #[test]
    fn damage_skips_spawning_grace() {
        let mut app = App::new();
        app.add_systems(Update, damage_application_system);
        let drone = spawn_test_drone(&mut app, 1, 100, 100);
        app.world_mut()
            .entity_mut(drone)
            .insert(SpawningGrace { remaining: 1 });
        app.insert_resource(PendingDamage {
            entries: vec![PendingDamageEntry {
                target: drone,
                amount: 30,
                damage_type: "Kinetic".into(),
            }],
        });
        app.insert_resource(PendingHeal::default());

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 100, "SpawningGrace protects from damage");
    }

    fn install_leech_resources(app: &mut App, intents: Vec<crate::systems::LeechCombatIntent>) {
        app.insert_resource(PendingDamage::default());
        app.insert_resource(PendingHeal::default());
        app.insert_resource(PendingLeechCombat { intents });
        app.insert_resource(LeechResolution::default());
        app.insert_resource(CombatRules::default());
    }

    #[test]
    fn leech_worked_example_uses_actual_damage_and_caps_self_heal() {
        let mut app = App::new();
        app.add_systems(Update, damage_application_system);
        let source = spawn_test_drone(&mut app, 1, 99, 100);
        let target = spawn_test_drone(&mut app, 2, 6, 100);
        app.world_mut()
            .entity_mut(target)
            .insert(Attributes(vec!["Shielded".to_string()]));
        let mut damage_registry = DamageTypeRegistry::default();
        damage_registry
            .damage_types
            .get_mut("Kinetic")
            .unwrap()
            .attribute_multipliers_bps
            .insert("Shielded".to_string(), 5_000);
        app.insert_resource(damage_registry);
        install_leech_resources(
            &mut app,
            vec![crate::systems::LeechCombatIntent {
                source,
                target,
                base_damage: 15,
                damage_type: swarm_engine_api::ids::DamageType::Kinetic,
                heal_bps: 5_000,
                sort_key: 0,
            }],
        );

        app.update();

        assert_eq!(app.world().entity(target).get::<Drone>().unwrap().hits, 0);
        assert_eq!(app.world().entity(source).get::<Drone>().unwrap().hits, 100);
        assert!(app.world().entity(target).contains::<DeathMark>());
        let resolutions = app.world().resource::<LeechResolution>();
        assert_eq!(resolutions.entries[0].actual_damage, 6);
        assert_eq!(resolutions.entries[0].self_heal, 1);
    }

    #[test]
    fn leech_skips_spawning_grace_target() {
        let mut app = App::new();
        app.add_systems(Update, damage_application_system);
        let source = spawn_test_drone(&mut app, 1, 90, 100);
        let target = spawn_test_drone(&mut app, 2, 50, 100);
        app.world_mut()
            .entity_mut(target)
            .insert(SpawningGrace { remaining: 1 });
        install_leech_resources(
            &mut app,
            vec![crate::systems::LeechCombatIntent {
                source,
                target,
                base_damage: 15,
                damage_type: swarm_engine_api::ids::DamageType::Kinetic,
                heal_bps: 5_000,
                sort_key: 0,
            }],
        );

        app.update();

        assert_eq!(app.world().entity(target).get::<Drone>().unwrap().hits, 50);
        assert_eq!(app.world().entity(source).get::<Drone>().unwrap().hits, 90);
        assert!(app.world().resource::<LeechResolution>().entries.is_empty());
    }

    #[test]
    fn leech_skips_death_mark_target() {
        let mut app = App::new();
        app.add_systems(Update, damage_application_system);
        let source = spawn_test_drone(&mut app, 1, 90, 100);
        let target = spawn_test_drone(&mut app, 2, 0, 100);
        app.world_mut().entity_mut(target).insert(DeathMark);
        install_leech_resources(
            &mut app,
            vec![crate::systems::LeechCombatIntent {
                source,
                target,
                base_damage: 15,
                damage_type: swarm_engine_api::ids::DamageType::Kinetic,
                heal_bps: 5_000,
                sort_key: 0,
            }],
        );

        app.update();

        assert_eq!(app.world().entity(target).get::<Drone>().unwrap().hits, 0);
        assert_eq!(app.world().entity(source).get::<Drone>().unwrap().hits, 90);
        assert!(app.world().resource::<LeechResolution>().entries.is_empty());
    }
}
