use bevy::prelude::*;

use crate::command::{
    apply_command, source_gate, validate_command, CommandIntent, CommandResult, CommandSource,
    RawCommand, Tick,
};
use crate::components::*;
use crate::resources::ResourceRegistry;
use crate::rule_module::{rhai_rule_module_tick_end_system, RhaiRuleModules};
use crate::systems::*;

pub struct SwarmWorld {
    pub app: App,
}

impl SwarmWorld {
    pub fn run_tick(&mut self) {
        self.app.update();
    }

    pub fn submit_intent(
        &mut self,
        player_id: PlayerId,
        tick: Tick,
        source: CommandSource,
        intent: CommandIntent,
    ) -> CommandResult {
        let raw = source_gate(player_id, tick, source, intent)?;
        self.submit_raw_command(raw)
    }

    pub fn submit_raw_command(&mut self, raw: RawCommand) -> CommandResult {
        let validated = validate_command(self.app.world_mut(), raw)?;
        apply_command(self.app.world_mut(), validated)
    }

    pub fn spawn_drone(&mut self, owner: PlayerId, x: i32, y: i32, body: Vec<BodyPart>) -> Entity {
        let position = Position {
            x,
            y,
            room: RoomId(0),
        };
        let entity = self
            .app
            .world_mut()
            .spawn((position, Owner(owner), Drone::new(owner, body)))
            .id();
        let mut counts = self.app.world_mut().resource_mut::<RoomDroneCounts>();
        *counts.0.entry((position.room, owner)).or_default() += 1;
        entity
    }

    pub fn queue_spawn(&mut self, owner: PlayerId, x: i32, y: i32, body: Vec<BodyPart>) {
        self.app
            .world_mut()
            .resource_mut::<PendingSpawnQueue>()
            .0
            .push(PendingSpawn {
                owner,
                body,
                position: Position {
                    x,
                    y,
                    room: RoomId(0),
                },
            });
    }

    pub fn get_terrain(&self, room: RoomId, x: i32, y: i32) -> Option<TerrainType> {
        self.app
            .world()
            .resource::<RoomTerrains>()
            .get_terrain(Position { x, y, room })
    }

    pub fn set_terrain(&mut self, room: RoomId, x: i32, y: i32, terrain: TerrainType) -> bool {
        let position = Position { x, y, room };
        if !self
            .app
            .world_mut()
            .resource_mut::<RoomTerrains>()
            .set_terrain(position, terrain)
        {
            return false;
        }

        let mut query = self.app.world_mut().query::<(&Position, &mut Terrain)>();
        for (terrain_position, mut terrain_component) in query.iter_mut(self.app.world_mut()) {
            if *terrain_position == position {
                terrain_component.0 = terrain;
                return true;
            }
        }

        self.app.world_mut().spawn((position, Terrain(terrain)));
        true
    }

    pub fn is_passable(&self, room: RoomId, x: i32, y: i32) -> bool {
        self.app
            .world()
            .resource::<RoomTerrains>()
            .is_passable(Position { x, y, room })
    }

    pub fn state_checksum(&mut self) -> u64 {
        state_checksum(self.app.world_mut())
    }
}

pub fn create_world() -> SwarmWorld {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.init_resource::<PendingSpawnQueue>();
    app.init_resource::<RoomDroneCounts>();
    app.init_resource::<PendingCombat>();
    app.init_resource::<ResourceRegistry>();
    app.init_resource::<RhaiRuleModules>();
    app.add_systems(
        Update,
        (
            death_mark_system,
            spawn_system,
            regeneration_system,
            combat_system,
            decay_system,
            rhai_rule_module_tick_end_system,
            death_cleanup_system,
        )
            .chain(),
    );

    let room = RoomId(0);
    let mut terrains = RoomTerrains::default();
    terrains.0.insert(room, RoomTerrain::default_room());
    app.insert_resource(terrains.clone());

    for (x, y, terrain) in terrains.0[&room].iter() {
        app.world_mut()
            .spawn((Position { x, y, room }, Terrain(terrain)));
    }

    let source_def = app
        .world()
        .resource::<ResourceRegistry>()
        .source("EnergyField")
        .cloned()
        .expect("default ResourceRegistry must define EnergyField");
    app.world_mut().spawn((
        Position { x: 25, y: 25, room },
        Source {
            produces: source_def.produces,
            capacity: source_def.capacity,
            ticks_to_regeneration: source_def.regeneration,
        },
    ));

    SwarmWorld { app }
}

/// Compute a deterministic, stable checksum over the full world state.
///
/// The checksum is built from a canonical byte stream of tracked ECS components:
/// each component group is tagged, each entity/component row is sorted into a
/// deterministic order, and the resulting stream is hashed with BLAKE3. The
/// first eight digest bytes are returned as a little-endian `u64` for compact
/// tick traces and replay comparisons.
pub fn state_checksum(world: &mut World) -> u64 {
    fn hash_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    fn tag(hasher: &mut blake3::Hasher, name: &str) {
        hash_bytes(hasher, name.as_bytes());
    }

    fn opt_u32_bytes(value: Option<u32>) -> [u8; 8] {
        match value {
            Some(value) => {
                let mut bytes = [0_u8; 8];
                bytes[0] = 1;
                bytes[4..].copy_from_slice(&value.to_le_bytes());
                bytes
            }
            None => [0_u8; 8],
        }
    }

    let mut hasher = blake3::Hasher::new();

    // --- Terrain ---
    tag(&mut hasher, "terrain");
    let mut terrain = world
        .query::<(&Position, &Terrain)>()
        .iter(world)
        .map(|(p, t)| (p.room.0, p.x, p.y, t.0 as u8))
        .collect::<Vec<_>>();
    terrain.sort_unstable();
    for (room, x, y, t) in &terrain {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        hasher.update(&[*t]);
    }

    // --- Sources ---
    tag(&mut hasher, "sources");
    let mut sources = world
        .query::<(&Position, &Source)>()
        .iter(world)
        .map(|(p, s)| {
            // Flatten produces into sorted vec for determinism.
            let mut produces: Vec<(String, u32)> =
                s.produces.iter().map(|(k, v)| (k.clone(), *v)).collect();
            produces.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            (
                p.room.0,
                p.x,
                p.y,
                produces,
                s.capacity,
                s.ticks_to_regeneration,
            )
        })
        .collect::<Vec<_>>();
    sources.sort_unstable_by_key(|(room, x, y, _, capacity, regen)| {
        (*room, *x, *y, *capacity, *regen)
    });
    for (room, x, y, produces, capacity, regen) in &sources {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        for (k, v) in produces {
            hash_bytes(&mut hasher, k.as_bytes());
            hasher.update(&v.to_le_bytes());
        }
        hasher.update(&capacity.to_le_bytes());
        hasher.update(&regen.to_le_bytes());
    }

    // --- Drones ---
    tag(&mut hasher, "drones");
    let mut drones = world
        .query::<(&Position, &Drone)>()
        .iter(world)
        .map(|(p, d)| {
            let body_bytes: Vec<u8> = d.body.iter().map(|b| *b as u8).collect();
            (
                p.room.0,
                p.x,
                p.y,
                d.owner,
                body_bytes,
                d.carry
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect::<Vec<_>>(),
                d.carry_capacity,
                d.fatigue,
                d.hits,
                d.hits_max,
                d.spawning as u8,
                d.age,
                d.lifespan,
            )
        })
        .collect::<Vec<_>>();
    drones.sort_unstable_by_key(
        |(
            room,
            x,
            y,
            owner,
            _,
            _,
            carry_capacity,
            fatigue,
            hits,
            hits_max,
            spawning,
            age,
            lifespan,
        )| {
            (
                *room,
                *x,
                *y,
                *owner,
                *carry_capacity,
                *fatigue,
                *hits,
                *hits_max,
                *spawning,
                *age,
                *lifespan,
            )
        },
    );
    for (
        room,
        x,
        y,
        owner,
        body,
        carry,
        carry_capacity,
        fatigue,
        hits,
        hits_max,
        spawning,
        age,
        lifespan,
    ) in &drones
    {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        hasher.update(&owner.to_le_bytes());
        hash_bytes(&mut hasher, body);
        for (name, amount) in carry {
            hash_bytes(&mut hasher, name.as_bytes());
            hasher.update(&amount.to_le_bytes());
        }
        hasher.update(&carry_capacity.to_le_bytes());
        hasher.update(&fatigue.to_le_bytes());
        hasher.update(&hits.to_le_bytes());
        hasher.update(&hits_max.to_le_bytes());
        hasher.update(&[*spawning]);
        hasher.update(&age.to_le_bytes());
        hasher.update(&lifespan.to_le_bytes());
    }

    // --- Structures ---
    tag(&mut hasher, "structures");
    let mut structures = world
        .query::<(&Position, &Structure)>()
        .iter(world)
        .map(|(p, s)| {
            (
                p.room.0,
                p.x,
                p.y,
                s.structure_type as u8,
                s.owner.unwrap_or(u32::MAX),
                s.hits,
                s.hits_max,
                s.energy,
                s.energy_capacity,
                s.cooldown,
            )
        })
        .collect::<Vec<_>>();
    structures.sort_unstable_by_key(
        |(room, x, y, structure_type, owner, hits, hits_max, energy, capacity, cooldown)| {
            (
                *room,
                *x,
                *y,
                *structure_type,
                *owner,
                *hits,
                *hits_max,
                *energy,
                *capacity,
                *cooldown,
            )
        },
    );
    for (room, x, y, structure_type, owner, hits, hits_max, energy, capacity, cooldown) in
        &structures
    {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        hasher.update(&[*structure_type]);
        hasher.update(&owner.to_le_bytes());
        hasher.update(&hits.to_le_bytes());
        hasher.update(&hits_max.to_le_bytes());
        hasher.update(&opt_u32_bytes(*energy));
        hasher.update(&opt_u32_bytes(*capacity));
        hasher.update(&cooldown.to_le_bytes());
    }

    // --- Controllers ---
    tag(&mut hasher, "controllers");
    let mut controllers = world
        .query::<(&Position, &Controller)>()
        .iter(world)
        .map(|(p, c)| {
            (
                p.room.0,
                p.x,
                p.y,
                c.owner.unwrap_or(u32::MAX),
                c.level,
                c.progress,
                c.progress_total,
                c.downgrade_timer,
                c.safe_mode,
                c.safe_mode_available,
                c.safe_mode_cooldown,
            )
        })
        .collect::<Vec<_>>();
    controllers.sort_unstable();
    for (
        room,
        x,
        y,
        owner,
        level,
        progress,
        progress_total,
        downgrade_timer,
        safe_mode,
        safe_mode_available,
        safe_mode_cooldown,
    ) in &controllers
    {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        hasher.update(&owner.to_le_bytes());
        hasher.update(&[*level]);
        hasher.update(&progress.to_le_bytes());
        hasher.update(&progress_total.to_le_bytes());
        hasher.update(&downgrade_timer.to_le_bytes());
        hasher.update(&safe_mode.to_le_bytes());
        hasher.update(&safe_mode_available.to_le_bytes());
        hasher.update(&safe_mode_cooldown.to_le_bytes());
    }

    // --- Dropped resources ---
    tag(&mut hasher, "resources");
    let mut resources = world
        .query::<(&Position, &crate::components::Resource)>()
        .iter(world)
        .map(|(p, r)| {
            let mut amounts: Vec<(String, u32)> =
                r.amounts.iter().map(|(k, v)| (k.clone(), *v)).collect();
            amounts.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            (p.room.0, p.x, p.y, amounts)
        })
        .collect::<Vec<_>>();
    resources.sort_unstable_by_key(|(room, x, y, _)| (*room, *x, *y));
    for (room, x, y, amounts) in &resources {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        for (name, amount) in amounts {
            hash_bytes(&mut hasher, name.as_bytes());
            hasher.update(&amount.to_le_bytes());
        }
    }

    let digest = hasher.finalize();
    u64::from_le_bytes(
        digest.as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 digest has 32 bytes"),
    )
}
