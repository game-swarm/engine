use std::collections::HashMap;

use bevy::prelude::Resource as BevyResource;
use serde::{Deserialize, Serialize};

use crate::components::PlayerId;

/// Arena room mode per spec §9.1.5
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArenaRoomMode {
    /// Player vs Player (1v1 or NvN)
    PvP,
    /// Player vs Environment (NPC scenario)
    PvE,
}

/// Admin registry for active Arena rooms
#[derive(BevyResource, Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArenaRoomAdmin {
    pub active_rooms: HashMap<u64, ArenaRoomRecord>,
    next_room_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaRoomRecord {
    pub room_id: u64,
    pub mode: ArenaRoomMode,
    pub owner: Option<PlayerId>,
    pub players: Vec<PlayerId>,
    pub state: String,
}

impl ArenaRoomAdmin {
    /// Create a new room and return its ID.
    pub fn create_room(
        &mut self,
        mode: ArenaRoomMode,
        owner: Option<PlayerId>,
    ) -> u64 {
        let room_id = self.next_room_id;
        self.next_room_id += 1;
        self.active_rooms.insert(
            room_id,
            ArenaRoomRecord {
                room_id,
                mode,
                owner,
                players: Vec::new(),
                state: "creating".to_string(),
            },
        );
        room_id
    }

    /// List all active room IDs.
    pub fn list_rooms(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.active_rooms.keys().copied().collect();
        ids.sort();
        ids
    }

    /// Kick a player from a room. Returns true if the player was present.
    pub fn kick_player(&mut self, room_id: u64, player_id: PlayerId) -> bool {
        if let Some(room) = self.active_rooms.get_mut(&room_id) {
            let len_before = room.players.len();
            room.players.retain(|p| *p != player_id);
            room.players.len() < len_before
        } else {
            false
        }
    }

    /// Close a room, removing it from the registry.
    pub fn close_room(&mut self, room_id: u64) -> bool {
        self.active_rooms.remove(&room_id).is_some()
    }

    /// Get a room record.
    pub fn get_room(&self, room_id: u64) -> Option<&ArenaRoomRecord> {
        self.active_rooms.get(&room_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_admin_create_list_close() {
        let mut admin = ArenaRoomAdmin::default();
        let id = admin.create_room(ArenaRoomMode::PvP, Some(1));
        assert_eq!(admin.list_rooms(), vec![id]);
        assert!(admin.close_room(id));
        assert!(admin.list_rooms().is_empty());
    }

    #[test]
    fn arena_admin_kick_player() {
        let mut admin = ArenaRoomAdmin::default();
        let id = admin.create_room(ArenaRoomMode::PvP, Some(1));
        if let Some(room) = admin.active_rooms.get_mut(&id) {
            room.players.push(2);
            room.players.push(3);
        }
        assert!(admin.kick_player(id, 2));
        assert!(!admin.kick_player(id, 99)); // Not in room
        let room = admin.get_room(id).unwrap();
        assert_eq!(room.players, vec![3]);
    }

    #[test]
    fn arena_pve_room_creation() {
        let mut admin = ArenaRoomAdmin::default();
        let pve_id = admin.create_room(ArenaRoomMode::PvE, None);
        let pvp_id = admin.create_room(ArenaRoomMode::PvP, Some(1));
        assert_eq!(admin.list_rooms(), vec![pve_id, pvp_id]);
        let pve = admin.get_room(pve_id).unwrap();
        assert_eq!(pve.mode, ArenaRoomMode::PvE);
    }
}
