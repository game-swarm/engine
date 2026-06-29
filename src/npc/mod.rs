pub mod ai;
pub mod components;
pub mod events;
pub mod loot;
pub mod strongholds;

use bevy::prelude::*;

pub use ai::{
    DEFAULT_NPC_AGGRO_RANGE, DEFAULT_NPC_ATTACK_RANGE, DEFAULT_NPC_DAMAGE, Npc, NpcMode,
    NpcSpecialAttack, npc_ai_system, npc_combat_system,
};
pub use components::*;

use crate::components::{Position, RoomId, RoomTerrains};
use crate::resources::CurrentTick;

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct NpcSpawnState {
    next_index: u32,
}

pub fn npc_spawn_system(
    mut commands: Commands,
    current_tick: Res<CurrentTick>,
    mut spawn_state: ResMut<NpcSpawnState>,
    terrains: Res<RoomTerrains>,
) {
    let tick = current_tick.0;
    for npc_type in NpcType::spawn_cycle_types(tick) {
        let count = npc_type.spawn_count(tick);
        for _ in 0..count {
            let position = next_spawn_position(&mut spawn_state, &terrains);
            commands.spawn((
                position,
                npc_type,
                NpcHp::for_type(npc_type),
                NpcDamage::for_type(npc_type),
                NpcBehavior::for_type(npc_type),
                NpcZone::for_position(position),
                NpcDrop::for_type(npc_type),
            ));
        }
    }
}

fn next_spawn_position(spawn_state: &mut NpcSpawnState, terrains: &RoomTerrains) -> Position {
    let room = RoomId(0);
    for _ in 0..2_500 {
        let index = spawn_state.next_index;
        spawn_state.next_index = spawn_state.next_index.wrapping_add(1);
        let position = Position {
            x: 2 + ((index.wrapping_mul(7)) % 46) as i32,
            y: 2 + ((index.wrapping_mul(11)) % 46) as i32,
            room,
        };
        if terrains.is_passable(position) {
            return position;
        }
    }
    Position { x: 25, y: 25, room }
}
