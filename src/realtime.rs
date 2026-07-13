use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use serde::{Deserialize, Serialize};

use crate::command::{ObjectId, Tick};
use crate::components::PlayerId;
use crate::mcp::{VisibleEntity, visible_entities_for_player};
use crate::world::SwarmWorld;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeDelta {
    pub tick: Tick,
    pub last_tick: Tick,
    pub player_id: PlayerId,
    #[serde(default)]
    pub full_snapshot: bool,
    pub changed_entities: Vec<VisibleEntity>,
    pub removed_entities: Vec<ObjectId>,
    pub state_checksum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeEnvelope {
    pub schema: String,
    pub payload: RealtimeDelta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WebSocketMessage {
    Connected {
        player_id: PlayerId,
    },
    Delta(RealtimeDelta),
    TickGap {
        expected_tick: Tick,
        actual_tick: Tick,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeError {
    Serialize(String),
    Publish(String),
}

pub trait NatsPublisher {
    fn publish(&mut self, subject: &str, payload: Vec<u8>) -> Result<(), RealtimeError>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryNats {
    pub messages: Vec<(String, Vec<u8>)>,
    pub fail_next: bool,
}

impl NatsPublisher for InMemoryNats {
    fn publish(&mut self, subject: &str, payload: Vec<u8>) -> Result<(), RealtimeError> {
        if self.fail_next {
            self.fail_next = false;
            return Err(RealtimeError::Publish(
                "in-memory NATS publish failed".to_string(),
            ));
        }

        self.messages.push((subject.to_string(), payload));
        Ok(())
    }
}

pub struct NatsRealtimePublisher<P> {
    nats: P,
}

impl<P> NatsRealtimePublisher<P>
where
    P: NatsPublisher,
{
    pub fn new(nats: P) -> Self {
        Self { nats }
    }

    pub fn into_inner(self) -> P {
        self.nats
    }

    pub fn publish_delta(&mut self, delta: &RealtimeDelta) -> Result<(), RealtimeError> {
        let payload = serde_json::to_vec(&RealtimeEnvelope {
            schema: "swarm.realtime.v1".to_string(),
            payload: delta.clone(),
        })
        .map_err(|error| RealtimeError::Serialize(error.to_string()))?;
        self.nats.publish("swarm.realtime.v1", payload)
    }
}

#[derive(Debug)]
pub struct WebSocketClient {
    player_id: PlayerId,
    last_tick: Option<Tick>,
    pending: VecDeque<WebSocketMessage>,
    fetch_from_tick: Option<Tick>,
    receiver: Receiver<WebSocketMessage>,
}

impl WebSocketClient {
    pub fn player_id(&self) -> PlayerId {
        self.player_id
    }

    pub fn fetch_from_tick(&self) -> Option<Tick> {
        self.fetch_from_tick
    }

    pub fn recv(&mut self) -> Option<WebSocketMessage> {
        if let Some(message) = self.pending.pop_front() {
            return Some(message);
        }

        match self.receiver.try_recv() {
            Ok(message) => {
                match &message {
                    WebSocketMessage::Delta(delta) => {
                        if let Some(last_tick) = self.last_tick
                            && delta.last_tick != last_tick
                        {
                            let expected_tick = last_tick + 1;
                            let actual_tick = delta.tick;
                            self.last_tick = Some(actual_tick);
                            self.fetch_from_tick = Some(expected_tick);
                            self.pending.push_back(message);
                            return Some(WebSocketMessage::TickGap {
                                expected_tick,
                                actual_tick,
                            });
                        }
                        self.last_tick = Some(delta.tick);
                    }
                    WebSocketMessage::TickGap { actual_tick, .. } => {
                        self.last_tick = Some(*actual_tick)
                    }
                    WebSocketMessage::Connected { .. } => {}
                }
                Some(message)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct RealtimeGateway {
    clients: Vec<WebSocketConnection>,
}

#[derive(Debug)]
struct WebSocketConnection {
    player_id: PlayerId,
    last_tick: Option<Tick>,
    sender: Sender<WebSocketMessage>,
}

impl RealtimeGateway {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn connect_websocket(&mut self, player_id: PlayerId) -> WebSocketClient {
        let (sender, receiver) = mpsc::channel();
        let _ = sender.send(WebSocketMessage::Connected { player_id });
        self.clients.push(WebSocketConnection {
            player_id,
            last_tick: None,
            sender,
        });
        WebSocketClient {
            player_id,
            last_tick: None,
            pending: VecDeque::new(),
            fetch_from_tick: None,
            receiver,
        }
    }

    pub fn receive_nats(&mut self, payload: &[u8]) -> Result<(), RealtimeError> {
        let envelope: RealtimeEnvelope = serde_json::from_slice(payload)
            .map_err(|error| RealtimeError::Serialize(error.to_string()))?;
        if envelope.schema != "swarm.realtime.v1" {
            return Err(RealtimeError::Serialize(format!(
                "unsupported realtime schema {}",
                envelope.schema
            )));
        }
        let delta = envelope.payload;

        self.clients.retain_mut(|client| {
            if client.player_id != delta.player_id {
                return true;
            }

            client.last_tick = Some(delta.tick);
            client
                .sender
                .send(WebSocketMessage::Delta(delta.clone()))
                .is_ok()
        });
        Ok(())
    }
}

pub fn compute_realtime_delta(
    world: &mut SwarmWorld,
    player_id: PlayerId,
    tick: Tick,
    last_tick: Tick,
    before: &[VisibleEntity],
) -> RealtimeDelta {
    let after = visible_entities_for_player(world.app.world_mut(), player_id);
    let before_by_id = before
        .iter()
        .map(|entity| (entity_id(entity), entity))
        .collect::<BTreeMap<_, _>>();
    let after_by_id = after
        .iter()
        .map(|entity| (entity_id(entity), entity))
        .collect::<BTreeMap<_, _>>();

    let changed_entities = after_by_id
        .iter()
        .filter_map(|(id, entity)| {
            (before_by_id.get(id).copied() != Some(*entity)).then_some((*entity).clone())
        })
        .collect::<Vec<_>>();
    let removed_entities = before_by_id
        .keys()
        .filter(|id| !after_by_id.contains_key(id))
        .copied()
        .collect::<Vec<_>>();

    RealtimeDelta {
        tick,
        last_tick,
        player_id,
        full_snapshot: false,
        changed_entities,
        removed_entities,
        state_checksum: world.state_checksum(),
    }
}

pub fn entity_id(entity: &VisibleEntity) -> ObjectId {
    match entity {
        VisibleEntity::Drone(entity) => entity.id,
        VisibleEntity::Structure(entity) => entity.id,
        VisibleEntity::Source(entity) => entity.id,
        VisibleEntity::Resource(entity) => entity.id,
        VisibleEntity::Controller(entity) => entity.id,
    }
}

#[cfg(test)]
mod tests {
    use crate::command::{CommandAction, CommandIntent, CommandSource, Direction, object_id};
    use crate::components::BodyPart;
    use crate::mcp::visible_entities_for_player;
    use crate::{RealtimeGateway, WebSocketMessage, create_world, replay_visible_entities};

    use super::{InMemoryNats, NatsRealtimePublisher, compute_realtime_delta, entity_id};

    #[test]
    fn websocket_connection_sends_connected_message() {
        let mut gateway = RealtimeGateway::new();
        let mut client = gateway.connect_websocket(7);

        assert_eq!(client.player_id(), 7);
        assert_eq!(
            client.recv(),
            Some(WebSocketMessage::Connected { player_id: 7 })
        );
        assert_eq!(client.recv(), None);
    }

    #[test]
    fn nats_gateway_pushes_delta_only_changed_entities_and_detects_gap() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let before = visible_entities_for_player(world.app.world_mut(), 1);

        world
            .submit_intent(
                1,
                1,
                CommandSource::Wasm,
                CommandIntent {
                    sequence: 1,
                    action: CommandAction::Move {
                        object_id: object_id(drone),
                        direction: Direction::Bottom,
                    },
                },
            )
            .expect("move command should be accepted");
        let delta = compute_realtime_delta(&mut world, 1, 1, 0, &before);

        assert_eq!(delta.changed_entities.len(), 1);
        assert_eq!(entity_id(&delta.changed_entities[0]), object_id(drone));
        assert!(delta.removed_entities.is_empty());

        let mut publisher = NatsRealtimePublisher::new(InMemoryNats::default());
        publisher.publish_delta(&delta).expect("publish delta");
        let nats = publisher.into_inner();
        assert_eq!(nats.messages[0].0, "swarm.realtime.v1");
        let envelope: super::RealtimeEnvelope =
            serde_json::from_slice(&nats.messages[0].1).unwrap();
        assert_eq!(envelope.schema, "swarm.realtime.v1");
        assert_eq!(envelope.payload, delta);

        let mut gateway = RealtimeGateway::new();
        let mut client = gateway.connect_websocket(1);
        assert!(matches!(
            client.recv(),
            Some(WebSocketMessage::Connected { player_id: 1 })
        ));

        gateway
            .receive_nats(&nats.messages[0].1)
            .expect("gateway consumes NATS payload");
        assert_eq!(client.recv(), Some(WebSocketMessage::Delta(delta.clone())));

        let gap_delta = super::RealtimeDelta {
            tick: 3,
            last_tick: 2,
            ..delta
        };
        let payload = serde_json::to_vec(&super::RealtimeEnvelope {
            schema: "swarm.realtime.v1".to_string(),
            payload: gap_delta.clone(),
        })
        .expect("serialize gap delta");
        gateway
            .receive_nats(&payload)
            .expect("gateway consumes gap payload");
        assert_eq!(
            client.recv(),
            Some(WebSocketMessage::TickGap {
                expected_tick: 2,
                actual_tick: 3,
            })
        );
        assert_eq!(client.fetch_from_tick(), Some(2));
        assert_eq!(client.recv(), Some(WebSocketMessage::Delta(gap_delta)));
    }

    #[test]
    fn snapshot_websocket_and_replay_share_visible_entity_filter() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let visible_enemy = world.spawn_drone(2, 12, 10, vec![BodyPart::Move]);
        let hidden_enemy = world.spawn_drone(2, 40, 40, vec![BodyPart::Move]);

        let snapshot = crate::swarm_get_snapshot(
            &mut world,
            crate::McpContext {
                player_id: 1,
                tick: 9,
            },
        );
        let before = Vec::new();
        let delta = compute_realtime_delta(&mut world, 1, 9, 8, &before);
        let trace = crate::TickTrace {
            tick: 9,
            player_id: 1,
            commands: Vec::new(),
            state: crate::TickState::capture(world.app.world_mut()),
            rejections: Vec::new(),
            metrics: crate::TickMetrics::default(),
            state_checksum: world.state_checksum(),
            system_manifest_hash: [0; 32],
            action_manifest_hash: [0; 32],
            security_alerts: Vec::new(),
            trace_events: Vec::new(),
        };
        let replay_entities = replay_visible_entities(&trace, 1);

        let snapshot_ids = snapshot.entities.iter().map(entity_id).collect::<Vec<_>>();
        let ws_ids = delta
            .changed_entities
            .iter()
            .map(entity_id)
            .collect::<Vec<_>>();
        let replay_ids = replay_entities.iter().map(entity_id).collect::<Vec<_>>();

        assert!(snapshot_ids.contains(&object_id(visible_enemy)));
        assert!(!snapshot_ids.contains(&object_id(hidden_enemy)));
        assert_eq!(snapshot_ids, ws_ids);
        assert_eq!(snapshot_ids, replay_ids);
    }
}
