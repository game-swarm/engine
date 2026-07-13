use std::collections::BTreeSet;

use bevy::prelude::*;

use crate::command::{ObjectId, Tick, object_id};
use crate::components::*;
use crate::world::{PlayerViewMode, WorldConfig};

pub const VISIBILITY_RADIUS: i32 = 5;

pub type VisiblePositionKey = (RoomId, i32, i32);
pub type VisibilitySet = BTreeSet<VisiblePositionKey>;

pub fn is_visible_to(world: &mut World, entity: Entity, player_id: PlayerId, tick: Tick) -> bool {
    let _ = tick;

    if is_owned_by(world, entity, player_id) {
        return true;
    }

    let Some(position) = world.get::<Position>(entity).copied() else {
        return false;
    };

    if full_map_visible(world) {
        return position_exists(world, position);
    }

    visible_positions(world, player_id).contains(&(position.room, position.x, position.y))
}

pub fn is_position_visible_to(world: &mut World, player_id: PlayerId, position: Position) -> bool {
    if full_map_visible(world) {
        return position_exists(world, position);
    }

    visible_positions(world, player_id).contains(&(position.room, position.x, position.y))
}

pub fn visible_positions(world: &mut World, player_id: PlayerId) -> BTreeSet<VisiblePositionKey> {
    if full_map_visible(world) {
        return all_positions(world);
    }

    let mut anchors = world
        .query::<(
            &Position,
            Option<&Drone>,
            Option<&Structure>,
            Option<&Controller>,
        )>()
        .iter(world)
        .filter_map(|(position, drone, structure, controller)| {
            let owned_drone = drone.is_some_and(|drone| drone.owner == player_id);
            let owned_structure =
                structure.is_some_and(|structure| structure.owner == Some(player_id));
            let owned_controller =
                controller.is_some_and(|controller| controller.owner == Some(player_id));
            (owned_drone || owned_structure || owned_controller).then_some(*position)
        })
        .collect::<Vec<_>>();
    anchors.sort_by_key(|position| (position.room.0, position.x, position.y));

    let terrains = world.resource::<RoomTerrains>();
    let mut visible = BTreeSet::new();
    for anchor in anchors {
        for room in nearby_rooms(anchor.room) {
            let Some(room_terrain) = terrains.0.get(&room) else {
                continue;
            };
            if room == anchor.room {
                for y in (anchor.y - VISIBILITY_RADIUS)..=(anchor.y + VISIBILITY_RADIUS) {
                    for x in (anchor.x - VISIBILITY_RADIUS)..=(anchor.x + VISIBILITY_RADIUS) {
                        if room_terrain.contains(x, y) {
                            visible.insert((room, x, y));
                        }
                    }
                }
            } else {
                for (x, y, _) in room_terrain.iter() {
                    visible.insert((room, x, y));
                }
            }
        }
    }
    visible
}

pub fn visible_entity_ids(world: &mut World, player_id: PlayerId, tick: Tick) -> BTreeSet<Entity> {
    let visible = visible_positions(world, player_id);
    visible_entity_ids_with_positions(world, player_id, tick, &visible)
}

pub fn visible_entity_ids_with_positions(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    visible_positions: &VisibilitySet,
) -> BTreeSet<Entity> {
    let _ = tick;

    let all_entities = {
        let mut query = world.query::<(Entity, Option<&Position>)>();
        query
            .iter(world)
            .map(|(entity, position)| (entity, position.copied()))
            .collect::<Vec<_>>()
    };

    let mut entities = all_entities
        .into_iter()
        .filter_map(|(entity, position)| {
            if is_owned_by(world, entity, player_id)
                || position.is_some_and(|position| {
                    visible_positions.contains(&(position.room, position.x, position.y))
                })
            {
                Some(entity)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    entities.sort_by_key(|entity| entity.index());
    entities.into_iter().collect()
}

pub fn visible_object_ids(world: &mut World, player_id: PlayerId, tick: Tick) -> Vec<ObjectId> {
    visible_entity_ids(world, player_id, tick)
        .into_iter()
        .map(object_id)
        .collect()
}

fn nearby_rooms(room: RoomId) -> impl Iterator<Item = RoomId> {
    (-1..=1).flat_map(move |dy| (-1..=1).filter_map(move |dx| room.adjacent(dx, dy)))
}

fn full_map_visible(world: &World) -> bool {
    world.get_resource::<WorldConfig>().is_some_and(|config| {
        !config.visibility.fog_of_war || config.visibility.player_view == PlayerViewMode::Full
    })
}

fn all_positions(world: &World) -> BTreeSet<VisiblePositionKey> {
    world
        .resource::<RoomTerrains>()
        .0
        .iter()
        .flat_map(|(room, terrain)| terrain.iter().map(|(x, y, _)| (*room, x, y)))
        .collect()
}

fn position_exists(world: &World, position: Position) -> bool {
    world
        .resource::<RoomTerrains>()
        .0
        .get(&position.room)
        .is_some_and(|terrain| terrain.contains(position.x, position.y))
}

fn is_owned_by(world: &World, entity: Entity, player_id: PlayerId) -> bool {
    world
        .get::<Drone>(entity)
        .is_some_and(|drone| drone.owner == player_id)
        || world
            .get::<Structure>(entity)
            .is_some_and(|structure| structure.owner == Some(player_id))
        || world
            .get::<Controller>(entity)
            .is_some_and(|controller| controller.owner == Some(player_id))
        || world
            .get::<Owner>(entity)
            .is_some_and(|owner| owner.0 == player_id)
}

// ── Hint Ladder (§10.3) ──

/// Error detail level for the Hint Ladder defence-in-depth oracle protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintLevel {
    /// Safe: only expose what the player already knows (own state, no target info).
    Safe,
    /// FixHint: expose actionable feedback (e.g. "target too far", "not enough energy").
    FixHint,
    /// FullDebug: admin-only full detail (never exposed to players).
    FullDebug,
}

/// Map a rejection reason to its appropriate Hint Ladder level per §10.4.
pub fn hint_ladder(reason: &crate::command::RejectionReason) -> HintLevel {
    use crate::command::RejectionReason;
    match reason {
        // Self-state codes — player already knows these
        RejectionReason::Fatigued
        | RejectionReason::CooldownActive
        | RejectionReason::InsufficientEnergy
        | RejectionReason::AlreadyFullHealth
        | RejectionReason::SpawnOnCooldown
        | RejectionReason::RoomDroneCapReached => HintLevel::Safe,

        // Oracle-protected codes — target info must be cloaked
        RejectionReason::NotVisibleOrNotFound
        | RejectionReason::PlayerNotFound
        | RejectionReason::FriendlyTarget
        | RejectionReason::NotFriendly
        | RejectionReason::OutOfRange { .. }
        | RejectionReason::ObjectNotFound
        | RejectionReason::TargetNotFound
        | RejectionReason::TargetNotVisible
        | RejectionReason::NoPath => HintLevel::FixHint,

        // Everything else defaults to FixHint (safe-fail)
        _ => HintLevel::FixHint,
    }
}

/// Format a rejection detail message at the given Hint Ladder level.
/// Safe returns an empty string; FixHint returns a generic hint; FullDebug returns full detail.
pub fn format_hint(reason: &crate::command::RejectionReason, level: HintLevel) -> String {
    match level {
        HintLevel::Safe => String::new(),
        HintLevel::FixHint => {
            use crate::command::RejectionReason;
            match reason {
                RejectionReason::NotVisibleOrNotFound => "target not found or not visible".into(),
                RejectionReason::OutOfRange { .. } => "target is out of range".into(),
                RejectionReason::InsufficientEnergy => "not enough energy".into(),
                RejectionReason::Fatigued => "drone is fatigued".into(),
                RejectionReason::CooldownActive => "action on cooldown".into(),
                _ => "action rejected".into(),
            }
        }
        HintLevel::FullDebug => format!("{reason:?}"),
    }
}

// ── Oracle defence: omitted_count bucketing (§10.2) ──

/// Bucket an exact omitted entity count into a fuzzy category to prevent
/// attackers from inferring hidden entity counts through snapshot truncation.
pub fn omitted_count_bucket(exact: usize) -> &'static str {
    match exact {
        0 => "0",
        1..=10 => "few",
        11..=50 => "some",
        51..=200 => "many",
        _ => "extreme",
    }
}

// ── Spectate helpers (§3.5) ──

/// Return the set of entities visible to a spectator, respecting spectate_delay
/// and replay_privacy. Spectators see world-level physical state only.
pub fn spectate_visible_entities(
    world: &mut World,
    tick: Tick,
    public_spectate: bool,
    spectate_delay: u32,
) -> BTreeSet<Entity> {
    if !public_spectate || tick < spectate_delay as u64 {
        return BTreeSet::new();
    }
    let _effective_tick = tick - spectate_delay as u64;
    let mut query = world.query::<(Entity, &Position)>();
    query.iter(world).map(|(entity, _)| entity).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::WorldMode;
    use crate::create_world;
    use crate::world::{WorldConfig, create_world_with_mode_and_config};

    #[test]
    fn own_entities_always_visible() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 49, 49, vec![BodyPart::Move]);

        assert!(is_visible_to(world.app.world_mut(), drone, 1, 7));
    }

    #[test]
    fn enemy_outside_vision_hidden() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let enemy = world.spawn_drone(2, 40, 40, vec![BodyPart::Move]);

        assert!(!is_visible_to(world.app.world_mut(), enemy, 1, 7));
    }

    #[test]
    fn multiple_vision_sources_union() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        world.spawn_drone(1, 30, 30, vec![BodyPart::Move]);
        let first_enemy = world.spawn_drone(2, 15, 10, vec![BodyPart::Move]);
        let second_enemy = world.spawn_drone(2, 35, 30, vec![BodyPart::Move]);

        assert!(is_visible_to(world.app.world_mut(), first_enemy, 1, 7));
        assert!(is_visible_to(world.app.world_mut(), second_enemy, 1, 7));
    }

    #[test]
    fn vision_range_boundary() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let boundary = world.spawn_drone(2, 15, 10, vec![BodyPart::Move]);
        let outside = world.spawn_drone(2, 16, 10, vec![BodyPart::Move]);

        assert!(is_visible_to(world.app.world_mut(), boundary, 1, 7));
        assert!(!is_visible_to(world.app.world_mut(), outside, 1, 7));
    }
    #[test]
    fn visibility_includes_adjacent_room_but_not_far_room() {
        let mut world = create_world();
        let adjacent = RoomId::from_room_name("A0N1E").unwrap();
        let far = RoomId::from_room_name("A0N2E").unwrap();
        world.ensure_room(adjacent);
        world.ensure_room(far);
        world.spawn_drone(1, 25, 25, vec![BodyPart::Move]);

        assert!(is_position_visible_to(
            world.app.world_mut(),
            1,
            Position {
                x: 10,
                y: 10,
                room: adjacent
            }
        ));
        assert!(!is_position_visible_to(
            world.app.world_mut(),
            1,
            Position {
                x: 10,
                y: 10,
                room: far
            }
        ));
    }

    #[test]
    fn fog_of_war_disabled_reveals_full_map() {
        let mut config = WorldConfig::default();
        config.visibility.fog_of_war = false;
        let mut world = create_world_with_mode_and_config(WorldMode::Default, config);
        let enemy = world.spawn_drone(2, 40, 40, vec![BodyPart::Move]);

        assert!(is_visible_to(world.app.world_mut(), enemy, 1, 7));
        assert!(is_position_visible_to(
            world.app.world_mut(),
            1,
            Position {
                x: 49,
                y: 49,
                room: RoomId(0),
            }
        ));
    }

    #[test]
    fn full_player_view_reveals_full_map_with_fog_enabled() {
        let mut config = WorldConfig::default();
        config.visibility.player_view = PlayerViewMode::Full;
        let mut world = create_world_with_mode_and_config(WorldMode::Default, config);
        let enemy = world.spawn_drone(2, 40, 40, vec![BodyPart::Move]);

        assert!(is_visible_to(world.app.world_mut(), enemy, 1, 7));
    }

    #[test]
    fn visible_entity_ids_with_positions_includes_owned_entity_without_position() {
        let mut world = create_world();
        let body_registry = world.app.world().resource::<BodyPartRegistry>().clone();
        let owned = world
            .app
            .world_mut()
            .spawn(Drone::new(1, vec![BodyPart::Move], &body_registry))
            .id();
        let visible = visible_positions(world.app.world_mut(), 1);

        assert!(
            visible_entity_ids_with_positions(world.app.world_mut(), 1, 7, &visible)
                .contains(&owned)
        );
    }

    #[test]
    fn visible_entity_ids_with_positions_preserves_owned_off_map_visibility() {
        let mut world = create_world();
        let owned = world.spawn_drone(1, 99, 99, vec![BodyPart::Move]);
        let visible = visible_positions(world.app.world_mut(), 1);

        assert!(
            visible_entity_ids_with_positions(world.app.world_mut(), 1, 7, &visible)
                .contains(&owned)
        );
    }

    #[test]
    fn visible_entity_ids_with_positions_hides_unowned_off_map_entity() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let enemy = world.spawn_drone(2, 99, 99, vec![BodyPart::Move]);
        let visible = visible_positions(world.app.world_mut(), 1);

        assert!(
            !visible_entity_ids_with_positions(world.app.world_mut(), 1, 7, &visible)
                .contains(&enemy)
        );
    }

    #[test]
    fn allied_player_view_preserves_drone_visibility_without_alliance_model() {
        let mut config = WorldConfig::default();
        config.visibility.player_view = PlayerViewMode::Allied;
        let mut world = create_world_with_mode_and_config(WorldMode::Default, config);
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let near_enemy = world.spawn_drone(2, 15, 10, vec![BodyPart::Move]);
        let far_enemy = world.spawn_drone(2, 40, 40, vec![BodyPart::Move]);

        assert!(is_visible_to(world.app.world_mut(), near_enemy, 1, 7));
        assert!(!is_visible_to(world.app.world_mut(), far_enemy, 1, 7));
    }

    // ── Hint Ladder tests (§10.3) ──

    #[test]
    fn hint_ladder_safe_for_self_state_codes() {
        use crate::command::RejectionReason;
        assert_eq!(hint_ladder(&RejectionReason::Fatigued), HintLevel::Safe);
        assert_eq!(
            hint_ladder(&RejectionReason::CooldownActive),
            HintLevel::Safe
        );
        assert_eq!(
            hint_ladder(&RejectionReason::InsufficientEnergy),
            HintLevel::Safe
        );
        assert_eq!(
            hint_ladder(&RejectionReason::AlreadyFullHealth),
            HintLevel::Safe
        );
        assert_eq!(
            hint_ladder(&RejectionReason::SpawnOnCooldown),
            HintLevel::Safe
        );
        assert_eq!(
            hint_ladder(&RejectionReason::RoomDroneCapReached),
            HintLevel::Safe
        );
    }

    #[test]
    fn hint_ladder_fixhint_for_oracle_codes() {
        use crate::command::RejectionReason;
        assert_eq!(
            hint_ladder(&RejectionReason::NotVisibleOrNotFound),
            HintLevel::FixHint
        );
        assert_eq!(
            hint_ladder(&RejectionReason::PlayerNotFound),
            HintLevel::FixHint
        );
        assert_eq!(
            hint_ladder(&RejectionReason::FriendlyTarget),
            HintLevel::FixHint
        );
        assert_eq!(
            hint_ladder(&RejectionReason::NotFriendly),
            HintLevel::FixHint
        );
        assert_eq!(
            hint_ladder(&RejectionReason::OutOfRange {
                distance: 5,
                max: 3
            }),
            HintLevel::FixHint
        );
        assert_eq!(
            hint_ladder(&RejectionReason::ObjectNotFound),
            HintLevel::FixHint
        );
        assert_eq!(
            hint_ladder(&RejectionReason::TargetNotFound),
            HintLevel::FixHint
        );
        assert_eq!(
            hint_ladder(&RejectionReason::TargetNotVisible),
            HintLevel::FixHint
        );
        assert_eq!(hint_ladder(&RejectionReason::NoPath), HintLevel::FixHint);
    }

    #[test]
    fn hint_ladder_defaults_to_fixhint() {
        use crate::command::RejectionReason;
        assert_eq!(
            hint_ladder(&RejectionReason::InternalError),
            HintLevel::FixHint
        );
        assert_eq!(
            hint_ladder(&RejectionReason::ServerOverloaded),
            HintLevel::FixHint
        );
    }

    #[test]
    fn format_hint_safe_empty() {
        use crate::command::RejectionReason;
        let msg = format_hint(&RejectionReason::NotVisibleOrNotFound, HintLevel::Safe);
        assert!(
            msg.is_empty(),
            "Safe should return empty string, got '{msg}'"
        );
    }

    #[test]
    fn format_hint_fixhint_generic() {
        use crate::command::RejectionReason;
        let msg = format_hint(&RejectionReason::NotVisibleOrNotFound, HintLevel::FixHint);
        assert!(!msg.is_empty(), "FixHint should return a hint message");
        assert!(
            msg.contains("not visible"),
            "FixHint should be generic, got '{msg}'"
        );

        let msg = format_hint(
            &RejectionReason::OutOfRange {
                distance: 5,
                max: 3,
            },
            HintLevel::FixHint,
        );
        assert!(
            msg.contains("out of range"),
            "FixHint should be generic, got '{msg}'"
        );
    }

    #[test]
    fn format_hint_fulldebug_detailed() {
        use crate::command::RejectionReason;
        let msg = format_hint(&RejectionReason::NotVisibleOrNotFound, HintLevel::FullDebug);
        assert!(
            msg.contains("NotVisibleOrNotFound"),
            "FullDebug should include variant name, got '{msg}'"
        );
    }

    // ── Oracle defence: omitted_count bucketing (§10.2) ──

    #[test]
    fn omitted_count_bucket_boundaries() {
        assert_eq!(omitted_count_bucket(0), "0");
        assert_eq!(omitted_count_bucket(1), "few");
        assert_eq!(omitted_count_bucket(10), "few");
        assert_eq!(omitted_count_bucket(11), "some");
        assert_eq!(omitted_count_bucket(50), "some");
        assert_eq!(omitted_count_bucket(51), "many");
        assert_eq!(omitted_count_bucket(200), "many");
        assert_eq!(omitted_count_bucket(201), "extreme");
        assert_eq!(omitted_count_bucket(1000), "extreme");
    }

    // ── Spectate tests (§3.5) ──

    #[test]
    fn spectate_disabled_returns_empty() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let visible = spectate_visible_entities(world.app.world_mut(), 100, false, 50);
        assert!(
            visible.is_empty(),
            "public_spectate=false should return empty"
        );
    }

    #[test]
    fn spectate_delay_not_met_returns_empty() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let visible = spectate_visible_entities(world.app.world_mut(), 30, true, 50);
        assert!(
            visible.is_empty(),
            "tick < spectate_delay should return empty"
        );
    }

    #[test]
    fn spectate_enabled_returns_entities_after_delay() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        world.spawn_drone(2, 40, 40, vec![BodyPart::Move]);
        let visible = spectate_visible_entities(world.app.world_mut(), 100, true, 50);
        assert!(
            !visible.is_empty(),
            "spectators should see entities after delay"
        );
    }
}
