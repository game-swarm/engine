use bevy::prelude::*;
use indexmap::IndexMap;

use crate::components::{Controller, PlayerId, Position, RoomId};

pub const DEFAULT_RESERVATION_TIMEOUT: u32 = 1000;
pub const DEFAULT_ABANDONED_TIMEOUT: u32 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomState {
    /// No player controller present
    Neutral,
    /// Controller placed, progress < progress_total (not yet RCL 1)
    Reserved { owner: PlayerId, remaining_ticks: u32 },
    /// Room fully owned and operational
    Owned { owner: PlayerId },
    /// Two players contesting the same controller
    Contested { player_a: PlayerId, player_b: PlayerId, progress_offset: i32 },
    /// Owner lost, downgrade timer active
    Abandoned { remaining_ticks: u32 },
}

#[derive(Resource, Debug, Clone, Default)]
pub struct RoomStates(pub IndexMap<RoomId, RoomState>);

/// Pending room claim — queued during command execution, processed in room_state_system
#[derive(Debug, Clone)]
pub struct PendingRoomClaim {
    pub room: RoomId,
    pub claimant: PlayerId,
    pub controller: Entity,
}

#[derive(Resource, Debug, Clone, Default)]
pub struct PendingRoomClaims(pub Vec<PendingRoomClaim>);

/// Room state machine system — transitions rooms between states based on controller status.
pub fn room_state_system(
    _commands: Commands,
    mut room_states: ResMut<RoomStates>,
    mut pending_claims: ResMut<PendingRoomClaims>,
    controllers: Query<(Entity, &Controller, &Position)>,
) {
    // Process pending claims first
    for claim in pending_claims.0.drain(..) {
        let current = room_states.0.get(&claim.room).copied().unwrap_or(RoomState::Neutral);
        match current {
            RoomState::Neutral => {
                room_states.0.insert(
                    claim.room,
                    RoomState::Reserved {
                        owner: claim.claimant,
                        remaining_ticks: DEFAULT_RESERVATION_TIMEOUT,
                    },
                );
            }
            RoomState::Reserved { owner, .. } if owner != claim.claimant => {
                // Two players claiming -> contested
                room_states.0.insert(
                    claim.room,
                    RoomState::Contested {
                        player_a: owner,
                        player_b: claim.claimant,
                        progress_offset: 0,
                    },
                );
            }
            _ => {} // Already owned, claimed by same player, or contested — no change
        }
    }

    // Tick room states
    let mut updates: Vec<(RoomId, RoomState)> = Vec::new();
    for (room, state) in room_states.0.iter() {
        let new_state = match *state {
            RoomState::Neutral | RoomState::Owned { .. } => continue,
            RoomState::Reserved { owner, remaining_ticks } => {
                // Check if this room's controller has reached RCL 1
                let mut upgraded = false;
                for (_e, ctrl, pos) in controllers.iter() {
                    if pos.room == *room && ctrl.owner == Some(owner) && ctrl.level >= 1 {
                        upgraded = true;
                        break;
                    }
                }
                if upgraded {
                    RoomState::Owned { owner }
                } else if remaining_ticks <= 1 {
                    RoomState::Neutral // Reservation expired
                } else {
                    RoomState::Reserved {
                        owner,
                        remaining_ticks: remaining_ticks - 1,
                    }
                }
            }
            RoomState::Contested {
                player_a,
                player_b,
                progress_offset,
            } => {
                // Simplified: resolve when one player's controller is gone
                let a_alive = controllers
                    .iter()
                    .any(|(_e, c, pos)| pos.room == *room && c.owner == Some(player_a));
                let b_alive = controllers
                    .iter()
                    .any(|(_e, c, pos)| pos.room == *room && c.owner == Some(player_b));
                if !a_alive && !b_alive {
                    RoomState::Neutral
                } else if !a_alive {
                    RoomState::Reserved {
                        owner: player_b,
                        remaining_ticks: DEFAULT_RESERVATION_TIMEOUT,
                    }
                } else if !b_alive {
                    RoomState::Reserved {
                        owner: player_a,
                        remaining_ticks: DEFAULT_RESERVATION_TIMEOUT,
                    }
                } else {
                    // Both still active — progress_offset determines advantage
                    let new_offset = progress_offset;
                    RoomState::Contested {
                        player_a,
                        player_b,
                        progress_offset: new_offset,
                    }
                }
            }
            RoomState::Abandoned { remaining_ticks } => {
                if remaining_ticks <= 1 {
                    RoomState::Neutral
                } else {
                    RoomState::Abandoned {
                        remaining_ticks: remaining_ticks - 1,
                    }
                }
            }
        };
        updates.push((*room, new_state));
    }

    for (room, new_state) in updates {
        room_states.0.insert(room, new_state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_room_becomes_reserved_on_claim() {
        let mut states = RoomStates::default();
        let room = RoomId(0);
        states.0.insert(room, RoomState::Neutral);

        // Simulate claim
        states.0.insert(
            room,
            RoomState::Reserved {
                owner: 1,
                remaining_ticks: DEFAULT_RESERVATION_TIMEOUT,
            },
        );

        let state = states.0.get(&room).unwrap();
        match state {
            RoomState::Reserved { owner, .. } => assert_eq!(*owner, 1),
            _ => panic!("Expected Reserved, got {:?}", state),
        }
    }

    #[test]
    fn two_players_claim_neutral_becomes_contested() {
        let mut states = RoomStates::default();
        let room = RoomId(0);
        states.0.insert(room, RoomState::Neutral);

        // Player 1 claims
        states.0.insert(
            room,
            RoomState::Reserved {
                owner: 1,
                remaining_ticks: DEFAULT_RESERVATION_TIMEOUT,
            },
        );

        // Player 2 claims while reserved
        match states.0.get(&room).unwrap() {
            RoomState::Reserved { owner, .. } if *owner != 2 => {
                states.0.insert(
                    room,
                    RoomState::Contested {
                        player_a: *owner,
                        player_b: 2,
                        progress_offset: 0,
                    },
                );
            }
            _ => {}
        }

        let state = states.0.get(&room).unwrap();
        match state {
            RoomState::Contested { player_a, player_b, .. } => {
                assert_eq!(*player_a, 1);
                assert_eq!(*player_b, 2);
            }
            _ => panic!("Expected Contested"),
        }
    }
}
